use super::*;

#[test]
fn channels_changed_mid_scan_recovers() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);
    // User deletes channel A — the one being listened to — and
    // adds C. (Edits that leave the active channel's tune config
    // alone no longer restart the rotation, #758.)
    let commands = s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("B", 162_550_000, 0),
        ch("C", 28_400_000, 0),
    ]));
    // Scanner recovers by restarting rotation at cursor 0.
    assert_eq!(s.state(), ScannerState::Retuning);
    // First retune after list change goes to B.
    assert!(commands.iter().any(|c| matches!(
        c,
        ScannerCommand::Retune {
            freq_hz: 162_550_000,
            ..
        }
    )));
}

/// #758 — a list update that leaves the active channel's tune
/// config untouched (here: a default-hang nudge applied to every
/// channel plus a new channel) must not retune mid-transmission.
#[test]
fn channels_changed_keeps_listening_when_active_channel_is_unchanged() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);

    let mut a = ch("A", 146_520_000, 0);
    a.hang_ms = DEFAULT_HANG_MS + 100;
    let mut b = ch("B", 162_550_000, 0);
    b.hang_ms = DEFAULT_HANG_MS + 100;
    let commands = s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("C", 28_400_000, 0),
        a,
        b,
    ]));
    assert_eq!(s.state(), ScannerState::Listening, "{commands:?}");
    assert!(!has_retune(&commands), "{commands:?}");
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(true))),
        "{commands:?}"
    );

    // The active index was re-resolved: locking out A (now at
    // index 1) still force-advances away from it.
    let commands = s.handle_event(ScannerEvent::LockoutChannel(ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    }));
    assert_eq!(s.state(), ScannerState::Retuning);
    assert!(has_retune(&commands));
}

/// #758 — a change to the active channel's own tune config
/// (bandwidth here) still retunes.
#[test]
fn channels_changed_retunes_when_active_channel_config_changes() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);

    let mut a = ch("A", 146_520_000, 0);
    a.bandwidth = 25_000.0;
    let commands = s.handle_event(ScannerEvent::ChannelsChanged(vec![a]));
    assert_eq!(s.state(), ScannerState::Retuning);
    assert!(has_retune(&commands));
}

/// #758 — a list update that moves the Retuning target to another
/// position rebinds the index: the settle expiry still reads that
/// channel's dwell and the next hop continues from it.
#[test]
fn channels_changed_rebinds_the_retuning_target_index() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    assert_eq!(s.state(), ScannerState::Retuning);

    // A moves to index 2 with a long dwell override; no retune.
    let mut a = ch("A", 146_520_000, 0);
    a.dwell_ms = 1_000;
    let commands = s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("C", 28_400_000, 0),
        ch("B", 162_550_000, 0),
        a,
    ]));
    assert_eq!(s.state(), ScannerState::Retuning);
    assert!(!has_retune(&commands), "{commands:?}");

    // Settle expires into A's dwell: the rebound index reads A's
    // 1 s override, so the default 100 ms does not time out.
    s.handle_event(tick(TICK_SETTLE_COMPLETE));
    assert_eq!(s.state(), ScannerState::Dwelling);
    let commands = s.handle_event(tick(TICK_PAST_DWELL));
    assert_eq!(s.state(), ScannerState::Dwelling, "{commands:?}");
    // And the rotation continues after A — wrapping to C.
    let commands = s.handle_event(tick(RATE));
    assert!(
        commands.iter().any(|c| matches!(
            c,
            ScannerCommand::Retune {
                freq_hz: 28_400_000,
                ..
            }
        )),
        "{commands:?}"
    );
}

/// CR round 1 on PR #798 — a list push while the scanner sits on a
/// priority-sweep pick must not anchor the cursor past that
/// channel: normal rotation resumes where the sweep interrupted
/// it (#756), exactly as if no push had happened.
#[test]
fn channels_changed_during_a_priority_sweep_keeps_the_rotation_cursor() {
    let mut s = Scanner::new();
    let mut channels: Vec<ScannerChannel> = (0..SWEEP_NORMAL_CHANNELS)
        .map(|i| ch(&format!("N{i}"), normal_channel_hz(i), 0))
        .collect();
    channels.push(ch("P", SWEEP_PRIORITY_HZ, 1));
    s.handle_event(ScannerEvent::ChannelsChanged(channels.clone()));
    s.handle_event(ScannerEvent::SetEnabled(true)); // N0

    // Hop until the sweep picks P.
    let mut hops = 0;
    while hop_on_dwell_timeout(&mut s) != Some(SWEEP_PRIORITY_HZ) {
        hops += 1;
        assert!(hops < SWEEP_HOPS, "sweep never picked the priority channel");
    }
    assert!(
        s.priority_sweep_visited.is_some(),
        "sweep must be in progress"
    );

    // The UI re-pushes the same list (a default-hang nudge).
    let commands = s.handle_event(ScannerEvent::ChannelsChanged(channels));
    assert!(!has_retune(&commands), "{commands:?}");

    assert_eq!(
        hop_on_dwell_timeout(&mut s),
        Some(normal_channel_hz(SWEEP_EXPECTED_RESUME_IDX)),
        "rotation must resume at N{SWEEP_EXPECTED_RESUME_IDX} after the sweep"
    );
}
