use super::*;

/// #756 — the priority sweep must arm after exactly
/// `PRIORITY_CHECK_INTERVAL` normal hops, not after ~half that
/// (the hop counter used to be incremented twice per hop).
#[test]
fn priority_sweep_arms_after_exactly_the_check_interval() {
    let mut s = Scanner::new();
    let mut channels: Vec<ScannerChannel> = (0..SWEEP_NORMAL_CHANNELS)
        .map(|i| ch(&format!("N{i}"), normal_channel_hz(i), 0))
        .collect();
    channels.push(ch("P", SWEEP_PRIORITY_HZ, 1));
    s.handle_event(ScannerEvent::ChannelsChanged(channels));
    s.handle_event(ScannerEvent::SetEnabled(true)); // retunes to N0 (hop 1)

    let mut visited = vec![normal_channel_hz(0)];
    for _ in 0..SWEEP_HOPS {
        if let Some(f) = hop_on_dwell_timeout(&mut s) {
            visited.push(f);
        }
    }
    let first_priority = visited
        .iter()
        .position(|f| *f == SWEEP_PRIORITY_HZ)
        .expect("priority channel must be visited");
    assert_eq!(
        first_priority, PRIORITY_CHECK_INTERVAL as usize,
        "priority channel should be the hop right after {PRIORITY_CHECK_INTERVAL} normal hops, visited order: {visited:?}"
    );
    // The sweep is an interruption: normal rotation resumes at the
    // next unvisited normal channel, not past the priority channel.
    assert_eq!(
        visited[first_priority + 1],
        normal_channel_hz(SWEEP_EXPECTED_RESUME_IDX),
        "rotation must resume at N{SWEEP_EXPECTED_RESUME_IDX} after the sweep, visited order: {visited:?}"
    );
}

/// #756 (second symptom) — on a priority-only list the fallback path
/// must not keep arming sweeps (sweep↔fallback flip-flop).
#[test]
fn priority_only_list_does_not_oscillate_between_sweep_and_fallback() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("P1", SWEEP_PRIORITY_HZ, 1),
        ch("P2", SWEEP_PRIORITY_HZ + SWEEP_NORMAL_SPACING_HZ, 1),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    for _ in 0..PRIORITY_ONLY_CYCLES {
        assert!(
            hop_on_dwell_timeout(&mut s).is_some(),
            "priority-only hop must retune"
        );
        assert_eq!(
            s.hops_since_priority_sweep, 0,
            "fallback hops on a priority-only list must not count toward a sweep"
        );
    }
}

#[test]
fn lockouts_survive_disable_and_reenable() {
    let (mut s, key_a, _key_b) = enabled_scanner_ab();
    s.handle_event(ScannerEvent::LockoutChannel(key_a.clone()));
    s.handle_event(ScannerEvent::SetEnabled(false));
    assert!(
        s.locked_out.contains(&key_a),
        "lockout must survive disable"
    );
    let cmds = s.handle_event(ScannerEvent::SetEnabled(true));
    assert!(
        matches!(
            cmds[0],
            ScannerCommand::Retune {
                freq_hz: 162_550_000,
                ..
            }
        ),
        "re-enable must skip the locked-out channel, got {cmds:?}"
    );
}

/// The #757 UI flow: locking out the last available channel empties
/// the rotation, the UI flips the master switch off, the user flips
/// it back on — both lockouts must still be in force.
#[test]
fn all_lockouts_survive_the_empty_rotation_cycle() {
    let (mut s, key_a, key_b) = enabled_scanner_ab();
    s.handle_event(ScannerEvent::LockoutChannel(key_a.clone()));
    let empty = s.handle_event(ScannerEvent::LockoutChannel(key_b.clone()));
    assert!(
        empty
            .iter()
            .any(|c| matches!(c, ScannerCommand::EmptyRotation)),
        "locking out the last channel must empty the rotation, got {empty:?}"
    );
    s.handle_event(ScannerEvent::SetEnabled(false));
    let cmds = s.handle_event(ScannerEvent::SetEnabled(true));
    assert!(
        s.locked_out.contains(&key_a) && s.locked_out.contains(&key_b),
        "both lockouts must survive the EmptyRotation → off → on cycle"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, ScannerCommand::EmptyRotation))
            && !cmds
                .iter()
                .any(|c| matches!(c, ScannerCommand::Retune { .. })),
        "re-enable must not retune to a locked-out channel, got {cmds:?}"
    );
}

#[test]
fn priority_sweep_triggers_after_interval_hops() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
        ch("P", 121_500_000, 1), // priority
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));

    // Burn through 5+ normal hops. Each hop = Retuning→Dwelling→advance.
    // Need to settle (tick past 30ms), then timeout dwell (tick past 100ms).
    let mut retune_freqs: Vec<u64> = Vec::new();
    for _ in 0..6 {
        s.handle_event(tick(TICK_PAST_SETTLE)); // settle
        let cmds = s.handle_event(tick(TICK_PAST_DWELL)); // dwell timeout → next retune
        for c in &cmds {
            if let ScannerCommand::Retune { freq_hz, .. } = c {
                retune_freqs.push(*freq_hz);
            }
        }
    }
    // After 5 normal hops, the priority channel should have appeared.
    assert!(
        retune_freqs.contains(&121_500_000),
        "priority channel should have appeared after 5 normal hops, got {retune_freqs:?}"
    );
}

#[test]
fn lockout_skips_channel() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::LockoutChannel(ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    }));
    let commands = s.handle_event(ScannerEvent::SetEnabled(true));
    // First retune should skip A and go to B.
    let first_retune = commands.iter().find_map(|c| match c {
        ScannerCommand::Retune { freq_hz, .. } => Some(*freq_hz),
        _ => None,
    });
    assert_eq!(first_retune, Some(162_550_000));
}

#[test]
fn all_channels_locked_emits_empty_rotation() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::LockoutChannel(ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    }));
    let commands = s.handle_event(ScannerEvent::SetEnabled(true));
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::EmptyRotation))
    );
    assert_eq!(s.state(), ScannerState::Idle);
}

#[test]
fn lockout_cleared_when_channel_removed() {
    let mut s = Scanner::new();
    let key_a = ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    };
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::LockoutChannel(key_a.clone()));
    // Remove A.
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("B", 162_550_000, 0)]));
    // Internal set should have pruned.
    assert!(!s.locked_out.contains(&key_a));
}

#[test]
fn lockout_of_active_channel_advances_immediately() {
    // Real scenario: scanner stopped on a channel with a
    // persistent-open carrier; user hits "lockout current
    // channel" to escape. Without force-advance the scanner
    // would sit forever — no dwell timeout, no hang-elapse,
    // no squelch-close fires.
    let mut s = Scanner::new();
    let key_a = ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    };
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);

    // Lockout the channel the scanner is currently listening on.
    let commands = s.handle_event(ScannerEvent::LockoutChannel(key_a));
    assert_eq!(s.state(), ScannerState::Retuning);
    // Next channel in rotation is B.
    assert!(commands.iter().any(|c| matches!(
        c,
        ScannerCommand::Retune {
            freq_hz: 162_550_000,
            ..
        }
    )));
}

#[test]
fn unlockout_resumes_scanning_from_empty_rotation_idle() {
    // Scenario: scanner is enabled but all channels are locked
    // out, so it drained to Idle via EmptyRotation. Unlocking
    // a channel should kick rotation back into motion rather
    // than leaving the scanner stuck until some unrelated
    // event fires.
    let mut s = Scanner::new();
    let key_a = ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    };
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::LockoutChannel(key_a.clone()));
    s.handle_event(ScannerEvent::SetEnabled(true));
    assert_eq!(s.state(), ScannerState::Idle);

    let commands = s.handle_event(ScannerEvent::UnlockChannel(key_a));
    assert_eq!(s.state(), ScannerState::Retuning);
    assert!(commands.iter().any(|c| matches!(
        c,
        ScannerCommand::Retune {
            freq_hz: 146_520_000,
            ..
        }
    )));
}

/// #759 — locking out the only channel while listening must not
/// un-gate the sink: the radio is still on that channel.
#[test]
fn lockout_into_empty_rotation_keeps_audio_muted() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);

    let commands = s.handle_event(ScannerEvent::LockoutChannel(ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    }));
    assert_eq!(s.state(), ScannerState::Idle);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::EmptyRotation))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(true))),
        "{commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(false))),
        "{commands:?}"
    );
}
