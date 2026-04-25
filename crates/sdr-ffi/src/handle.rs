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

use std::sync::{Arc, Condvar, Mutex};

use sdr_config::ConfigManager;
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

    /// Registered event callback + user_data + quiescence protocol.
    /// Wrapped in `Arc` so the dispatcher thread can hold a clone
    /// without borrowing the `SdrCore` handle directly.
    pub(crate) event_callback: Arc<EventCallbackGuard>,

    /// Path the host provided to `sdr_core_create`. Stored for
    /// diagnostics / future migrations — the live read/write path
    /// goes through `config` below, which owns a `ConfigManager`
    /// tied to this same file.
    ///
    /// `#[allow(dead_code)]`: read by `#[cfg(test)]` integration
    /// tests in `lifecycle.rs` (which assert `core.config_path`
    /// matches what `sdr_core_create` was called with) but not
    /// by non-test code. Clippy doesn't see test-only reads
    /// on the lib target.
    #[allow(dead_code)]
    pub(crate) config_path: std::path::PathBuf,

    /// Shared JSON config, tied to `config_path`. Built by
    /// `sdr_core_create` (loading the on-disk file or starting
    /// in-memory when `config_path` is empty) and owned for the
    /// lifetime of the handle. `Arc` so future consumers (an
    /// eventual engine-side config reader, the RadioReference
    /// keyring already uses `Arc<ConfigManager>`, …) can clone
    /// their own reference cheaply. Auto-save runs off a worker
    /// thread enabled at create time; the writer is joined
    /// automatically on `Drop` so ffi destroy doesn't need
    /// special handling.
    ///
    /// `None` when the host passed an empty path — in-memory
    /// mode, used by tests + by future embedders that want
    /// ephemeral config. FFI config read/write entry points
    /// return `InvalidArg` in that state so the host can
    /// distinguish "key absent" from "no config file at all".
    pub(crate) config: Option<Arc<ConfigManager>>,

    /// FFI event dispatcher thread join handle. Spawned at
    /// `sdr_core_create` time, joined at `sdr_core_destroy` so the
    /// teardown is deterministic. Stored in a `Mutex<Option<_>>` so
    /// destroy can `take()` the handle out for joining without
    /// needing a `&mut SdrCore`.
    pub(crate) dispatcher_handle: Mutex<Option<std::thread::JoinHandle<()>>>,

    /// FFI audio-tap dispatcher thread join handle. Distinct from
    /// `dispatcher_handle` (the event dispatcher) because the audio
    /// tap has a start/stop lifecycle — the thread is `Some` only
    /// between a successful `sdr_core_start_audio_tap` and the
    /// corresponding `sdr_core_stop_audio_tap` (or `destroy`, which
    /// stops the tap as part of teardown so dangling `user_data`
    /// never outlives the handle). Per issue #314.
    pub(crate) audio_tap_dispatcher: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Bundle of `(callback fn pointer, user_data void*)` so the
/// dispatcher thread can fire the callback with the host's context.
///
/// `user_data` is treated as opaque on our side: we never deref it,
/// just hand it back to the callback. Wrapping in a struct lets us
/// derive `Send`-by-construction (see the unsafe impl below).
#[allow(dead_code)] // fields read in the event-dispatcher checkpoint (later in this PR)
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

/// Mutex-protected callback slot + in-flight counter for quiescence.
///
/// When the dispatcher thread invokes the host callback, it increments
/// `in_flight` (under the mutex) before dropping the lock and calling
/// the host, then decrements it afterwards. When the host clears or
/// replaces the callback via `sdr_core_set_event_callback`, the setter
/// waits on the `quiesced` condvar until `in_flight` reaches zero
/// before returning — guaranteeing the old callback (and its
/// `user_data`) is no longer in use when the setter returns.
pub(crate) struct EventCallbackGuard {
    pub(crate) state: Mutex<EventCallbackState>,
    pub(crate) quiesced: Condvar,
}

pub(crate) struct EventCallbackState {
    pub slot: Option<EventCallbackSlot>,
    pub in_flight: usize,
}

// `ConfigManager` stores an `Option<JoinHandle<()>>` for its
// auto-save worker, and `JoinHandle` transitively contains an
// `UnsafeCell<Option<Result<(), PanicPayload>>>` that isn't
// `RefUnwindSafe` by default. The handle's other fields
// (`Engine`, `Mutex`es, `Arc`s) are all unwind-safe individually;
// the compound type just can't auto-derive. Asserting these
// traits by hand is correct because the FFI catch_unwind
// boundary only ever reads the handle — it doesn't observe
// partial mutations, and a panic can't leave the handle in an
// observably-broken state (the poison mechanism on `Mutex` /
// `RwLock` handles the lock-held case; the `JoinHandle` just
// owns a finished thread's result).
impl std::panic::UnwindSafe for SdrCore {}
impl std::panic::RefUnwindSafe for SdrCore {}

impl SdrCore {
    /// Construct from a successfully-built [`Engine`], the
    /// host-provided config path, and the spawned dispatcher
    /// thread handle.
    pub(crate) fn new(
        engine: Engine,
        config_path: std::path::PathBuf,
        config: Option<Arc<ConfigManager>>,
        event_callback: Arc<EventCallbackGuard>,
        dispatcher_handle: std::thread::JoinHandle<()>,
    ) -> Self {
        Self {
            engine,
            event_callback,
            config_path,
            config,
            dispatcher_handle: Mutex::new(Some(dispatcher_handle)),
            audio_tap_dispatcher: Mutex::new(None),
        }
    }

    /// Validate a raw `SdrCore *` from the C ABI and return a typed
    /// reference. Returns `None` (caller maps to `InvalidHandle`)
    /// when the pointer is null.
    ///
    /// Not yet called in production code — the command-function
    /// checkpoint later in this PR is the first consumer. Kept
    /// here (with `allow(dead_code)`) so the next checkpoint is a
    /// pure add rather than needing to introduce the helper at
    /// the same time as its first caller.
    ///
    /// # Safety
    ///
    /// The caller asserts that `ptr` either points to a valid
    /// `SdrCore` produced by `sdr_core_create` and not yet destroyed,
    /// or is null. Use-after-free or double-free is on the C-side
    /// caller, not on us.
    #[allow(dead_code)]
    pub(crate) unsafe fn from_raw<'a>(ptr: *const SdrCore) -> Option<&'a SdrCore> {
        if ptr.is_null() {
            None
        } else {
            unsafe { Some(&*ptr) }
        }
    }

    /// Mutable variant of [`Self::from_raw`]. See that function for
    /// the safety contract and the "not yet used in production code"
    /// note.
    ///
    /// # Safety
    ///
    /// Same contract as `from_raw`, plus the caller asserts there is
    /// no concurrent borrow of `*ptr`. The Rust side never holds
    /// a `&mut SdrCore` across an FFI boundary, so concurrency is
    /// the host's responsibility — this is the standard "C side
    /// owns aliasing rules" model.
    #[allow(dead_code)]
    pub(crate) unsafe fn from_raw_mut<'a>(ptr: *mut SdrCore) -> Option<&'a mut SdrCore> {
        if ptr.is_null() {
            None
        } else {
            unsafe { Some(&mut *ptr) }
        }
    }
}
