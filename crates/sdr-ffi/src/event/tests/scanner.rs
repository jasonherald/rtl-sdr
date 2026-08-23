use super::*;

#[test]
fn scanner_state_discriminants_match_header() {
    // The host-side scanner state readout reads these wire
    // integers directly, so any renumber breaks every C ABI
    // consumer without failing a Rust-level type check.
    assert_eq!(SDR_SCANNER_STATE_IDLE, 0);
    assert_eq!(SDR_SCANNER_STATE_RETUNING, 1);
    assert_eq!(SDR_SCANNER_STATE_DWELLING, 2);
    assert_eq!(SDR_SCANNER_STATE_LISTENING, 3);
    assert_eq!(SDR_SCANNER_STATE_HANGING, 4);
}

#[test]
fn scanner_mutex_reason_discriminants_match_header() {
    assert_eq!(SDR_SCANNER_MUTEX_RECORDING_STOPPED_FOR_SCANNER, 0);
    // Discriminants 1 and 3 are reserved ABI slots — the
    // scanner ↔ transcription mutex variants that used to
    // live here were removed in PR #558 when the two
    // subsystems were redesigned to coexist. The slots stay
    // pinned so future discriminants don't reuse them and
    // perturb the wire format.
    assert_eq!(SDR_SCANNER_MUTEX_RESERVED_1, 1);
    assert_eq!(SDR_SCANNER_MUTEX_SCANNER_STOPPED_FOR_RECORDING, 2);
    assert_eq!(SDR_SCANNER_MUTEX_RESERVED_3, 3);
}

#[test]
fn translate_scanner_state_changed_listening() {
    let (event, owned_cstring, _) = translate_event(&DspToUi::ScannerStateChanged(
        sdr_scanner::ScannerState::Listening,
    ))
    .expect("ScannerStateChanged should translate");
    assert_eq!(event.kind, SDR_EVT_SCANNER_STATE_CHANGED);
    let payload = unsafe { event.payload.scanner_state };
    assert_eq!(payload.state, SDR_SCANNER_STATE_LISTENING);
    assert!(owned_cstring.is_none());
}

/// Build a synthetic `DspToUi::ScannerActiveChannelChanged`
/// for the FFI translation tests. The flattened `freq_hz` /
/// `name` fields are populated by the controller-side helper
/// in lockstep with the `key`, so the test fixture mirrors
/// that contract — passing the same string for `name` and
/// `key.name`. Other fields are filler the FFI doesn't read.
fn scanner_active_channel_event(latched_name: Option<&str>, freq_hz: u64) -> DspToUi {
    let key = latched_name.map(|name| sdr_scanner::ChannelKey {
        name: name.to_string(),
        frequency_hz: freq_hz,
    });
    DspToUi::ScannerActiveChannelChanged {
        key,
        freq_hz,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth: TEST_SCANNER_BANDWIDTH_HZ,
        name: latched_name.unwrap_or("").to_string(),
        ctcss: None,
        voice_squelch: None,
    }
}

#[test]
fn translate_scanner_active_channel_latched_carries_name_and_frequency() {
    let (event, owned_cstring, _) = translate_event(&scanner_active_channel_event(
        Some(TEST_SCANNER_NAME),
        TEST_SCANNER_FREQ_HZ,
    ))
    .expect("ScannerActiveChannelChanged should translate");
    assert_eq!(event.kind, SDR_EVT_SCANNER_ACTIVE_CHANNEL_CHANGED);
    let payload = unsafe { event.payload.scanner_active_channel };
    assert!(!payload.name_utf8.is_null());
    assert_eq!(payload.frequency_hz, TEST_SCANNER_FREQ_HZ);
    // Name lives in the owned CString — pointer must still
    // resolve to the original bytes via the standard
    // round-trip through CStr.
    let as_cstr = unsafe { std::ffi::CStr::from_ptr(payload.name_utf8) };
    assert_eq!(as_cstr.to_str().unwrap(), TEST_SCANNER_NAME);
    assert!(owned_cstring.is_some());
}

#[test]
fn translate_scanner_active_channel_idle_has_null_name_and_zero_freq() {
    let (event, owned_cstring, _) = translate_event(&scanner_active_channel_event(None, 0))
        .expect("idle ScannerActiveChannelChanged should translate");
    assert_eq!(event.kind, SDR_EVT_SCANNER_ACTIVE_CHANNEL_CHANGED);
    let payload = unsafe { event.payload.scanner_active_channel };
    // Idle sentinel: the host clears its "active channel"
    // readout when it sees (null, 0).
    assert!(payload.name_utf8.is_null());
    assert_eq!(payload.frequency_hz, 0);
    assert!(owned_cstring.is_none());
}

#[test]
fn translate_scanner_active_channel_sanitizes_interior_nul_in_name() {
    // NUL inside the channel name would normally break the
    // CString conversion; we replace it with '?' rather than
    // dropping the event (same policy as DeviceInfo /
    // endpoint strings). A channel name really shouldn't
    // contain a NUL, but if a host projects one accidentally
    // we'd rather surface a mangled name than silently drop
    // the latch event.
    //
    // **Bind the owned CString return** — `_owned` rather
    // than `_` keeps the storage alive for the
    // `CStr::from_ptr` call below. A bare `_` drops it
    // immediately at the end of the let binding, leaving
    // `name_utf8` dangling. Same lifetime contract as the
    // other translate_*-pattern tests.
    let (event, _owned_cstring, _) = translate_event(&scanner_active_channel_event(
        Some(TEST_SCANNER_NAME_WITH_INTERIOR_NUL),
        TEST_SCANNER_FREQ_HZ,
    ))
    .expect("sanitized ScannerActiveChannelChanged should translate");
    let payload = unsafe { event.payload.scanner_active_channel };
    let as_cstr = unsafe { std::ffi::CStr::from_ptr(payload.name_utf8) };
    assert_eq!(as_cstr.to_str().unwrap(), TEST_SCANNER_NAME_SANITIZED);
}

#[test]
fn translate_scanner_empty_rotation() {
    let (event, owned_cstring, _) = translate_event(&DspToUi::ScannerEmptyRotation)
        .expect("ScannerEmptyRotation should translate");
    assert_eq!(event.kind, SDR_EVT_SCANNER_EMPTY_ROTATION);
    assert!(owned_cstring.is_none());
}

#[test]
fn translate_scanner_mutex_stopped_maps_all_reasons() {
    // Pin every `ScannerMutexReason` arm individually — the
    // host-side toast routing branches on the wire integer
    // directly, so a silent remap (e.g., a refactor that
    // reorders the Rust enum) breaks every C ABI consumer
    // without failing any Rust-level type check. Per
    // `CodeRabbit` round 1 on PR #497.
    use sdr_core::messages::ScannerMutexReason;
    let cases = [
        (
            ScannerMutexReason::RecordingStoppedForScanner,
            SDR_SCANNER_MUTEX_RECORDING_STOPPED_FOR_SCANNER,
        ),
        (
            ScannerMutexReason::ScannerStoppedForRecording,
            SDR_SCANNER_MUTEX_SCANNER_STOPPED_FOR_RECORDING,
        ),
    ];
    for (reason, expected_int) in cases {
        let (event, owned_cstring, _) = translate_event(&DspToUi::ScannerMutexStopped(reason))
            .expect("ScannerMutexStopped should translate");
        assert_eq!(event.kind, SDR_EVT_SCANNER_MUTEX_STOPPED);
        let payload = unsafe { event.payload.scanner_mutex_stopped };
        assert_eq!(payload.reason, expected_int);
        assert!(owned_cstring.is_none());
    }
}
