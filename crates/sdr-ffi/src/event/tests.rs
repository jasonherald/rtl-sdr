use super::*;
use crate::error::SdrCoreError;
use crate::handle::SdrCore;
use crate::lifecycle::{sdr_core_create, sdr_core_destroy};
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};

fn make_handle() -> *mut SdrCore {
    let path = CString::new("").unwrap();
    let mut handle: *mut SdrCore = std::ptr::null_mut();
    let rc = unsafe { sdr_core_create(path.as_ptr(), &raw mut handle) };
    assert_eq!(rc, SdrCoreError::Ok.as_int());
    handle
}

#[test]
fn set_event_callback_null_handle_returns_invalid_handle() {
    let rc =
        unsafe { sdr_core_set_event_callback(std::ptr::null_mut(), None, std::ptr::null_mut()) };
    assert_eq!(rc, SdrCoreError::InvalidHandle.as_int());
}

// Top-of-module dummy callbacks. Rust lints (clippy's
// `items_after_statements`) complains when these are defined
// inside a test function body.
unsafe extern "C" fn noop_cb(_event: *const SdrEvent, _user_data: *mut c_void) {}

#[test]
fn set_event_callback_clear_then_set_then_clear() {
    let h = make_handle();
    // Clearing on a fresh engine is a no-op but must succeed.
    assert_eq!(
        unsafe { sdr_core_set_event_callback(h, None, std::ptr::null_mut()) },
        SdrCoreError::Ok.as_int()
    );

    assert_eq!(
        unsafe { sdr_core_set_event_callback(h, Some(noop_cb), std::ptr::null_mut()) },
        SdrCoreError::Ok.as_int()
    );

    // Clear again.
    assert_eq!(
        unsafe { sdr_core_set_event_callback(h, None, std::ptr::null_mut()) },
        SdrCoreError::Ok.as_int()
    );

    unsafe { sdr_core_destroy(h) };
}

// Shared atomic for the counting callback test below.
// Each test has its own static to avoid cross-test
// contamination in parallel runs.
static DISPATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn counting_cb(_event: *const SdrEvent, _user_data: *mut c_void) {
    DISPATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn dispatcher_exits_cleanly_on_destroy_with_callback_registered() {
    // Whether any events actually fire depends on what the
    // DSP controller happens to emit on startup (without a
    // real source running it may emit zero). What we're
    // really testing is that registering a callback and
    // then destroying the engine doesn't crash, hang, or
    // leave the dispatcher thread alive.
    DISPATCH_COUNTER.store(0, Ordering::Relaxed);

    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_event_callback(h, Some(counting_cb), std::ptr::null_mut()) },
        SdrCoreError::Ok.as_int()
    );

    // Give the dispatcher a tiny moment to process any
    // initial events before destroying.
    std::thread::sleep(std::time::Duration::from_millis(20));

    unsafe { sdr_core_destroy(h) };
    // Counter may be 0 (no events) or >0 (some fired). Both
    // are fine; the contract we're testing is just that
    // destroy returned, which it did.
    let _ = DISPATCH_COUNTER.load(Ordering::Relaxed);
}

// ------------------------------------------------------
//  Stateless construction of the event struct itself.
//  These don't need a real engine.
// ------------------------------------------------------

#[test]
fn event_kind_discriminants_match_header() {
    // Locks in the values against the header. If these drift,
    // `make ffi-header-check` (next checkpoint) will also
    // catch it, but this runs as a plain unit test.
    assert_eq!(SDR_EVT_SOURCE_STOPPED, 1);
    assert_eq!(SDR_EVT_SAMPLE_RATE_CHANGED, 2);
    assert_eq!(SDR_EVT_SIGNAL_LEVEL, 3);
    assert_eq!(SDR_EVT_DEVICE_INFO, 4);
    assert_eq!(SDR_EVT_GAIN_LIST, 5);
    assert_eq!(SDR_EVT_DISPLAY_BANDWIDTH, 6);
    assert_eq!(SDR_EVT_ERROR, 7);
    assert_eq!(SDR_EVT_AUDIO_RECORDING_STARTED, 8);
    assert_eq!(SDR_EVT_AUDIO_RECORDING_STOPPED, 9);
    assert_eq!(SDR_EVT_IQ_RECORDING_STARTED, 10);
    assert_eq!(SDR_EVT_IQ_RECORDING_STOPPED, 11);
    assert_eq!(SDR_EVT_NETWORK_SINK_STATUS, 12);
    assert_eq!(SDR_EVT_RTL_TCP_CONNECTION_STATE, 13);
    assert_eq!(SDR_EVT_SCANNER_STATE_CHANGED, 14);
    assert_eq!(SDR_EVT_SCANNER_ACTIVE_CHANNEL_CHANGED, 15);
    assert_eq!(SDR_EVT_SCANNER_EMPTY_ROTATION, 16);
    assert_eq!(SDR_EVT_SCANNER_MUTEX_STOPPED, 17);
    assert_eq!(SDR_EVT_VFO_OFFSET_CHANGED, 18);
    assert_eq!(SDR_EVT_BANDWIDTH_CHANGED, 19);
}

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

/// Fixture name for the latched-channel scanner tests. NOAA
/// Weather Radio is a canonical NFM channel with a recognisable
/// string, making debug output easy to read. Per `CodeRabbit`
/// round 2 on PR #497.
const TEST_SCANNER_NAME: &str = "NOAA Weather";
/// Fixture frequency paired with `TEST_SCANNER_NAME` — 162.550
/// MHz is the NOAA Weather Radio canonical channel. Same
/// constant appears in the command-side tests under the same
/// name, but keep them crate-private-per-test-module so a
/// future refactor that splits the modules doesn't collide.
const TEST_SCANNER_FREQ_HZ: u64 = 162_550_000;
/// Fixture bandwidth the active-channel event carries in the
/// flattened `bandwidth` field. NFM default — the FFI layer
/// doesn't read this field, so the exact value is only
/// meaningful as a recognisable placeholder in debug dumps.
const TEST_SCANNER_BANDWIDTH_HZ: f64 = 12_500.0;
/// Interior-NUL fixture for the sanitization test. The
/// dispatcher's `replace('\0', '?')` call should turn this
/// into "Weather?Channel" before it reaches the callback.
const TEST_SCANNER_NAME_WITH_INTERIOR_NUL: &str = "Weather\0Channel";
/// Expected output after the dispatcher sanitizes the
/// interior NUL in `TEST_SCANNER_NAME_WITH_INTERIOR_NUL`.
const TEST_SCANNER_NAME_SANITIZED: &str = "Weather?Channel";

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

#[test]
fn translate_apt_line_is_dropped_at_ffi_boundary() {
    // `DspToUi::AptLine` is intentionally dropped by the FFI
    // translation layer — the macOS frontend's APT viewer is a
    // separate ticket, and forwarding 2 KB-per-line image data
    // through a C ABI without a host consumer would be wasted
    // work. Pin that policy with a regression test: a future
    // change that exposes APT lines through the FFI (or
    // accidentally lets the variant fall through to the
    // catch-all panic arm) trips this assert before it can
    // reach the Mac side. Per CodeRabbit on PR #503.
    let line = sdr_core::messages::AptLine::default();
    let msg = DspToUi::AptLine(Box::new(line));
    assert!(
        translate_event(&msg).is_none(),
        "AptLine must not translate to a wire event yet",
    );
}

/// Build a minimal `AcarsMessage` for the FFI drop-policy
/// tests. `AcarsMessage` doesn't derive `Default` because
/// `SystemTime` has no canonical default; pin a fixture
/// here so the policy tests don't repeat the boilerplate.
fn dummy_acars_message() -> sdr_acars::AcarsMessage {
    sdr_acars::AcarsMessage {
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        channel_idx: 0,
        freq_hz: 131_550_000.0,
        level_db: -42.0,
        error_count: 0,
        mode: b'2',
        label: *b"H1",
        block_id: b'A',
        ack: b'!',
        aircraft: arrayvec::ArrayString::from(".TEST123").unwrap_or_default(),
        flight_id: None,
        message_no: None,
        text: String::new(),
        end_of_message: true,
        reassembled_block_count: 1,
        parsed: None,
    }
}

#[test]
fn translate_acars_message_is_dropped_at_ffi_boundary() {
    // ACARS variants (epic #474) follow the same drop-here
    // policy as `AptLine` until the macOS frontend ships its
    // own Aviation viewer. Pin the contract: any future
    // change that surfaces ACARS through the C ABI without
    // also wiring the host consumer trips this assert.
    let msg = DspToUi::AcarsMessage(Box::new(dummy_acars_message()));
    assert!(
        translate_event(&msg).is_none(),
        "AcarsMessage must not translate to a wire event yet",
    );
}

#[test]
fn translate_acars_channel_stats_is_dropped_at_ffi_boundary() {
    // Cover three widths to exercise the variable-length
    // payload (post-#592 the variant is `Box<[ChannelStats]>`,
    // not the fixed-width `[ChannelStats; 6]`):
    //   - 1 channel (Custom region with a single freq)
    //   - ACARS_CHANNEL_COUNT (predefined US-6 / Europe)
    //   - ACARS_CHANNEL_COUNT + 1 (Custom region wider than
    //     the predefined count, up to MAX_CUSTOM_CHANNELS=8)
    // CR round 1 on PR #598.
    for n in [
        1,
        sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT,
        sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT + 1,
    ] {
        let msg = DspToUi::AcarsChannelStats(
            vec![sdr_acars::ChannelStats::default(); n].into_boxed_slice(),
        );
        assert!(
            translate_event(&msg).is_none(),
            "AcarsChannelStats(width={n}) must not translate to a wire event yet",
        );
    }
}

#[test]
fn translate_acars_enabled_changed_is_dropped_at_ffi_boundary() {
    let msg = DspToUi::AcarsEnabledChanged(Ok(true));
    assert!(
        translate_event(&msg).is_none(),
        "AcarsEnabledChanged(Ok) must not translate to a wire event yet",
    );
    let msg = DspToUi::AcarsEnabledChanged(Err(
        sdr_core::acars_airband_lock::AcarsEnableError::UnsupportedSourceType(
            sdr_core::messages::SourceType::File,
        ),
    ));
    assert!(
        translate_event(&msg).is_none(),
        "AcarsEnabledChanged(Err) must not translate to a wire event yet",
    );
}

#[test]
fn translate_acars_output_error_is_dropped_at_ffi_boundary() {
    let msg = DspToUi::AcarsOutputError {
        kind: "udp",
        message: "could not resolve host".to_string(),
    };
    assert!(
        translate_event(&msg).is_none(),
        "AcarsOutputError must not translate to a wire event yet",
    );
}

#[test]
fn translate_sstv_vis_detected_is_dropped_at_ffi_boundary() {
    // Same drop-at-boundary contract as `SstvLineDecoded` /
    // `SstvImageComplete` — the mode-display follow-up keeps
    // SSTV strictly Linux-only at the FFI boundary until the
    // macOS viewer ticket lands. Per epic #472 mode-display
    // follow-up.
    let msg = DspToUi::SstvVisDetected {
        mode_label: "PD120",
    };
    assert!(
        translate_event(&msg).is_none(),
        "SstvVisDetected must not translate to a wire event yet",
    );
}

#[test]
fn translate_sstv_line_decoded_is_dropped_at_ffi_boundary() {
    // SSTV variants (epic #472) are Linux-only for V1; the
    // macOS FFI layer will get its own ticket when the FFI
    // layer gains an SSTV viewer. Pin the drop-at-boundary
    // contract so a future change that accidentally surfaces
    // SSTV through the C ABI trips this assert first.
    // Per CodeRabbit round 1 on PR #599.
    let msg = DspToUi::SstvLineDecoded(0);
    assert!(
        translate_event(&msg).is_none(),
        "SstvLineDecoded must not translate to a wire event yet",
    );
}

#[test]
fn translate_sstv_image_complete_is_dropped_at_ffi_boundary() {
    // Same drop-at-boundary contract as `SstvLineDecoded`
    // above — the pixel Vec should never flow through the
    // C ABI until the macOS viewer ticket ships. Per
    // CodeRabbit round 1 on PR #599.
    let msg = DspToUi::SstvImageComplete {
        width: 1,
        height: 1,
        pixels: vec![[0_u8, 0, 0]],
    };
    assert!(
        translate_event(&msg).is_none(),
        "SstvImageComplete must not translate to a wire event yet",
    );
}

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

// ------------------------------------------------------
//  translate_event — network sink status (ABI 0.9, #247)
//
//  Direct Rust-side coverage of the three NetworkSinkStatus
//  arms, including NULL vs non-NULL string cases and the
//  `Protocol::TcpClient` → `SDR_NETWORK_PROTOCOL_TCP_SERVER`
//  name-bridge. Locks the contract in before Swift decoding
//  sees it. Per `CodeRabbit` round 1 on PR #352.
// ------------------------------------------------------

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

#[test]
fn vfo_offset_changed_translates_to_event() {
    // Pin the ABI 0.23 surface (#488). Replaces the prior
    // "intentionally withheld" regression that asserted the
    // variant was dropped — that was the v1 contract before
    // the macOS VFO reset affordances landed.
    /// Representative non-zero VFO offset — 25 kHz is typical
    /// of what click-to-tune and drag flows emit in practice.
    const TEST_VFO_OFFSET_HZ: f64 = 25_000.0;
    use sdr_core::DspToUi;
    let (event, owned, _) = translate_event(&DspToUi::VfoOffsetChanged(TEST_VFO_OFFSET_HZ))
        .expect("VfoOffsetChanged should translate to an event in ABI 0.23");
    assert_eq!(event.kind, SDR_EVT_VFO_OFFSET_CHANGED);
    // SAFETY: kind matches the union variant we wrote.
    let payload = unsafe { event.payload.vfo_offset_hz };
    assert!((payload - TEST_VFO_OFFSET_HZ).abs() < f64::EPSILON);
    assert!(owned.is_none());
}

#[test]
fn bandwidth_changed_translates_to_event() {
    // Pin the ABI 0.24 surface (#616 / CodeRabbit round 1).
    // The `vfo_offset_changed` symmetric event landed in
    // 0.23; this test guards the matching bandwidth path
    // so the macOS bandwidth-row reset icon's enabled
    // state doesn't go stale on engine-internal changes.
    /// Representative NFM voice channel width — exercises
    /// the round-trip with a realistic value rather than
    /// a magic round number.
    const TEST_BANDWIDTH_HZ: f64 = 12_500.0;
    use sdr_core::DspToUi;
    let (event, owned, _) = translate_event(&DspToUi::BandwidthChanged(TEST_BANDWIDTH_HZ))
        .expect("BandwidthChanged should translate to an event in ABI 0.24");
    assert_eq!(event.kind, SDR_EVT_BANDWIDTH_CHANGED);
    // SAFETY: kind matches the union variant we wrote.
    let payload = unsafe { event.payload.bandwidth_hz };
    assert!((payload - TEST_BANDWIDTH_HZ).abs() < f64::EPSILON);
    assert!(owned.is_none());
}

#[test]
fn sdr_event_payload_size_is_reasonable() {
    // Sanity check on the union layout. The largest payload
    // today is `SdrEventRtlTcpConnectionState` (kind i32 +
    // utf8 ptr + attempt u32 + retry_in_secs f64 + gain_count
    // u32) which lands at 40 bytes with natural alignment on
    // 64-bit targets. Budget is 48 so a future connection-
    // state extension (e.g. endpoint string alongside tuner
    // name) has a little headroom before the size check
    // tightens. Past budgets: 32 (pre-ABI-0.11 with only the
    // network sink status payload).
    let size = std::mem::size_of::<SdrEvent>();
    assert!(
        size <= 48,
        "SdrEvent size {size} exceeds 48-byte budget — may indicate an unintended union growth"
    );
}
