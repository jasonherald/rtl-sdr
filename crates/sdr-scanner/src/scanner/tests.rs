use super::*;
use crate::{DEFAULT_DWELL_MS, DEFAULT_HANG_MS};
use sdr_types::DemodMode;

fn ch(name: &str, freq: u64, priority: u8) -> ScannerChannel {
    ScannerChannel {
        key: ChannelKey {
            name: name.to_string(),
            frequency_hz: freq,
        },
        demod_mode: DemodMode::Nfm,
        bandwidth: 12_500.0,
        ctcss: None,
        voice_squelch: None,
        priority,
        dwell_ms: DEFAULT_DWELL_MS,
        hang_ms: DEFAULT_HANG_MS,
    }
}

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

/// Test sample rate. At 48 kHz, `SETTLE_MS = 30` resolves to
/// 1440 samples, `DEFAULT_DWELL_MS = 100` to 4800 samples,
/// and `DEFAULT_HANG_MS = 2000` to 96000 samples — the
/// constants below are sized to land inside / past those
/// windows with a small margin.
const RATE: u32 = 48_000;

/// Sample count well short of the 1440-sample settle window.
/// Used when a test needs the scanner to be mid-settle
/// (ignoring edges, not yet transitioning to Dwelling).
const TICK_IN_SETTLE: u32 = 500;

/// Sample count that clears the 1440-sample settle window
/// with margin. Most tests use this to get past settle into
/// `Dwelling` (or directly `Listening` if squelch latched
/// open during settle).
const TICK_PAST_SETTLE: u32 = 1500;

/// Slightly larger settle-clearing tick used in the
/// persistent-open-carrier test, where two ticks are fed in
/// sequence and the second one must finish draining the
/// settle counter that was partially consumed by the first.
const TICK_SETTLE_COMPLETE: u32 = 2000;

/// Sample count that clears the 4800-sample default dwell
/// window (`DEFAULT_DWELL_MS = 100` at 48 kHz). Causes a
/// Dwelling → advance transition when squelch never opened.
const TICK_PAST_DWELL: u32 = 5000;

/// Sample count well inside the 96000-sample default hang
/// window. Used to advance part of the hang before a
/// squelch-reopen event.
const TICK_INSIDE_HANG: u32 = 10_000;

/// Sample count that clears a 500 ms channel-level dwell
/// override (= 24000 samples at 48 kHz) with margin. Used
/// by the `dwell_ms_override` test.
const TICK_PAST_OVERRIDE_DWELL: u32 = 25_000;

/// Sample count that clears the 96000-sample default hang
/// window with margin.
const TICK_PAST_HANG: u32 = 100_000;

fn tick(samples: u32) -> ScannerEvent {
    ScannerEvent::SampleTick {
        samples_consumed: samples,
        sample_rate_hz: NonZeroU32::new(RATE).expect("RATE > 0"),
    }
}

/// Run one dwell-timeout hop (settle, then dwell expiry) and return the
/// frequency the scanner retuned to next, if any.
fn hop_on_dwell_timeout(s: &mut Scanner) -> Option<u64> {
    s.handle_event(tick(TICK_PAST_SETTLE));
    let cmds = s.handle_event(tick(TICK_PAST_DWELL));
    cmds.iter().find_map(|c| match c {
        ScannerCommand::Retune { freq_hz, .. } => Some(*freq_hz),
        _ => None,
    })
}

// ---- priority-sweep fixture topology (#756) ----
/// Normal channels N0..N7 at 25 kHz spacing from this base, plus one
/// priority channel. More normal channels than the check interval so
/// the cursor-starvation symptom (N3..N7 never visited) is observable.
const SWEEP_NORMAL_CHANNELS: u64 = 8;
const SWEEP_NORMAL_BASE_HZ: u64 = 146_000_000;
const SWEEP_NORMAL_SPACING_HZ: u64 = 25_000;
const SWEEP_PRIORITY_HZ: u64 = 155_000_000;
/// Hops driven after enable: enough to pass the first sweep and
/// observe where rotation resumes.
const SWEEP_HOPS: usize = 12;
/// After `PRIORITY_CHECK_INTERVAL` normal hops (N0..N4) and the sweep,
/// rotation must resume at N5.
const SWEEP_EXPECTED_RESUME_IDX: u64 = PRIORITY_CHECK_INTERVAL as u64;
/// Hops driven on the priority-only list — several sweep intervals.
const PRIORITY_ONLY_CYCLES: usize = 10;

fn normal_channel_hz(i: u64) -> u64 {
    SWEEP_NORMAL_BASE_HZ + i * SWEEP_NORMAL_SPACING_HZ
}

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

/// #757 — lockouts are scoped to the app session, not to one
/// enable/disable cycle: the UI turns the master switch off on
/// `EmptyRotation`, and a user who just locked out the last noisy
/// channel must not get them all back on the next enable.
#[test]
fn lockouts_survive_disable_and_reenable() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    let key_a = ChannelKey {
        name: "A".to_string(),
        frequency_hz: 146_520_000,
    };
    let key_b = ChannelKey {
        name: "B".to_string(),
        frequency_hz: 162_550_000,
    };
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

    // The #757 UI flow: locking out the last available channel empties
    // the rotation, the UI flips the master switch off, the user flips
    // it back on — both lockouts must still be in force.
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
fn dwell_elapsed_without_squelch_advances_to_next() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    // Skip settle window.
    s.handle_event(tick(TICK_PAST_SETTLE));
    assert_eq!(s.state(), ScannerState::Dwelling);
    // Dwell is 100ms = 4800 samples at 48kHz. Tick past it.
    let commands = s.handle_event(tick(TICK_PAST_DWELL));
    assert_eq!(s.state(), ScannerState::Retuning);
    // Should have retuned to channel B (frequency 162_550_000).
    assert!(commands.iter().any(|c| matches!(
        c,
        ScannerCommand::Retune {
            freq_hz: 162_550_000,
            ..
        }
    )));
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
fn channel_override_respected_for_dwell() {
    let mut s = Scanner::new();
    let mut longer = ch("L", 146_520_000, 0);
    longer.dwell_ms = 500;
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        longer,
        ch("N", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    // Settle.
    s.handle_event(tick(TICK_PAST_SETTLE));
    // Default dwell would be 100ms = 4800 samples. Channel
    // overrides to 500ms = 24000 samples. Tick 5000 — should
    // still be Dwelling (not advanced) because override kicks in.
    s.handle_event(tick(TICK_PAST_DWELL));
    assert_eq!(s.state(), ScannerState::Dwelling);
    // Tick past 500ms → advance.
    s.handle_event(tick(TICK_PAST_OVERRIDE_DWELL));
    assert_eq!(s.state(), ScannerState::Retuning);
}

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

// --- #758 / #759 (Aug 2026 deep review) ---

fn has_retune(cmds: &[ScannerCommand]) -> bool {
    cmds.iter()
        .any(|c| matches!(c, ScannerCommand::Retune { .. }))
}

fn tick_at(samples: u32, rate: u32) -> ScannerEvent {
    ScannerEvent::SampleTick {
        samples_consumed: samples,
        sample_rate_hz: NonZeroU32::new(rate).expect("rate > 0"),
    }
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

/// #759 — countdowns are wall-clock, not a sample count frozen at
/// the rate of the phase-entry tick: 50 ms of dwell at 250 ksps
/// plus 40 ms at 2.4 Msps is 90 ms, short of the 100 ms dwell.
#[test]
fn dwell_countdown_survives_a_sample_rate_change() {
    const LOW_RATE: u32 = 250_000;
    const HIGH_RATE: u32 = 2_400_000;
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    // Settle (30 ms) exactly, then dwell starts from a clean seed.
    s.handle_event(tick_at(LOW_RATE * SETTLE_MS / 1000, LOW_RATE));
    assert_eq!(s.state(), ScannerState::Dwelling);
    s.handle_event(tick_at(LOW_RATE / 20, LOW_RATE)); // 50 ms
    assert_eq!(s.state(), ScannerState::Dwelling);
    let commands = s.handle_event(tick_at(HIGH_RATE / 25, HIGH_RATE)); // 40 ms
    assert_eq!(s.state(), ScannerState::Dwelling, "{commands:?}");
    let commands = s.handle_event(tick_at(HIGH_RATE / 50, HIGH_RATE)); // 20 ms → 110 ms
    assert!(has_retune(&commands), "{commands:?}");
}

/// #759 — the part of the settle-expiry block that overshoots the
/// settle window counts toward the dwell: 2000 samples at 48 kHz
/// is 30 ms settle + 11.7 ms of dwell, so 4300 more samples
/// (89.6 ms) complete the 100 ms dwell.
#[test]
fn dwell_is_charged_for_the_settle_overshoot() {
    // 89.6 ms at 48 kHz — completes the 100 ms dwell after the
    // 11.7 ms of settle overshoot charged by TICK_SETTLE_COMPLETE.
    const TICK_REMAINDER_OF_DWELL: u32 = 4300;
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_SETTLE_COMPLETE));
    assert_eq!(s.state(), ScannerState::Dwelling);
    let commands = s.handle_event(tick(TICK_REMAINDER_OF_DWELL));
    assert!(has_retune(&commands), "{commands:?}");
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

/// #759 — a settle-expiry block that overshoots the whole dwell
/// (a 64 K block at a low rate) advances immediately instead of
/// waiting a further full dwell.
#[test]
fn settle_block_overshooting_the_whole_dwell_advances_immediately() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    // 200 ms at 48 kHz: 30 ms settle + 100 ms dwell + 70 ms spare.
    let commands = s.handle_event(tick(RATE / 5));
    assert_eq!(s.state(), ScannerState::Retuning, "{commands:?}");
    assert!(commands.iter().any(|c| matches!(
        c,
        ScannerCommand::Retune {
            freq_hz: 162_550_000,
            ..
        }
    )));
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

/// CR round 2 on PR #798 — tick durations must not be rounded up
/// per event: at 2.4 Msps a one-sample tick is 0.417 µs, and
/// ceiling each one to 1 µs expired a 100 ms dwell after ~42 ms of
/// audio. The same duration delivered as one tick or as
/// one-sample ticks must expire the dwell at the same point.
#[test]
fn countdown_is_exact_across_many_tiny_ticks() {
    const HIGH_RATE: u32 = 2_400_000;
    let dwell_samples = HIGH_RATE / 1000 * DEFAULT_DWELL_MS;
    let settle_samples = HIGH_RATE / 1000 * SETTLE_MS;

    let mut one_shot = Scanner::new();
    let mut tiny = Scanner::new();
    for s in [&mut one_shot, &mut tiny] {
        s.handle_event(ScannerEvent::ChannelsChanged(vec![
            ch("A", 146_520_000, 0),
            ch("B", 162_550_000, 0),
        ]));
        s.handle_event(ScannerEvent::SetEnabled(true));
        s.handle_event(tick_at(settle_samples, HIGH_RATE));
        assert_eq!(s.state(), ScannerState::Dwelling);
    }

    // All but one sample of the dwell: neither may advance.
    let commands = one_shot.handle_event(tick_at(dwell_samples - 1, HIGH_RATE));
    assert!(!has_retune(&commands), "one-shot: {commands:?}");
    for _ in 0..dwell_samples - 1 {
        let commands = tiny.handle_event(tick_at(1, HIGH_RATE));
        assert!(
            !has_retune(&commands),
            "tiny ticks advanced early: {commands:?}"
        );
    }
    // The last sample completes the dwell for both.
    assert!(has_retune(&one_shot.handle_event(tick_at(1, HIGH_RATE))));
    assert!(has_retune(&tiny.handle_event(tick_at(1, HIGH_RATE))));
}

/// CR round 3 on PR #798 — the carry is in sample·µs units of the
/// rate that produced it and must be discarded on a rate change:
/// 3 samples at 48 kHz leave a 0.5 µs carry, and one sample at
/// 30 kHz is 33.33 µs — 33 without the stale carry, 34 with it.
#[test]
fn tick_carry_is_discarded_on_a_sample_rate_change() {
    let mut s = Scanner::new();
    let rate_48k = NonZeroU32::new(48_000).expect("rate > 0");
    let rate_30k = NonZeroU32::new(30_000).expect("rate > 0");
    assert_eq!(s.tick_elapsed_us(3, rate_48k), 62);
    assert_eq!(s.tick_carry, 24_000, "0.5 µs carry in sample·µs units");
    assert_eq!(s.tick_elapsed_us(1, rate_30k), 33);
    // Same rate: the carry is honoured (33.33 + 0.33 carry → 33, then 34).
    assert_eq!(s.tick_elapsed_us(1, rate_30k), 33);
    assert_eq!(s.tick_elapsed_us(1, rate_30k), 34);
}

/// CR round 4 on PR #798 — the settle-expiry block's overshoot is
/// post-settle audio and counts toward confirming a latched
/// carrier: one 200 ms tick (30 ms settle + 170 ms, beyond the
/// 100 ms confirmation) makes the Listening genuine, so a close
/// hangs instead of resuming the dwell.
#[test]
fn settle_overshoot_counts_toward_provisional_confirmation() {
    let mut s = Scanner::new();
    s.handle_event(ScannerEvent::ChannelsChanged(vec![
        ch("A", 146_520_000, 0),
        ch("B", 162_550_000, 0),
    ]));
    s.handle_event(ScannerEvent::SetEnabled(true));
    s.handle_event(tick(TICK_IN_SETTLE));
    s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
    s.handle_event(tick(RATE / 5)); // 200 ms
    assert_eq!(s.state(), ScannerState::Listening);

    let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
    assert_eq!(s.state(), ScannerState::Hanging, "{commands:?}");
}
