//! Config C ABI (ABI 0.21 / issue #449): key/value get + set against
//! the shared `sdr-config` JSON file the host passed to
//! `sdr_core_create`.
//!
//! The engine doesn't yet consume its own config (the GTK frontend
//! and the eventual Mac-native config readers both drive persistence
//! themselves through this surface). What lives behind this ABI:
//!
//!   - `get_string` / `set_string` — arbitrary UTF-8 values.
//!     Read path uses the standard size-then-fill pattern.
//!   - `get_bool`  / `set_bool`   — JSON bools.
//!   - `get_u32`   / `set_u32`    — JSON numbers, narrowed to `u32`.
//!
//! All getters return `KeyNotFound` (not `InvalidArg`) when the key
//! is absent or present with a mismatched type, so hosts can cleanly
//! branch on "use default" vs "surface error".
//!
//! The host-provided config path governs whether on-disk persistence
//! is active: an empty path at `sdr_core_create` time leaves
//! `core.config` as `None`, and every entry point in this module
//! returns `InvalidArg` with an explanatory last-error — no crash,
//! but the host is told it's operating in an ephemeral mode.
//!
//! Auto-save is enabled on the `ConfigManager` at create time so
//! writes through this surface land on disk without the host
//! calling any sync helper. The auto-save thread is joined on the
//! manager's `Drop`, which fires as part of `sdr_core_destroy`.

use std::ffi::{CStr, c_char};

use crate::error::{SdrCoreError, clear_last_error, set_last_error};
use crate::handle::SdrCore;
use crate::lifecycle::panic_message;

/// Shared boilerplate wrapper for every config entry point.
///
/// Validates the handle, rejects an empty-config (in-memory) handle
/// with `InvalidArg`, reads the key string, and hands the closure a
/// borrow of both. The closure returns a `Result<(), SdrCoreError>`
/// the same way the command-module `with_core` does.
///
/// # Safety
///
/// Same contract as `SdrCore::from_raw` plus `key_utf8` must be
/// either null (rejected as `InvalidArg`) or a pointer to a NUL-
/// terminated UTF-8 string for the duration of the call.
unsafe fn with_config<F>(handle: *mut SdrCore, key_utf8: *const c_char, f: F) -> i32
where
    F: FnOnce(&std::sync::Arc<sdr_config::ConfigManager>, &str) -> Result<(), SdrCoreError>
        + std::panic::UnwindSafe,
{
    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract matches SdrCore::from_raw.
        let Some(core) = (unsafe { SdrCore::from_raw(handle) }) else {
            set_last_error("sdr_core_config: null or invalid handle");
            return SdrCoreError::InvalidHandle.as_int();
        };
        let Some(config) = core.config.as_ref() else {
            set_last_error(
                "sdr_core_config: engine was created with an empty path (in-memory \
                 mode) — config get/set is not available. Pass a non-empty \
                 config_path_utf8 to sdr_core_create to enable persistence.",
            );
            return SdrCoreError::InvalidArg.as_int();
        };
        if key_utf8.is_null() {
            set_last_error("sdr_core_config: key pointer is null");
            return SdrCoreError::InvalidArg.as_int();
        }
        // SAFETY: caller contract — key_utf8 is NUL-terminated UTF-8
        // for the duration of this call.
        let cstr = unsafe { CStr::from_ptr(key_utf8) };
        let Ok(key) = cstr.to_str() else {
            set_last_error("sdr_core_config: key is not valid UTF-8");
            return SdrCoreError::InvalidArg.as_int();
        };
        if key.is_empty() {
            set_last_error("sdr_core_config: key is empty");
            return SdrCoreError::InvalidArg.as_int();
        }
        match f(config, key) {
            Ok(()) => {
                clear_last_error();
                SdrCoreError::Ok.as_int()
            }
            Err(e) => e.as_int(),
        }
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_core_config: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

// ============================================================
//  String
// ============================================================

/// Read a string-valued config key. Size-then-fill pattern: pass a
/// NULL `out_buf` + zero `buf_len` to query the required buffer
/// size (written to `*out_required`, including the NUL terminator);
/// then retry with a buffer of at least that size.
///
/// Returns `KeyNotFound` if the key doesn't exist or exists with a
/// non-string type. Returns `InvalidArg` with `*out_required`
/// populated when the provided buffer is too small — the host
/// should retry with at least that many bytes.
///
/// `out_required` may be null if the caller doesn't care about the
/// required size (e.g., has a known-large buffer). `out_buf` may be
/// null ONLY when `buf_len == 0` (the pure-query form). Any other
/// combination returns `InvalidArg`.
///
/// # Safety
///
/// Same contract as `sdr_core_config_set_string` (see below) plus
/// `out_buf` must point to writable storage of at least `buf_len`
/// bytes when non-null, and `out_required` must point to writable
/// storage for one `usize` when non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_config_get_string(
    handle: *mut SdrCore,
    key_utf8: *const c_char,
    out_buf: *mut c_char,
    buf_len: usize,
    out_required: *mut usize,
) -> i32 {
    unsafe {
        with_config(handle, key_utf8, |config, key| {
            if out_buf.is_null() && buf_len != 0 {
                set_last_error("sdr_core_config_get_string: out_buf is null but buf_len != 0");
                return Err(SdrCoreError::InvalidArg);
            }

            let value: Option<String> = config.read(|v| {
                v.get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
            let Some(s) = value else {
                set_last_error(format!(
                    "sdr_core_config_get_string: key '{key}' not found or not a string"
                ));
                return Err(SdrCoreError::KeyNotFound);
            };

            // Required buffer = bytes + trailing NUL.
            let required = s.len().saturating_add(1);
            if !out_required.is_null() {
                // SAFETY: caller contract — already inside the
                // outer fn-level unsafe block.
                *out_required = required;
            }

            // Pure-query form — host just wanted the size.
            if out_buf.is_null() {
                return Ok(());
            }
            if buf_len < required {
                set_last_error(format!(
                    "sdr_core_config_get_string: buffer too small for key '{key}' \
                     — need {required} bytes including NUL, got {buf_len}"
                ));
                return Err(SdrCoreError::InvalidArg);
            }

            let bytes = s.as_bytes();
            // SAFETY: out_buf is non-null, buf_len >= required >=
            // bytes.len() + 1, and we hold the read lock guard's
            // reference through the memcpy. Already inside the
            // outer fn-level unsafe block.
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out_buf, bytes.len());
            *out_buf.add(bytes.len()) = 0;
            Ok(())
        })
    }
}

/// Write a string-valued config key. Empty strings are accepted and
/// stored verbatim — an empty value is distinct from key absence.
///
/// # Safety
///
/// `handle` must be either null or a pointer previously returned by
/// `sdr_core_create` and not yet destroyed. `key_utf8` and
/// `value_utf8` must be either null (both rejected with `InvalidArg`)
/// or pointers to NUL-terminated UTF-8 strings for the duration of
/// the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_config_set_string(
    handle: *mut SdrCore,
    key_utf8: *const c_char,
    value_utf8: *const c_char,
) -> i32 {
    unsafe {
        with_config(handle, key_utf8, |config, key| {
            if value_utf8.is_null() {
                set_last_error("sdr_core_config_set_string: value pointer is null");
                return Err(SdrCoreError::InvalidArg);
            }
            // SAFETY: caller contract — value_utf8 is NUL-terminated
            // UTF-8 for the duration of this call. Already inside
            // the outer fn-level unsafe block.
            let cstr = CStr::from_ptr(value_utf8);
            let Ok(value) = cstr.to_str() else {
                set_last_error("sdr_core_config_set_string: value is not valid UTF-8");
                return Err(SdrCoreError::InvalidArg);
            };
            let owned = value.to_owned();
            config.write(|v| {
                v[key] = serde_json::Value::String(owned);
            });
            Ok(())
        })
    }
}

// ============================================================
//  Bool
// ============================================================

/// Read a bool-valued config key. Returns `KeyNotFound` when the
/// key is absent or present with a non-bool JSON type.
///
/// # Safety
///
/// Same contract as `sdr_core_config_set_bool`, plus `out_value`
/// must point to writable storage for one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_config_get_bool(
    handle: *mut SdrCore,
    key_utf8: *const c_char,
    out_value: *mut bool,
) -> i32 {
    unsafe {
        with_config(handle, key_utf8, |config, key| {
            if out_value.is_null() {
                set_last_error("sdr_core_config_get_bool: out_value is null");
                return Err(SdrCoreError::InvalidArg);
            }
            let value: Option<bool> =
                config.read(|v| v.get(key).and_then(serde_json::Value::as_bool));
            let Some(b) = value else {
                set_last_error(format!(
                    "sdr_core_config_get_bool: key '{key}' not found or not a bool"
                ));
                return Err(SdrCoreError::KeyNotFound);
            };
            // SAFETY: caller contract — out_value points to
            // writable `bool` storage. Already inside the
            // outer fn-level unsafe block.
            *out_value = b;
            Ok(())
        })
    }
}

/// Write a bool-valued config key.
///
/// # Safety
///
/// `handle` must be either null or a pointer previously returned by
/// `sdr_core_create`. `key_utf8` must be either null or a NUL-
/// terminated UTF-8 string for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_config_set_bool(
    handle: *mut SdrCore,
    key_utf8: *const c_char,
    value: bool,
) -> i32 {
    unsafe {
        with_config(handle, key_utf8, |config, key| {
            config.write(|v| {
                v[key] = serde_json::Value::Bool(value);
            });
            Ok(())
        })
    }
}

// ============================================================
//  u32
// ============================================================

/// Read a u32-valued config key. Returns `KeyNotFound` when the key
/// is absent, present with a non-numeric JSON type, or present with
/// a number that doesn't fit in `u32`.
///
/// # Safety
///
/// Same contract as `sdr_core_config_set_u32`, plus `out_value`
/// must point to writable storage for one `u32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_config_get_u32(
    handle: *mut SdrCore,
    key_utf8: *const c_char,
    out_value: *mut u32,
) -> i32 {
    unsafe {
        with_config(handle, key_utf8, |config, key| {
            if out_value.is_null() {
                set_last_error("sdr_core_config_get_u32: out_value is null");
                return Err(SdrCoreError::InvalidArg);
            }
            let value: Option<u32> = config.read(|v| {
                v.get(key)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
            });
            let Some(n) = value else {
                set_last_error(format!(
                    "sdr_core_config_get_u32: key '{key}' not found or not a u32"
                ));
                return Err(SdrCoreError::KeyNotFound);
            };
            // SAFETY: caller contract — out_value points to
            // writable `u32` storage. Already inside the
            // outer fn-level unsafe block.
            *out_value = n;
            Ok(())
        })
    }
}

/// Write a u32-valued config key.
///
/// # Safety
///
/// Same contract as `sdr_core_config_set_bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_config_set_u32(
    handle: *mut SdrCore,
    key_utf8: *const c_char,
    value: u32,
) -> i32 {
    unsafe {
        with_config(handle, key_utf8, |config, key| {
            config.write(|v| {
                v[key] = serde_json::Value::Number(value.into());
            });
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::lifecycle::{sdr_core_create, sdr_core_destroy};
    use std::ffi::CString;

    /// Build a handle backed by an on-disk config file under the
    /// system temp directory. The file path is unique per test
    /// (pid + monotonic nanos) so parallel tests don't collide.
    fn make_handle_with_config() -> (*mut SdrCore, std::path::PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "sdr-ffi-config-{}-{nonce}.json",
            std::process::id()
        ));
        let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut handle: *mut SdrCore = std::ptr::null_mut();
        let rc = unsafe { sdr_core_create(path_c.as_ptr(), &raw mut handle) };
        assert_eq!(rc, SdrCoreError::Ok.as_int());
        assert!(!handle.is_null());
        (handle, path)
    }

    fn destroy(handle: *mut SdrCore, path: std::path::PathBuf) {
        unsafe { sdr_core_destroy(handle) };
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn all_getters_reject_null_handle() {
        let key = CString::new("ui_sidebar_left_selected").unwrap();
        let mut out_bool = false;
        let mut out_u32: u32 = 0;
        assert_eq!(
            unsafe {
                sdr_core_config_get_string(
                    std::ptr::null_mut(),
                    key.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                )
            },
            SdrCoreError::InvalidHandle.as_int()
        );
        assert_eq!(
            unsafe {
                sdr_core_config_get_bool(std::ptr::null_mut(), key.as_ptr(), &raw mut out_bool)
            },
            SdrCoreError::InvalidHandle.as_int()
        );
        assert_eq!(
            unsafe {
                sdr_core_config_get_u32(std::ptr::null_mut(), key.as_ptr(), &raw mut out_u32)
            },
            SdrCoreError::InvalidHandle.as_int()
        );
    }

    #[test]
    fn missing_key_returns_key_not_found() {
        let (h, path) = make_handle_with_config();
        let key = CString::new("never_written").unwrap();
        let mut out_bool = true;
        let rc = unsafe { sdr_core_config_get_bool(h, key.as_ptr(), &raw mut out_bool) };
        assert_eq!(rc, SdrCoreError::KeyNotFound.as_int());
        // out_value must be untouched on KeyNotFound so the host
        // can keep its default — no partial-write surprises.
        assert!(out_bool);
        destroy(h, path);
    }

    #[test]
    fn round_trip_string() {
        let (h, path) = make_handle_with_config();
        let key = CString::new("ui_sidebar_left_selected").unwrap();
        let value = CString::new("radio").unwrap();

        assert_eq!(
            unsafe { sdr_core_config_set_string(h, key.as_ptr(), value.as_ptr()) },
            SdrCoreError::Ok.as_int()
        );

        // Query-size form: NULL buf + zero len + non-null out_required.
        let mut required: usize = 0;
        assert_eq!(
            unsafe {
                sdr_core_config_get_string(
                    h,
                    key.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    &raw mut required,
                )
            },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(required, "radio".len() + 1); // NUL-inclusive.

        // Fill form.
        let mut buf = vec![0i8; required];
        assert_eq!(
            unsafe {
                sdr_core_config_get_string(
                    h,
                    key.as_ptr(),
                    buf.as_mut_ptr(),
                    buf.len(),
                    std::ptr::null_mut(),
                )
            },
            SdrCoreError::Ok.as_int()
        );
        let read = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert_eq!(read, "radio");

        destroy(h, path);
    }

    #[test]
    fn round_trip_bool_and_u32() {
        let (h, path) = make_handle_with_config();
        let open_key = CString::new("ui_sidebar_left_open").unwrap();
        let width_key = CString::new("ui_sidebar_left_width_px").unwrap();

        assert_eq!(
            unsafe { sdr_core_config_set_bool(h, open_key.as_ptr(), true) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_config_set_u32(h, width_key.as_ptr(), 420) },
            SdrCoreError::Ok.as_int()
        );

        let mut out_bool = false;
        let mut out_u32: u32 = 0;
        assert_eq!(
            unsafe { sdr_core_config_get_bool(h, open_key.as_ptr(), &raw mut out_bool) },
            SdrCoreError::Ok.as_int()
        );
        assert!(out_bool);
        assert_eq!(
            unsafe { sdr_core_config_get_u32(h, width_key.as_ptr(), &raw mut out_u32) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(out_u32, 420);

        destroy(h, path);
    }

    #[test]
    fn mismatched_type_returns_key_not_found() {
        // Writing a bool and then trying to read it as a string
        // should be KeyNotFound, not a coerced value. Same for
        // u32-vs-bool etc.
        let (h, path) = make_handle_with_config();
        let key = CString::new("ui_sidebar_right_open").unwrap();
        assert_eq!(
            unsafe { sdr_core_config_set_bool(h, key.as_ptr(), false) },
            SdrCoreError::Ok.as_int()
        );

        let mut required: usize = 0;
        let rc = unsafe {
            sdr_core_config_get_string(h, key.as_ptr(), std::ptr::null_mut(), 0, &raw mut required)
        };
        assert_eq!(rc, SdrCoreError::KeyNotFound.as_int());

        let mut out_u32: u32 = 99;
        let rc_u32 = unsafe { sdr_core_config_get_u32(h, key.as_ptr(), &raw mut out_u32) };
        assert_eq!(rc_u32, SdrCoreError::KeyNotFound.as_int());
        assert_eq!(out_u32, 99); // untouched.

        destroy(h, path);
    }

    #[test]
    fn empty_config_path_returns_invalid_arg() {
        // Empty path at create time means the handle runs with no
        // on-disk config. Every config entry point surfaces that
        // as InvalidArg so the host knows it's in the ephemeral
        // mode and persistence isn't available.
        let empty = CString::new("").unwrap();
        let mut handle: *mut SdrCore = std::ptr::null_mut();
        let rc = unsafe { sdr_core_create(empty.as_ptr(), &raw mut handle) };
        assert_eq!(rc, SdrCoreError::Ok.as_int());

        let key = CString::new("anything").unwrap();
        let mut out_bool = false;
        assert_eq!(
            unsafe { sdr_core_config_get_bool(handle, key.as_ptr(), &raw mut out_bool) },
            SdrCoreError::InvalidArg.as_int()
        );

        unsafe { sdr_core_destroy(handle) };
    }

    #[test]
    fn buffer_too_small_reports_required_size() {
        let (h, path) = make_handle_with_config();
        let key = CString::new("ui_sidebar_left_selected").unwrap();
        let value = CString::new("something_longer_than_10_bytes").unwrap();
        assert_eq!(
            unsafe { sdr_core_config_set_string(h, key.as_ptr(), value.as_ptr()) },
            SdrCoreError::Ok.as_int()
        );

        let mut small_buf = [0i8; 4];
        let mut required: usize = 0;
        let rc = unsafe {
            sdr_core_config_get_string(
                h,
                key.as_ptr(),
                small_buf.as_mut_ptr(),
                small_buf.len(),
                &raw mut required,
            )
        };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
        assert_eq!(required, "something_longer_than_10_bytes".len() + 1);

        destroy(h, path);
    }

    #[test]
    fn null_key_rejected() {
        let (h, path) = make_handle_with_config();
        let mut out_bool = false;
        let rc = unsafe { sdr_core_config_get_bool(h, std::ptr::null(), &raw mut out_bool) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
        let rc_set = unsafe { sdr_core_config_set_bool(h, std::ptr::null(), true) };
        assert_eq!(rc_set, SdrCoreError::InvalidArg.as_int());
        destroy(h, path);
    }
}
