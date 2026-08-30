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
///
/// CR round 3 (smoke-test fix) extends this: engaging via the real
/// handler also forces frontend decimation to 1, so `cleanup()` must
/// restore it too — otherwise a Stop while Orbcomm is enabled would
/// leave every OTHER mode (NFM, WFM, ...) stuck decoding at decim=1
/// on the next Start.
#[test]
fn cleanup_forces_orbcomm_enabled_off_with_ack() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let prior_decim = state.frontend.decim_ratio();
    handle_set_orbcomm_enabled(&mut state, &dsp_tx, true);
    assert!(state.orbcomm_enabled, "test setup: orbcomm must be engaged");
    assert_eq!(state.frontend.decim_ratio(), 1, "test setup: decim forced");
    state.orbcomm_bank = Some(test_bank());
    state.orbcomm_geometry = Some((TEST_SOURCE_RATE_HZ, TEST_CENTER_HZ));
    let _ = drain(&dsp_rx);

    cleanup(&mut state, &dsp_tx);

    assert!(!state.orbcomm_enabled, "cleanup must force the toggle off");
    assert!(state.orbcomm_bank.is_none(), "cleanup must drop the bank");
    assert!(!state.orbcomm_init_failed);
    assert!(state.orbcomm_geometry.is_none());
    assert!(
        state.orbcomm_pre_decim.is_none(),
        "cleanup must clear the saved decimation"
    );
    assert_eq!(
        state.frontend.decim_ratio(),
        prior_decim,
        "cleanup must restore the pre-engage decimation"
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(false))),
        "expected OrbcommEnabledChanged(false) from cleanup, got {events:?}"
    );
}

/// Issue #865, CR round 3 (smoke-test fix) — engaging Orbcomm must
/// force frontend decimation to 1 (mirrors ACARS's
/// `ACARS_FRONTEND_DECIM` engage forcing) and remember whatever
/// decimation was active beforehand so disable can restore it.
#[test]
fn enable_forces_decim_1_and_saves_prior() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let prior_decim = state.frontend.decim_ratio();
    assert_ne!(
        prior_decim, 1,
        "test assumes the default DspState decimation isn't already 1"
    );
    let _ = drain(&dsp_rx);

    handle_set_orbcomm_enabled(&mut state, &dsp_tx, true);

    assert_eq!(
        state.frontend.decim_ratio(),
        1,
        "engage must force frontend decimation to 1"
    );
    assert_eq!(
        state.orbcomm_pre_decim,
        Some(prior_decim),
        "engage must save the pre-engage decimation for disable to restore"
    );
    assert!(state.orbcomm_enabled);
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(true))),
        "expected OrbcommEnabledChanged(true), got {events:?}"
    );
}

/// Issue #865, CR round 3 — the counterpart of
/// `enable_forces_decim_1_and_saves_prior`: disabling must restore
/// the decimation engage saved, not leave the frontend pinned at 1.
#[test]
fn disable_restores_prior_decim() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let prior_decim = state.frontend.decim_ratio();
    handle_set_orbcomm_enabled(&mut state, &dsp_tx, true);
    assert_eq!(state.frontend.decim_ratio(), 1, "test setup: decim forced");
    let _ = drain(&dsp_rx);

    handle_set_orbcomm_enabled(&mut state, &dsp_tx, false);

    assert_eq!(
        state.frontend.decim_ratio(),
        prior_decim,
        "disable must restore the pre-engage decimation"
    );
    assert!(
        state.orbcomm_pre_decim.is_none(),
        "disable must clear the saved decimation"
    );
    assert!(!state.orbcomm_enabled);
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(false))),
        "expected OrbcommEnabledChanged(false), got {events:?}"
    );
}

/// Issue #865, CR round 3 — engage must be refused while the scanner
/// is running: the scanner mutates frontend decimation directly
/// (`handle_scanner_retune` / demod-mode hops), which would fight the
/// decim=1 Orbcomm forces. Mirrors ACARS's own scanner-running
/// engage refusal.
#[test]
fn enable_refused_while_scanner_enabled() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetScannerEnabled(true));
    assert!(
        state.scanner.is_enabled(),
        "test setup: scanner must be running"
    );
    let prior_decim = state.frontend.decim_ratio();
    let _ = drain(&dsp_rx);

    handle_set_orbcomm_enabled(&mut state, &dsp_tx, true);

    assert!(
        !state.orbcomm_enabled,
        "engage must be refused while the scanner runs"
    );
    assert!(state.orbcomm_pre_decim.is_none());
    assert_eq!(
        state.frontend.decim_ratio(),
        prior_decim,
        "a refused engage must not touch the frontend"
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::OrbcommEnabledChanged(false))),
        "expected OrbcommEnabledChanged(false), got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            DspToUi::Error(msg) if msg.to_lowercase().contains("scanner")
        )),
        "expected a scanner-related refusal Error, got {events:?}"
    );
}

/// Issue #865, CR round 3 — the symmetric refusal: scanner enable
/// must be rejected while Orbcomm is engaged, mirroring the ACARS
/// airband-lock check right beside it in
/// `controller/scanner.rs::handle_set_scanner_enabled`.
#[test]
fn scanner_enable_refused_while_orbcomm_enabled() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_set_orbcomm_enabled(&mut state, &dsp_tx, true);
    assert!(state.orbcomm_enabled, "test setup: orbcomm must be engaged");
    let _ = drain(&dsp_rx);

    handle_command(&mut state, &dsp_tx, UiToDsp::SetScannerEnabled(true));

    assert!(
        !state.scanner.is_enabled(),
        "scanner enable must be refused while Orbcomm is active"
    );
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DspToUi::Error(msg) if msg.to_lowercase().contains("orbcomm")
        )),
        "expected an orbcomm-related refusal Error, got {events:?}"
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

/// Count the `DspToUi::Error` messages whose text starts with the
/// Orbcomm init-failure prefix.
fn orbcomm_init_errors(events: &[DspToUi]) -> Vec<&String> {
    events
        .iter()
        .filter_map(|e| match e {
            DspToUi::Error(text)
                if text.starts_with(crate::controller::orbcomm::ORBCOMM_INIT_ERROR_PREFIX) =>
            {
                Some(text)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn init_failure_latches_at_the_same_geometry() {
    let mut bank: Option<sdr_orbcomm::ChannelBank> = None;
    let mut init_failed = false;
    let mut geometry: Option<(f64, f64)> = None;
    let mut events = Vec::new();
    let (tx, rx) = mpsc::channel::<DspToUi>();
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

    // Several more calls at the exact SAME (still-bad) geometry must
    // no-op — no repeated construction attempt, no warn-spam. This
    // is the steady-state case: same block cadence, same geometry,
    // every call.
    for _ in 0..5 {
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
    }
    assert!(
        bank.is_none(),
        "latched init_failed must skip the retry at unchanged geometry"
    );
    assert!(init_failed);
    assert_eq!(geometry, Some((0.0, TEST_CENTER_HZ)));

    // Final review, C2 — the failure has to reach the user, not just
    // the log: the toggle stays ON with a dead activity strip behind
    // it otherwise. Exactly one error across all six calls, and it
    // must carry the `OrbcommError` display text so the message says
    // *why*.
    let sent = drain(&rx);
    let errors = orbcomm_init_errors(&sent);
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one surfaced init error across 6 tap calls, got {sent:?}"
    );
    let expected =
        sdr_orbcomm::ChannelBank::new(0.0, TEST_CENTER_HZ, &sdr_orbcomm::ORBCOMM_CHANNELS_HZ)
            .err()
            .map(|e| e.to_string())
            .expect("the absurd geometry must fail to construct");
    assert!(
        errors[0].ends_with(&expected),
        "surfaced error {:?} does not carry the OrbcommError text {expected:?}",
        errors[0]
    );
}

/// Issue #865, CR round 1 — the tap must self-check its tracked
/// geometry against the live `(source_rate_hz, center_hz)` on every
/// call and rebuild on a mismatch, so ordinary geometry-mutating call
/// sites (`handle_tune`, `handle_set_sample_rate`, ...) don't need an
/// invalidation clear of their own. (The scanner's own decimation
/// writes are ruled out by mutual exclusion instead — see
/// `enable_refused_while_scanner_enabled` /
/// `scanner_enable_refused_while_orbcomm_enabled` below, CR round 3 —
/// but this self-check remains the general safety net.)
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
