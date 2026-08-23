use super::*;

#[test]
fn set_bias_tee_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_bias_tee(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_bias_tee(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_direct_sampling_accepts_valid_modes() {
    let h = make_handle();
    for mode in SDR_DIRECT_SAMPLING_MIN..=SDR_DIRECT_SAMPLING_MAX {
        assert_eq!(
            unsafe { sdr_core_set_direct_sampling(h, mode) },
            SdrCoreError::Ok.as_int(),
            "direct-sampling mode {mode} should be accepted"
        );
    }
    destroy(h);
}

#[test]
fn set_direct_sampling_rejects_out_of_range() {
    // Reference the MIN/MAX constants so the test stays in
    // sync with the FFI contract instead of hardcoding
    // boundary literals that could drift. Per `CodeRabbit`
    // round 1 on PR #360.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_direct_sampling(h, SDR_DIRECT_SAMPLING_MAX + 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_direct_sampling(h, SDR_DIRECT_SAMPLING_MIN - 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_offset_tuning_round_trips() {
    // Exercise both polarities so a future regression that
    // silently breaks one branch is caught. Per `CodeRabbit`
    // round 3 on PR #360.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_offset_tuning(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_offset_tuning(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_rtl_agc_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_rtl_agc(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_rtl_agc(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_gain_by_index_accepts_nonnegative_indices() {
    // Engine bounds-checks against the source's `gains()`
    // length (or the rtl_tcp advertised gain_count); a bad
    // index becomes a `DspToUi::Error` event. The FFI
    // itself accepts any u32 — we're testing the dispatch
    // path here, not the bounds check.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_gain_by_index(h, TEST_GAIN_INDEX_BASELINE) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_gain_by_index(h, TEST_GAIN_INDEX_REPRESENTATIVE_HIGH) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn rtl_tcp_disconnect_dispatches() {
    // The engine drops `DisconnectRtlTcp` with a warn log
    // when the active source isn't rtl_tcp — our test
    // fixture builds a default core which starts on the
    // RTL-SDR source, so we're exercising the FFI dispatch
    // path + engine guard, not the actual socket teardown.
    // That's the right scope for a unit test; the socket
    // path is covered by `sdr-source-network` tests.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_disconnect(h) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn rtl_tcp_retry_now_dispatches() {
    // Same coverage as the disconnect test — the engine
    // drops the message with a warn log outside rtl_tcp;
    // we're checking that the FFI return code is `Ok`
    // when the dispatch succeeds. Per issue #326.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_retry_now(h) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn rtl_tcp_disconnect_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_disconnect(std::ptr::null_mut()) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn rtl_tcp_retry_now_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_rtl_tcp_retry_now(std::ptr::null_mut()) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn source_type_from_c_covers_all_variants() {
    assert_eq!(
        source_type_from_c(SDR_SOURCE_RTLSDR),
        Some(SourceType::RtlSdr)
    );
    assert_eq!(
        source_type_from_c(SDR_SOURCE_NETWORK),
        Some(SourceType::Network)
    );
    assert_eq!(source_type_from_c(SDR_SOURCE_FILE), Some(SourceType::File));
    assert_eq!(
        source_type_from_c(SDR_SOURCE_RTLTCP),
        Some(SourceType::RtlTcp)
    );
    assert_eq!(source_type_from_c(99), None);
    assert_eq!(source_type_from_c(-1), None);
}

#[test]
fn source_protocol_from_c_covers_all_variants() {
    assert_eq!(
        source_protocol_from_c(SDR_SOURCE_PROTOCOL_TCP),
        Some(Protocol::TcpClient)
    );
    assert_eq!(
        source_protocol_from_c(SDR_SOURCE_PROTOCOL_UDP),
        Some(Protocol::Udp)
    );
    assert_eq!(source_protocol_from_c(99), None);
}

#[test]
fn set_source_type_accepts_all_variants() {
    let h = make_handle();
    for t in [
        SDR_SOURCE_RTLSDR,
        SDR_SOURCE_NETWORK,
        SDR_SOURCE_FILE,
        SDR_SOURCE_RTLTCP,
    ] {
        assert_eq!(
            unsafe { sdr_core_set_source_type(h, t) },
            SdrCoreError::Ok.as_int(),
            "source type {t} should be accepted"
        );
    }
    destroy(h);
}

#[test]
fn set_source_type_rejects_out_of_range_value() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_source_type(h, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_source_type(h, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_config_accepts_valid_input() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                SDR_SOURCE_PROTOCOL_TCP,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_UDP,
                SDR_SOURCE_PROTOCOL_UDP,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_config_rejects_null_and_empty_hostname() {
    let h = make_handle();
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                std::ptr::null(),
                TEST_NETWORK_PORT_TCP,
                SDR_SOURCE_PROTOCOL_TCP,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_config(
                h,
                empty.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                SDR_SOURCE_PROTOCOL_TCP,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_config_rejects_zero_port_and_unknown_protocol() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe { sdr_core_set_network_config(h, host.as_ptr(), 0, SDR_SOURCE_PROTOCOL_TCP) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_network_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_network_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_path_accepts_valid_path() {
    let h = make_handle();
    let path = CString::new("/tmp/some-iq.wav").unwrap();
    assert_eq!(
        unsafe { sdr_core_set_file_path(h, path.as_ptr()) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_path_rejects_null_and_empty() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_file_path(h, std::ptr::null()) },
        SdrCoreError::InvalidArg.as_int()
    );
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe { sdr_core_set_file_path(h, empty.as_ptr()) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_looping_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_file_looping(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_file_looping(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_file_looping_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_set_file_looping(std::ptr::null_mut(), true) },
        SdrCoreError::InvalidHandle.as_int()
    );
}
