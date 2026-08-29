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
/// An alternate, still-valid center used to prove the self-check
/// reacts to a center-only geometry change (e.g. a retune) and not
/// just a rate change.
const OTHER_CENTER_HZ: f64 = 137_600_000.0;

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
    state.orbcomm_geometry = Some((TEST_SOURCE_RATE_HZ, TEST_CENTER_HZ));
    let _ = drain(&dsp_rx);

    handle_set_orbcomm_enabled(&mut state, &dsp_tx, true);

    assert!(state.orbcomm_enabled, "enable must set the flag");
    assert!(
        state.orbcomm_bank.is_none(),
        "enable must clear the bank for fresh geometry pickup"
    );
    assert!(!state.orbcomm_init_failed, "enable must clear the latch");
    assert!(
        state.orbcomm_geometry.is_none(),
        "enable must clear the tracked geometry"
    );
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
    state.orbcomm_geometry = Some((TEST_SOURCE_RATE_HZ, TEST_CENTER_HZ));
    let _ = drain(&dsp_rx);

    handle_set_orbcomm_enabled(&mut state, &dsp_tx, false);

    assert!(!state.orbcomm_enabled, "disable must clear the flag");
    assert!(
        state.orbcomm_bank.is_none(),
        "disable must clear the bank too"
    );
    assert!(!state.orbcomm_init_failed);
    assert!(state.orbcomm_geometry.is_none());
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(false))),
        "expected OrbcommEnabledChanged(false), got {events:?}"
    );
}

/// Issue #865, CR round 2 — `orbcomm_enabled` is the actual on/off
/// state (unlike `acars_region`, a config preference that legitimately
/// persists across a stop). `cleanup()` must force it off, tear down
/// the bank/latch/geometry, and ack `OrbcommEnabledChanged(false)` so
/// a UI toggle left on can't survive a Stop with no live tap behind it.
#[test]
fn cleanup_forces_orbcomm_enabled_off_with_ack() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.orbcomm_enabled = true;
    state.orbcomm_bank = Some(test_bank());
    state.orbcomm_geometry = Some((TEST_SOURCE_RATE_HZ, TEST_CENTER_HZ));
    let _ = drain(&dsp_rx);

    cleanup(&mut state, &dsp_tx);

    assert!(!state.orbcomm_enabled, "cleanup must force the toggle off");
    assert!(state.orbcomm_bank.is_none(), "cleanup must drop the bank");
    assert!(!state.orbcomm_init_failed);
    assert!(state.orbcomm_geometry.is_none());
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(false))),
        "expected OrbcommEnabledChanged(false) from cleanup, got {events:?}"
    );
}

#[test]
fn tap_lazy_inits_and_emits_events() {
    let mut bank: Option<sdr_orbcomm::ChannelBank> = None;
    let mut init_failed = false;
    let mut geometry: Option<(f64, f64)> = None;
    let mut events = Vec::new();
    let (tx, rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 4096];

    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        &mut geometry,
        TEST_SOURCE_RATE_HZ,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );

    assert!(bank.is_some(), "first call should lazily build the bank");
    assert!(!init_failed);
    assert_eq!(geometry, Some((TEST_SOURCE_RATE_HZ, TEST_CENTER_HZ)));
    // Zero-filled IQ carries no signal, so no packet/message events —
    // real-signal decode coverage is `sdr_orbcomm`'s own job (see the
    // module doc comment above).
    assert!(events.is_empty(), "zero-filled IQ must not produce events");
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

#[test]
fn init_failure_latches_at_the_same_geometry() {
    let mut bank: Option<sdr_orbcomm::ChannelBank> = None;
    let mut init_failed = false;
    let mut geometry: Option<(f64, f64)> = None;
    let mut events = Vec::new();
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 16];

    // Absurd geometry: a zero source rate is never in-span for any
    // channel (`channel_in_span`'s `source_rate_hz <= 0.0` guard),
    // so construction fails deterministically.
    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        &mut geometry,
        0.0,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );
    assert!(bank.is_none());
    assert!(init_failed, "bad geometry should set the latch");
    assert_eq!(geometry, Some((0.0, TEST_CENTER_HZ)));

    // A second call at the exact SAME (still-bad) geometry must
    // no-op — no repeated construction attempt, no warn-spam. This
    // is the steady-state case: same block cadence, same geometry,
    // every call.
    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        &mut geometry,
        0.0,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );
    assert!(
        bank.is_none(),
        "latched init_failed must skip the retry at unchanged geometry"
    );
    assert!(init_failed);
    assert_eq!(geometry, Some((0.0, TEST_CENTER_HZ)));
}

/// Issue #865, CR round 1 — the tap must self-check its tracked
/// geometry against the live `(source_rate_hz, center_hz)` on every
/// call and rebuild on a mismatch, so call sites that bypass
/// `handle_tune` / `handle_set_sample_rate` / `handle_set_decimation`
/// (the scanner's direct `state.center_freq` / decimation writes in
/// `controller/scanner.rs`, for one) can't leave the bank silently
/// decoding stale geometry.
#[test]
fn tap_rebuilds_on_geometry_mismatch() {
    let mut bank: Option<sdr_orbcomm::ChannelBank> = None;
    let mut init_failed = false;
    let mut geometry: Option<(f64, f64)> = None;
    let mut events = Vec::new();
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 16];

    // First call builds successfully at the initial geometry.
    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        &mut geometry,
        TEST_SOURCE_RATE_HZ,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );
    assert!(bank.is_some(), "first call builds at the initial geometry");
    assert_eq!(geometry, Some((TEST_SOURCE_RATE_HZ, TEST_CENTER_HZ)));

    // Geometry changed underneath the tap (e.g. a retune or scanner
    // hop) WITHOUT any explicit invalidation call — simulated here by
    // simply passing a different center on the next call, plus an
    // absurd rate. If the tap were reusing the stale bank rather than
    // self-checking, `bank` would stay `Some` regardless of what
    // geometry is passed now; observing `None` here proves a fresh
    // construction attempt was made (and failed, at the absurd rate).
    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        &mut geometry,
        0.0,
        OTHER_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );
    assert!(
        bank.is_none(),
        "mismatched geometry must drop the stale bank and attempt a rebuild"
    );
    assert!(
        init_failed,
        "the rebuild attempt at the absurd rate must fail"
    );
    assert_eq!(geometry, Some((0.0, OTHER_CENTER_HZ)));

    // A THIRD call back at valid geometry must retry despite the
    // latch — proving the latch clears on ANY geometry change, not
    // just a successful rebuild (a failed attempt must be retriable
    // too, since the new geometry may well be valid).
    super::orbcomm_decode_tap(
        &mut bank,
        &mut init_failed,
        &mut geometry,
        TEST_SOURCE_RATE_HZ,
        TEST_CENTER_HZ,
        &iq,
        &mut events,
        &tx,
    );
    assert!(
        bank.is_some(),
        "valid geometry after a failed attempt must retry and succeed"
    );
    assert!(!init_failed);
    assert_eq!(geometry, Some((TEST_SOURCE_RATE_HZ, TEST_CENTER_HZ)));
}
