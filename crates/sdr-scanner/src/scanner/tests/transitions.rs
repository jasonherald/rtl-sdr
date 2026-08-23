use super::*;

#[test]
fn enable_with_channels_transitions_to_retuning() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    let commands = s.handle_event(ScannerEvent::SetEnabled(true));
    assert_eq!(s.state(), ScannerState::Retuning);
    // Expect Retune → MuteAudio(true) → ActiveChannelChanged → StateChanged
    assert!(matches!(
        commands[0],
        ScannerCommand::Retune {
            freq_hz: 146_520_000,
            ..
        }
    ));
    assert!(matches!(commands[1], ScannerCommand::MuteAudio(true)));
    assert!(matches!(
        commands[2],
        ScannerCommand::ActiveChannelChanged(Some(_))
    ));
    assert!(matches!(
        commands[3],
        ScannerCommand::StateChanged(ScannerState::Retuning)
    ));
}

#[test]
fn disable_emits_idle_transition() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    let commands = s.handle_event(ScannerEvent::SetEnabled(false));
    assert_eq!(s.state(), ScannerState::Idle);
    assert!(matches!(commands[0], ScannerCommand::MuteAudio(false)));
    assert!(matches!(
        commands[1],
        ScannerCommand::ActiveChannelChanged(None)
    ));
    assert!(matches!(
        commands[2],
        ScannerCommand::StateChanged(ScannerState::Idle)
    ));
}

#[test]
fn enable_with_no_channels_emits_empty_rotation() {
    let mut s = Scanner::new();
    let commands = s.handle_event(ScannerEvent::SetEnabled(true));
    assert_eq!(s.state(), ScannerState::Idle);
    assert!(matches!(commands[0], ScannerCommand::EmptyRotation));
}

#[test]
fn settle_window_ignores_squelch_open() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    // Feed a squelch open during the settle window.
    s.handle_event(tick(TICK_IN_SETTLE));
    let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Retuning);
    // No MuteAudio(false) should have been emitted.
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(false))),
        "mute was released during settle window"
    );
}

#[test]
fn post_settle_squelch_open_transitions_to_listening() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    // Elapse the settle window (1440 samples for 30ms at 48kHz).
    s.handle_event(tick(TICK_PAST_SETTLE));
    assert_eq!(s.state(), ScannerState::Dwelling);
    let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(false)))
    );
}

#[test]
fn squelch_close_in_listening_enters_hanging_and_mutes() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);
    let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
    assert_eq!(s.state(), ScannerState::Hanging);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(true)))
    );
}

#[test]
fn squelch_reopen_before_hang_end_returns_to_listening() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
    assert_eq!(s.state(), ScannerState::Hanging);
    // Advance partway into hang (2000ms hang = 96000 samples).
    s.handle_event(tick(TICK_INSIDE_HANG));
    let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(false)))
    );
}

#[test]
fn hang_elapsed_advances_to_next_channel() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
    let commands = s.handle_event(tick(TICK_PAST_HANG));
    assert_eq!(s.state(), ScannerState::Retuning);
    assert!(commands.iter().any(|c| matches!(
        c,
        ScannerCommand::Retune {
            freq_hz: 162_550_000,
            ..
        }
    )));
}

#[test]
fn persistent_open_during_settle_goes_directly_to_listening() {
    // Real-world scenario: scanner hops to a channel that
    // already has a carrier active. The squelch detector
    // fires Open during the retune's settle window, which
    // phase transitions ignore — but the latch still
    // records it. Settle expiry must consult the latch and
    // go straight to Listening, not sit in Dwelling waiting
    // for an edge that already fired.
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    // During settle: feed a squelch-open edge. Phase stays
    // Retuning; latch moves to open.
    s.handle_event(tick(TICK_IN_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Retuning);
    // Settle expires. Scanner should land in Listening
    // directly, with audio unmuted.
    let commands = s.handle_event(tick(TICK_SETTLE_COMPLETE));
    assert_eq!(s.state(), ScannerState::Listening);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(false)))
    );
}

#[test]
fn disable_clears_rotation_state_but_preserves_lockouts() {
    // Disable resets only the rotation cursor, hop counter and
    // priority-sweep state so re-enable starts the cycle fresh;
    // lockouts persist for the whole app session (#757).
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
    s.handle_event(ScannerEvent::LockoutChannel(key_a.clone()));
    // Advance through a few hops so cursors + priority counter are non-zero.
    s.handle_event(tick(TICK_PAST_SETTLE));
    s.handle_event(tick(TICK_PAST_DWELL));
    assert!(s.locked_out.contains(&key_a));
    assert!(s.hops_since_priority_sweep > 0);

    s.handle_event(ScannerEvent::SetEnabled(false));
    // Rotation state clears after disable; lockouts persist (#757).
    assert!(
        s.locked_out.contains(&key_a),
        "lockouts must persist across disable"
    );
    assert_eq!(s.next_channel_idx, 0);
    assert_eq!(s.hops_since_priority_sweep, 0);
    assert!(
        s.priority_sweep_visited.is_none(),
        "priority sweep state not cleared on disable"
    );
}

/// #759 — a Listening entered from the settle-window latch is
/// provisional: if the carrier "closes" right after settle it was
/// the previous channel's IQ still draining from the ring, so the
/// scanner resumes the dwell instead of hanging for 2 s of dead air.
#[test]
fn latched_open_that_closes_after_settle_resumes_dwell() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_IN_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    s.handle_event(tick(TICK_SETTLE_COMPLETE));
    assert_eq!(s.state(), ScannerState::Listening);

    let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
    assert_eq!(s.state(), ScannerState::Dwelling, "{commands:?}");
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(true))),
        "{commands:?}"
    );
    // A genuine carrier that opens during that dwell is a normal,
    // non-provisional Listening: closing it hangs as usual.
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    assert_eq!(s.state(), ScannerState::Listening);
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
    assert_eq!(s.state(), ScannerState::Hanging);
}

/// CR round 1 on PR #798 — a latched carrier that is still open
/// after `PROVISIONAL_CONFIRM_MS` of post-settle audio is genuine:
/// when it finally closes the scanner hangs as usual instead of
/// dropping into a muted dwell mid-conversation.
#[test]
fn latched_open_confirmed_by_post_settle_audio_hangs_on_close() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_IN_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    s.handle_event(tick(TICK_SETTLE_COMPLETE));
    assert_eq!(s.state(), ScannerState::Listening);

    // PROVISIONAL_CONFIRM_MS of audio at 48 kHz, plus a little.
    let confirm_samples = RATE * PROVISIONAL_CONFIRM_MS / 1000 + 1;
    s.handle_event(tick(confirm_samples));
    assert_eq!(s.state(), ScannerState::Listening);

    let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
    assert_eq!(s.state(), ScannerState::Hanging, "{commands:?}");
}
