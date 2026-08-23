use super::*;

#[test]
fn network_sink_status_discriminants_match_header() {
    // Same lock-in for the tagged-payload sub-discriminants
    // and the protocol values — these are part of the ABI
    // just like the outer event kinds. Per `CodeRabbit`
    // round 1 on PR #352.
    assert_eq!(SDR_NETWORK_SINK_STATUS_INACTIVE, 0);
    assert_eq!(SDR_NETWORK_SINK_STATUS_ACTIVE, 1);
    assert_eq!(SDR_NETWORK_SINK_STATUS_ERROR, 2);
    assert_eq!(SDR_NETWORK_PROTOCOL_TCP_SERVER, 0);
    assert_eq!(SDR_NETWORK_PROTOCOL_UDP, 1);
}

#[test]
fn translate_network_sink_status_inactive_has_null_utf8_and_unused_protocol() {
    use sdr_core::{DspToUi, NetworkSinkStatus};
    let (event, owned_cstring, owned_vec) =
        translate_event(&DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive))
            .expect("inactive event should translate");
    assert_eq!(event.kind, SDR_EVT_NETWORK_SINK_STATUS);
    // SAFETY: kind dispatch above narrows the union field.
    let payload = unsafe { event.payload.network_sink_status };
    assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_INACTIVE);
    assert!(payload.utf8.is_null());
    assert_eq!(payload.protocol, -1);
    assert!(owned_cstring.is_none());
    assert!(owned_vec.is_none());
}

#[test]
fn translate_network_sink_status_active_tcp_maps_to_tcp_server() {
    use sdr_core::{DspToUi, NetworkSinkStatus};
    let status = NetworkSinkStatus::Active {
        endpoint: "127.0.0.1:1234".to_string(),
        protocol: sdr_types::Protocol::TcpClient,
    };
    let (event, owned_cstring, _) = translate_event(&DspToUi::NetworkSinkStatus(status))
        .expect("active event should translate");
    assert_eq!(event.kind, SDR_EVT_NETWORK_SINK_STATUS);
    let payload = unsafe { event.payload.network_sink_status };
    assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_ACTIVE);
    assert!(!payload.utf8.is_null());
    // Rust-side `TcpClient` bridges to the clearer C name
    // `TCP_SERVER`. This is the contract the Swift side
    // relies on — lock it here.
    assert_eq!(payload.protocol, SDR_NETWORK_PROTOCOL_TCP_SERVER);

    // SAFETY: utf8 points into `owned_cstring` which is kept
    // alive by the `_` binding in the destructure above for
    // the duration of this test.
    let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
    assert_eq!(cstr.to_str().unwrap(), "127.0.0.1:1234");
    assert!(owned_cstring.is_some(), "endpoint CString must be owned");
}

#[test]
fn translate_network_sink_status_active_udp_maps_to_udp_constant() {
    use sdr_core::{DspToUi, NetworkSinkStatus};
    let status = NetworkSinkStatus::Active {
        endpoint: "192.168.1.10:9000".to_string(),
        protocol: sdr_types::Protocol::Udp,
    };
    let (event, _owned_cstring, _) = translate_event(&DspToUi::NetworkSinkStatus(status))
        .expect("active event should translate");
    let payload = unsafe { event.payload.network_sink_status };
    assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_ACTIVE);
    assert_eq!(payload.protocol, SDR_NETWORK_PROTOCOL_UDP);
}

#[test]
fn translate_network_sink_status_error_carries_message_and_unused_protocol() {
    use sdr_core::{DspToUi, NetworkSinkStatus};
    let status = NetworkSinkStatus::Error {
        message: "bind failed: address already in use".to_string(),
    };
    let (event, owned_cstring, _) =
        translate_event(&DspToUi::NetworkSinkStatus(status)).expect("error event should translate");
    let payload = unsafe { event.payload.network_sink_status };
    assert_eq!(payload.kind, SDR_NETWORK_SINK_STATUS_ERROR);
    assert!(!payload.utf8.is_null());
    // Protocol is unused for the error arm per the ABI doc.
    assert_eq!(payload.protocol, -1);
    let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
    assert_eq!(
        cstr.to_str().unwrap(),
        "bind failed: address already in use"
    );
    assert!(
        owned_cstring.is_some(),
        "error message CString must be owned"
    );
}

#[test]
fn translate_network_sink_status_sanitizes_interior_nul_in_endpoint() {
    // Regression guard: a stray NUL in an endpoint string
    // must not drop the event silently. The translate path
    // replaces interior NULs with `?` before `CString::new`,
    // same as the DeviceInfo and Error paths.
    use sdr_core::{DspToUi, NetworkSinkStatus};
    let status = NetworkSinkStatus::Active {
        endpoint: "host\0injected:1234".to_string(),
        protocol: sdr_types::Protocol::TcpClient,
    };
    let (event, _owned, _) = translate_event(&DspToUi::NetworkSinkStatus(status))
        .expect("sanitized active event should translate");
    let payload = unsafe { event.payload.network_sink_status };
    assert!(!payload.utf8.is_null());
    let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
    assert_eq!(cstr.to_str().unwrap(), "host?injected:1234");
}
