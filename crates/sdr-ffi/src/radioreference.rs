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

/// Expected length of a US ZIP code in ASCII digits.
/// RadioReference only serves US frequencies, so callers must
/// pass a 5-digit zip; we validate on our side before hitting
/// the network so a typo doesn't round-trip to RR as a generic
/// SOAP fault.
const RR_ZIP_LEN: usize = 5;

/// Size of the caller-allocated load buffer on the Swift side.
/// `write_cstr_to_buf` reserves one byte for the NUL terminator,
/// so the maximum UTF-8 payload that will round-trip intact is
/// `MAX_CREDENTIAL_FIELD_LEN - 1` (511 bytes). `save_credentials`
/// enforces a strict `len < MAX_CREDENTIAL_FIELD_LEN` guard (via
/// `validate_rr_credentials`) so an exact-buffer-sized value
/// can't sneak in and later reload truncated. Per CodeRabbit
/// rounds 6 + 10 on PR #346.
///
/// RadioReference has no documented limit; 511 bytes each is
/// comfortably more than any human typed, and matches the
/// SwiftUI wrapper's fixed buffer minus the NUL.
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
            // Callers: `load_credentials` doesn't even reach this
            // arm any more — it uses the OK-plus-empty-buffer
            // sentinel for the "not stored" case and reserves
            // `Io` strictly for backend failures. This branch is
            // kept for other keyring operations (delete's
            // `NotFound`, for instance, is absorbed upstream in
            // `delete_credentials`'s idempotent handling). The
            // Swift wrapper propagates every non-zero rc via
            // `checkRc`, so anything hitting this path surfaces
            // as an `SdrCoreError` with the message above.
            SdrCoreError::Io
        }
        KeyringError::NoBackend => {
            // Generic message is correct on every platform —
            // on macOS, the Apple Keychain backend failing is
            // a platform issue, not a missing install. Linux
            // actually benefits from a remediation hint since
            // the secret-service daemon might not be running.
            // Per CodeRabbit round 6 on PR #346.
            #[cfg(target_os = "linux")]
            set_last_error(format!(
                "{fn_name}: no secure storage available — install GNOME Keyring or KeePassXC"
            ));
            #[cfg(not(target_os = "linux"))]
            set_last_error(format!("{fn_name}: no secure storage available"));
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

/// Enforce the credential contract (non-empty + within the
/// round-trip cap) in one place so `save`, `test`, and `search`
/// all agree. Without this shared guard, an inconsistent state
/// was possible: a 1 KB password could be tested successfully
/// against RadioReference but then fail the length check on
/// save, or vice versa — confusing the user with different
/// errors depending on button order. Per CodeRabbit round 8 on
/// PR #346.
fn validate_rr_credentials(fn_name: &str, user: &str, pass: &str) -> Result<(), SdrCoreError> {
    if user.is_empty() || pass.is_empty() {
        set_last_error(format!("{fn_name}: empty user or password"));
        return Err(SdrCoreError::InvalidArg);
    }
    // Strict `>=` — the load buffer reserves one byte for the
    // NUL terminator, so the largest payload that round-trips
    // intact is `MAX_CREDENTIAL_FIELD_LEN - 1`. A value exactly
    // equal to the buffer size would truncate silently on
    // reload. Per CodeRabbit round 10 on PR #346.
    if user.len() >= MAX_CREDENTIAL_FIELD_LEN || pass.len() >= MAX_CREDENTIAL_FIELD_LEN {
        set_last_error(format!(
            "{fn_name}: user or password must fit in {} UTF-8 bytes plus NUL",
            MAX_CREDENTIAL_FIELD_LEN - 1
        ));
        return Err(SdrCoreError::InvalidArg);
    }
    Ok(())
}

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
/// Both `user_utf8` and `pass_utf8` must be non-null, NUL-terminated,
/// **non-empty** UTF-8 strings. Empty inputs are rejected with
/// `INVALID_ARG` — the rest of the ABI (has / load) treats
/// "empty" as the "not stored" sentinel, so accepting an empty
/// save would leave the credentials ABI self-inconsistent: save
/// would succeed, but `has_credentials` would immediately return
/// false and `load_credentials` would fall into the empty-buffer
/// sentinel. Per CodeRabbit round 3 on PR #346.
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
        // Enforce the shared credential contract (non-empty +
        // within round-trip cap). Keeps save / test / search
        // consistent — see `validate_rr_credentials` for the
        // rationale. Per CodeRabbit rounds 3/6/8 on PR #346.
        if let Err(e) =
            validate_rr_credentials("sdr_core_radioreference_save_credentials", &user, &pass)
        {
            return e.as_int();
        }

        let store = KeyringStore::new(KEYRING_SERVICE);
        if let Err(e) = store.set(KEY_RR_USERNAME, &user) {
            return map_keyring_error("sdr_core_radioreference_save_credentials", &e).as_int();
        }
        if let Err(e) = store.set(KEY_RR_PASSWORD, &pass) {
            // Break the pair from BOTH sides. Deleting only the
            // username would leave a prior-session's stale
            // password still sitting in the keyring, which —
            // paired with the just-written username — would
            // surface on the next load as valid credentials.
            // Delete both so the caller sees "no credentials
            // stored" on the next load, regardless of which
            // delete succeeds. Per CodeRabbit round 2 + 5 on
            // PR #346.
            let _ = store.delete(KEY_RR_USERNAME);
            let _ = store.delete(KEY_RR_PASSWORD);
            return map_keyring_error("sdr_core_radioreference_save_credentials", &e).as_int();
        }

        // Defensive readback: a `set` success whose `get`
        // doesn't return the value we just wrote is the smoking
        // gun for a misconfigured keyring backend — notably
        // keyring 3.x's mock fallback when `apple-native` /
        // `sync-secret-service` isn't enabled (see the
        // `feedback_keyring_crate_features` memory). Strict
        // equality, not just "non-empty": if the keyring
        // silently ignored the password write and left a prior
        // stored password in place, an "any non-empty" check
        // would still pass and we'd ship a mixed pair. On any
        // mismatch, delete both secrets so the next load
        // reports "not stored" cleanly instead of returning a
        // stale half-written pair. Per CodeRabbit round 2.
        let verify_user = store.get(KEY_RR_USERNAME);
        let verify_pass = store.get(KEY_RR_PASSWORD);
        match (verify_user, verify_pass) {
            (Ok(Some(u)), Ok(Some(p))) if u == user && p == pass => {
                // Save round-tripped, values match exactly.
            }
            (u, p) => {
                let _ = store.delete(KEY_RR_USERNAME);
                let _ = store.delete(KEY_RR_PASSWORD);
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
/// Returns `OK` in both normal cases:
///   - both fields non-empty → credentials are present and
///     were written to the buffers
///   - either field empty (only the NUL) → nothing is stored;
///     this is the "not yet configured" sentinel, not an error
///
/// Returns `IO` only for genuine keyring backend failures
/// (backend unavailable, platform error, …). Callers can use
/// `sdr_core_radioreference_has_credentials` as a cheap probe
/// that captures the presence-check in a single bool without
/// round-tripping through this call.
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
        // Distinguish three cases:
        //   - `Ok(Some(non-empty))` → real stored value; copy out.
        //   - `Ok(None)` or `Ok(Some(""))` → nothing stored;
        //     return `OK` with an empty NUL-terminated buffer so
        //     the caller can tell "no credentials" apart from a
        //     keyring backend error. Per CodeRabbit round 1 on
        //     PR #346 (the earlier shape collapsed both into
        //     `SDR_CORE_ERR_IO`, which the Swift wrapper then
        //     mapped to `nil` — masking real keyring failures).
        //   - `Err(_)` → genuine keyring backend error; propagate
        //     as `SDR_CORE_ERR_IO` with a descriptive last-error
        //     message so hosts can surface it to the user.
        let user_opt = match store.get(KEY_RR_USERNAME) {
            Ok(v) => v.unwrap_or_default(),
            Err(e) => {
                return map_keyring_error("sdr_core_radioreference_load_credentials", &e).as_int();
            }
        };
        let pass_opt = match store.get(KEY_RR_PASSWORD) {
            Ok(v) => v.unwrap_or_default(),
            Err(e) => {
                return map_keyring_error("sdr_core_radioreference_load_credentials", &e).as_int();
            }
        };

        // Partial-keyring guard: the sentinel contract says
        // "either buffer empty ⇒ nothing stored." If we copy an
        // orphaned username through while the password is empty
        // (or vice versa), a caller that only inspects one
        // buffer before checking the other would see a "live"
        // value even though the ABI is signaling "not stored."
        // Collapse both sides to empty on any partial state so
        // the sentinel is self-consistent no matter which buffer
        // the caller reads first. Per CodeRabbit round 10 on PR
        // #346.
        let (user_bytes, pass_bytes) = if user_opt.is_empty() || pass_opt.is_empty() {
            (&b""[..], &b""[..])
        } else {
            (user_opt.as_bytes(), pass_opt.as_bytes())
        };

        // SAFETY: null + zero-length checked above; buffers are
        // writable per the caller contract. Empty `user_bytes` /
        // `pass_bytes` produce a single NUL byte in each output
        // buffer — the "no credentials stored" sentinel.
        unsafe {
            write_cstr_to_buf(user_bytes, out_user, user_buf_len);
            write_cstr_to_buf(pass_bytes, out_pass, pass_buf_len);
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
        // Attempt BOTH deletes even if the first raises a
        // non-NotFound error — leaving one secret behind when
        // the other is already removed is worse than reporting
        // a partial failure. Then pick whichever real error
        // happened (preferring the username path for message
        // ordering). `KeyringError::NotFound` is treated as
        // success — matches the header's "idempotent" contract.
        // Per CodeRabbit round 1 on PR #346.
        let user_err = match store.delete(KEY_RR_USERNAME) {
            Ok(()) | Err(KeyringError::NotFound) => None,
            Err(e) => Some(e),
        };
        let pass_err = match store.delete(KEY_RR_PASSWORD) {
            Ok(()) | Err(KeyringError::NotFound) => None,
            Err(e) => Some(e),
        };
        if let Some(e) = user_err.or(pass_err) {
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
    // Clear the thread-local last-error slot on BOTH the success
    // path and the catch_unwind path. Without this, a stale
    // message from an earlier failing FFI call (e.g. a failed
    // save) would remain visible through
    // `sdr_core_last_error_message` after a later probe here
    // succeeded — and since this is the only credential FFI
    // that swallows backend failures into a bool rather than
    // returning a non-zero rc, it was the one path that silently
    // inherited whatever was in the slot. Per CodeRabbit round
    // 9 on PR #346.
    std::panic::catch_unwind(|| {
        let store = KeyringStore::new(KEYRING_SERVICE);
        let user = matches!(store.get(KEY_RR_USERNAME), Ok(Some(s)) if !s.is_empty());
        let pass = matches!(store.get(KEY_RR_PASSWORD), Ok(Some(s)) if !s.is_empty());
        clear_last_error();
        user && pass
    })
    .unwrap_or_else(|_| {
        clear_last_error();
        false
    })
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

        // Match the save contract — reject empties and values
        // longer than the round-trip cap before the network call.
        if let Err(e) =
            validate_rr_credentials("sdr_core_radioreference_test_credentials", &user, &pass)
        {
            return e.as_int();
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
/// Buffer sizing: `out_required` (optional — pass NULL to ignore)
/// is filled with the exact number of bytes the caller must
/// allocate to receive the full payload, **including the
/// trailing NUL**. Callers that pass a too-small buffer get
/// `INVALID_ARG` (not a silently-truncated body) with
/// `out_required` set to the correct allocation size so they
/// can retry.
///
/// Returns `OK` on successful search + serialization, `AUTH` on
/// bad credentials, `IO` on network failure, `INVALID_ARG` on
/// bad inputs **or a too-small output buffer** (check
/// `out_required` to distinguish), `INTERNAL` on JSON encoding
/// failure (shouldn't happen for the types involved).
///
/// # Safety
///
/// `user_utf8`, `pass_utf8`, `zip_utf8` must each be null or a
/// NUL-terminated UTF-8 C string. `out_buf` must point to at
/// least `out_buf_len` writable bytes. `out_required` must be
/// either null or point to a writable `usize`.
///
/// The `#[allow(clippy::too_many_lines)]` is deliberate — this
/// function's body is a single top-to-bottom dispatch:
/// validate → HTTP round-trip → map response → serialize →
/// emit. Splitting it would just spread the same linear flow
/// across helpers without any meaningful reuse.
#[allow(clippy::too_many_lines)]
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

        // Match the save contract — reject empties and values
        // longer than the round-trip cap before the network call.
        // Previously search was the only credential-taking path
        // that let an over-length value HTTP round-trip to
        // RadioReference before failing, which was inconsistent
        // with save's pre-check.
        if let Err(e) = validate_rr_credentials("sdr_core_radioreference_search_zip", &user, &pass)
        {
            return e.as_int();
        }

        // RR expects a 5-digit US ZIP — validate on our side so a
        // typo doesn't round-trip to the network and come back as
        // a generic SOAP fault. The exact length lives at the
        // top of the module as `RR_ZIP_LEN`.
        if zip.len() != RR_ZIP_LEN || !zip.chars().all(|c| c.is_ascii_digit()) {
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
        // Total bytes the caller must allocate to receive the
        // full payload — the JSON plus one byte for the trailing
        // NUL that `write_cstr_to_buf` always emits. Report this
        // whether or not we're about to truncate so callers can
        // reallocate + retry. Per CodeRabbit round 3 on PR #346.
        let required_len = bytes.len().saturating_add(1);
        if !out_required.is_null() {
            // SAFETY: non-null check above; caller contract says
            // the pointer is writable.
            unsafe {
                *out_required = required_len;
            }
        }

        // If the buffer isn't big enough for the payload plus
        // NUL, return `INVALID_ARG` rather than OK with a
        // silently-truncated body. The previous shape let C
        // callers who passed `out_required = NULL` miss the
        // truncation entirely — caller then parsed invalid JSON.
        // Swift sees InvalidArg + a populated `out_required`,
        // reallocates, and retries.
        if required_len > out_buf_len {
            set_last_error(format!(
                "sdr_core_radioreference_search_zip: output buffer too small ({required_len} needed, got {out_buf_len})"
            ));
            return SdrCoreError::InvalidArg.as_int();
        }

        // SAFETY: out_buf null + zero-length checked above;
        // size fit check immediately above.
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

    /// Output-buffer size for `search_zip` rejection tests.
    /// The tests fail before any JSON is written (bad zip,
    /// null buffers, empty credentials), so this is never
    /// filled — it just has to be nonzero so the initial
    /// out_buf / out_buf_len validation doesn't trip on it.
    /// Per CodeRabbit round 7 on PR #346.
    const SEARCH_REJECTION_BUF_LEN: usize = 128;

    /// `out_buf_len` passed alongside a null `out_buf` in
    /// `search_zip_rejects_null_buf`. Nonzero so the null
    /// check is what trips, not the `out_buf_len == 0`
    /// check — the exact value is arbitrary.
    const NULL_BUF_PROBE_LEN: usize = 64;

    #[test]
    fn save_rejects_null_pointers() {
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(std::ptr::null(), std::ptr::null()) },
            SdrCoreError::InvalidArg.as_int()
        );
    }

    #[test]
    fn save_rejects_oversize_fields() {
        // save_credentials must refuse values whose UTF-8 length
        // is >= MAX_CREDENTIAL_FIELD_LEN — the load buffer
        // reserves one byte for the NUL, so a value exactly
        // MAX_CREDENTIAL_FIELD_LEN bytes would truncate silently
        // on the next load. Regression for CodeRabbit rounds
        // 6 + 10 on PR #346. The exact-size case guards the
        // off-by-one explicitly.
        let real = CString::new("jason").unwrap();
        let long = CString::new("x".repeat(MAX_CREDENTIAL_FIELD_LEN + 1)).unwrap();
        let at_cap = CString::new("x".repeat(MAX_CREDENTIAL_FIELD_LEN)).unwrap();
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(long.as_ptr(), real.as_ptr()) },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(real.as_ptr(), long.as_ptr()) },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(at_cap.as_ptr(), real.as_ptr()) },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(real.as_ptr(), at_cap.as_ptr()) },
            SdrCoreError::InvalidArg.as_int()
        );
    }

    #[test]
    fn save_rejects_empty_fields() {
        // Empty user or password must be rejected — otherwise
        // save would succeed but `has_credentials` / `load_credentials`
        // would immediately report "not stored" because they use
        // the empty-buffer sentinel. Regression for CodeRabbit
        // round 3 on PR #346.
        let empty = CString::new("").unwrap();
        let real = CString::new("jason").unwrap();
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(empty.as_ptr(), real.as_ptr()) },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(real.as_ptr(), empty.as_ptr()) },
            SdrCoreError::InvalidArg.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_radioreference_save_credentials(empty.as_ptr(), empty.as_ptr()) },
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

    /// Pin the "not stored" sentinel contract: after delete,
    /// load must return OK with both buffers NUL-only (i.e.
    /// first byte == 0). Distinct from IO (backend error) so
    /// the Swift wrapper can return `nil` instead of throwing
    /// — see the function-level docstring above. Per
    /// CodeRabbit round 6 on PR #346.
    ///
    /// **Marked `#[ignore]`** because it deletes the
    /// shared-service credentials from the user's real
    /// keyring. A developer running `cargo test` with their
    /// RadioReference login saved would lose it. Run
    /// explicitly with `cargo test ... -- --ignored` when
    /// vetting this contract; CI skips it.
    #[test]
    #[ignore = "deletes real keyring credentials — run only with --ignored after vetting"]
    fn load_returns_ok_with_empty_buffers_when_not_stored() {
        let handle = std::thread::spawn(|| {
            let _ = sdr_core_radioreference_delete_credentials();
            let mut u = [0_u8; CREDENTIAL_BUF_LEN];
            let mut p = [0_u8; CREDENTIAL_BUF_LEN];
            let rc = unsafe {
                sdr_core_radioreference_load_credentials(
                    u.as_mut_ptr().cast::<c_char>(),
                    CREDENTIAL_BUF_LEN,
                    p.as_mut_ptr().cast::<c_char>(),
                    CREDENTIAL_BUF_LEN,
                )
            };
            assert_eq!(rc, SdrCoreError::Ok.as_int());
            assert_eq!(u[0], 0, "user buffer should be NUL-only when not stored");
            assert_eq!(p[0], 0, "pass buffer should be NUL-only when not stored");
        });
        handle.join().expect("thread should exit cleanly");
    }

    #[test]
    fn search_zip_rejects_bad_zip() {
        let u = CString::new("user").unwrap();
        let p = CString::new("pass").unwrap();
        let bad = CString::new("9021").unwrap(); // 4 digits
        let mut buf = [0_u8; SEARCH_REJECTION_BUF_LEN];
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
        let mut buf = [0_u8; SEARCH_REJECTION_BUF_LEN];
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
                NULL_BUF_PROBE_LEN,
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
