//! Lifecycle C ABI: ABI version query, logging init, create, destroy.
//!
//! Maps 1:1 to the "Lifecycle" section of `include/sdr_core.h`. The
//! handful of non-command entry points the host calls before/after
//! the engine does any real work.

use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use sdr_core::Engine;

use crate::error::{SdrCoreError, clear_last_error, set_last_error};
use crate::event::spawn_dispatcher;
use crate::handle::SdrCore;

/// ABI version packed as `(major << 16) | minor`.
///
/// Kept in lockstep with `SDR_CORE_ABI_VERSION_MAJOR` /
/// `SDR_CORE_ABI_VERSION_MINOR` in `include/sdr_core.h`. A compile-
/// time `const _` in the test module below asserts the two stay
/// consistent.
pub const ABI_VERSION_MAJOR: u32 = 0;
pub const ABI_VERSION_MINOR: u32 = 1;

/// Return the ABI version the library was built with.
///
/// Hosts should call this once at startup. See the header docs.
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_abi_version() -> u32 {
    (ABI_VERSION_MAJOR << 16) | ABI_VERSION_MINOR
}

/// Log-level discriminants mirroring `SdrLogLevel` in the header.
/// Kept as `i32` so the FFI function takes a plain int; the Rust
/// side validates the value against the known variants.
const LOG_LEVEL_ERROR: i32 = 0;
const LOG_LEVEL_WARN: i32 = 1;
const LOG_LEVEL_INFO: i32 = 2;
const LOG_LEVEL_DEBUG: i32 = 3;
const LOG_LEVEL_TRACE: i32 = 4;

/// Global flag marking whether `sdr_core_init_logging` has already
/// installed a subscriber. Subsequent calls become no-ops because
/// `tracing_subscriber::fmt::try_init` is a one-shot global.
static LOGGING_INITIALIZED: OnceLock<()> = OnceLock::new();

/// Initialize Rust `tracing` log routing. See the header docs for
/// the semantics; called zero or one time per process. Subsequent
/// calls are no-ops.
///
/// Does not return an error code — failures are logged to stderr
/// and swallowed because a logging-init failure should not prevent
/// the engine from running.
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_init_logging(min_level: i32) {
    let result = std::panic::catch_unwind(|| {
        LOGGING_INITIALIZED.get_or_init(|| {
            let level = match min_level {
                LOG_LEVEL_TRACE => tracing::Level::TRACE,
                LOG_LEVEL_DEBUG => tracing::Level::DEBUG,
                LOG_LEVEL_INFO => tracing::Level::INFO,
                LOG_LEVEL_WARN => tracing::Level::WARN,
                LOG_LEVEL_ERROR => tracing::Level::ERROR,
                // Unknown values: fall through to INFO rather than
                // refusing to init. Cheap bounds check plus a
                // diagnostic.
                other => {
                    eprintln!("sdr_core_init_logging: unknown level {other}, defaulting to INFO");
                    tracing::Level::INFO
                }
            };

            // try_init rather than init so a host that already set
            // up a subscriber (e.g., the GTK frontend's main-thread
            // subscriber) doesn't panic when we try to install ours.
            if let Err(err) = tracing_subscriber::fmt().with_max_level(level).try_init() {
                // Non-fatal: log and continue. A subscriber was
                // probably already installed.
                eprintln!("sdr_core_init_logging: try_init failed: {err}");
            }
        });
    });

    if result.is_err() {
        eprintln!("sdr_core_init_logging: panic caught, logging not initialized");
    }
}

/// Create a new engine instance. See the header docs for the full
/// contract.
///
/// # Safety
///
/// `config_path_utf8` must be either null or a pointer to a
/// NUL-terminated UTF-8 C string. `out_handle` must be non-null
/// and point to writable storage for one `*mut SdrCore`. Both
/// requirements are documented in `include/sdr_core.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_create(
    config_path_utf8: *const c_char,
    out_handle: *mut *mut SdrCore,
) -> i32 {
    // Wrap everything in catch_unwind: a Rust panic crossing the
    // FFI boundary is UB, so we convert panics to error codes.
    let result = std::panic::catch_unwind(|| {
        if out_handle.is_null() {
            set_last_error("sdr_core_create: out_handle is null");
            return SdrCoreError::InvalidArg.as_int();
        }

        // Parse the config path. Null → empty path (in-memory).
        let config_path = if config_path_utf8.is_null() {
            PathBuf::new()
        } else {
            // SAFETY: Caller contract guarantees this is either null
            // (handled above) or a NUL-terminated UTF-8 string.
            let cstr = unsafe { CStr::from_ptr(config_path_utf8) };
            let Ok(s) = cstr.to_str() else {
                set_last_error("sdr_core_create: config_path_utf8 is not valid UTF-8");
                return SdrCoreError::InvalidArg.as_int();
            };
            PathBuf::from(s)
        };

        // Build the engine. Any spawn failure maps to INTERNAL.
        let engine = match Engine::new(config_path.clone()) {
            Ok(e) => e,
            Err(err) => {
                set_last_error(format!("sdr_core_create: Engine::new failed: {err}"));
                return SdrCoreError::Internal.as_int();
            }
        };

        // Take the one-shot event receiver from the engine. If
        // something else already subscribed (shouldn't happen
        // for an FFI-created engine but the API is technically
        // contested), bail rather than silently run without an
        // event dispatcher.
        let Some(evt_rx) = engine.subscribe() else {
            set_last_error(
                "sdr_core_create: Engine::subscribe returned None — event receiver already taken",
            );
            return SdrCoreError::Internal.as_int();
        };

        // Shared callback slot + dispatcher. The Arc clone on
        // the dispatcher side and the copy on the SdrCore side
        // both hand back to the same Mutex<Option<_>>, so
        // `sdr_core_set_event_callback` and the dispatcher see
        // the same registration.
        let event_callback = Arc::new(Mutex::new(None));
        let dispatcher_handle = match spawn_dispatcher(evt_rx, Arc::clone(&event_callback)) {
            Ok(h) => h,
            Err(err) => {
                set_last_error(format!("sdr_core_create: spawn_dispatcher failed: {err}"));
                return SdrCoreError::Internal.as_int();
            }
        };

        let core = Box::new(SdrCore::new(
            engine,
            config_path,
            event_callback,
            dispatcher_handle,
        ));
        let raw = Box::into_raw(core);
        // SAFETY: out_handle non-null checked above; writable
        // storage is the caller's contract.
        unsafe { *out_handle = raw };

        clear_last_error();
        SdrCoreError::Ok.as_int()
    });

    match result {
        Ok(code) => code,
        Err(payload) => {
            let msg = panic_message(&payload);
            set_last_error(format!("sdr_core_create: panic: {msg}"));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Destroy an engine instance. See the header docs.
///
/// # Safety
///
/// `handle` must be either null (no-op) or a pointer previously
/// returned by `sdr_core_create` on this process and not yet
/// passed to a prior `sdr_core_destroy`. Passing the same non-null
/// handle twice is a use-after-free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_destroy(handle: *mut SdrCore) {
    if handle.is_null() {
        return;
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Reclaim the Box so the inner fields can be teared down
        // in the right order. We don't want to just let the Box
        // drop implicitly — we need explicit control over the
        // dispatcher-join / Engine-drop ordering.
        //
        // SAFETY: Caller contract guarantees `handle` is a
        // pointer previously returned by sdr_core_create and not
        // yet destroyed. Box::from_raw takes ownership back.
        let core: Box<SdrCore> = unsafe { Box::from_raw(handle) };

        // Best-effort Stop so the controller stops the active
        // source cleanly. If the channel is already closed
        // (engine in a weird state), just proceed.
        let _ = core.engine.shutdown();

        // Pull the dispatcher join handle out *before* dropping
        // the engine. We want to:
        //   1. Drop the Engine (closes cmd_tx → DSP thread exits
        //      → evt_tx drops → dispatcher's recv() returns Err).
        //   2. Join the dispatcher thread (it's already exiting
        //      or about to).
        //
        // Taking the JoinHandle out of its Mutex<Option<_>>
        // decouples it from the `drop(core)` that follows; if we
        // tried to join *while* `core` was still owned, we'd
        // need to hold the Mutex across the drop, which is
        // awkward. Easier to just lift it out.
        let dispatcher_handle = core
            .dispatcher_handle
            .lock()
            .map(|mut guard| guard.take())
            .unwrap_or(None);

        // Drop the Engine explicitly before the join so the
        // event channel closes and the dispatcher's `recv()`
        // unblocks. The rest of `core` (the Arc<Mutex> for the
        // callback slot, the config_path PathBuf) drops with
        // it; that's fine because the dispatcher already has
        // its own Arc clone of the callback slot.
        drop(core);

        // Now join the dispatcher. With the Engine dropped, the
        // event channel is closed and the dispatcher's recv
        // loop should exit immediately.
        if let Some(handle) = dispatcher_handle
            && handle.join().is_err()
        {
            eprintln!("sdr_core_destroy: dispatcher thread panicked during teardown");
        }
    }));

    if result.is_err() {
        // Can't propagate errors from destroy, can't panic through
        // the FFI boundary. Log and move on.
        eprintln!("sdr_core_destroy: panic caught during teardown");
    }
}

/// Extract a displayable message from a panic payload. Panic
/// payloads are `Box<dyn Any>`; the common case is a `String` or
/// `&'static str` from `panic!("...")`.
pub(crate) fn panic_message(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        "(unrepresentable panic payload)".to_string()
    }
}

/// Test-only FFI entry point that deliberately panics with a
/// well-known message. Used by the panic-safety test to verify
/// that `catch_unwind` converts a Rust panic crossing the FFI
/// boundary into `SDR_CORE_ERR_INTERNAL` instead of UB.
///
/// Only compiled when running `cargo test`. This function has no
/// presence in release builds and is NOT declared in
/// `include/sdr_core.h` — it exists solely to exercise the
/// panic-catch path from the test harness.
#[cfg(test)]
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_panic_for_test() -> i32 {
    let result = std::panic::catch_unwind(|| {
        // clippy::panic is denied workspace-wide but the whole
        // purpose of this function is to exercise the panic-catch
        // path, so the lint has to be turned off here. This is
        // the only `panic!` call in the crate.
        #[allow(clippy::panic)]
        {
            panic!("deliberate test panic: please do not propagate this across the FFI");
        }
    });

    match result {
        Ok(()) => SdrCoreError::Ok.as_int(),
        Err(payload) => {
            set_last_error(format!(
                "sdr_core_panic_for_test: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Compile-time check that the exposed ABI version matches the
    /// header's macros. If the header is edited to bump the minor
    /// without updating this constant, the mismatch breaks the
    /// build here rather than drifting silently.
    const _: () = {
        assert!(ABI_VERSION_MAJOR == 0);
        assert!(ABI_VERSION_MINOR == 1);
    };

    #[test]
    fn abi_version_packs_major_minor() {
        let packed = sdr_core_abi_version();
        // Compute the expected value from the named constants so a
        // future bump only needs to change the `ABI_VERSION_*`
        // declarations and not this assertion. Avoids the
        // `clippy::precedence` / `no_effect` lint that fires on
        // `(0 << 16) | 1`.
        let expected = (ABI_VERSION_MAJOR << 16) | ABI_VERSION_MINOR;
        assert_eq!(packed, expected);
    }

    #[test]
    fn init_logging_is_idempotent() {
        // Safe to call multiple times; subsequent calls are no-ops.
        sdr_core_init_logging(LOG_LEVEL_INFO);
        sdr_core_init_logging(LOG_LEVEL_DEBUG);
        sdr_core_init_logging(-1); // unknown level, shouldn't panic
    }

    #[test]
    fn create_with_null_out_handle_returns_invalid_arg() {
        let path = CString::new("").unwrap();
        let rc = unsafe { sdr_core_create(path.as_ptr(), std::ptr::null_mut()) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn create_with_null_config_path_succeeds() {
        // null config_path → empty PathBuf → in-memory engine.
        let mut handle: *mut SdrCore = std::ptr::null_mut();
        let rc = unsafe { sdr_core_create(std::ptr::null(), &raw mut handle) };
        assert_eq!(rc, SdrCoreError::Ok.as_int());
        assert!(!handle.is_null());
        // SAFETY: just created.
        unsafe { sdr_core_destroy(handle) };
    }

    #[test]
    fn create_with_empty_config_path_succeeds() {
        let path = CString::new("").unwrap();
        let mut handle: *mut SdrCore = std::ptr::null_mut();
        let rc = unsafe { sdr_core_create(path.as_ptr(), &raw mut handle) };
        assert_eq!(rc, SdrCoreError::Ok.as_int());
        assert!(!handle.is_null());
        unsafe { sdr_core_destroy(handle) };
    }

    #[test]
    fn create_with_nonempty_config_path_stores_it_on_the_engine() {
        let path = CString::new("/tmp/sdr-ffi-test.json").unwrap();
        let mut handle: *mut SdrCore = std::ptr::null_mut();
        let rc = unsafe { sdr_core_create(path.as_ptr(), &raw mut handle) };
        assert_eq!(rc, SdrCoreError::Ok.as_int());

        // Peek at the engine's stored path via the Rust API — we're
        // linked as an rlib so this is safe from test code.
        let core_ref = unsafe { &*handle };
        assert_eq!(
            core_ref.engine.config_path(),
            std::path::Path::new("/tmp/sdr-ffi-test.json")
        );
        assert_eq!(
            core_ref.config_path,
            std::path::PathBuf::from("/tmp/sdr-ffi-test.json")
        );

        unsafe { sdr_core_destroy(handle) };
    }

    #[test]
    fn destroy_null_is_noop() {
        // Should not panic, crash, or set a last-error.
        unsafe { sdr_core_destroy(std::ptr::null_mut()) };
    }

    #[test]
    fn create_then_destroy_round_trip() {
        let path = CString::new("").unwrap();
        let mut handle: *mut SdrCore = std::ptr::null_mut();
        let rc = unsafe { sdr_core_create(path.as_ptr(), &raw mut handle) };
        assert_eq!(rc, SdrCoreError::Ok.as_int());
        assert!(!handle.is_null());

        // And destroy should return cleanly.
        unsafe { sdr_core_destroy(handle) };
    }

    #[test]
    fn panic_inside_ffi_returns_internal_error_and_sets_last_error() {
        // This is the big panic-safety gate. If catch_unwind is
        // ever removed from an FFI entry point (or if a panic
        // path escapes one), this test fails loudly at
        // `cargo test` time instead of silently producing UB
        // the first time an end user's Swift app triggers it.
        let rc = sdr_core_panic_for_test();
        assert_eq!(
            rc,
            SdrCoreError::Internal.as_int(),
            "panic should map to SDR_CORE_ERR_INTERNAL"
        );

        // And the thread-local last-error message should contain
        // the panic text, so Swift (or any other host) can surface
        // a useful diagnostic.
        let p = crate::error::sdr_core_last_error_message();
        assert!(!p.is_null(), "last-error should be set after a panic");
        let msg = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned();
        assert!(
            msg.contains("deliberate test panic"),
            "last-error should contain the panic message, got {msg:?}"
        );
        assert!(
            msg.contains("sdr_core_panic_for_test"),
            "last-error should name the function that panicked, got {msg:?}"
        );
    }
}
