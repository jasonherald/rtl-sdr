//! Scanner state machine. Owns rotation position, phase, timing
//! counters, and session lockouts. No threading, no I/O — the
//! DSP controller drives it via `handle_event` and applies the
//! returned commands.

use std::collections::HashSet;
use std::num::NonZeroU32;

use crate::channel::{ChannelKey, ScannerChannel};
use crate::commands::ScannerCommand;
use crate::events::{ScannerEvent, SquelchState};
use crate::state::ScannerState;
use crate::{PRIORITY_CHECK_INTERVAL, SETTLE_MS};

/// Lowest `ScannerChannel::priority` value that counts as
/// "priority" for the priority-sweep logic. Channels at or above
/// this tier are pulled into the priority sub-list; below are
/// normal rotation. Single-tier in Phase 1 — `1` is the only
/// promoted value — but centralizing the threshold makes the
/// multi-tier #365 work a one-constant change.
const MIN_PRIORITY_TIER: u8 = 1;

/// Internal phase carrying per-phase bookkeeping. The outer
/// `ScannerState` surfaced to the UI is a flattened view of this.
#[derive(Debug, Clone)]
enum Phase {
    Idle,
    Retuning {
        target_idx: usize,
        /// `None` → seed on next `SampleTick`; `Some(n)` → n samples remaining.
        samples_until_settled: Option<u64>,
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
        /// `None` → seed on next `SampleTick`; `Some(n)` → n samples remaining.
        samples_until_timeout: Option<u64>,
    },
    /// Transition marker: advance rotation after a Dwelling timeout.
    /// Never stored in `self.phase`; used only inside `handle_sample_tick`.
    AdvanceFromDwell,
    /// Transition marker: advance rotation after a Hanging timeout.
    /// Never stored in `self.phase`; used only inside `handle_sample_tick`.
    AdvanceFromHang,
}

impl Phase {
    fn as_state(&self) -> ScannerState {
        match self {
            Phase::Idle => ScannerState::Idle,
            Phase::Retuning { .. } => ScannerState::Retuning,
            Phase::Dwelling { .. } => ScannerState::Dwelling,
            Phase::Listening { .. } => ScannerState::Listening,
            Phase::Hanging { .. } => ScannerState::Hanging,
            // `AdvanceFromDwell` / `AdvanceFromHang` are synthetic
            // transition markers returned from `handle_sample_tick`
            // and immediately consumed by its downstream match — they
            // should never appear here. Debug builds assert, release
            // falls back to `Idle` rather than panicking a library.
            Phase::AdvanceFromDwell | Phase::AdvanceFromHang => {
                debug_assert!(
                    false,
                    "advance markers should never sit as the active phase"
                );
                ScannerState::Idle
            }
        }
    }
}

/// Scanner state machine. Instantiate once, feed events, apply
/// emitted commands. All methods are synchronous and cheap.
///
/// Timing defaults (dwell, hang) live OUTSIDE the scanner — the
/// UI folds them into each `ScannerChannel` at projection time.
/// Scanner only sees resolved per-channel `dwell_ms` / `hang_ms`,
/// so a default-slider change on the UI side triggers a fresh
/// `ChannelsChanged` push rather than a separate "update defaults"
/// signal the scanner would otherwise have to propagate.
pub struct Scanner {
    enabled: bool,
    channels: Vec<ScannerChannel>,
    locked_out: HashSet<ChannelKey>,
    phase: Phase,
    /// Absolute position in `self.channels` to consider first on
    /// the next `pick_next_channel` call. Wrap-around semantics
    /// (`(i + 1) % channels.len()`) after every pick.
    ///
    /// Deliberately an absolute index rather than a position
    /// inside a filtered sub-list: lockouts and channel-list
    /// edits change the sub-list shape, but the absolute index
    /// remains a stable anchor. Any channel that no longer
    /// matches the selection criteria (locked / priority filter
    /// mismatch) is just skipped by the forward scan — no
    /// cursor rebase needed.
    next_channel_idx: usize,
    /// Count of completed NORMAL-ROTATION hops since the last
    /// priority sweep. Only incremented in the normal-pick
    /// branch of `pick_next_channel` — fallback (any-unlocked)
    /// and priority-sweep picks do NOT advance this counter.
    /// When it reaches `PRIORITY_CHECK_INTERVAL` AND there's at
    /// least one unlocked priority channel, a priority sweep is
    /// armed on the next pick.
    hops_since_priority_sweep: u32,
    /// Channel keys visited during the current priority sweep.
    /// `None` means no sweep in progress; `Some(set)` means the
    /// scanner is mid-sweep and the set tracks which priority
    /// channels have already been picked this sweep. When the
    /// set size equals the count of priority+unlocked channels,
    /// the sweep ends and normal rotation resumes.
    ///
    /// Using `Option<HashSet>` rather than `HashSet::new()` as a
    /// sentinel because an empty in-progress sweep (just
    /// started, no picks yet) and a non-sweep state must be
    /// distinguishable.
    priority_sweep_visited: Option<HashSet<ChannelKey>>,
    /// Latched squelch state. Updated on every `SquelchEdge`
    /// event regardless of phase so a persistent-open carrier
    /// that triggered during the settle window (where phase
    /// transitions ignore edges) isn't forgotten — we consult
    /// this at settle expiry to decide Dwelling vs direct
    /// Listening. Reset to `false` on every retune entry.
    squelch_open: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            enabled: false,
            channels: Vec::new(),
            locked_out: HashSet::new(),
            phase: Phase::Idle,
            next_channel_idx: 0,
            hops_since_priority_sweep: 0,
            priority_sweep_visited: None,
            squelch_open: false,
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
            ScannerEvent::UnlockChannel(key) => self.handle_unlockout(&key),
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
            // Session-scoped state clears on disable so re-enabling
            // starts fresh rather than carrying stale lockouts or
            // mid-cycle rotation cursors into the next session.
            self.locked_out.clear();
            self.next_channel_idx = 0;
            self.hops_since_priority_sweep = 0;
            self.priority_sweep_visited = None;
            self.stop_rotation()
        }
    }

    fn handle_channels_changed(&mut self, channels: Vec<ScannerChannel>) -> Vec<ScannerCommand> {
        self.channels = channels;
        // Any stale lockout keys for channels that no longer exist
        // are harmless (the set is only consulted against the
        // live channel list), but we'll prune for cleanliness.
        let valid: HashSet<ChannelKey> = self.channels.iter().map(|c| c.key.clone()).collect();
        self.locked_out.retain(|k| valid.contains(k));

        if !self.enabled {
            return Vec::new();
        }
        // Currently-scanning mid-list-change: recover from wherever
        // the phase left us by re-starting rotation. Also reset
        // the rotation cursor + sweep state + hops counter so a
        // list edit doesn't leave stale pointers or trigger an
        // immediate priority sweep just because the pre-edit
        // session had accumulated hops.
        self.next_channel_idx = 0;
        self.hops_since_priority_sweep = 0;
        self.priority_sweep_visited = None;
        self.start_rotation()
    }

    /// Begin or resume rotation from the current cursor. Emits
    /// Retune + `MuteAudio` + `ActiveChannelChanged` + `StateChanged`.
    /// Returns `EmptyRotation` if no scannable + unlocked channels
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
    /// the first `SampleTick` after entering Retuning (the sample
    /// rate isn't known here).
    fn enter_retuning(&mut self, idx: usize) -> Vec<ScannerCommand> {
        let channel = &self.channels[idx];
        // Clear the latched squelch state — previous channel's
        // open/closed state is irrelevant post-retune, and samples
        // arriving during the new channel's settle window will
        // update the latch so the settle-expiry decision has the
        // right information.
        self.squelch_open = false;
        self.phase = Phase::Retuning {
            target_idx: idx,
            samples_until_settled: None, // seeded on first SampleTick
        };
        vec![
            ScannerCommand::Retune {
                freq_hz: channel.key.frequency_hz,
                demod_mode: channel.demod_mode,
                bandwidth: channel.bandwidth,
                ctcss: channel.ctcss,
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
        if self.channels.is_empty() {
            return None;
        }

        // Arm a priority sweep if hops since last sweep crossed
        // the threshold AND at least one unlocked priority
        // channel exists.
        if self.priority_sweep_visited.is_none()
            && self.hops_since_priority_sweep >= PRIORITY_CHECK_INTERVAL
            && self
                .channels
                .iter()
                .any(|c| c.priority >= MIN_PRIORITY_TIER && !self.locked_out.contains(&c.key))
        {
            self.priority_sweep_visited = Some(HashSet::new());
        }

        // --- Priority sweep path ---
        if self.priority_sweep_visited.is_some() {
            let chosen = self.scan_forward(|c| {
                c.priority >= MIN_PRIORITY_TIER
                    && !self.locked_out.contains(&c.key)
                    && !self
                        .priority_sweep_visited
                        .as_ref()
                        .is_some_and(|v| v.contains(&c.key))
            });
            if let Some(idx) = chosen {
                let key = self.channels[idx].key.clone();
                if let Some(visited) = self.priority_sweep_visited.as_mut() {
                    visited.insert(key);
                }
                self.advance_cursor_past(idx);
                return Some(idx);
            }
            // Sweep exhausted — no more unvisited + unlocked priorities.
            self.priority_sweep_visited = None;
            self.hops_since_priority_sweep = 0;
            // Fall through to normal rotation.
        }

        // --- Normal rotation path ---
        if let Some(idx) =
            self.scan_forward(|c| c.priority == 0 && !self.locked_out.contains(&c.key))
        {
            self.advance_cursor_past(idx);
            // Only normal-rotation picks count toward the next
            // priority sweep. Fallback (any-unlocked) and sweep
            // picks do NOT — they don't represent "normal hops
            // seen between sweeps."
            self.hops_since_priority_sweep = self.hops_since_priority_sweep.saturating_add(1);
            return Some(idx);
        }

        // --- Fallback: any unlocked channel (priority-only lists) ---
        if let Some(idx) = self.scan_forward(|c| !self.locked_out.contains(&c.key)) {
            self.advance_cursor_past(idx);
            // Deliberately NOT incrementing `hops_since_priority_sweep`
            // here: on a priority-only list there is no "normal
            // rotation" to count against, and arming another
            // sweep would be redundant (the fallback is already
            // visiting priorities) — could cause out-of-order
            // replay via sweep↔fallback mode flipping.
            return Some(idx);
        }

        None
    }

    /// Scan `self.channels` starting at `next_channel_idx`, wrapping
    /// around once, returning the index of the first channel that
    /// satisfies `predicate`. Allocation-free; single pass.
    fn scan_forward<F>(&self, predicate: F) -> Option<usize>
    where
        F: Fn(&ScannerChannel) -> bool,
    {
        let n = self.channels.len();
        if n == 0 {
            return None;
        }
        (0..n)
            .map(|offset| (self.next_channel_idx + offset) % n)
            .find(|&idx| predicate(&self.channels[idx]))
    }

    /// Advance the absolute cursor past the just-picked index so
    /// the next `scan_forward` starts at the subsequent position
    /// (with wrap).
    fn advance_cursor_past(&mut self, idx: usize) {
        let n = self.channels.len();
        if n == 0 {
            self.next_channel_idx = 0;
            return;
        }
        self.next_channel_idx = (idx + 1) % n;
    }

    fn handle_squelch_edge(&mut self, state: SquelchState) -> Vec<ScannerCommand> {
        // Always latch the current squelch state, regardless of
        // phase. Retuning drops the phase transition (transients
        // would false-trigger) but must still remember that a
        // carrier is present, otherwise settle-expiry on a
        // persistent-open channel would hop straight to
        // `Dwelling` and wait indefinitely for an edge that
        // already fired.
        self.squelch_open = matches!(state, SquelchState::Open);
        match (&self.phase, state) {
            (Phase::Retuning { .. }, _) => {
                // Ignore edges during settle window — `squelch_open`
                // latch is consulted when settle expires.
                Vec::new()
            }
            (Phase::Dwelling { idx, .. } | Phase::Hanging { idx, .. }, SquelchState::Open) => {
                let idx = *idx;
                self.phase = Phase::Listening { idx };
                vec![
                    ScannerCommand::MuteAudio(false),
                    ScannerCommand::StateChanged(ScannerState::Listening),
                ]
            }
            (Phase::Listening { idx }, SquelchState::Closed) => {
                let idx = *idx;
                self.phase = Phase::Hanging {
                    idx,
                    samples_until_timeout: None, // seed on first tick
                };
                vec![
                    ScannerCommand::MuteAudio(true),
                    ScannerCommand::StateChanged(ScannerState::Hanging),
                ]
            }
            _ => Vec::new(),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "state-machine match arms with per-phase countdown logic + downstream dispatch — splitting would fragment a single conceptual transition"
    )]
    fn handle_sample_tick(
        &mut self,
        samples_consumed: u32,
        sample_rate_hz: NonZeroU32,
    ) -> Vec<ScannerCommand> {
        // `sample_rate_hz > 0` is now enforced at the event-type
        // level via `NonZeroU32` — no runtime guard needed.
        let sample_rate_hz = sample_rate_hz.get();
        let samples = u64::from(samples_consumed);
        let next_phase: Option<Phase> = match &mut self.phase {
            Phase::Idle | Phase::Listening { .. } => return Vec::new(),
            Phase::Retuning {
                target_idx,
                samples_until_settled,
            } => {
                let remaining = match samples_until_settled {
                    None => {
                        let seeded =
                            ms_to_samples(SETTLE_MS, sample_rate_hz).saturating_sub(samples);
                        *samples_until_settled = Some(seeded);
                        seeded
                    }
                    Some(remaining) => {
                        *remaining = remaining.saturating_sub(samples);
                        *remaining
                    }
                };
                if remaining == 0 {
                    let idx = *target_idx;
                    // Settle complete. If the channel's squelch was
                    // already open (persistent carrier, tracked via
                    // the `squelch_open` latch through the ignored-
                    // edges settle window), jump directly to
                    // Listening rather than Dwelling — otherwise
                    // we'd sit silent waiting for a second edge
                    // that the squelch detector already fired.
                    if self.squelch_open {
                        Some(Phase::Listening { idx })
                    } else {
                        let dwell_ms = self.channels[idx].dwell_ms;
                        Some(Phase::Dwelling {
                            idx,
                            samples_until_timeout: ms_to_samples(dwell_ms, sample_rate_hz),
                        })
                    }
                } else {
                    None
                }
            }
            Phase::Dwelling {
                samples_until_timeout,
                ..
            } => {
                *samples_until_timeout = samples_until_timeout.saturating_sub(samples);
                if *samples_until_timeout == 0 {
                    Some(Phase::AdvanceFromDwell)
                } else {
                    None
                }
            }
            Phase::Hanging {
                idx,
                samples_until_timeout,
            } => {
                let remaining = match samples_until_timeout {
                    None => {
                        let hang_ms = self.channels[*idx].hang_ms;
                        let seeded = ms_to_samples(hang_ms, sample_rate_hz).saturating_sub(samples);
                        *samples_until_timeout = Some(seeded);
                        seeded
                    }
                    Some(remaining) => {
                        *remaining = remaining.saturating_sub(samples);
                        *remaining
                    }
                };
                if remaining == 0 {
                    Some(Phase::AdvanceFromHang)
                } else {
                    None
                }
            }
            Phase::AdvanceFromDwell | Phase::AdvanceFromHang => {
                // Defensive: see comment on `Phase::as_state`. These
                // markers are only ever returned from the inner
                // match; hitting them here would be a state-machine
                // bug, but panicking a library for it is wrong.
                debug_assert!(
                    false,
                    "advance markers should never sit as the active phase"
                );
                None
            }
        };

        match next_phase {
            Some(Phase::Dwelling {
                idx,
                samples_until_timeout,
            }) => {
                self.phase = Phase::Dwelling {
                    idx,
                    samples_until_timeout,
                };
                vec![ScannerCommand::StateChanged(ScannerState::Dwelling)]
            }
            Some(Phase::Listening { idx }) => {
                // Persistent-open-carrier path from settle expiry.
                self.phase = Phase::Listening { idx };
                vec![
                    ScannerCommand::MuteAudio(false),
                    ScannerCommand::StateChanged(ScannerState::Listening),
                ]
            }
            Some(Phase::AdvanceFromDwell | Phase::AdvanceFromHang) => {
                self.hops_since_priority_sweep += 1;
                self.advance_rotation()
            }
            None | Some(_) => Vec::new(),
        }
    }

    fn advance_rotation(&mut self) -> Vec<ScannerCommand> {
        if let Some(idx) = self.pick_next_channel() {
            self.enter_retuning(idx)
        } else {
            self.phase = Phase::Idle;
            vec![
                ScannerCommand::EmptyRotation,
                ScannerCommand::MuteAudio(false),
                ScannerCommand::ActiveChannelChanged(None),
                ScannerCommand::StateChanged(ScannerState::Idle),
            ]
        }
    }

    fn handle_lockout(&mut self, key: ChannelKey) -> Vec<ScannerCommand> {
        // If the user locks out the currently-active channel, the
        // "skip on next rotation advance" strategy isn't enough:
        // a persistent-open carrier never triggers an advance
        // (no dwell-timeout / hang-elapse / squelch-close fires),
        // so the scanner would sit indefinitely on a channel the
        // user just asked to skip. Force-advance now.
        let lock_current = self.enabled
            && self
                .current_channel_key()
                .is_some_and(|current| current == &key);
        self.locked_out.insert(key);
        if lock_current {
            self.advance_rotation()
        } else {
            Vec::new()
        }
    }

    /// Identity of the channel the scanner currently considers
    /// active, regardless of phase. Returns `None` when idle or
    /// mid-transition-marker (which should never persist in
    /// `self.phase` anyway).
    fn current_channel_key(&self) -> Option<&ChannelKey> {
        match &self.phase {
            Phase::Retuning { target_idx, .. } => self.channels.get(*target_idx).map(|c| &c.key),
            Phase::Dwelling { idx, .. } | Phase::Listening { idx } | Phase::Hanging { idx, .. } => {
                self.channels.get(*idx).map(|c| &c.key)
            }
            Phase::Idle | Phase::AdvanceFromDwell | Phase::AdvanceFromHang => None,
        }
    }

    fn handle_unlockout(&mut self, key: &ChannelKey) -> Vec<ScannerCommand> {
        let removed = self.locked_out.remove(key);
        // If the scanner stalled into `Idle` because every channel
        // was locked out (EmptyRotation → Idle), unlocking a
        // channel while still enabled should resume scanning
        // automatically — otherwise the user would have to
        // disable + re-enable to kick it back into motion.
        if removed && self.enabled && matches!(self.phase, Phase::Idle) {
            return self.start_rotation();
        }
        Vec::new()
    }
}

/// Convert milliseconds to a sample count at the given sample rate,
/// rounding up. Uses `div_ceil` so 30 ms at 48 000 Hz = 1440 samples
/// (exact), 30 ms at 44 100 Hz = 1323 samples. Caller uses this to
/// seed `samples_until_*`.
fn ms_to_samples(ms: u32, sample_rate_hz: u32) -> u64 {
    (u64::from(ms) * u64::from(sample_rate_hz)).div_ceil(1000)
}

#[cfg(test)]
mod tests {
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
        // User deletes channel B and adds C.
        let commands = s.handle_event(ScannerEvent::ChannelsChanged(vec![
            ch("A", 146_520_000, 0),
            ch("C", 28_400_000, 0),
        ]));
        // Scanner recovers by restarting rotation at cursor 0.
        assert_eq!(s.state(), ScannerState::Retuning);
        // First retune after list change goes to A.
        assert!(commands.iter().any(|c| matches!(
            c,
            ScannerCommand::Retune {
                freq_hz: 146_520_000,
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
    fn disable_clears_session_state() {
        // Re-enabling after a disable should start fresh — no
        // carried-over lockouts, cursors, or hop counter.
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
        // Session state should be fully clear after disable.
        assert!(s.locked_out.is_empty(), "locked_out not cleared on disable");
        assert_eq!(s.next_channel_idx, 0);
        assert_eq!(s.hops_since_priority_sweep, 0);
        assert!(
            s.priority_sweep_visited.is_none(),
            "priority sweep state not cleared on disable"
        );
    }
}
