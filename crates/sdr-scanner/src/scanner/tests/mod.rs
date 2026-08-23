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

/// #757 — lockouts are scoped to the app session, not to one
/// enable/disable cycle: the UI turns the master switch off on
/// `EmptyRotation`, and a user who just locked out the last noisy
/// channel must not get them all back on the next enable.
/// Enabled scanner on channels A (146.52) and B (162.55), plus their
/// lockout keys.
fn enabled_scanner_ab() -> (Scanner, ChannelKey, ChannelKey) {
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
    (s, key_a, key_b)
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

mod channels_changed;
mod dwell_timing;
mod sweep_lockout;
mod transitions;
