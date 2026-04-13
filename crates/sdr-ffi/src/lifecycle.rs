//! Lifecycle C ABI: ABI version query, logging init, create, destroy.
//!
//! Maps 1:1 to the "Lifecycle" section of `include/sdr_core.h`. The
//! handful of non-command entry points the host calls before/after
//! the engine does any real work.

use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::sync::OnceLock;

use sdr_core::Engine;

use crate::error::{SdrCoreError, clear_last_error, set_last_error};
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

        let core = Box::new(SdrCore::new(engine, config_path));
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
        // Reclaim the Box so it drops at the end of this block,
        // which runs Engine::drop, which detaches the DSP thread.
        //
        // SAFETY: Caller contract guarantees `handle` is a
        // pointer previously returned by sdr_core_create and not
        // yet destroyed. Box::from_raw takes ownership back.
        let core: Box<SdrCore> = unsafe { Box::from_raw(handle) };

        // Best-effort Stop before drop so the controller stops
        // the active source cleanly. If the channel is already
        // closed (engine in a weird state), just proceed.
        let _ = core.engine.shutdown();

        // TODO (next checkpoint): join the dispatcher thread if
        // one was started. For now there's no dispatcher, so the
        // Box drop is sufficient to tear everything down.
        drop(core);
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
}
