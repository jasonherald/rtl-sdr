//! RadioReference C ABI — credentials (keyring) + search.
//!
//! All functions in this module are **handle-free**. They don't touch
//! the DSP engine because:
//!   - Credentials live in `sdr_config::KeyringStore` (OS keychain on
//!     macOS / libsecret on Linux), completely independent of the
//!     live engine state.
//!   - Search calls are synchronous blocking HTTP via
//!     `sdr_radioreference::RrClient`; running them on the DSP thread
//!     would stall audio. Callers are expected to dispatch these on
//!     a background thread (SwiftUI does this via `Task` detached
//!     from the main actor).
//!
//! Credential storage mirrors the GTK side exactly — same keyring
//! service (`"sdr-rs"`) and key names (`"radioreference-username"` /
//! `"radioreference-password"`). A user who has GTK + SDRMac on the
//! same machine shares one set of stored credentials.
//!
//! Search returns a JSON document instead of a bespoke struct layout
//! for two reasons:
//!   - The result is a variable-length list of records with nested
//!     optional fields (tone, tags). A caller-allocated-buffer
//!     pattern per field would balloon the ABI surface.
//!   - SwiftUI's `Codable` can decode the JSON into native structs
//!     cheaply (~1000 frequencies ≈ 100 KB — negligible).
//!
//! Mode mapping (RadioReference mode string → engine demod mode +
//! bandwidth) is done on the Rust side via
//! `sdr_radioreference::mode_map::map_rr_mode` so Swift consumers
//! don't need a parallel lookup table.

use std::ffi::{CStr, c_char};

use sdr_config::KeyringStore;
use sdr_config::keyring_store::KeyringError;
use sdr_radioreference::{RrClient, SoapError};
use serde::Serialize;

use crate::error::{SdrCoreError, clear_last_error, set_last_error};

// ============================================================
//  Keyring addressing — must match the GTK side byte-for-byte
//  so credentials saved from one app show up in the other.
// ============================================================

/// Keyring service (the "app" within the OS keyring namespace).
const KEYRING_SERVICE: &str = "sdr-rs";

/// Keyring key for the RadioReference username.
const KEY_RR_USERNAME: &str = "radioreference-username";

/// Keyring key for the RadioReference password.
const KEY_RR_PASSWORD: &str = "radioreference-password";

/// Upper bound on usernames / passwords we'll round-trip through
/// the load call. RadioReference usernames and passwords don't
/// have documented length limits, but 512 bytes each is
/// comfortably more than any human typed. If a longer value shows
/// up in the wild, `_load_credentials` truncates to `buf_len - 1`
/// and the caller can bump its buffer — truncation does not set
/// an error.
#[allow(dead_code)]
const MAX_CREDENTIAL_FIELD_LEN: usize = 512;

// ============================================================
//  Error translation
// ============================================================

/// Map a `SoapError` to the matching C ABI error code and set a
/// descriptive thread-local last-error message for the FFI
/// caller.
fn map_soap_error(fn_name: &str, err: &SoapError) -> SdrCoreError {
    match err {
        SoapError::AuthFailed => {
            set_last_error(format!("{fn_name}: authentication failed: {err}"));
            SdrCoreError::Auth
        }
        SoapError::Http(_) | SoapError::Io(_) | SoapError::Fault(_) => {
            set_last_error(format!("{fn_name}: network error: {err}"));
            SdrCoreError::Io
        }
        SoapError::Xml(_) | SoapError::Unexpected(_) => {
            set_last_error(format!("{fn_name}: unexpected response: {err}"));
            SdrCoreError::Internal
        }
    }
}

/// Map a `KeyringError` to the matching C ABI error code and set a
/// descriptive thread-local last-error message.
fn map_keyring_error(fn_name: &str, err: &KeyringError) -> SdrCoreError {
    match err {
        KeyringError::NotFound => {
            set_last_error(format!("{fn_name}: credential not found"));
            // Not strictly an error for some callers — but we return
            // a distinct code and let the caller decide. The Swift
            // wrapper maps `NotFound` to returning `nil` rather than
            // throwing.
            SdrCoreError::Io
        }
        KeyringError::NoBackend => {
            set_last_error(format!(
                "{fn_name}: no secure storage available — install GNOME Keyring or KeePassXC (Linux)"
            ));
            SdrCoreError::Io
        }
        KeyringError::Platform(_) => {
            set_last_error(format!("{fn_name}: keyring error: {err}"));
            SdrCoreError::Io
        }
    }
}

// ============================================================
//  Small utility shared by the command-style FFIs here.
// ============================================================

/// Decode a caller-provided NUL-terminated UTF-8 pointer to an
/// owned `String`. Returns `InvalidArg` on null / non-UTF-8.
///
/// # Safety
///
/// `ptr` must be null or a pointer to a NUL-terminated UTF-8 C
/// string.
unsafe fn cstr_to_string(fn_name: &str, ptr: *const c_char) -> Result<String, SdrCoreError> {
    if ptr.is_null() {
        set_last_error(format!("{fn_name}: string pointer is null"));
        return Err(SdrCoreError::InvalidArg);
    }
    // SAFETY: caller contract.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    if let Ok(s) = cstr.to_str() {
        Ok(s.to_string())
    } else {
        set_last_error(format!("{fn_name}: string is not valid UTF-8"));
        Err(SdrCoreError::InvalidArg)
    }
}

/// Write `bytes` into a caller-allocated buffer with truncation.
/// Returns the number of bytes written (not counting the NUL).
///
/// # Safety
///
/// `out_buf` must point to at least `buf_len` writable bytes.
/// `buf_len` must be at least 1 (a single NUL fits).
unsafe fn write_cstr_to_buf(bytes: &[u8], out_buf: *mut c_char, buf_len: usize) -> usize {
    let max_payload = buf_len.saturating_sub(1); // reserve 1 for NUL
    let to_copy = bytes.len().min(max_payload);
    // SAFETY: caller contract guarantees `out_buf` is writable for
    // `buf_len` bytes. `to_copy <= buf_len - 1 < buf_len` and the
    // NUL write at `out_buf.add(to_copy)` is within `buf_len`
    // because `to_copy <= buf_len - 1`.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf.cast::<u8>(), to_copy);
        *out_buf.add(to_copy) = 0;
    }
    to_copy
}

// ============================================================
//  Credentials — keyring-backed
// ============================================================

/// Save RadioReference credentials to the OS keyring.
///
/// Both `user_utf8` and `pass_utf8` must be non-null NUL-terminated
/// UTF-8 strings. Empty strings are accepted and stored as-is (the
/// keyring backend doesn't distinguish "empty value" from "no
/// value", so consumers should use `sdr_core_radioreference_has_credentials`
/// to probe existence rather than round-tripping through load).
///
/// # Safety
///
/// Both pointers must be either null (returns `INVALID_ARG`) or
/// NUL-terminated UTF-8 C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_radioreference_save_credentials(
    user_utf8: *const c_char,
    pass_utf8: *const c_char,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        let user = match unsafe {
            cstr_to_string("sdr_core_radioreference_save_credentials", user_utf8)
        } {
            Ok(s) => s,
            Err(e) => return e.as_int(),
        };
        let pass = match unsafe {
            cstr_to_string("sdr_core_radioreference_save_credentials", pass_utf8)
        } {
            Ok(s) => s,
            Err(e) => return e.as_int(),
        };

        let store = KeyringStore::new(KEYRING_SERVICE);
        if let Err(e) = store.set(KEY_RR_USERNAME, &user) {
            return map_keyring_error("sdr_core_radioreference_save_credentials", &e).as_int();
        }
        if let Err(e) = store.set(KEY_RR_PASSWORD, &pass) {
            return map_keyring_error("sdr_core_radioreference_save_credentials", &e).as_int();
        }

        // Defensive readback: a `set` success with a failing
        // `get` is the smoking gun for a misconfigured keyring
        // backend (notably keyring 3.x's mock fallback when
        // `apple-native` / `sync-secret-service` isn't enabled —
        // see the `feedback_keyring_crate_features` memory).
        // Catching it here, right after the write, means a
        // future regression never silently ships.
        let verify_user = store.get(KEY_RR_USERNAME);
        let verify_pass = store.get(KEY_RR_PASSWORD);
        match (verify_user, verify_pass) {
            (Ok(Some(u)), Ok(Some(_))) if !u.is_empty() => {
                // Save round-tripped.
            }
            (u, p) => {
                set_last_error(format!(
                    "sdr_core_radioreference_save_credentials: set returned Ok but readback failed: user={:?} pass={:?}",
                    u.map(|o| o.is_some()),
                    p.map(|o| o.is_some())
                ));
                return SdrCoreError::Io.as_int();
            }
        }

        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    result.unwrap_or_else(|_| {
        set_last_error("sdr_core_radioreference_save_credentials: panic");
        SdrCoreError::Internal.as_int()
    })
}

/// Load RadioReference credentials from the OS keyring into
/// caller-allocated UTF-8 buffers.
///
/// Both `out_user` and `out_pass` are NUL-terminated on success.
/// Truncation is not an error: if the stored value doesn't fit,
/// the output is truncated at `buf_len - 1` and NUL-terminated.
/// Callers that want guaranteed fidelity should pass large
/// buffers (`MAX_CREDENTIAL_FIELD_LEN` is a safe ceiling in
/// practice).
///
/// Returns `OK` if both fields were found and written. Returns
/// `IO` (with a `"credential not found"` last-error) if either
/// field is missing. Checking
/// `sdr_core_radioreference_has_credentials` first avoids the
/// error-message churn for the expected-empty case.
///
/// # Safety
///
/// Both output buffers must point to at least their respective
/// `_buf_len` writable bytes and `_buf_len` must be ≥ 1 (room for
/// the NUL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_radioreference_load_credentials(
    out_user: *mut c_char,
    user_buf_len: usize,
    out_pass: *mut c_char,
    pass_buf_len: usize,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if out_user.is_null() || out_pass.is_null() || user_buf_len == 0 || pass_buf_len == 0 {
            set_last_error("sdr_core_radioreference_load_credentials: null buffer or zero length");
            return SdrCoreError::InvalidArg.as_int();
        }

        let store = KeyringStore::new(KEYRING_SERVICE);
        let user = match store.get(KEY_RR_USERNAME) {
            Ok(Some(s)) if !s.is_empty() => s,
            Ok(_) => {
                set_last_error("sdr_core_radioreference_load_credentials: username not stored");
                return SdrCoreError::Io.as_int();
            }
            Err(e) => {
                return map_keyring_error("sdr_core_radioreference_load_credentials", &e).as_int();
            }
        };
        let pass = match store.get(KEY_RR_PASSWORD) {
            Ok(Some(s)) if !s.is_empty() => s,
            Ok(_) => {
                set_last_error("sdr_core_radioreference_load_credentials: password not stored");
                return SdrCoreError::Io.as_int();
            }
            Err(e) => {
                return map_keyring_error("sdr_core_radioreference_load_credentials", &e).as_int();
            }
        };

        // SAFETY: null + zero-length checked above; buffers are
        // writable per the caller contract.
        unsafe {
            write_cstr_to_buf(user.as_bytes(), out_user, user_buf_len);
            write_cstr_to_buf(pass.as_bytes(), out_pass, pass_buf_len);
        }
        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    result.unwrap_or_else(|_| {
        set_last_error("sdr_core_radioreference_load_credentials: panic");
        SdrCoreError::Internal.as_int()
    })
}

/// Delete stored RadioReference credentials. Returns `OK` whether
/// or not credentials were present — "already gone" is idempotent.
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_radioreference_delete_credentials() -> i32 {
    let result = std::panic::catch_unwind(|| {
        let store = KeyringStore::new(KEYRING_SERVICE);
        if let Err(e) = store.delete(KEY_RR_USERNAME) {
            return map_keyring_error("sdr_core_radioreference_delete_credentials", &e).as_int();
        }
        if let Err(e) = store.delete(KEY_RR_PASSWORD) {
            return map_keyring_error("sdr_core_radioreference_delete_credentials", &e).as_int();
        }
        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    result.unwrap_or_else(|_| {
        set_last_error("sdr_core_radioreference_delete_credentials: panic");
        SdrCoreError::Internal.as_int()
    })
}

/// Return `true` if both username and password are stored AND
/// non-empty. Returns `false` if either is missing, empty, or the
/// keyring backend is unavailable.
///
/// This is a cheap existence probe — it doesn't return the values,
/// so UIs can gate "show Connect button" without loading the
/// password into memory until an actual search happens.
#[unsafe(no_mangle)]
pub extern "C" fn sdr_core_radioreference_has_credentials() -> bool {
    std::panic::catch_unwind(|| {
        let store = KeyringStore::new(KEYRING_SERVICE);
        let user = matches!(store.get(KEY_RR_USERNAME), Ok(Some(s)) if !s.is_empty());
        let pass = matches!(store.get(KEY_RR_PASSWORD), Ok(Some(s)) if !s.is_empty());
        user && pass
    })
    .unwrap_or(false)
}

// ============================================================
//  Test connection — minimal roundtrip, no results
// ============================================================

/// Validate credentials by issuing a lightweight RadioReference
/// query (zipcode 90210 — the canonical "is the API reachable +
/// is this account authorized" probe used by the RR crate and
/// mirrored from the GTK side's "Test & Save" button).
///
/// Returns `OK` on success, `AUTH` on bad credentials, `IO` on
/// network errors.
///
/// # Safety
///
/// `user_utf8` / `pass_utf8` must be either null or NUL-terminated
/// UTF-8 C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_radioreference_test_credentials(
    user_utf8: *const c_char,
    pass_utf8: *const c_char,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        let user = match unsafe {
            cstr_to_string("sdr_core_radioreference_test_credentials", user_utf8)
        } {
            Ok(s) => s,
            Err(e) => return e.as_int(),
        };
        let pass = match unsafe {
            cstr_to_string("sdr_core_radioreference_test_credentials", pass_utf8)
        } {
            Ok(s) => s,
            Err(e) => return e.as_int(),
        };

        if user.is_empty() || pass.is_empty() {
            set_last_error("sdr_core_radioreference_test_credentials: empty user or password");
            return SdrCoreError::InvalidArg.as_int();
        }

        let client = match RrClient::new(&user, &pass) {
            Ok(c) => c,
            Err(e) => {
                return map_soap_error("sdr_core_radioreference_test_credentials", &e).as_int();
            }
        };
        if let Err(e) = client.test_connection() {
            return map_soap_error("sdr_core_radioreference_test_credentials", &e).as_int();
        }
        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    result.unwrap_or_else(|_| {
        set_last_error("sdr_core_radioreference_test_credentials: panic");
        SdrCoreError::Internal.as_int()
    })
}

// ============================================================
//  Search — zip → county → frequencies, returned as one JSON blob
// ============================================================

/// JSON wire format for `sdr_core_radioreference_search_zip`.
/// Kept separate from `sdr_radioreference::RrFrequency` so the
/// ABI isn't coupled to the upstream crate's internal layout —
/// if that struct grows or is reshaped, the JSON stays stable.
#[derive(Serialize)]
struct WireSearchResult {
    county_id: u32,
    county_name: String,
    state_id: u32,
    city: String,
    frequencies: Vec<WireFrequency>,
}

/// JSON wire format for a single frequency row.
#[derive(Serialize)]
struct WireFrequency {
    /// Opaque RadioReference frequency ID (for future dedup /
    /// import-tracking use; Mac treats it as a string).
    id: String,
    freq_hz: u64,
    /// Raw RR mode string ("FM", "FMN", "FMW", …) — surfaced so
    /// power users can see what RR reported.
    rr_mode: String,
    /// Engine demod mode name ("NFM", "WFM", …) mapped via
    /// `sdr_radioreference::mode_map::map_rr_mode`. Swift uses
    /// this directly without re-implementing the lookup.
    demod_mode: String,
    /// Channel bandwidth in Hz (mapped alongside `demod_mode`).
    bandwidth_hz: f64,
    /// CTCSS / PL tone in Hz when present. Not sent to the DSP on
    /// import — stored in the bookmark for later restore.
    tone_hz: Option<f32>,
    description: String,
    alpha_tag: String,
    /// First tag description, used as the "Category" filter key
    /// in the GTK UI. Empty string when the row has no tags.
    category: String,
    /// All tag descriptions, in order. Mostly the first is the
    /// same as `category`; surfaced in full for future UIs.
    tags: Vec<String>,
}

/// Search RadioReference for frequencies covering a US ZIP code.
///
/// Does `get_zip_info(zip)` then `get_county_frequencies(county_id)`
/// and serializes the combined result as JSON into `out_buf`:
///
/// ```json
/// {
///   "county_id": 2437,
///   "county_name": "Santa Clara",
///   "state_id": 5,
///   "city": "San Jose",
///   "frequencies": [
///     {
///       "id": "123",
///       "freq_hz": 146520000,
///       "rr_mode": "FM",
///       "demod_mode": "NFM",
///       "bandwidth_hz": 12500.0,
///       "tone_hz": 127.3,
///       "description": "Main call",
///       "alpha_tag": "SJ RPTR",
///       "category": "Amateur Repeaters",
///       "tags": ["Amateur Repeaters"]
///     },
///     ...
///   ]
/// }
/// ```
///
/// Buffer growth: `out_required` (optional) is filled with the
/// JSON payload size **in bytes** (not counting the NUL). Callers
/// that pass a too-small buffer get truncated JSON AND a non-zero
/// `out_required` they can use to reallocate + retry. Pass NULL
/// when you don't need it.
///
/// Returns `OK` on successful search + serialization, `AUTH` on
/// bad credentials, `IO` on network failure, `INVALID_ARG` on bad
/// inputs, `INTERNAL` on JSON encoding failure (shouldn't happen
/// for the types involved).
///
/// # Safety
///
/// `user_utf8`, `pass_utf8`, `zip_utf8` must each be null or a
/// NUL-terminated UTF-8 C string. `out_buf` must point to at
/// least `out_buf_len` writable bytes. `out_required` must be
/// either null or point to a writable `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_core_radioreference_search_zip(
    user_utf8: *const c_char,
    pass_utf8: *const c_char,
    zip_utf8: *const c_char,
    out_buf: *mut c_char,
    out_buf_len: usize,
    out_required: *mut usize,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if out_buf.is_null() || out_buf_len == 0 {
            set_last_error("sdr_core_radioreference_search_zip: out_buf null or zero length");
            return SdrCoreError::InvalidArg.as_int();
        }

        let user = match unsafe { cstr_to_string("sdr_core_radioreference_search_zip", user_utf8) }
        {
            Ok(s) => s,
            Err(e) => return e.as_int(),
        };
        let pass = match unsafe { cstr_to_string("sdr_core_radioreference_search_zip", pass_utf8) }
        {
            Ok(s) => s,
            Err(e) => return e.as_int(),
        };
        let zip = match unsafe { cstr_to_string("sdr_core_radioreference_search_zip", zip_utf8) } {
            Ok(s) => s,
            Err(e) => return e.as_int(),
        };

        // RR expects a 5-digit US ZIP — validate on our side so a
        // typo doesn't round-trip to the network and come back as
        // a generic SOAP fault.
        if zip.len() != 5 || !zip.chars().all(|c| c.is_ascii_digit()) {
            set_last_error(format!(
                "sdr_core_radioreference_search_zip: zip must be 5 digits, got {zip:?}"
            ));
            return SdrCoreError::InvalidArg.as_int();
        }

        let client = match RrClient::new(&user, &pass) {
            Ok(c) => c,
            Err(e) => return map_soap_error("sdr_core_radioreference_search_zip", &e).as_int(),
        };

        let zip_info = match client.get_zip_info(&zip) {
            Ok(info) => info,
            Err(e) => return map_soap_error("sdr_core_radioreference_search_zip", &e).as_int(),
        };

        let (county_name, freqs) = match client.get_county_frequencies(zip_info.county_id) {
            Ok(pair) => pair,
            Err(e) => return map_soap_error("sdr_core_radioreference_search_zip", &e).as_int(),
        };

        // Translate to wire format, mapping RR modes to engine
        // demod modes + bandwidths so Swift doesn't re-implement
        // the lookup.
        let frequencies: Vec<WireFrequency> = freqs
            .into_iter()
            .map(|f| {
                let mapped = sdr_radioreference::mode_map::map_rr_mode(&f.mode);
                let category = f
                    .tags
                    .first()
                    .map(|t| t.description.clone())
                    .unwrap_or_default();
                let tags = f.tags.into_iter().map(|t| t.description).collect();
                WireFrequency {
                    id: f.id,
                    freq_hz: f.freq_hz,
                    rr_mode: f.mode,
                    demod_mode: mapped.demod_mode.to_string(),
                    bandwidth_hz: mapped.bandwidth,
                    tone_hz: f.tone,
                    description: f.description,
                    alpha_tag: f.alpha_tag,
                    category,
                    tags,
                }
            })
            .collect();

        let wire = WireSearchResult {
            county_id: zip_info.county_id,
            county_name,
            state_id: zip_info.state_id,
            city: zip_info.city,
            frequencies,
        };

        let json = match serde_json::to_string(&wire) {
            Ok(s) => s,
            Err(e) => {
                set_last_error(format!(
                    "sdr_core_radioreference_search_zip: serialization failed: {e}"
                ));
                return SdrCoreError::Internal.as_int();
            }
        };

        let bytes = json.as_bytes();
        // Report the required size regardless of buffer adequacy.
        // Callers can detect truncation by comparing with buf_len.
        if !out_required.is_null() {
            // SAFETY: non-null check above; caller contract says
            // the pointer is writable.
            unsafe {
                *out_required = bytes.len();
            }
        }

        // SAFETY: out_buf null + zero-length checked above.
        unsafe {
            write_cstr_to_buf(bytes, out_buf, out_buf_len);
        }
        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    result.unwrap_or_else(|_| {
        set_last_error("sdr_core_radioreference_search_zip: panic");
        SdrCoreError::Internal.as_int()
    })
}

// ============================================================
//  Tests
// ============================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Size big enough for the longest realistic RR username /
    /// password — sanity, not a hard ABI bound.
    const CREDENTIAL_BUF_LEN: usize = 512;

    #[test]
    fn save_rejects_null_pointers() {
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(std::ptr::null(), std::ptr::null()) },
            SdrCoreError::InvalidArg.as_int()
        );
    }

    #[test]
    fn load_rejects_null_or_zero_length_buffers() {
        let mut u = [0_u8; CREDENTIAL_BUF_LEN];
        let mut p = [0_u8; CREDENTIAL_BUF_LEN];
        assert_eq!(
            unsafe {
                sdr_core_radioreference_load_credentials(
                    std::ptr::null_mut(),
                    CREDENTIAL_BUF_LEN,
                    p.as_mut_ptr().cast::<c_char>(),
                    CREDENTIAL_BUF_LEN,
                )
            },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe {
                sdr_core_radioreference_load_credentials(
                    u.as_mut_ptr().cast::<c_char>(),
                    0,
                    p.as_mut_ptr().cast::<c_char>(),
                    CREDENTIAL_BUF_LEN,
                )
            },
            SdrCoreError::InvalidArg.as_int()
        );
    }

    #[test]
    fn search_zip_rejects_bad_zip() {
        let u = CString::new("user").unwrap();
        let p = CString::new("pass").unwrap();
        let bad = CString::new("9021").unwrap(); // 4 digits
        let mut buf = [0_u8; 128];
        let rc = unsafe {
            sdr_core_radioreference_search_zip(
                u.as_ptr(),
                p.as_ptr(),
                bad.as_ptr(),
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn search_zip_rejects_non_digit_zip() {
        let u = CString::new("user").unwrap();
        let p = CString::new("pass").unwrap();
        let bad = CString::new("abcde").unwrap();
        let mut buf = [0_u8; 128];
        let rc = unsafe {
            sdr_core_radioreference_search_zip(
                u.as_ptr(),
                p.as_ptr(),
                bad.as_ptr(),
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn search_zip_rejects_null_buf() {
        let u = CString::new("user").unwrap();
        let p = CString::new("pass").unwrap();
        let zip = CString::new("90210").unwrap();
        let rc = unsafe {
            sdr_core_radioreference_search_zip(
                u.as_ptr(),
                p.as_ptr(),
                zip.as_ptr(),
                std::ptr::null_mut(),
                64,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn test_credentials_rejects_empty() {
        let empty = CString::new("").unwrap();
        let rc =
            unsafe { sdr_core_radioreference_test_credentials(empty.as_ptr(), empty.as_ptr()) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn has_credentials_is_callable() {
        // Runs on any CI host without requiring a configured
        // keyring backend — the panic guard + "false on error"
        // fallback makes this safe to exercise.
        let _ = sdr_core_radioreference_has_credentials();
    }
}
