use super::*;

#[test]
fn before_pass_advances_to_recording_after_settle() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    // Initial arm.
    tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::BeforePass { .. }));
    // Pre-settle tick: still in BeforePass.
    let later = now + ChronoDuration::seconds(2);
    tick(&mut r, later, &pass, true, false);
    assert!(matches!(r.state(), State::BeforePass { .. }));
    // Past settle window: advance to Recording.
    let later = now + ChronoDuration::seconds(SETTLE_SECS);
    tick(&mut r, later, &pass, true, false);
    assert!(matches!(r.state(), State::Recording { .. }));
}

#[test]
fn recording_advances_to_finalizing_at_los() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 600, 50.0); // 10 min pass
    tick(&mut r, now, &pass, true, false);
    let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle, &pass, true, false);
    assert!(matches!(r.state(), State::Recording { .. }));
    // Tick past LOS.
    let los_plus_one = pass.end + ChronoDuration::seconds(1);
    let actions = tick(&mut r, los_plus_one, &pass, true, false);
    assert!(matches!(r.state(), State::Finalizing { .. }));
    // METEOR-M 2 dispatches `SaveLrptPass`, not `SavePng`; the
    // helper used to be NOAA 19 (APT, hence `SavePng`) but
    // POES decommissioning forced a swap to a still-cataloged
    // satellite. The state-machine transition is what this
    // test pins — the specific pass-output variant is
    // protocol-dependent and tested per-protocol elsewhere.
    // The success / failure toast is the wiring layer's
    // responsibility now (it knows the export outcome).
    // Asserting absence of any Toast keeps the recorder
    // honest about what it claims.
    assert!(matches!(
        actions[0],
        Action::SavePng(_) | Action::SaveLrptPass(_) | Action::SaveSstvPass(_)
    ));
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Toast { .. })),
        "recorder must not announce save success before export — actions: {actions:?}"
    );
}

fn pinned_saved_tune() -> SavedTune {
    SavedTune {
        freq_hz: SAVED_FREQ_HZ,
        vfo_offset_hz: SAVED_VFO_OFFSET_HZ, // pin a non-zero offset for the round trip
        mode: DemodMode::Wfm,
        bandwidth_hz: SAVED_BANDWIDTH_HZ,
        was_running: false,
        scanner_running: true, // pin: pre-AOS scan must come back at LOS
        // pin: pre-AOS audio-chain settings must come back at LOS
        squelch_enabled: true,
        auto_squelch_enabled: true,
        squelch_db: SAVED_SQUELCH_DB,
        ctcss_mode: CtcssMode::Tone(SAVED_CTCSS_TONE_HZ),
        fm_if_nr_enabled: true,
        // pin: pre-AOS deemphasis (US 75 µs index = 2) and
        // notch must come back at LOS so the user's
        // FM-broadcast listening setup isn't silently
        // wiped after the pass.
        deemphasis_idx: 2,
        notch_enabled: true,
        // pin: pre-AOS Doppler-tracker switch must come
        // back at LOS so the user's preference survives
        // the imaging-protocol pass-time disable.
        doppler_enabled: true,
    }
}

/// Assert `t` is exactly `pinned_saved_tune()`.
fn assert_pinned_tune_restored(t: &SavedTune) {
    assert_eq!(t.freq_hz, SAVED_FREQ_HZ);
    assert_eq!(t.vfo_offset_hz, SAVED_VFO_OFFSET_HZ);
    assert_eq!(t.mode, DemodMode::Wfm);
    assert_eq!(t.bandwidth_hz, SAVED_BANDWIDTH_HZ);
    assert!(!t.was_running);
    assert!(t.scanner_running);
    assert!(t.squelch_enabled);
    assert!(t.auto_squelch_enabled);
    assert!((t.squelch_db - SAVED_SQUELCH_DB).abs() < f32::EPSILON);
    assert!(
        matches!(t.ctcss_mode, CtcssMode::Tone(hz) if (hz - SAVED_CTCSS_TONE_HZ).abs() < f32::EPSILON)
    );
    assert!(t.fm_if_nr_enabled);
    assert_eq!(t.deemphasis_idx, 2);
    assert!(t.notch_enabled);
    assert!(t.doppler_enabled);
}

#[test]
fn finalizing_advances_to_idle_with_restore_action() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 60, 50.0);
    // The pre-AOS tune is captured on the arming tick.
    r.tick(
        now,
        std::slice::from_ref(&pass),
        true,
        false,
        DEFAULT_MIN_ELEV_DEG,
        pinned_saved_tune(),
    );
    let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle, &pass, true, false);
    let los_plus = pass.end + ChronoDuration::seconds(1);
    tick(&mut r, los_plus, &pass, true, false);
    assert!(matches!(r.state(), State::Finalizing { .. }));
    let actions = tick(&mut r, los_plus, &pass, true, false);
    assert!(matches!(r.state(), State::Idle));
    match &actions[0] {
        Action::RestoreTune(t) => assert_pinned_tune_restored(t),
        other => panic!("expected RestoreTune, got {other:?}"),
    }
}

#[test]
#[ignore = "exercises APT-specific recorder dispatch (SavePng / audio); APT path is dormant pending a future Cubesat catalog entry — see KNOWN_SATELLITES doc comment about August 2025 NOAA POES decommissioning"]
fn los_during_before_pass_still_emits_save_png() {
    // Regression: a 1 Hz driver stall (sleep / suspend) can
    // jump the recorder from BeforePass to Finalizing without
    // ever entering Recording. The PNG must still be saved —
    // otherwise the pass completes silently and the user
    // loses whatever decoder lines did arrive during the
    // stall window.
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 60, 50.0); // 1 min pass, 3 s lead-in
    tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::BeforePass { .. }));
    // Jump to a moment past LOS (simulate stalled driver).
    let post_los = pass.end + ChronoDuration::seconds(5);
    let actions = tick(&mut r, post_los, &pass, true, false);
    assert!(matches!(r.state(), State::Finalizing { .. }));
    assert!(
        actions.iter().any(|a| matches!(a, Action::SavePng(_))),
        "BeforePass→Finalizing must emit SavePng even when stalled past LOS"
    );
}

/// Regression for the BeforePass-stall LOS edge case
/// (`#544` + `CodeRabbit` round 1 on PR #560). The
/// `tick_before_pass` short-circuit jumps straight to
/// `Finalizing` without entering `Recording` — earlier
/// drafts of the #544 fix only added `ResetImagingDecoders`
/// to the `tick_recording` LOS path, so a stalled driver
/// would skip the reset and leak APT/LRPT state into the
/// next pass on exactly the edge case the issue is trying
/// to close. Both LOS paths now go through the shared
/// `los_actions_for` helper; this test pins that the
/// stalled-BeforePass path still emits the reset.
#[test]
fn los_during_before_pass_still_emits_reset_imaging_decoders() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 60, 50.0);
    tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::BeforePass { .. }));
    let post_los = pass.end + ChronoDuration::seconds(5);
    let actions = tick(&mut r, post_los, &pass, true, false);
    assert!(matches!(r.state(), State::Finalizing { .. }));
    assert_save_before_reset(&actions, "BeforePass-stall LOS");
}

/// Per #544: LOS must emit `ResetImagingDecoders` so the
/// in-flight APT/LRPT decoder state from the just-finished
/// pass doesn't bleed into the next one when the source
/// stays open across the LOS → AOS boundary. Pinning the
/// emit here so the wiring layer's between-pass cleanup
/// hook can't go stealth-quiet on a refactor.
///
/// Helper for the three LOS-reset tests below: assert the
/// LOS contract that save runs BEFORE reset — the save
/// action's snapshot read of the shared `LrptImage` would
/// otherwise capture an empty buffer instead of the
/// just-finished pass. Pure positional assertion, panics
/// with a clear message that includes the offending action
/// vec. Per `CodeRabbit` round 2 on PR #560.
fn assert_save_before_reset(actions: &[Action], label: &str) {
    let save_idx = actions
        .iter()
        .position(|a| matches!(a, Action::SavePng(_) | Action::SaveLrptPass(_)))
        .unwrap_or_else(|| panic!("{label}: LOS must emit a save action; got {actions:?}"));
    let reset_idx = actions
        .iter()
        .position(|a| matches!(a, Action::ResetImagingDecoders))
        .unwrap_or_else(|| {
            panic!("{label}: LOS must emit Action::ResetImagingDecoders; got {actions:?}")
        });
    assert!(
        save_idx < reset_idx,
        "{label}: save must precede reset; got {actions:?}",
    );
}

#[test]
fn los_emits_reset_imaging_decoders() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    tick(&mut r, now, &pass, true, false);
    let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle, &pass, true, false);
    let los_plus = pass.end + ChronoDuration::seconds(1);
    let los_actions = tick(&mut r, los_plus, &pass, true, false);
    assert_save_before_reset(&los_actions, "single-pass LOS");
}

/// Per #544: two back-to-back passes must each emit their
/// own `ResetImagingDecoders` — the recorder's state machine
/// is reusable across passes (`Idle → BeforePass → Recording
/// → Finalizing → Idle` is the per-pass cycle). Without a
/// reset between them, an overnight unattended setup with
/// 4-6 LRPT passes would accrete the pipeline's
/// `ImageAssembler` state monotonically.
#[test]
fn two_back_to_back_passes_each_emit_reset() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();

    // Pass 1.
    let pass1 = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    tick(&mut r, now, &pass1, true, false);
    let after_settle_1 = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle_1, &pass1, true, false);
    let los_1 = pass1.end + ChronoDuration::seconds(1);
    let los_1_actions = tick(&mut r, los_1, &pass1, true, false);
    let reset_count_1 = los_1_actions
        .iter()
        .filter(|a| matches!(a, Action::ResetImagingDecoders))
        .count();
    assert_eq!(reset_count_1, 1, "pass 1 LOS must emit exactly one reset");
    assert_save_before_reset(&los_1_actions, "pass 1 LOS");

    // Settle from Finalizing back to Idle (the next tick after
    // LOS does Finalizing → Idle and emits RestoreTune).
    let post_los_1 = los_1 + ChronoDuration::seconds(1);
    tick(&mut r, post_los_1, &pass1, true, false);

    // Pass 2 — fresh AOS, schedule it after pass 1 completed.
    let pass2_aos = post_los_1 + ChronoDuration::seconds(60);
    let pass2 = synthetic_meteor_m2_3(pass2_aos, 0, 720, 50.0);
    tick(&mut r, pass2_aos, &pass2, true, false);
    let after_settle_2 = pass2_aos + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle_2, &pass2, true, false);
    let los_2 = pass2.end + ChronoDuration::seconds(1);
    let los_2_actions = tick(&mut r, los_2, &pass2, true, false);
    let reset_count_2 = los_2_actions
        .iter()
        .filter(|a| matches!(a, Action::ResetImagingDecoders))
        .count();
    assert_eq!(reset_count_2, 1, "pass 2 LOS must emit exactly one reset");
    assert_save_before_reset(&los_2_actions, "pass 2 LOS");
}
