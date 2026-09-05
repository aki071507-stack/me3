use std::{
    ffi::c_void,
    io::Write,
    iter, mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsHandle, AsRawHandle, OwnedHandle},
        process::{ChildExt, CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use eyre::{eyre, Context};
use me3_env::{deserialize_from_env, serialize_into_command, TelemetryVars};
use me3_ipc::{bridge::BridgeToChild, message::MsgToParent, request::Response};
use me3_launcher_attach_protocol::{AttachRequest, Attachment};
use tracing::{error, info, instrument};
use tracing_subscriber::fmt::MakeWriter;
use windows::{
    core::{s, w, Error as WinError},
    Win32::{
        Foundation::{CloseHandle, ERROR_ELEVATION_REQUIRED, HANDLE, WAIT_OBJECT_0, WIN32_ERROR},
        System::{
            Diagnostics::Debug::WriteProcessMemory,
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE},
            Threading::{
                CreateRemoteThread, GetExitCodeThread, ResumeThread, WaitForSingleObject,
                CREATE_SUSPENDED, INFINITE,
            },
        },
    },
};

use crate::{writer::MakeWriterWrapper, LauncherResult};

pub struct Game {
    pub(crate) child: std::process::Child,
    pub(crate) bridge: Arc<BridgeToChild>,
}

impl Game {
    #[instrument(skip_all, err)]
    pub fn launch(
        game_binary: &Path,
        args: Vec<String>,
        game_directory: Option<&Path>,
    ) -> LauncherResult<Self> {
        let mut command = Command::new(game_binary);
        command.current_dir(
            game_directory
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or(PathBuf::from(".")),
        );

        let mut telemetry_vars: TelemetryVars = deserialize_from_env()?;
        telemetry_vars.trace_id = me3_telemetry::trace_id();

        info!(trace_id = ?telemetry_vars.trace_id);
        serialize_into_command(telemetry_vars, &mut command);

        command.args(args);
        command.creation_flags(CREATE_SUSPENDED.0);

        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let bridge = Arc::new(me3_ipc::bridge::to_child(32, &mut command)?);

        let child = command.spawn().map_err(|e| match e.raw_os_error().map(|i| WIN32_ERROR(i as u32)) {
            Some(ERROR_ELEVATION_REQUIRED) => eyre!(
                "Elevation is required to launch the game. Disable \"Run this program as an administrator\" and try again."
            ),
            _ => e.into()
        })?;

        Ok(Self { child, bridge })
    }

    #[instrument(skip_all, err)]
    pub fn attach(
        &self,
        dll_path: &Path,
        console_log: MakeWriterWrapper,
        file_log: MakeWriterWrapper,
        attach_request: AttachRequest,
    ) -> LauncherResult<Attachment> {
        let pid = self.child.id();

        info!(pid, "attaching to process");

        self.spawn_msg_thread(console_log, file_log);

        let thread_handle = self.child.main_thread_handle();
        let process_handle = self.child.as_handle().try_clone_to_owned()?;

        let no_steam_diag_stage = if attach_request.config.no_steam {
            std::env::var("ME3_NO_STEAM_DIAG_STAGE").unwrap_or_default()
        } else {
            String::new()
        };

        if no_steam_diag_stage == "resume-only" {
            unsafe {
                ResumeThread(HANDLE(thread_handle.as_raw_handle()));
            }

            info!("No-Steam diagnostic stage=resume-only: resumed without DLL injection");
            return Ok(Attachment);
        }

        let missing_probe_path = Path::new(r"C:\__ME3_NO_STEAM_DIAG_MISSING__.dll");

        if no_steam_diag_stage == "probe-only" {
            let load_result = inject_dll(&process_handle, missing_probe_path)
                .wrap_err("failed to execute missing-DLL remote-thread probe")?;

            info!(
                load_result,
                "No-Steam diagnostic stage=probe-only: remote LoadLibraryW missing-DLL probe completed"
            );

            unsafe {
                ResumeThread(HANDLE(thread_handle.as_raw_handle()));
            }

            return Ok(Attachment);
        }

        let production_no_steam_late_attach =
            attach_request.config.no_steam && no_steam_diag_stage.is_empty();

        let delayed_stage = production_no_steam_late_attach
            || matches!(
                no_steam_diag_stage.as_str(),
                "late-probe-only"
                    | "late-load-only"
                    | "late-dll-only"
                    | "late-host-only"
                    | "late-dearxan-only"
                    | "late-filesystem-only"
                    | "late-full"
                    | "late-full-immediate"
            );

        if delayed_stage {
            unsafe {
                ResumeThread(HANDLE(thread_handle.as_raw_handle()));
            }

            let stage_name = if production_no_steam_late_attach {
                "no-steam-default"
            } else {
                no_steam_diag_stage.as_str()
            };

            info!(
                stage = stage_name,
                "No-Steam: process resumed before delayed injection"
            );

            let delay_ms = std::env::var("ME3_NO_STEAM_ATTACH_DELAY_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(8_000);

            info!(
                stage = stage_name,
                delay_ms,
                "No-Steam: waiting before delayed injection"
            );

            std::thread::sleep(std::time::Duration::from_millis(delay_ms));

            let target_path = if no_steam_diag_stage == "late-probe-only" {
                missing_probe_path
            } else {
                dll_path
            };

            let load_result = inject_dll(&process_handle, target_path)
                .wrap_err("failed delayed remote LoadLibraryW probe")?;

            info!(
                stage = stage_name,
                load_result,
                "No-Steam: delayed remote LoadLibraryW completed"
            );

            if no_steam_diag_stage == "late-probe-only"
                || no_steam_diag_stage == "late-load-only"
            {
                return Ok(Attachment);
            }

            let response = self
                .bridge
                .request(attach_request)?
                .map_err(|e| eyre!(e.0))?;

            info!(
                stage = stage_name,
                "No-Steam: delayed host attach request completed"
            );

            return Ok(response);
        }

        let load_result =
            inject_dll(&process_handle, dll_path).wrap_err("failed to inject mod host DLL")?;

        info!(
            load_result,
            "No-Steam diagnostic: remote LoadLibraryW for mod host completed"
        );

        if no_steam_diag_stage == "load-only" {
            unsafe {
                ResumeThread(HANDLE(thread_handle.as_raw_handle()));
            }

            info!(
                "No-Steam diagnostic stage=load-only: DLL load attempted, attach request skipped, process resumed"
            );
            return Ok(Attachment);
        }

        if attach_request.config.suspend {
            info!("Process will be suspended until a debugger is attached...");
        }

        let response = self
            .bridge
            .request(attach_request)?
            .map_err(|e| eyre!(e.0))?;

        unsafe {
            ResumeThread(HANDLE(thread_handle.as_raw_handle()));
        }

        info!("Successfully attached");

        Ok(response)
    }

    pub fn join(mut self) {
        let _ = self.child.wait();
    }

    fn spawn_msg_thread(&self, console_log: MakeWriterWrapper, file_log: MakeWriterWrapper) {
        let bridge = self.bridge.clone();
        std::thread::spawn(move || {
            let recv_span = bridge.enter_recv_span().unwrap();

            loop {
                let msg = match recv_span.recv() {
                    Ok(msg) => msg,
                    Err(error) => {
                        error!(%error, "failed to receive message");
                        continue;
                    }
                };

                match msg {
                    MsgToParent::Response(res) => Response::forward(res),
                    MsgToParent::ConsoleLog(s) => {
                        let _ = console_log.make_writer().write_all(s.as_bytes());
                    }
                    MsgToParent::FileLog(s) => {
                        let _ = file_log.make_writer().write_all(s.as_bytes());
                    }
                }
            }
        });
    }
}

fn inject_dll(process: &OwnedHandle, path: &Path) -> LauncherResult<u32> {
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(b'\0' as u16))
        .collect::<Vec<_>>();

    unsafe {
        let process_handle = HANDLE(process.as_raw_handle());

        let kernel32 = GetModuleHandleW(w!("kernel32.dll"))?;
        let load_library = GetProcAddress(kernel32, s!("LoadLibraryW"));

        let path_str = VirtualAllocEx(
            process_handle,
            None,
            path.len() * 2,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );

        WriteProcessMemory(
            process_handle,
            path_str,
            path.as_ptr() as *const c_void,
            path.len() * 2,
            None,
        )?;

        let thread = CreateRemoteThread(
            process_handle,
            None,
            0,
            Some(mem::transmute(load_library)),
            Some(path_str),
            0,
            None,
        )?;

        if WaitForSingleObject(thread, INFINITE) != WAIT_OBJECT_0 {
            return Err(WinError::from_thread().into());
        }

        let mut load_result = 0u32;
        GetExitCodeThread(thread, &mut load_result)?;

        CloseHandle(thread)?;

        Ok(load_result)
    }
}
