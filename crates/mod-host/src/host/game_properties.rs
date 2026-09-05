use std::{
    borrow::Cow,
    ffi::c_char,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use eyre::ContextCompat;
use from_singleton::FromSingleton;
use me3_mod_host_types::{
    dlrf::RuntimeClassEntry,
    string::{custom::DlCustomUtf16Str, DlUtf16String},
    tree::{Tree, TreeMap},
};
use me3_mod_protocol::Game;
use rdvec::Vec as _;
use tracing::{instrument, Span};

use crate::{
    deferred::{defer_init, Deferred},
    host::ModHost,
};

type SetGameProperty = unsafe extern "C" fn(*const c_char, *const c_char);

static SYSTEM_PROPERTIES_APPLIED: AtomicBool = AtomicBool::new(false);
static DEBUG_PROPERTIES_APPLIED: AtomicBool = AtomicBool::new(false);

pub fn start_offline() {
    ModHost::get_attached()
        .override_game_property("Menu.IsEnableOnlineMode", "false")
        .unwrap();
}

#[instrument(skip_all)]
pub fn attach_override(
    game: Game,
    runtime_classes: &[RuntimeClassEntry<'_>],
    no_steam: bool,
) -> Result<(), eyre::Error> {
    let set_game_prop = override_debug_properties(game, runtime_classes)?;
    override_system_properties(game)?;

    if no_steam {
        let span = Span::current();

        std::thread::spawn(move || {
            // Keep the original hooks as the primary path. The worker only catches
            // up when late attach missed one or both one-shot property-init events.
            // Poll a concrete singleton-readiness signal rather than assuming the
            // host attach timestamp itself means properties are initialized.
            const POLL_INTERVAL_MS: u64 = 25;
            const MAX_WAIT_MS: u64 = 10_000;
            let max_polls = MAX_WAIT_MS / POLL_INTERVAL_MS;

            for _ in 0..max_polls {
                if SYSTEM_PROPERTIES_APPLIED.load(Ordering::Acquire) {
                    break;
                }

                if span.in_scope(|| apply_system_properties(game, "no-steam-catch-up")) {
                    break;
                }

                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            }

            if !SYSTEM_PROPERTIES_APPLIED.load(Ordering::Acquire) {
                tracing::warn!(
                    max_wait_ms = MAX_WAIT_MS,
                    "No-Steam property catch-up: system properties did not become ready"
                );
                return;
            }

            // Give the original debug-property init hook the first opportunity to
            // fire after system properties are known to exist. If it was already
            // missed, apply the same override set through SetGameProperty.
            for _ in 0..40 {
                if DEBUG_PROPERTIES_APPLIED.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            }

            if !DEBUG_PROPERTIES_APPLIED.load(Ordering::Acquire) {
                span.in_scope(|| {
                    apply_debug_properties(set_game_prop, "no-steam-catch-up");
                });
            }

            tracing::info!(
                system_applied = SYSTEM_PROPERTIES_APPLIED.load(Ordering::Acquire),
                debug_applied = DEBUG_PROPERTIES_APPLIED.load(Ordering::Acquire),
                "No-Steam property catch-up complete"
            );
        });
    }

    Ok(())
}

#[instrument(skip_all)]
fn override_debug_properties(
    game: Game,
    runtime_classes: &[RuntimeClassEntry<'_>],
) -> Result<SetGameProperty, eyre::Error> {
    let capi_name = if game < Game::EldenRing {
        "SprjAutoControlAPI"
    } else {
        "CSAutoControlAPI"
    };

    let capi_class = runtime_classes
        .iter()
        .find(|entry| entry.class.name == capi_name)
        .wrap_err_with(|| format!("failed to find runtime class for {capi_name}"))?
        .class;

    let set_game_prop_resolver = capi_class
        .methods
        .iter()
        .find(|m| m.name == "SetGameProperty")
        .wrap_err("SetGameProperty method not found")?
        .resolver;

    let set_game_prop_addr = set_game_prop_resolver
        .invokers
        .first()
        .wrap_err("SetGameProperty has no method invokers")?
        .addr;

    tracing::debug!(?set_game_prop_addr);

    let set_game_prop: SetGameProperty = unsafe { std::mem::transmute(set_game_prop_addr) };

    defer_init(Span::current(), Deferred::AfterDbgPropsInit, move || {
        apply_debug_properties(set_game_prop, "deferred-hook");
    })?;

    Ok(set_game_prop)
}

fn apply_debug_properties(set_game_prop: SetGameProperty, source: &'static str) {
    if DEBUG_PROPERTIES_APPLIED.swap(true, Ordering::AcqRel) {
        return;
    }

    let overrides = ModHost::get_attached()
        .property_overrides
        .lock()
        .expect("poisoned");

    tracing::debug!("applying game property overrides (user has priority): {overrides:#?}");
    for (property, value) in overrides.internal.iter().chain(overrides.user.iter()) {
        unsafe { set_game_prop(property.as_ptr(), value.as_ptr()) }
    }

    tracing::info!(source, "game debug property overrides applied");
}

#[instrument(skip_all)]
fn override_system_properties(game: Game) -> Result<(), eyre::Error> {
    defer_init(Span::current(), Deferred::AfterSysPropsInit, move || {
        if !apply_system_properties(game, "deferred-hook") {
            tracing::error!("system property mapping is uninitialized or was not found");
        }
    })
}

fn apply_system_properties(game: Game, source: &'static str) -> bool {
    if SYSTEM_PROPERTIES_APPLIED.load(Ordering::Acquire) {
        return true;
    }

    // address_of() returns None while the static singleton pointer is still null.
    // Do not construct/dereference PropertyMap until that concrete readiness signal
    // exists. A compare_exchange then guarantees only one path mutates the map.
    if !PropertyMap::singleton_present(game) {
        return false;
    }

    if SYSTEM_PROPERTIES_APPLIED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return true;
    }

    let Some(mut system_properties) = (unsafe { PropertyMap::from_singleton(game) }) else {
        SYSTEM_PROPERTIES_APPLIED.store(false, Ordering::Release);
        return false;
    };

    tracing::debug!(
        "found system properties at {:016x}",
        system_properties.addr()
    );

    let overrides = ModHost::get_attached()
        .property_overrides
        .lock()
        .expect("poisoned");

    for (property, value) in overrides.internal.iter().chain(overrides.user.iter()) {
        // Property value pairs are sourced from Rust &str.
        system_properties.insert(property.to_str().unwrap(), value.to_str().unwrap());
    }

    tracing::info!(source, "game system property overrides applied");
    true
}

#[repr(C)]
struct SystemProperties<T> {
    _vtable: usize,
    properties: NonNull<TreeMap<T, T>>,
}

#[repr(transparent)]
struct SprjSystemProperties(SystemProperties<DlUtf16String>);

#[repr(transparent)]
struct CSSystemProperties<T>(SystemProperties<T>);

enum PropertyMap<'a> {
    String(&'a mut dyn Tree<DlUtf16String, DlUtf16String>),
    Custom(&'a mut dyn Tree<DlCustomUtf16Str, DlCustomUtf16Str>),
}

impl<'a> PropertyMap<'a> {
    fn singleton_present(game: Game) -> bool {
        match game {
            Game::DarkSouls3 | Game::Sekiro => {
                from_singleton::address_of::<SprjSystemProperties>().is_some()
            }
            Game::EldenRing | Game::ArmoredCore6 => {
                from_singleton::address_of::<CSSystemProperties<DlUtf16String>>().is_some()
            }
            Game::Nightreign => {
                from_singleton::address_of::<CSSystemProperties<DlCustomUtf16Str>>().is_some()
            }
        }
    }

    unsafe fn from_singleton(game: Game) -> Option<PropertyMap<'a>> {
        match game {
            Game::DarkSouls3 | Game::Sekiro => unsafe {
                SprjSystemProperties::get_mut_dyn_map().map(PropertyMap::String)
            },
            Game::EldenRing | Game::ArmoredCore6 => unsafe {
                CSSystemProperties::get_mut_dyn_map().map(PropertyMap::String)
            },
            Game::Nightreign => unsafe {
                CSSystemProperties::get_mut_dyn_map().map(PropertyMap::Custom)
            },
        }
    }

    fn insert(&mut self, property: &str, value: &str) {
        match self {
            Self::String(map) => map.insert(property.into(), value.into()),
            Self::Custom(map) => map.insert(property.into(), value.into()),
        }
    }

    fn addr(&self) -> usize {
        match self {
            Self::String(tree) => (&raw const *tree).addr(),
            Self::Custom(tree) => (&raw const *tree).addr(),
        }
    }
}

trait PropertySingleton<T>: FromSingleton + Sized + 'static {
    fn as_mut_dyn_map(&mut self) -> &mut dyn Tree<T, T>;

    unsafe fn get_mut_dyn_map<'a>() -> Option<&'a mut dyn Tree<T, T>> {
        let instance = unsafe { from_singleton::address_of::<Self>()?.as_mut() };
        Some(instance.as_mut_dyn_map())
    }
}

impl PropertySingleton<DlUtf16String> for SprjSystemProperties {
    fn as_mut_dyn_map(&mut self) -> &mut dyn Tree<DlUtf16String, DlUtf16String> {
        unsafe { self.0.properties.as_mut().as_mut_dyn() }
    }
}

impl<T: PartialOrd + 'static> PropertySingleton<T> for CSSystemProperties<T> {
    fn as_mut_dyn_map(&mut self) -> &mut dyn Tree<T, T> {
        unsafe { self.0.properties.as_mut().as_mut_dyn() }
    }
}

impl FromSingleton for SprjSystemProperties {}

impl<T> FromSingleton for CSSystemProperties<T> {
    fn name() -> std::borrow::Cow<'static, str> {
        Cow::Borrowed("CSSystemProperties")
    }
}
