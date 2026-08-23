use super::*;

#[test]
fn set_audio_sink_type_accepts_both_variants() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, SDR_AUDIO_SINK_LOCAL) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, SDR_AUDIO_SINK_NETWORK) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_audio_sink_type_rejects_out_of_range_value() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_audio_sink_type(h, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_accepts_valid_input() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_PORT_UDP,
                crate::event::SDR_NETWORK_PROTOCOL_UDP,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_null_hostname() {
    let h = make_handle();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                std::ptr::null(),
                TEST_NETWORK_PORT_TCP,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_empty_hostname() {
    let h = make_handle();
    let empty = CString::new("").unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                empty.as_ptr(),
                TEST_NETWORK_PORT_TCP,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_zero_port() {
    // Port 0 is rejected at the ABI boundary — UDP would
    // silently drop to a bogus destination and TCP server
    // mode would bind an undiscoverable ephemeral port.
    // Per `CodeRabbit` round 2 on PR #352.
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                0,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::InvalidArg.as_int()
    );
    // And accepts the minimum legal port as a boundary check.
    assert_eq!(
        unsafe {
            sdr_core_set_network_sink_config(
                h,
                host.as_ptr(),
                TEST_NETWORK_MIN_VALID_PORT,
                crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER,
            )
        },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_network_sink_config_rejects_out_of_range_protocol() {
    let h = make_handle();
    let host = CString::new(TEST_NETWORK_HOST_LOOPBACK).unwrap();
    assert_eq!(
        unsafe { sdr_core_set_network_sink_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_network_sink_config(h, host.as_ptr(), TEST_NETWORK_PORT_TCP, -1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn audio_sink_type_from_c_covers_all_variants() {
    assert_eq!(
        audio_sink_type_from_c(SDR_AUDIO_SINK_LOCAL),
        Some(sdr_core::AudioSinkType::Local)
    );
    assert_eq!(
        audio_sink_type_from_c(SDR_AUDIO_SINK_NETWORK),
        Some(sdr_core::AudioSinkType::Network)
    );
    assert_eq!(audio_sink_type_from_c(99), None);
    assert_eq!(audio_sink_type_from_c(-1), None);
}

#[test]
fn protocol_from_c_covers_all_variants() {
    assert_eq!(
        protocol_from_c(crate::event::SDR_NETWORK_PROTOCOL_TCP_SERVER),
        Some(sdr_types::Protocol::TcpClient)
    );
    assert_eq!(
        protocol_from_c(crate::event::SDR_NETWORK_PROTOCOL_UDP),
        Some(sdr_types::Protocol::Udp)
    );
    assert_eq!(protocol_from_c(99), None);
}
