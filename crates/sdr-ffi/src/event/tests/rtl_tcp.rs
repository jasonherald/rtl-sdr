use super::*;

#[test]
fn rtl_tcp_state_discriminants_match_header() {
    assert_eq!(SDR_RTL_TCP_STATE_DISCONNECTED, 0);
    assert_eq!(SDR_RTL_TCP_STATE_CONNECTING, 1);
    assert_eq!(SDR_RTL_TCP_STATE_CONNECTED, 2);
    assert_eq!(SDR_RTL_TCP_STATE_RETRYING, 3);
    assert_eq!(SDR_RTL_TCP_STATE_FAILED, 4);
    // ABI 0.18 role-denial states (#396) — the host-side
    // toast routing reads these wire integers directly,
    // so any accidental renumber breaks every client of
    // the C ABI without failing any Rust-level type check.
    assert_eq!(SDR_RTL_TCP_STATE_CONTROLLER_BUSY, 5);
    assert_eq!(SDR_RTL_TCP_STATE_AUTH_REQUIRED, 6);
    assert_eq!(SDR_RTL_TCP_STATE_AUTH_FAILED, 7);
}

#[test]
fn translate_rtl_tcp_connection_state_controller_busy() {
    use sdr_types::RtlTcpConnectionState;
    let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
        RtlTcpConnectionState::ControllerBusy,
    ))
    .expect("ControllerBusy event should translate");
    let payload = unsafe { event.payload.rtl_tcp_connection_state };
    assert_eq!(payload.kind, SDR_RTL_TCP_STATE_CONTROLLER_BUSY);
    // Role-denial states are payload-less — no tuner
    // string, no counters. Host UIs dispatch on `kind`
    // alone and look up localized copy on their side.
    assert!(payload.utf8.is_null());
    assert_eq!(payload.attempt, 0);
    assert!(payload.retry_in_secs.abs() < f64::EPSILON);
    assert_eq!(payload.gain_count, 0);
    assert!(owned_cstring.is_none());
}

#[test]
fn translate_rtl_tcp_connection_state_auth_required() {
    use sdr_types::RtlTcpConnectionState;
    let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
        RtlTcpConnectionState::AuthRequired,
    ))
    .expect("AuthRequired event should translate");
    let payload = unsafe { event.payload.rtl_tcp_connection_state };
    assert_eq!(payload.kind, SDR_RTL_TCP_STATE_AUTH_REQUIRED);
    assert!(payload.utf8.is_null());
    assert_eq!(payload.attempt, 0);
    assert!(payload.retry_in_secs.abs() < f64::EPSILON);
    assert_eq!(payload.gain_count, 0);
    assert!(owned_cstring.is_none());
}

#[test]
fn translate_rtl_tcp_connection_state_auth_failed() {
    use sdr_types::RtlTcpConnectionState;
    let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
        RtlTcpConnectionState::AuthFailed,
    ))
    .expect("AuthFailed event should translate");
    let payload = unsafe { event.payload.rtl_tcp_connection_state };
    assert_eq!(payload.kind, SDR_RTL_TCP_STATE_AUTH_FAILED);
    assert!(payload.utf8.is_null());
    assert_eq!(payload.attempt, 0);
    assert!(payload.retry_in_secs.abs() < f64::EPSILON);
    assert_eq!(payload.gain_count, 0);
    assert!(owned_cstring.is_none());
}

#[test]
fn translate_rtl_tcp_connection_state_disconnected() {
    use sdr_types::RtlTcpConnectionState;
    let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
        RtlTcpConnectionState::Disconnected,
    ))
    .expect("Disconnected event should translate");
    assert_eq!(event.kind, SDR_EVT_RTL_TCP_CONNECTION_STATE);
    let payload = unsafe { event.payload.rtl_tcp_connection_state };
    assert_eq!(payload.kind, SDR_RTL_TCP_STATE_DISCONNECTED);
    assert!(payload.utf8.is_null());
    assert_eq!(payload.attempt, 0);
    // `retry_in_secs` is populated from `Duration::as_secs_f64`
    // only on the Retrying arm; Disconnected leaves it at the
    // struct's zero-init. Exact-zero compare is fine here —
    // we put the 0.0 there deterministically.
    assert!(payload.retry_in_secs.abs() < f64::EPSILON);
    assert_eq!(payload.gain_count, 0);
    assert!(owned_cstring.is_none());
}

#[test]
fn translate_rtl_tcp_connection_state_connected_carries_tuner() {
    use sdr_types::RtlTcpConnectionState;
    let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
        RtlTcpConnectionState::Connected {
            tuner_name: "R820T".to_string(),
            gain_count: 29,
            codec: "None".to_string(),
            granted_role: Some(true),
        },
    ))
    .expect("Connected event should translate");
    let payload = unsafe { event.payload.rtl_tcp_connection_state };
    assert_eq!(payload.kind, SDR_RTL_TCP_STATE_CONNECTED);
    assert_eq!(payload.gain_count, 29);
    assert!(!payload.utf8.is_null());
    let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
    assert_eq!(cstr.to_str().unwrap(), "R820T");
    assert!(owned_cstring.is_some());
}

#[test]
fn translate_rtl_tcp_connection_state_retrying_carries_attempt_and_seconds() {
    use sdr_types::RtlTcpConnectionState;
    let (event, _, _) = translate_event(&DspToUi::RtlTcpConnectionState(
        RtlTcpConnectionState::Retrying {
            attempt: 7,
            retry_in: std::time::Duration::from_millis(2_500),
        },
    ))
    .expect("Retrying event should translate");
    let payload = unsafe { event.payload.rtl_tcp_connection_state };
    assert_eq!(payload.kind, SDR_RTL_TCP_STATE_RETRYING);
    assert_eq!(payload.attempt, 7);
    assert!((payload.retry_in_secs - 2.5).abs() < 1e-9);
    assert!(payload.utf8.is_null());
}

#[test]
fn translate_rtl_tcp_connection_state_failed_carries_reason() {
    use sdr_types::RtlTcpConnectionState;
    let (event, owned_cstring, _) = translate_event(&DspToUi::RtlTcpConnectionState(
        RtlTcpConnectionState::Failed {
            reason: "handshake rejected: not RTL0".to_string(),
        },
    ))
    .expect("Failed event should translate");
    let payload = unsafe { event.payload.rtl_tcp_connection_state };
    assert_eq!(payload.kind, SDR_RTL_TCP_STATE_FAILED);
    assert!(!payload.utf8.is_null());
    let cstr = unsafe { std::ffi::CStr::from_ptr(payload.utf8) };
    assert_eq!(cstr.to_str().unwrap(), "handshake rejected: not RTL0");
    assert!(owned_cstring.is_some());
}
