use super::*;

#[test]
fn scanner_commands_reject_null_handle() {
    // Matches the `all_commands_reject_null_handle` pattern
    // above — pins the null-handle guard on all three
    // new entry points so a refactor of `with_core`
    // can't silently drop it for one arm. Per `CodeRabbit`
    // round 1 on PR #497.
    assert_eq!(
        unsafe { sdr_core_set_scanner_enabled(std::ptr::null_mut(), true) },
        SdrCoreError::InvalidHandle.as_int()
    );
    let name = CString::new("Test").unwrap();
    assert_eq!(
        unsafe {
            sdr_core_lockout_scanner_channel(
                std::ptr::null_mut(),
                name.as_ptr(),
                TEST_SCANNER_FREQ_HZ,
            )
        },
        SdrCoreError::InvalidHandle.as_int()
    );
    assert_eq!(
        unsafe {
            sdr_core_unlock_scanner_channel(
                std::ptr::null_mut(),
                name.as_ptr(),
                TEST_SCANNER_FREQ_HZ,
            )
        },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn scanner_lockout_rejects_null_name() {
    // The lockout / unlock commands need a NUL-terminated
    // UTF-8 name to build the `ChannelKey`. Null pointer
    // is a caller bug; surface it as InvalidArg instead
    // of dereferencing.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_lockout_scanner_channel(h, std::ptr::null(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_unlock_scanner_channel(h, std::ptr::null(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn scanner_lockout_rejects_invalid_utf8_name() {
    // `CStr::to_str` refuses non-UTF-8 bytes; the command
    // returns InvalidArg rather than taking the bytes
    // lossily. A lossy coerce would let a host write a
    // ChannelKey name that would never match the
    // projection-time name the scanner holds.
    let h = make_handle();
    // Lone 0xFF — invalid start byte for any UTF-8
    // codepoint. NUL-terminated so `CStr::from_ptr`
    // finds the end of string.
    let bad: [u8; 2] = [0xFF, 0x00];
    let bad_ptr = bad.as_ptr().cast::<c_char>();
    assert_eq!(
        unsafe { sdr_core_lockout_scanner_channel(h, bad_ptr, TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_unlock_scanner_channel(h, bad_ptr, TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn scanner_commands_happy_path_return_ok() {
    // Engine accepts the scanner command regardless of
    // whether any channels are projected — the state
    // machine just stays in Idle. Confirms dispatch
    // round-trips and the name-copy path works for a
    // normal UTF-8 string.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_scanner_enabled(h, true) },
        SdrCoreError::Ok.as_int()
    );
    let name = CString::new("NOAA Weather").unwrap();
    assert_eq!(
        unsafe { sdr_core_lockout_scanner_channel(h, name.as_ptr(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_unlock_scanner_channel(h, name.as_ptr(), TEST_SCANNER_FREQ_HZ) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_scanner_enabled(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn update_scanner_channels_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_update_scanner_channels(std::ptr::null_mut(), std::ptr::null(), 0) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn update_scanner_channels_empty_clears_rotation() {
    // The empty-list path is how a host clears the
    // scanner's rotation (e.g., user toggled scan_enabled
    // off on every bookmark). Both null+0 callable; the
    // engine accepts the empty Vec and the state machine
    // settles to Idle on the next tick.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_update_scanner_channels(h, std::ptr::null(), 0) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn update_scanner_channels_count_zero_with_nonnull_pointer_is_invalid() {
    // Catch a buggy host that has `count = 0` and a stale
    // pointer — refuse rather than silently accept, so the
    // off-by-one is caught at the boundary.
    let h = make_handle();
    let name = CString::new("Test").unwrap();
    let ch = SdrScannerChannel {
        name_utf8: name.as_ptr(),
        frequency_hz: TEST_SCANNER_FREQ_HZ,
        demod_mode: SDR_DEMOD_NFM,
        bandwidth_hz: TEST_SCANNER_BW_HZ,
        priority: 0,
        dwell_ms: TEST_SCANNER_DWELL_MS,
        hang_ms: TEST_SCANNER_HANG_MS,
    };
    let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 0) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_null_pointer_with_count_is_invalid() {
    let h = make_handle();
    let rc = unsafe { sdr_core_update_scanner_channels(h, std::ptr::null(), 1) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_rejects_null_name_in_entry() {
    let h = make_handle();
    let ch = SdrScannerChannel {
        name_utf8: std::ptr::null(),
        frequency_hz: TEST_SCANNER_FREQ_HZ,
        demod_mode: SDR_DEMOD_NFM,
        bandwidth_hz: TEST_SCANNER_BW_HZ,
        priority: 0,
        dwell_ms: TEST_SCANNER_DWELL_MS,
        hang_ms: TEST_SCANNER_HANG_MS,
    };
    let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 1) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_rejects_unknown_demod_mode() {
    let h = make_handle();
    let name = CString::new("Test").unwrap();
    let ch = SdrScannerChannel {
        name_utf8: name.as_ptr(),
        frequency_hz: TEST_SCANNER_FREQ_HZ,
        demod_mode: 99, // out of range — no `SDR_DEMOD_*` matches.
        bandwidth_hz: TEST_SCANNER_BW_HZ,
        priority: 0,
        dwell_ms: TEST_SCANNER_DWELL_MS,
        hang_ms: TEST_SCANNER_HANG_MS,
    };
    let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 1) };
    assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    destroy(h);
}

#[test]
fn update_scanner_channels_rejects_non_positive_bandwidth() {
    let h = make_handle();
    let name = CString::new("Test").unwrap();
    for bad_bw in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let ch = SdrScannerChannel {
            name_utf8: name.as_ptr(),
            frequency_hz: TEST_SCANNER_FREQ_HZ,
            demod_mode: SDR_DEMOD_NFM,
            bandwidth_hz: bad_bw,
            priority: 0,
            dwell_ms: TEST_SCANNER_DWELL_MS,
            hang_ms: TEST_SCANNER_HANG_MS,
        };
        let rc = unsafe { sdr_core_update_scanner_channels(h, &raw const ch, 1) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }
    destroy(h);
}

#[test]
fn update_scanner_channels_happy_path_round_trip() {
    // Two channels — one normal, one priority. Confirms
    // the slice walk + dispatch round-trips for a typical
    // host call.
    let h = make_handle();
    let name_a = CString::new("NOAA Weather").unwrap();
    let name_b = CString::new("FRS Ch 1").unwrap();
    let channels = [
        SdrScannerChannel {
            name_utf8: name_a.as_ptr(),
            frequency_hz: TEST_SCANNER_FREQ_HZ,
            demod_mode: SDR_DEMOD_NFM,
            bandwidth_hz: TEST_SCANNER_BW_HZ,
            priority: 1, // priority tier
            dwell_ms: TEST_SCANNER_DWELL_MS,
            hang_ms: TEST_SCANNER_HANG_MS,
        },
        SdrScannerChannel {
            name_utf8: name_b.as_ptr(),
            frequency_hz: 462_562_500,
            demod_mode: SDR_DEMOD_NFM,
            bandwidth_hz: TEST_SCANNER_BW_HZ,
            priority: 0,
            dwell_ms: TEST_SCANNER_DWELL_MS,
            hang_ms: TEST_SCANNER_HANG_MS,
        },
    ];
    let rc = unsafe { sdr_core_update_scanner_channels(h, channels.as_ptr(), channels.len()) };
    assert_eq!(rc, SdrCoreError::Ok.as_int());
    destroy(h);
}
