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
fn load_rejects_null_or_short_buffers() {
    // Per CodeRabbit round 11: load requires buf_len >= 2
    // so a 1-byte buffer can't silently alias stored creds
    // to the "not stored" sentinel (only the NUL fits).
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
    for bad_len in [0_usize, 1] {
        assert_eq!(
            unsafe {
                sdr_core_radioreference_load_credentials(
                    u.as_mut_ptr().cast::<c_char>(),
                    bad_len,
                    p.as_mut_ptr().cast::<c_char>(),
                    CREDENTIAL_BUF_LEN,
                )
            },
            SdrCoreError::InvalidArg.as_int(),
            "user buf_len={bad_len} must be rejected"
        );
        assert_eq!(
            unsafe {
                sdr_core_radioreference_load_credentials(
                    u.as_mut_ptr().cast::<c_char>(),
                    CREDENTIAL_BUF_LEN,
                    p.as_mut_ptr().cast::<c_char>(),
                    bad_len,
                )
            },
            SdrCoreError::InvalidArg.as_int(),
            "pass buf_len={bad_len} must be rejected"
        );
    }
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
    let rc = unsafe { sdr_core_radioreference_test_credentials(empty.as_ptr(), empty.as_ptr()) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
}

#[test]
fn has_credentials_is_callable() {
    // Runs on any CI host without requiring a configured
    // keyring backend — the panic guard + "false on error"
    // fallback makes this safe to exercise.
    let _ = sdr_core_radioreference_has_credentials();
}
