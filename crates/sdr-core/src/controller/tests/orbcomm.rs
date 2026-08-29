use super::*;

// Orbcomm decode-tap + enable-plumbing unit tests (issue #865).
// Inlined here per the workspace convention (tests at file bottom
// / dedicated test module, accessing `orbcomm_decode_tap` /
// `handle_set_orbcomm_enabled` directly through the module
// hierarchy — same pattern as `recording_acars.rs`'s
// `acars_decode_tap` tests). Real-signal decode coverage lives in
// `sdr_orbcomm`'s own crate tests; this file only exercises the
// controller-side plumbing.

/// Source rate + center used across these tests: a 2.4 Msps span
/// centred on the Orbcomm band puts all nine `ORBCOMM_CHANNELS_HZ`
/// comfortably in span (mirrors `sdr_orbcomm::channelizer`'s own
/// `SOURCE_RATE_HZ` / test geometry).
const TEST_SOURCE_RATE_HZ: f64 = 2_400_000.0;
const TEST_CENTER_HZ: f64 = 137_500_000.0;

fn test_bank() -> sdr_orbcomm::ChannelBank {
    sdr_orbcomm::ChannelBank::new(
        TEST_SOURCE_RATE_HZ,
        TEST_CENTER_HZ,
        &sdr_orbcomm::ORBCOMM_CHANNELS_HZ,
    )
    .expect("test geometry puts every Orbcomm channel in span")
}

#[test]
fn set_enabled_acks_and_clears_bank() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.orbcomm_bank = Some(test_bank());
    state.orbcomm_init_failed = true; // Simulate a stale prior failure.
    let _ = drain(&dsp_rx);

    handle_set_orbcomm_enabled(&mut state, &dsp_tx, true);

    assert!(state.orbcomm_enabled, "enable must set the flag");
    assert!(
        state.orbcomm_bank.is_none(),
        "enable must clear the bank for fresh geometry pickup"
    );
    assert!(!state.orbcomm_init_failed, "enable must clear the latch");
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(true))),
        "expected OrbcommEnabledChanged(true), got {events:?}"
    );

    // Disable must ALSO clear a freshly (post-enable) rebuilt bank —
    // both directions clear, not just enable.
    state.orbcomm_bank = Some(test_bank());
    let _ = drain(&dsp_rx);

    handle_set_orbcomm_enabled(&mut state, &dsp_tx, false);

    assert!(!state.orbcomm_enabled, "disable must clear the flag");
    assert!(
        state.orbcomm_bank.is_none(),
        "disable must clear the bank too"
    );
    assert!(!state.orbcomm_init_failed);
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(false))),
        "expected OrbcommEnabledChanged(false), got {events:?}"
    );
}

#[test]
fn tap_lazy_inits_and_emits_events() {
    let mut bank: Option<sdr_orbcomm::ChannelBank> = None;
    let mut init_failed = false;
    let mut events = Vec::new();
    let (tx, rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 4096];

    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        TEST_SOURCE_RATE_HZ,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );

    assert!(bank.is_some(), "first call should lazily build the bank");
    assert!(!init_failed);
    // Zero-filled IQ carries no signal, so no packet/message events —
    // real-signal decode coverage is `sdr_orbcomm`'s own job (see the
    // module doc comment above).
    assert!(events.is_empty(), "zero-filled IQ must not produce events");
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

#[test]
fn init_failure_latches_once() {
    let mut bank: Option<sdr_orbcomm::ChannelBank> = None;
    let mut init_failed = false;
    let mut events = Vec::new();
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 16];

    // Absurd geometry: a zero source rate is never in-span for any
    // channel (`channel_in_span`'s `source_rate_hz <= 0.0` guard),
    // so construction fails deterministically.
    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        0.0,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );
    assert!(bank.is_none());
    assert!(init_failed, "bad geometry should set the latch");

    // A second call with now-VALID geometry must still no-op: the
    // latch only clears via a geometry-invalidation site (source
    // stop / retune / rate change) or a fresh `SetOrbcommEnabled`
    // dispatch — never on its own inside the tap.
    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        TEST_SOURCE_RATE_HZ,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );
    assert!(
        bank.is_none(),
        "latched init_failed must skip the retry even under valid geometry"
    );
    assert!(init_failed);
}
