//! Scanner state machine. Owns rotation position, phase, timing
//! counters, and session lockouts. No threading, no I/O — the
//! DSP controller drives it via `handle_event` and applies the
//! returned commands.

use std::collections::HashSet;

use crate::channel::{ChannelKey, ScannerChannel};
use crate::commands::ScannerCommand;
use crate::events::{ScannerEvent, SquelchState};
use crate::state::ScannerState;
use crate::{DEFAULT_DWELL_MS, DEFAULT_HANG_MS, PRIORITY_CHECK_INTERVAL, SETTLE_MS};

/// Internal phase carrying per-phase bookkeeping. The outer
/// `ScannerState` surfaced to the UI is a flattened view of this.
#[derive(Debug, Clone)]
enum Phase {
    Idle,
    Retuning {
        target_idx: usize,
        samples_until_settled: u64,
    },
    Dwelling {
        idx: usize,
        samples_until_timeout: u64,
    },
    Listening {
        idx: usize,
    },
    Hanging {
        idx: usize,
        samples_until_timeout: u64,
    },
}

impl Phase {
    fn as_state(&self) -> ScannerState {
        match self {
            Phase::Idle => ScannerState::Idle,
            Phase::Retuning { .. } => ScannerState::Retuning,
            Phase::Dwelling { .. } => ScannerState::Dwelling,
            Phase::Listening { .. } => ScannerState::Listening,
            Phase::Hanging { .. } => ScannerState::Hanging,
        }
    }
}

/// Scanner state machine. Instantiate once, feed events, apply
/// emitted commands. All methods are synchronous and cheap.
pub struct Scanner {
    enabled: bool,
    channels: Vec<ScannerChannel>,
    locked_out: HashSet<ChannelKey>,
    default_dwell_ms: u32,
    default_hang_ms: u32,
    phase: Phase,
    /// Rotation index into the current sub-list. Normal and
    /// priority rotations are advanced independently.
    normal_cursor: usize,
    priority_cursor: usize,
    /// Count of completed normal hops since the last priority
    /// sweep. When this hits `PRIORITY_CHECK_INTERVAL`, the next
    /// rotation pass runs priority before normal.
    hops_since_priority_sweep: u32,
    /// Set while a priority sweep is in progress.
    in_priority_sweep: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            enabled: false,
            channels: Vec::new(),
            locked_out: HashSet::new(),
            default_dwell_ms: DEFAULT_DWELL_MS,
            default_hang_ms: DEFAULT_HANG_MS,
            phase: Phase::Idle,
            normal_cursor: 0,
            priority_cursor: 0,
            hops_since_priority_sweep: 0,
            in_priority_sweep: false,
        }
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current public-facing phase. Cheap (no allocation).
    pub fn state(&self) -> ScannerState {
        self.phase.as_state()
    }

    /// Feed an event, receive zero or more commands. Commands
    /// are returned in order of emission — caller applies them
    /// in sequence.
    pub fn handle_event(&mut self, event: ScannerEvent) -> Vec<ScannerCommand> {
        match event {
            ScannerEvent::SetEnabled(enabled) => self.handle_set_enabled(enabled),
            ScannerEvent::ChannelsChanged(channels) => self.handle_channels_changed(channels),
            ScannerEvent::SquelchEdge(state) => self.handle_squelch_edge(state),
            ScannerEvent::SampleTick {
                samples_consumed,
                sample_rate_hz,
            } => self.handle_sample_tick(samples_consumed, sample_rate_hz),
            ScannerEvent::LockoutChannel(key) => self.handle_lockout(key),
            ScannerEvent::UnlockoutChannel(key) => self.handle_unlockout(key),
            ScannerEvent::SetDefaultDwellMs(ms) => {
                self.default_dwell_ms = ms;
                Vec::new()
            }
            ScannerEvent::SetDefaultHangMs(ms) => {
                self.default_hang_ms = ms;
                Vec::new()
            }
        }
    }

    // --- Event handlers (stub bodies; next tasks fill in) -----

    fn handle_set_enabled(&mut self, enabled: bool) -> Vec<ScannerCommand> {
        // TODO (Task 1.5): Idle↔Retuning transitions.
        let _ = enabled;
        Vec::new()
    }

    fn handle_channels_changed(&mut self, channels: Vec<ScannerChannel>) -> Vec<ScannerCommand> {
        // TODO (Task 1.5): list swap + rotation recovery.
        self.channels = channels;
        Vec::new()
    }

    fn handle_squelch_edge(&mut self, state: SquelchState) -> Vec<ScannerCommand> {
        // TODO (Task 1.6): honor post-settle; drive Dwelling→Listening
        // and Listening→Hanging transitions.
        let _ = state;
        Vec::new()
    }

    fn handle_sample_tick(
        &mut self,
        samples_consumed: u32,
        sample_rate_hz: u32,
    ) -> Vec<ScannerCommand> {
        // TODO (Task 1.6): countdown settle / dwell / hang windows,
        // advance on timeout.
        let _ = (samples_consumed, sample_rate_hz);
        Vec::new()
    }

    fn handle_lockout(&mut self, key: ChannelKey) -> Vec<ScannerCommand> {
        self.locked_out.insert(key);
        // TODO (Task 1.7): if active channel got locked out,
        // advance rotation.
        Vec::new()
    }

    fn handle_unlockout(&mut self, key: ChannelKey) -> Vec<ScannerCommand> {
        self.locked_out.remove(&key);
        Vec::new()
    }
}

/// Convert ms to samples at the given sample rate. Rounded up so
/// a 30 ms window at 48000 Hz = 1440 samples (exact), and a 30 ms
/// at 44100 = 1323. Caller uses this to seed `samples_until_*`.
#[allow(clippy::cast_possible_truncation)]
fn ms_to_samples(ms: u32, sample_rate_hz: u32) -> u64 {
    // (ms * rate + 999) / 1000 — ceiling division.
    (u64::from(ms) * u64::from(sample_rate_hz) + 999) / 1000
}
