use super::*;

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
