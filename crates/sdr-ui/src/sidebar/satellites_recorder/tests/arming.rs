use super::*;

#[test]
fn idle_arms_when_pass_in_lead_window_and_eligible() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass starts in 3 s — inside the 5 s lead-in.
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    let actions = tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::BeforePass { .. }));
    assert!(matches!(actions[0], Action::StartAutoRecord { .. }));
    // Pre-AOS arming reports "starting" — the pass hasn't
    // crossed `pass.start` yet.
    match &actions[1] {
        Action::Toast { message, .. } => {
            assert!(
                message.contains("starting"),
                "expected starting copy, got: {message}",
            );
        }
        other => panic!("expected Toast, got {other:?}"),
    }
}

#[test]
fn idle_arms_for_already_started_pass_uses_in_progress_copy() {
    // Missed the lead window — the laptop woke from suspend
    // mid-pass, or the 1 Hz tick stalled long enough that the
    // displayed-pass entry crosses `pass.start` before
    // `tick_idle` saw it. Recorder still arms (pass.end is
    // future, eligibility holds) but the toast must read "in
    // progress" — saying "starting" when the pass clock is
    // already running would lie to the user.
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass started 30 s ago, ends in 9.5 min — eligible by
    // every other gate (NOAA, 50° peak, end > now).
    let pass = synthetic_meteor_m2_3(now, -30, 600, 50.0);
    let actions = tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::BeforePass { .. }));
    match &actions[1] {
        Action::Toast { message, .. } => {
            assert!(
                message.contains("in progress"),
                "expected in-progress copy, got: {message}",
            );
        }
        other => panic!("expected Toast, got {other:?}"),
    }
}

#[test]
fn idle_does_not_arm_when_toggle_off() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    let actions = tick(&mut r, now, &pass, false, false);
    assert!(matches!(r.state(), State::Idle));
    assert!(actions.is_empty());
}

#[test]
fn idle_does_not_arm_below_quality_threshold() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // 20° peak — "marginal" tier, below the 25° "good" floor.
    let pass = synthetic_meteor_m2_3(now, 3, 720, 20.0);
    let actions = tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::Idle));
    assert!(actions.is_empty());
}

#[test]
fn idle_does_not_arm_for_unknown_satellite() {
    // A satellite that isn't in the catalog at all produces no
    // tune target, so the recorder must skip it.  We use a
    // fictional name so this test doesn't accidentally break
    // when a real catalog entry gets its protocol filled in
    // (as happened when Meteor → Lrpt, then ISS → Sstv shipped
    // in epic #472). Pre-epic-#472 this test used "ISS (ZARYA)"
    // as the fixture because ISS had `imaging_protocol: None`;
    // now that ISS ships with `Some(Sstv)` the fixture is a
    // name that will never appear in the catalog.
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let mut pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    pass.satellite = "UNKNOWN-SAT-99".to_string();
    let actions = tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::Idle));
    assert!(actions.is_empty());
}

#[test]
fn idle_arms_for_iss_sstv_pass() {
    // ISS has `imaging_protocol: Some(Sstv)` since epic #472.
    // The recorder must arm for it when auto-record is on and
    // the pass is within AOS_LEAD_SECS.
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let mut pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    pass.satellite = "ISS (ZARYA)".to_string();
    let actions = tick(&mut r, now, &pass, true, false);
    // The recorder should have transitioned out of Idle.
    assert!(!matches!(r.state(), State::Idle));
    // Pin the protocol payload (and satellite) so SSTV-specific
    // dispatch regressions fail loudly here rather than silently
    // falling through to "wrong protocol but a StartAutoRecord
    // was present" — which the previous looser assertion would
    // have permitted. Per CR round 2 on PR #599.
    assert!(
        actions.iter().any(|a| matches!(
            a,
            Action::StartAutoRecord {
                protocol: sdr_sat::ImagingProtocol::Sstv,
                satellite,
                ..
            } if satellite == "ISS (ZARYA)"
        )),
        "expected StartAutoRecord(Sstv) for ISS (ZARYA) pass; got {actions:?}"
    );
}

#[test]
fn idle_skips_passes_already_past_los() {
    // Suspend / resume: the laptop sleeps mid-session and wakes
    // up after a pass has already ended. The displayed-pass
    // list may still carry the finished entry until the next
    // recompute fires. The recorder must NOT arm for a
    // finished pass — otherwise we'd briefly retune + open
    // the viewer and save an empty PNG on the next tick.
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass ended 30 seconds ago.
    let mut pass = synthetic_meteor_m2_3(now, -660, 600, 50.0); // started -660 s ago, ended -60 s ago
    // Sanity: this pass is in the past from `now`'s perspective.
    assert!(pass.end < now);
    let actions = tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::Idle));
    assert!(actions.is_empty());
    // Mutating the pass to make peak elevation match the
    // threshold + extending end past now would arm — pinning
    // that the only thing keeping us idle was the LOS check.
    pass.end = now + ChronoDuration::seconds(720);
    pass.start = now + ChronoDuration::seconds(3);
    let actions = tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::BeforePass { .. }));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::StartAutoRecord { .. }))
    );
}

#[test]
fn idle_skips_passes_outside_lead_window() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    // Pass starts in 10 min. Way outside the 5 s lead-in.
    let pass = synthetic_meteor_m2_3(now, 600, 720, 50.0);
    let actions = tick(&mut r, now, &pass, true, false);
    assert!(matches!(r.state(), State::Idle));
    assert!(actions.is_empty());
}

#[test]
fn overlapping_pass_does_not_re_arm_while_recording() {
    let mut r = AutoRecorder::new();
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass_a = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    // Arm + settle into Recording.
    tick(&mut r, now, &pass_a, true, false);
    let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    tick(&mut r, after_settle, &pass_a, true, false);
    assert!(matches!(r.state(), State::Recording { .. }));
    // A second pass appears in the list (mid-recording).
    // The recorder should ignore it — Recording stays put.
    // Use METEOR-M2 3 (also catalog-resident) as the distinct
    // second satellite since NOAA-15/18/19 were decommissioned
    // in 2025 and are no longer in `KNOWN_SATELLITES`.
    let mut pass_b = synthetic_meteor_m2_3(now, 30, 720, 60.0);
    pass_b.satellite = "METEOR-M2 3".to_string();
    let actions = r.tick(
        after_settle,
        &[pass_a, pass_b],
        true,
        false,
        DEFAULT_MIN_ELEV_DEG,
        default_tune(),
    );
    assert!(matches!(r.state(), State::Recording { .. }));
    // No StartAutoRecord action emitted.
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::StartAutoRecord { .. }))
    );
}

#[test]
fn unsupported_protocol_does_not_arm_recorder() {
    // CR round 2 on PR #541: a catalog entry with a protocol
    // outside `supported_protocols` must NOT arm the recorder
    // — `tick_idle` keeps the state at `Idle` rather than
    // transitioning to `BeforePass`. This is the primary
    // defense against "Meteor catalog flipped to Some(Lrpt)
    // ahead of Task 7 wiring": without this gate, the
    // wiring layer's fail-closed AOS branch would no-op,
    // but the LOS-side `SavePng` + `RestoreTune` would
    // still fire and clobber the user's mid-pass state.
    //
    // Simulate the bad-future scenario by using a recorder
    // configured for `Apt` only, then feeding it a synthetic
    // pass for a satellite the catalog flags as a different
    // protocol (METEOR-M 2 with — once Task 7 ships —
    // `Some(Lrpt)`). Even after we monkeypatch the catalog
    // entry to advertise Lrpt, the recorder must refuse to
    // arm.
    //
    // The recorder consults the catalog directly via
    // `tune_target_for_pass`, so monkeypatching the catalog
    // isn't possible from a test. Instead use the inverse
    // construction: build a recorder that supports NOTHING
    // (`with_supported_protocols(&[])`), then feed it an
    // APT-flagged NOAA pass. Same code path — the
    // protocol-gate `continue` fires either way.
    let mut r = AutoRecorder::with_supported_protocols(&[]);
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    let actions = tick(&mut r, now, &pass, true, false);
    // No actions of any kind — no StartAutoRecord, no Toast,
    // no transition.
    assert!(
        actions.is_empty(),
        "unsupported protocol must not arm the recorder; got {actions:?}"
    );
    assert!(
        matches!(r.state(), State::Idle),
        "state must stay Idle, not transition to BeforePass"
    );
}

#[test]
fn unsupported_protocol_blocks_full_pass_lifecycle() {
    // Drives the recorder through the entire would-be pass
    // lifecycle (AOS → settle → LOS) on an unsupported
    // protocol and asserts NO actions ever fire — most
    // importantly, no LOS-side `SavePng` or `RestoreTune`
    // (which would clobber the user's mid-pass state).
    let mut r = AutoRecorder::with_supported_protocols(&[]);
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);

    // AOS: gated out, no actions, state stays Idle.
    let aos = tick(&mut r, now, &pass, true, false);
    assert!(aos.is_empty());
    assert!(matches!(r.state(), State::Idle));

    // Settle window — still no transition.
    let after_settle = now + ChronoDuration::seconds(SETTLE_SECS + 1);
    let mid = tick(&mut r, after_settle, &pass, true, false);
    assert!(mid.is_empty());
    assert!(matches!(r.state(), State::Idle));

    // LOS — most important: NO SavePng / RestoreTune fire.
    // Without the supported-protocols gate, `BeforePass →
    // Recording → Finalizing` would have transitioned by
    // now and we'd see those cleanup actions here.
    let los_plus = pass.end + ChronoDuration::seconds(1);
    let los = tick(&mut r, los_plus, &pass, true, false);
    assert!(
        !los.iter().any(|a| matches!(a, Action::SavePng(_))),
        "no SavePng on unsupported-protocol LOS",
    );
    assert!(
        !los.iter().any(|a| matches!(a, Action::RestoreTune(_))),
        "no RestoreTune on unsupported-protocol LOS — the user's mid-pass state must not be clobbered",
    );
    assert!(matches!(r.state(), State::Idle));
}

#[test]
#[ignore = "exercises APT-specific recorder dispatch (SavePng / audio); APT path is dormant pending a future Cubesat catalog entry — see KNOWN_SATELLITES doc comment about August 2025 NOAA POES decommissioning"]
fn supported_protocol_arms_recorder_normally() {
    // Sanity: with `supported_protocols = [Apt]` (the
    // default), an APT-flagged catalog entry arms the
    // recorder as expected. Pins the negative-test contract
    // above by showing the gate is the only thing stopping
    // the unsupported case. Per CR round 2 on PR #541:
    // assert the dispatched payload's `protocol` field
    // explicitly so a future regression that ships
    // `StartAutoRecord` with the wrong protocol (e.g. an
    // off-by-one indexing into the catalog) fails here.
    let mut r = AutoRecorder::with_supported_protocols(&[sdr_sat::ImagingProtocol::Apt]);
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2_3(now, 3, 720, 50.0);
    let actions = tick(&mut r, now, &pass, true, false);
    let dispatched = actions.iter().find_map(|a| match a {
        Action::StartAutoRecord {
            satellite,
            protocol,
            ..
        } => Some((satellite.clone(), *protocol)),
        _ => None,
    });
    let (satellite, protocol) = dispatched.expect("supported protocol must emit StartAutoRecord");
    // METEOR-M 2 (LRPT) — `synthetic_meteor_m2_3` is now misnamed but
    // produces a Meteor pass since NOAA-19 was decommissioned in
    // August 2025.
    assert_eq!(satellite, "METEOR-M2 3");
    assert_eq!(
        protocol,
        sdr_sat::ImagingProtocol::Lrpt,
        "dispatched protocol must match the catalog entry's flag",
    );
    assert!(matches!(r.state(), State::BeforePass { .. }));
}

#[test]
fn meteor_synthetic_pass_helper_works() {
    // Sanity for the helper itself — pins the satellite
    // name our gate tests rely on.
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
    let pass = synthetic_meteor_m2(now, 0, 600, 50.0);
    assert_eq!(pass.satellite, "METEOR-M2 3");
}
