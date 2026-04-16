//! Error model for the C ABI.
//!
//! Functions that can fail return `i32` (a [`SdrCoreError`] discriminant).
//! `0` is OK; negative values are error variants. The matching error
//! message is stashed in a **thread-local** so callers can pull it via
//! [`sdr_core_last_error_message`] without managing string lifetimes
//! themselves.
//!
//! This is the standard `errno`-style C-FFI pattern. Spec:
//! `docs/superpowers/specs/2026-04-12-sdr-ffi-c-abi-design.md`
//! (see "Errors" section in the header sketch).

use std::cell::RefCell;
use std::ffi::{CString, c_char};

/// Mirrors `enum SdrCoreError` in `include/sdr_core.h`. Variants and
/// their `i32` discriminants are part of the ABI — never reorder or
/// renumber, only add new variants at the end.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdrCoreError {
    /// Success. Functions return this when they have nothing to report.
    Ok = 0,
    /// A Rust panic crossed the FFI boundary, was caught by
    /// `catch_unwind`, and converted to this code. The thread-local
    /// last-error message contains the panic payload (if it was a
    /// formattable string).
    Internal = -1,
    /// The handle passed in was null or has already been destroyed.
    InvalidHandle = -2,
    /// One of the arguments was malformed (a NULL string, an
    /// unparseable enum, a non-finite frequency, …).
    InvalidArg = -3,
    /// A command was issued that requires the engine to be running
    /// (or vice versa) but the engine was not in the expected state.
    NotRunning = -4,
    /// Device or USB error from the source backend.
    Device = -5,
    /// Audio backend (CoreAudio / PipeWire / stub) error.
    Audio = -6,
    /// File or network I/O error.
    Io = -7,
    /// Configuration load/save error.
    Config = -8,
}

impl SdrCoreError {
    /// Convert to the `i32` return value FFI functions hand back.
    #[must_use]
    pub fn as_int(self) -> i32 {
        self as i32
    }
}

thread_local! {
    /// Thread-local last-error message. Set by [`set_last_error`],
    /// read by [`sdr_core_last_error_message`]. Each thread sees only
    /// its own messages — the host is responsible for calling
    /// `sdr_core_last_error_message` from the same thread that
    /// observed the error code.
    ///
    /// Storage is `CString` so the buffer stays valid for the
    /// `*const c_char` we hand back; the buffer is owned by the
    /// thread-local and lives until the next `set_last_error` on
    /// this thread (or thread death).
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Stash an error message on the current thread. Called by the FFI
/// functions before returning a non-OK error code so the host can
/// fetch a human-readable explanation via
/// [`sdr_core_last_error_message`].
///
/// Replaces any previously stored message on this thread.
pub fn set_last_error(msg: impl Into<String>) {
    let owned = msg.into();
    // CString::new fails on interior NULs. We already replaced them
    // above so construction should always succeed — but fall back to
    // a static message just in case so we never panic on the error
    // path (which would be ironic).
    let sanitized = owned.replace('\0', "?");
    let Ok(cstring) = CString::new(sanitized) else {
        // Unreachable: replace('\0', "?") above removed all interior
        // NULs. Return without setting the error rather than panic.
        return;
    };
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = Some(cstring);
    });
}

/// Clear the last error on the current thread. Called by FFI
/// functions on the success path so a stale message from a previous
/// failed call doesn't linger and confuse the next failure report.
pub fn clear_last_error() {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// FFI: returns a pointer to the thread-local last-error message, or
/// `NULL` if no error has been recorded on this thread.
///
/// The pointer is valid until the next `sdr_core_*` call on this
/// thread (which may overwrite or clear the buffer). Callers that
/// want to persist the message should copy it immediately.
///
/// # Safety
///
/// The returned pointer is owned by the thread-local storage. Callers
/// must not free it, and must not retain it across other FFI calls
/// on the same thread.
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_last_error_message() -> *const c_char {
    LAST_ERROR.with(|cell| match cell.borrow().as_ref() {
        Some(cstr) => cstr.as_ptr(),
        None => std::ptr::null(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn last_error_starts_null() {
        // Fresh thread: no error set yet → NULL.
        clear_last_error();
        let p = sdr_core_last_error_message();
        assert!(p.is_null());
    }

    #[test]
    fn set_then_get_round_trips() {
        set_last_error("device not found");
        let p = sdr_core_last_error_message();
        assert!(!p.is_null());
        // SAFETY: The pointer comes from a thread-local CString that
        // is alive for the duration of this test.
        let got = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        assert_eq!(got, "device not found");
    }

    #[test]
    fn clear_returns_to_null() {
        set_last_error("oops");
        assert!(!sdr_core_last_error_message().is_null());
        clear_last_error();
        assert!(sdr_core_last_error_message().is_null());
    }

    #[test]
    fn interior_nul_is_sanitized_not_dropped() {
        // CString cannot hold interior NULs; we sanitize rather than
        // silently lose the diagnostic.
        set_last_error("bad\0string");
        let p = sdr_core_last_error_message();
        assert!(!p.is_null());
        let got = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        assert_eq!(got, "bad?string");
    }

    #[test]
    fn error_codes_match_abi() {
        // These discriminants are ABI — locking them in via test so
        // a careless reorder breaks `cargo test` instead of breaking
        // every binary that consumes the header.
        assert_eq!(SdrCoreError::Ok.as_int(), 0);
        assert_eq!(SdrCoreError::Internal.as_int(), -1);
        assert_eq!(SdrCoreError::InvalidHandle.as_int(), -2);
        assert_eq!(SdrCoreError::InvalidArg.as_int(), -3);
        assert_eq!(SdrCoreError::NotRunning.as_int(), -4);
        assert_eq!(SdrCoreError::Device.as_int(), -5);
        assert_eq!(SdrCoreError::Audio.as_int(), -6);
        assert_eq!(SdrCoreError::Io.as_int(), -7);
        assert_eq!(SdrCoreError::Config.as_int(), -8);
    }
}
