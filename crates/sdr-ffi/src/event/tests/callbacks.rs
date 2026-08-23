use super::*;

#[test]
fn set_event_callback_null_handle_returns_invalid_handle() {
    let rc =
        unsafe { sdr_core_set_event_callback(std::ptr::null_mut(), None, std::ptr::null_mut()) };
    assert_eq!(rc, SdrCoreError::InvalidHandle.as_int());
}

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
