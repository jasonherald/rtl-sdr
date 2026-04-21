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
        if self.enabled == enabled {
            return Vec::new();
        }
        self.enabled = enabled;
        if enabled {
            self.start_rotation()
        } else {
            self.stop_rotation()
        }
    }

    fn handle_channels_changed(&mut self, channels: Vec<ScannerChannel>) -> Vec<ScannerCommand> {
        self.channels = channels;
        // Any stale lockout keys for channels that no longer exist
        // are harmless (the set is only consulted against the
        // live channel list), but we'll prune for cleanliness.
        let valid: HashSet<ChannelKey> =
            self.channels.iter().map(|c| c.key.clone()).collect();
        self.locked_out.retain(|k| valid.contains(k));

        if !self.enabled {
            return Vec::new();
        }
        // Currently-scanning mid-list-change: recover from wherever
        // the phase left us by re-starting rotation.
        self.normal_cursor = 0;
        self.priority_cursor = 0;
        self.in_priority_sweep = false;
        self.start_rotation()
    }

    /// Begin or resume rotation from the current cursor. Emits
    /// Retune + MuteAudio + ActiveChannelChanged + StateChanged.
    /// Returns EmptyRotation if no scannable + unlocked channels
    /// exist, and transitions to Idle.
    fn start_rotation(&mut self) -> Vec<ScannerCommand> {
        let Some(idx) = self.pick_next_channel() else {
            // No scannable channels available.
            self.phase = Phase::Idle;
            return vec![
                ScannerCommand::EmptyRotation,
                ScannerCommand::MuteAudio(false),
                ScannerCommand::ActiveChannelChanged(None),
                ScannerCommand::StateChanged(ScannerState::Idle),
            ];
        };
        self.enter_retuning(idx)
    }

    fn stop_rotation(&mut self) -> Vec<ScannerCommand> {
        self.phase = Phase::Idle;
        vec![
            ScannerCommand::MuteAudio(false),
            ScannerCommand::ActiveChannelChanged(None),
            ScannerCommand::StateChanged(ScannerState::Idle),
        ]
    }

    /// Emit the retune command set for the given channel index
    /// and move to Retuning phase. Settle window initialized on
    /// the first SampleTick after entering Retuning (the sample
    /// rate isn't known here).
    fn enter_retuning(&mut self, idx: usize) -> Vec<ScannerCommand> {
        let channel = &self.channels[idx];
        self.phase = Phase::Retuning {
            target_idx: idx,
            samples_until_settled: 0, // seeded on first SampleTick
        };
        vec![
            ScannerCommand::Retune {
                freq_hz: channel.frequency_hz,
                demod_mode: channel.demod_mode,
                bandwidth: channel.bandwidth,
                ctcss: channel.ctcss.clone(),
                voice_squelch: channel.voice_squelch,
            },
            ScannerCommand::MuteAudio(true),
            ScannerCommand::ActiveChannelChanged(Some(channel.key.clone())),
            ScannerCommand::StateChanged(ScannerState::Retuning),
        ]
    }

    /// Pick the next channel to scan given current cursor and
    /// priority-sweep state. Returns None if no scannable+unlocked
    /// channels exist.
    fn pick_next_channel(&mut self) -> Option<usize> {
        // Trigger priority sweep if due.
        if !self.in_priority_sweep
            && self.hops_since_priority_sweep >= PRIORITY_CHECK_INTERVAL
            && self.channels.iter().any(|c| c.priority >= 1)
        {
            self.in_priority_sweep = true;
            self.priority_cursor = 0;
        }

        if self.in_priority_sweep {
            let pri_indices: Vec<usize> = self
                .channels
                .iter()
                .enumerate()
                .filter(|(_, c)| c.priority >= 1 && !self.locked_out.contains(&c.key))
                .map(|(i, _)| i)
                .collect();
            if self.priority_cursor < pri_indices.len() {
                let chosen = pri_indices[self.priority_cursor];
                self.priority_cursor += 1;
                return Some(chosen);
            }
            // Priority sweep exhausted.
            self.in_priority_sweep = false;
            self.priority_cursor = 0;
            self.hops_since_priority_sweep = 0;
            // Fall through to normal rotation.
        }

        let normal_indices: Vec<usize> = self
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| c.priority == 0 && !self.locked_out.contains(&c.key))
            .map(|(i, _)| i)
            .collect();

        if normal_indices.is_empty() {
            // If no normal channels, fall back to any unlocked
            // channel (priority-only lists).
            let any_unlocked: Vec<usize> = self
                .channels
                .iter()
                .enumerate()
                .filter(|(_, c)| !self.locked_out.contains(&c.key))
                .map(|(i, _)| i)
                .collect();
            if any_unlocked.is_empty() {
                return None;
            }
            if self.normal_cursor >= any_unlocked.len() {
                self.normal_cursor = 0;
            }
            let chosen = any_unlocked[self.normal_cursor];
            self.normal_cursor = (self.normal_cursor + 1) % any_unlocked.len();
            return Some(chosen);
        }

        if self.normal_cursor >= normal_indices.len() {
            self.normal_cursor = 0;
        }
        let chosen = normal_indices[self.normal_cursor];
        self.normal_cursor = (self.normal_cursor + 1) % normal_indices.len();
        Some(chosen)
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

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_types::DemodMode;

    fn ch(name: &str, freq: u64, priority: u8) -> ScannerChannel {
        ScannerChannel {
            key: ChannelKey {
                name: name.to_string(),
                frequency_hz: freq,
            },
            frequency_hz: freq,
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
        assert!(matches!(commands[0], ScannerCommand::Retune { freq_hz: 146_520_000, .. }));
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
}
