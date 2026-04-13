//! Opaque `SdrCore` handle that crosses the C ABI.
//!
//! From the C side, `SdrCore` is a forward-declared incomplete type
//! (`typedef struct SdrCore SdrCore;` in `include/sdr_core.h`).
//! Callers only ever hold a `SdrCore*` and pass it back to FFI
//! functions; they never dereference it directly.
//!
//! The actual Rust struct lives here. The FFI surface boxes it on
//! `sdr_core_create`, hands the host a raw pointer, and reclaims it
//! on `sdr_core_destroy`. Between those two calls the host owns the
//! pointer (in the C sense — the Rust `Box` is leaked into raw form).

use std::sync::Mutex;

use sdr_core::Engine;

/// Opaque handle the C ABI hands to consumers.
///
/// Wraps the headless [`Engine`] plus FFI-only state (the registered
/// event callback, the dispatcher thread join handle if/when we
/// retain one, the FFT pull buffer, …). The C side never sees inside
/// this struct.
pub struct SdrCore {
    /// The headless engine. Consumed by `Engine::shutdown` on destroy.
    pub(crate) engine: Engine,

    /// Registered event callback + user_data, set by
    /// `sdr_core_set_event_callback`. Wrapped in `Mutex<Option<_>>`
    /// because the dispatcher thread reads it from another thread.
    /// `None` until the host registers a callback.
    pub(crate) event_callback: Mutex<Option<EventCallbackSlot>>,

    /// Path the host provided to `sdr_core_create`. Stored for future
    /// config-persistence wiring (the v1 engine doesn't load it yet —
    /// see the M1 spec deviation note in
    /// `crates/sdr-core/src/engine.rs`). Holding it here means the
    /// path threads through the FFI surface in v1 even before the
    /// engine consumes it, so adding persistence in a follow-up PR
    /// doesn't require an ABI change.
    pub(crate) config_path: std::path::PathBuf,
}

/// Bundle of `(callback fn pointer, user_data void*)` so the
/// dispatcher thread can fire the callback with the host's context.
///
/// `user_data` is treated as opaque on our side: we never deref it,
/// just hand it back to the callback. Wrapping in a struct lets us
/// derive `Send`-by-construction (see the unsafe impl below).
pub(crate) struct EventCallbackSlot {
    pub callback: crate::event::SdrEventCallback,
    pub user_data: *mut std::ffi::c_void,
}

// SAFETY: `EventCallbackSlot` holds a function pointer (always Send)
// and a `*mut c_void` whose ownership is the *host's* responsibility.
// We never dereference `user_data` from Rust; we only pass it back to
// the callback. The host is contractually required to ensure that
// whatever lives behind `user_data` is safe to access from the
// dispatcher thread (the same way GTK requires its main-context
// closures to be Send-friendly).
unsafe impl Send for EventCallbackSlot {}

impl SdrCore {
    /// Construct from a successfully-built [`Engine`] and the
    /// host-provided config path.
    pub(crate) fn new(engine: Engine, config_path: std::path::PathBuf) -> Self {
        Self {
            engine,
            event_callback: Mutex::new(None),
            config_path,
        }
    }

    /// Validate a raw `SdrCore *` from the C ABI and return a typed
    /// reference. Returns `None` (caller maps to `InvalidHandle`)
    /// when the pointer is null.
    ///
    /// # Safety
    ///
    /// The caller asserts that `ptr` either points to a valid
    /// `SdrCore` produced by `sdr_core_create` and not yet destroyed,
    /// or is null. Use-after-free or double-free is on the C-side
    /// caller, not on us.
    pub(crate) unsafe fn from_raw<'a>(ptr: *const SdrCore) -> Option<&'a SdrCore> {
        if ptr.is_null() { None } else { unsafe { Some(&*ptr) } }
    }

    /// Mutable variant of [`Self::from_raw`].
    ///
    /// # Safety
    ///
    /// Same contract as `from_raw`, plus the caller asserts there is
    /// no concurrent borrow of `*ptr`. The Rust side never holds
    /// a `&mut SdrCore` across an FFI boundary, so concurrency is
    /// the host's responsibility — this is the standard "C side
    /// owns aliasing rules" model.
    pub(crate) unsafe fn from_raw_mut<'a>(ptr: *mut SdrCore) -> Option<&'a mut SdrCore> {
        if ptr.is_null() { None } else { unsafe { Some(&mut *ptr) } }
    }
}
