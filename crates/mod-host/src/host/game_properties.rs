use std::{
    borrow::Cow,
    ffi::c_char,
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
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
static SET_GAME_PROPERTY: OnceLock<SetGameProperty> = OnceLock::new();

pub fn start_offline() {
    ModHost::get_attached()
        .override_game_property("Menu.IsEnableOnlineMode", "false")
        .unwrap();
}

#[instrument(skip_all)]
pub fn attach_override(
    game: Game,
    runtime_classes: &[RuntimeClassEntry<'_>],
) -> Result<(), eyre::Error> {
    override_debug_properties(game, runtime_classes)?;
    override_system_properties(game)?;

    Ok(())
}

/// Catch up property overrides from a game-owned startup callback.
///
/// The ordinary AfterSysPropsInit / AfterDbgPropsInit hooks remain the primary
/// path. No-Steam late attach can miss those one-shot events, so FileStep calls
/// this after its original STEP_Init trampoline returns. This avoids mutating
/// game property structures or calling SetGameProperty from an arbitrary worker
/// thread while the game is running.
pub fn catch_up_no_steam_from_file_step(game: Game) -> bool {
    let system_applied = apply_system_properties(game, "no-steam-file-step");

    if system_applied
        && !DEBUG_PROPERTIES_APPLIED.load(Ordering::Acquire)
        && let Some(set_game_prop) = SET_GAME_PROPERTY.get().copied()
    {
        apply_debug_properties(set_game_prop, "no-steam-file-step");
    }

    let debug_applied = DEBUG_PROPERTIES_APPLIED.load(Ordering::Acquire);

    tracing::info!(
        system_applied,
        debug_applied,
        "No-Steam property catch-up evaluated from FileStep"
    );

    system_applied && debug_applied
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
    let _ = SET_GAME_PROPERTY.set(set_game_prop);

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
