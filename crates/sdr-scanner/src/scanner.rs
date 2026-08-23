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

/// How much post-settle audio confirms a latched carrier as genuine.
/// Covers the IQ ring drain after a retune (the settle window is
/// `SETTLE_MS`; the ring can hold a little more at high rates) while
/// staying far below any realistic hang time, so a real transmission
/// that was already active at retune keeps its `hang_ms` (#759).
const PROVISIONAL_CONFIRM_MS: u32 = 100;

/// Internal phase carrying per-phase bookkeeping. The outer
/// `ScannerState` surfaced to the UI is a flattened view of this.
#[derive(Debug, Clone)]
enum Phase {
    Idle,
    Retuning {
        target_idx: usize,
        /// `None` → seed on next `SampleTick`; `Some(n)` → n µs remaining.
        us_until_settled: Option<u64>,
    },
    Dwelling {
        idx: usize,
        us_until_timeout: u64,
    },
    Listening {
        idx: usize,
        /// `Some(µs)` while the Listening is provisional: it was
        /// entered from the settle-window latch rather than a
        /// post-settle edge, so the carrier may be the previous
        /// channel's IQ still draining from the ring. A close during
        /// that window resumes the dwell instead of hanging; once
        /// [`PROVISIONAL_CONFIRM_MS`] of post-settle audio has passed
        /// the carrier is confirmed and this becomes `None` (#759).
        provisional_us: Option<u64>,
    },
    Hanging {
        idx: usize,
        /// `None` → seed on next `SampleTick`; `Some(n)` → n µs remaining.
        us_until_timeout: Option<u64>,
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
    /// Sub-microsecond remainder of the last `SampleTick` conversion,
    /// in sample·µs units (`< tick_carry_rate`), carried into the next
    /// tick so many tiny ticks measure the same duration as one large
    /// one (CR on PR #798).
    tick_carry: u64,
    /// Sample rate the carry was produced at; a different rate
    /// discards the carry (it is meaningless in other units).
    tick_carry_rate: u32,
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
            tick_carry: 0,
            tick_carry_rate: 0,
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

    /// Whether the scanner is user-enabled. Distinct from
    /// `state() != Idle` because `Idle` is also the rest phase
    /// after `EmptyRotation` (enabled scanner with every channel
    /// locked) — callers gating "scanner is active" on phase
    /// alone miss that case. The mutex with
    /// recording/transcription wants `is_enabled()` so a pending
    /// re-resume via `UnlockScannerChannel` / `UpdateScannerChannels`
    /// counts as "scanner active."
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
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
            // Rotation state clears on disable so re-enabling starts
            // fresh rather than carrying mid-cycle cursors. Lockouts
            // are NOT cleared: they are scoped to the app session (the
            // panel promises "skip for the rest of this session"), and
            // the UI flips the master switch off on `EmptyRotation`, so
            // clearing here threw away every lockout the moment the
            // user locked out the last unlocked channel. Per #757.
            self.next_channel_idx = 0;
            self.hops_since_priority_sweep = 0;
            self.priority_sweep_visited = None;
            self.stop_rotation()
        }
    }

    fn handle_channels_changed(&mut self, channels: Vec<ScannerChannel>) -> Vec<ScannerCommand> {
        let active = self.current_channel_key().cloned();
        let previous_tune = active
            .as_ref()
            .and_then(|key| self.channels.iter().find(|c| &c.key == key))
            .map(ScannerChannel::tune_config);
        // Where normal rotation resumes, as a key so it survives a
        // reorder (matters during a priority sweep, see below).
        let cursor_key = self
            .channels
            .get(self.next_channel_idx)
            .map(|c| c.key.clone());
        self.channels = channels;
        // Any stale lockout keys for channels that no longer exist
        // are harmless (the set is only consulted against the
        // live channel list), but we'll prune for cleanliness.
        let valid: HashSet<ChannelKey> = self.channels.iter().map(|c| c.key.clone()).collect();
        self.locked_out.retain(|k| valid.contains(k));

        if !self.enabled {
            return Vec::new();
        }

        // Re-resolve the active channel by key. The UI pushes the
        // whole list on every bookmark edit and on every default
        // dwell/hang spin-row notify; restarting the rotation for
        // each of those retuned to index 0 and muted mid-word (#758).
        // Only the channel's own tune config (frequency, mode,
        // bandwidth, CTCSS, voice squelch) warrants a retune — dwell /
        // hang changes apply when those phases are next seeded.
        if let (Some(key), Some(previous_tune)) = (active, previous_tune)
            && !self.locked_out.contains(&key)
            && let Some(new_idx) = self.channels.iter().position(|c| c.key == key)
            && self.channels[new_idx].tune_config() == previous_tune
        {
            self.rebind_active_index(new_idx);
            if let Some(visited) = self.priority_sweep_visited.as_mut() {
                // A sweep is an interruption of the normal rotation,
                // which must resume where it left off: re-resolve the
                // old cursor by key rather than anchoring it past the
                // priority channel (that starved every normal channel
                // in between, #756).
                visited.retain(|k| valid.contains(k));
                self.next_channel_idx = cursor_key
                    .and_then(|key| self.channels.iter().position(|c| c.key == key))
                    .unwrap_or(0);
            } else {
                self.next_channel_idx = (new_idx + 1) % self.channels.len();
            }
            return Vec::new();
        }

        // The active channel is gone or its tune config changed:
        // recover by re-starting rotation. Reset the cursor + sweep
        // state + hops counter so a list edit doesn't leave stale
        // pointers or trigger an immediate priority sweep just
        // because the pre-edit session had accumulated hops.
        self.next_channel_idx = 0;
        self.hops_since_priority_sweep = 0;
        self.priority_sweep_visited = None;
        self.start_rotation()
    }

    /// Point the current phase at `idx` without changing anything
    /// else about it (the channel list was re-ordered around it).
    fn rebind_active_index(&mut self, idx: usize) {
        match &mut self.phase {
            Phase::Retuning { target_idx, .. } => *target_idx = idx,
            Phase::Dwelling { idx: i, .. }
            | Phase::Listening { idx: i, .. }
            | Phase::Hanging { idx: i, .. } => *i = idx,
            Phase::Idle | Phase::AdvanceFromDwell | Phase::AdvanceFromHang => {}
        }
    }

    /// Begin or resume rotation from the current cursor. Emits
    /// Retune + `MuteAudio` + `ActiveChannelChanged` + `StateChanged`.
    /// Returns `EmptyRotation` if no scannable + unlocked channels
    /// exist, and transitions to Idle.
    fn start_rotation(&mut self) -> Vec<ScannerCommand> {
        let Some(idx) = self.pick_next_channel() else {
            // No scannable channels available. An enabled scanner
            // with nothing to scan keeps the sink gated (same as the
            // `advance_rotation` empty branch, #759); disabling it
            // releases the mute.
            self.phase = Phase::Idle;
            return vec![
                ScannerCommand::EmptyRotation,
                ScannerCommand::MuteAudio(true),
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
            us_until_settled: None, // seeded on first SampleTick
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
                // Deliberately leave `next_channel_idx` alone: a sweep is
                // an interruption of the normal rotation, which must
                // resume where it left off. Advancing the shared cursor
                // past the priority channel permanently starved every
                // normal channel between the interruption point and the
                // priority channel's successor. The visited set already
                // prevents re-picking within one sweep. Per #756.
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
                self.phase = Phase::Listening {
                    idx,
                    provisional_us: None,
                };
                vec![
                    ScannerCommand::MuteAudio(false),
                    ScannerCommand::StateChanged(ScannerState::Listening),
                ]
            }
            (
                Phase::Listening {
                    idx,
                    provisional_us,
                },
                SquelchState::Closed,
            ) => {
                let (idx, provisional) = (*idx, provisional_us.is_some());
                self.close_from_listening(idx, provisional)
            }
            _ => Vec::new(),
        }
    }

    /// Squelch closed while Listening: hang, unless the Listening
    /// was provisional — entered from the settle-window latch — in
    /// which case the "carrier" was the previous channel's IQ still
    /// draining from the ring and a full dwell resumes instead of
    /// `hang_ms` of dead air (#759).
    fn close_from_listening(&mut self, idx: usize, provisional: bool) -> Vec<ScannerCommand> {
        if provisional {
            self.phase = Phase::Dwelling {
                idx,
                us_until_timeout: ms_to_us(self.channels[idx].dwell_ms),
            };
            vec![
                ScannerCommand::MuteAudio(true),
                ScannerCommand::StateChanged(ScannerState::Dwelling),
            ]
        } else {
            self.phase = Phase::Hanging {
                idx,
                us_until_timeout: None, // seed on first tick
            };
            vec![
                ScannerCommand::MuteAudio(true),
                ScannerCommand::StateChanged(ScannerState::Hanging),
            ]
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
        // Countdowns are kept in microseconds and each tick is
        // converted at the rate it was produced, so a mid-countdown
        // sample-rate change neither drains a dwell in a few ms nor
        // stretches a hang to tens of seconds (#759).
        let elapsed_us = self.tick_elapsed_us(samples_consumed, sample_rate_hz);
        let next_phase: Option<Phase> = match &mut self.phase {
            Phase::Idle
            | Phase::Listening {
                provisional_us: None,
                ..
            } => return Vec::new(),
            Phase::Listening {
                provisional_us: Some(remaining),
                ..
            } => {
                // Post-settle audio belongs to the new channel: once
                // enough has passed the latched carrier is genuine.
                *remaining = remaining.saturating_sub(elapsed_us);
                if *remaining == 0
                    && let Phase::Listening { provisional_us, .. } = &mut self.phase
                {
                    *provisional_us = None;
                }
                return Vec::new();
            }
            Phase::Retuning {
                target_idx,
                us_until_settled,
            } => {
                let before = us_until_settled.unwrap_or_else(|| ms_to_us(SETTLE_MS));
                let remaining = before.saturating_sub(elapsed_us);
                *us_until_settled = Some(remaining);
                if remaining == 0 {
                    let idx = *target_idx;
                    // Settle complete. If the channel's squelch was
                    // already open (persistent carrier, tracked via
                    // the `squelch_open` latch through the ignored-
                    // edges settle window), jump directly to
                    // Listening rather than Dwelling — otherwise
                    // we'd sit silent waiting for a second edge
                    // that the squelch detector already fired.
                    // The part of this block past the settle window is
                    // post-settle audio: dwell already spent, or
                    // confirmation of a latched carrier.
                    let overshoot_us = elapsed_us.saturating_sub(before);
                    if self.squelch_open {
                        Some(Phase::Listening {
                            idx,
                            provisional_us: ms_to_us(PROVISIONAL_CONFIRM_MS)
                                .checked_sub(overshoot_us)
                                .filter(|&remaining| remaining > 0),
                        })
                    } else {
                        let dwell_us = ms_to_us(self.channels[idx].dwell_ms);
                        Some(Phase::Dwelling {
                            idx,
                            us_until_timeout: dwell_us.saturating_sub(overshoot_us),
                        })
                    }
                } else {
                    None
                }
            }
            Phase::Dwelling {
                us_until_timeout, ..
            } => {
                *us_until_timeout = us_until_timeout.saturating_sub(elapsed_us);
                if *us_until_timeout == 0 {
                    Some(Phase::AdvanceFromDwell)
                } else {
                    None
                }
            }
            Phase::Hanging {
                idx,
                us_until_timeout,
            } => {
                let before =
                    us_until_timeout.unwrap_or_else(|| ms_to_us(self.channels[*idx].hang_ms));
                let remaining = before.saturating_sub(elapsed_us);
                *us_until_timeout = Some(remaining);
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
                us_until_timeout: 0,
            }) => {
                // The settle-expiry block overshot by a whole dwell
                // with nothing opening: advance straight away.
                self.phase = Phase::Dwelling {
                    idx,
                    us_until_timeout: 0,
                };
                self.advance_rotation()
            }
            Some(Phase::Dwelling {
                idx,
                us_until_timeout,
            }) => {
                self.phase = Phase::Dwelling {
                    idx,
                    us_until_timeout,
                };
                vec![ScannerCommand::StateChanged(ScannerState::Dwelling)]
            }
            Some(Phase::Listening {
                idx,
                provisional_us,
            }) => {
                // Persistent-open-carrier path from settle expiry.
                self.phase = Phase::Listening {
                    idx,
                    provisional_us,
                };
                vec![
                    ScannerCommand::MuteAudio(false),
                    ScannerCommand::StateChanged(ScannerState::Listening),
                ]
            }
            Some(Phase::AdvanceFromDwell | Phase::AdvanceFromHang) => {
                // The hop counter is owned by `pick_next_channel` (only
                // normal-rotation picks count). A second increment here
                // made sweeps fire after ~3 hops instead of
                // `PRIORITY_CHECK_INTERVAL` and flip-flopped priority-only
                // lists between sweep and fallback. Per #756.
                self.advance_rotation()
            }
            None | Some(_) => Vec::new(),
        }
    }

    fn advance_rotation(&mut self) -> Vec<ScannerCommand> {
        if let Some(idx) = self.pick_next_channel() {
            self.enter_retuning(idx)
        } else {
            // The radio is still tuned to the channel we are leaving
            // (a lockout of the active channel lands here), so keep
            // the sink gated — un-gating played the locked-out
            // transmission while the UI showed no active channel
            // (#759). Disabling the scanner releases the mute.
            self.phase = Phase::Idle;
            vec![
                ScannerCommand::EmptyRotation,
                ScannerCommand::MuteAudio(true),
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
            Phase::Dwelling { idx, .. }
            | Phase::Listening { idx, .. }
            | Phase::Hanging { idx, .. } => self.channels.get(*idx).map(|c| &c.key),
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

impl Scanner {
    /// Duration of `samples` at `sample_rate_hz` in whole microseconds,
    /// with the sub-microsecond remainder carried to the next call so
    /// the conversion is exact over any sequence of ticks. (Rounding
    /// each tick up expired a 100 ms dwell after ~42 ms of one-sample
    /// ticks at 2.4 Msps.) The carry is in sample·µs units and is only
    /// meaningful at the rate that produced it; a rate change discards
    /// it, which is at most one microsecond.
    fn tick_elapsed_us(&mut self, samples: u32, sample_rate_hz: NonZeroU32) -> u64 {
        let rate = u64::from(sample_rate_hz.get());
        let carry = if self.tick_carry_rate == sample_rate_hz.get() {
            self.tick_carry
        } else {
            0
        };
        let total = u64::from(samples) * US_PER_SEC + carry;
        self.tick_carry = total % rate;
        self.tick_carry_rate = sample_rate_hz.get();
        total / rate
    }
}

const US_PER_MS: u64 = 1_000;
const US_PER_SEC: u64 = 1_000_000;

/// Milliseconds → microseconds for seeding `us_until_*`.
fn ms_to_us(ms: u32) -> u64 {
    u64::from(ms) * US_PER_MS
}

#[cfg(test)]
mod tests;
