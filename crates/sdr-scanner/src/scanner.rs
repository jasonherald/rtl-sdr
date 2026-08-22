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
    /// in sample·µs units (`< sample_rate_hz`), carried into the next
    /// tick so many tiny ticks measure the same duration as one large
    /// one (CR on PR #798).
    tick_carry: u64,
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
                    if self.squelch_open {
                        Some(Phase::Listening {
                            idx,
                            provisional_us: Some(ms_to_us(PROVISIONAL_CONFIRM_MS)),
                        })
                    } else {
                        // The part of this block past the settle
                        // window is dwell time already spent.
                        let overshoot_us = elapsed_us.saturating_sub(before);
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
    /// meaningful at one rate; a rate change discards it, which is at
    /// most one microsecond.
    fn tick_elapsed_us(&mut self, samples: u32, sample_rate_hz: NonZeroU32) -> u64 {
        let rate = u64::from(sample_rate_hz.get());
        let carry = if self.tick_carry < rate {
            self.tick_carry
        } else {
            0
        };
        let total = u64::from(samples) * US_PER_SEC + carry;
        self.tick_carry = total % rate;
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
}
