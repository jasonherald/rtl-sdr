# Scanner Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship classic sequential-scanner behavior (#317) on top of the existing SDR pipeline: retune through scannable favorites, dwell on each, stop-and-play on squelch-open, resume when hang elapses. Single-tier priority cycling, session-scoped lockout, no per-hit recording/transcription (scanner mutually exclusive with both). Split across three PRs for manageable CR cycles.

**Architecture:** Pure state machine in a new `sdr-scanner` crate (no I/O, no threading) consumes events (sample ticks, squelch edges, user toggles) and emits commands (retune, mute, active-channel). `sdr-core::DspController` owns a `Scanner` instance, feeds it events, applies emitted commands. Mirrors the `AutoBreakMachine` pattern from #273. UI surface is a new sidebar panel at the bottom of the left column plus scan/priority toggles on bookmark rows.

**Tech Stack:** Rust 2024, `thiserror` for errors, `serde` for bookmark schema extensions, GTK4 + libadwaita for UI, `tracing` for diagnostics. Unit tests only in PR 1 (pure state machine); PRs 2 and 3 verify via log inspection and manual smoke test.

**Design doc:** `docs/superpowers/specs/2026-04-21-scanner-phase-1-design.md` (read first if unsure about a decision).

---

## PR 1 — `sdr-scanner` crate (engine only)

**Branch:** `feature/scanner-engine`
**Scope:** New workspace crate containing the pure state machine, types, and unit tests. Zero integration with other crates. ~600 lines.

---

### Task 1.1: Crate scaffolding

**Files:**
- Create: `crates/sdr-scanner/Cargo.toml`
- Create: `crates/sdr-scanner/src/lib.rs`
- Modify: `Cargo.toml` (workspace members + workspace.dependencies)

- [ ] **Step 1: Create `crates/sdr-scanner/Cargo.toml`**

```toml
[package]
name = "sdr-scanner"
version = "0.1.0"
description = "Scanner state machine — sequential channel monitoring for SDR pipelines"
edition.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
sdr-types.workspace = true
sdr-dsp.workspace = true
sdr-radio.workspace = true
thiserror.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Create stub `crates/sdr-scanner/src/lib.rs`**

```rust
//! Scanner state machine — sequential channel monitoring for SDR
//! pipelines. Pure no-I/O logic: consumes events, emits commands,
//! leaves all actual radio/audio wiring to the DSP controller that
//! owns an instance.
//!
//! See docs/superpowers/specs/2026-04-21-scanner-phase-1-design.md
//! for the design decisions behind this crate's shape.

pub mod channel;
pub mod commands;
pub mod events;
pub mod scanner;
pub mod state;

pub use channel::{ChannelKey, ScannerChannel};
pub use commands::ScannerCommand;
pub use events::{ScannerEvent, SquelchState};
pub use scanner::Scanner;
pub use state::ScannerState;

/// Default dwell time in ms when a channel doesn't override it.
pub const DEFAULT_DWELL_MS: u32 = 100;

/// Default hang time in ms when a channel doesn't override it.
pub const DEFAULT_HANG_MS: u32 = 2000;

/// Settle window in ms after a retune before the scanner honors
/// squelch edges on the new channel. Covers PLL lock + filter
/// warm-up transients — scanner decisions during this window are
/// unreliable.
pub const SETTLE_MS: u32 = 30;

/// How often (in normal-channel hops) the scanner sweeps priority
/// channels between normal rotations. `5` means every 5 normal
/// hops, all priority-1 channels get a check before resuming.
pub const PRIORITY_CHECK_INTERVAL: u32 = 5;
```

- [ ] **Step 3: Add crate to root workspace**

In `Cargo.toml` at root, add `"crates/sdr-scanner"` to `[workspace] members` list (alphabetical position after `sdr-rtltcp-discovery`):

```toml
members = [
    # ... existing entries ...
    "crates/sdr-rtltcp-discovery",
    "crates/sdr-scanner",
    "crates/sdr-source-file",
    # ... rest ...
]
```

And in `[workspace.dependencies]` after `sdr-rtltcp-discovery`:

```toml
sdr-scanner = { path = "crates/sdr-scanner" }
```

- [ ] **Step 4: Verify crate compiles (skip — expected to fail)**

Running `cargo build -p sdr-scanner` at this stage will FAIL because `lib.rs` declares five submodules (`channel`, `commands`, `events`, `scanner`, `state`) that don't exist yet. Tasks 1.2 and 1.3 create them. Don't try to fix the build here; skip the `cargo build` check and proceed directly to Step 5. `cargo check` / `cargo metadata` on the workspace root will still succeed and validate your Cargo.toml syntax.

- [ ] **Step 5: Commit scaffolding**

```bash
git checkout -b feature/scanner-engine
git add crates/sdr-scanner/ Cargo.toml
git commit -m "scaffold sdr-scanner crate (#317)"
```

---

### Task 1.2: Channel types (`channel.rs`)

**Files:**
- Create: `crates/sdr-scanner/src/channel.rs`

- [ ] **Step 1: Write `channel.rs` with `ChannelKey` + `ScannerChannel`**

```rust
//! Channel identity and per-channel config. `ScannerChannel` is
//! the resolved runtime shape — dwell/hang are already folded from
//! overrides + defaults; the scanner state machine doesn't need to
//! know about `Option`s here.

use sdr_types::DemodMode;

/// Stable identity for a channel across rebuilds of the channel
/// list. `(name, frequency_hz)` — same convention the bookmarks
/// flyout uses for the active-bookmark highlight.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChannelKey {
    pub name: String,
    pub frequency_hz: u64,
}

/// Fully-resolved scanner channel. The UI / controller builds
/// these from `Bookmark` entries at scan-start or on
/// `ChannelsChanged`; the state machine operates on them directly
/// and has no notion of bookmark storage.
#[derive(Debug, Clone)]
pub struct ScannerChannel {
    pub key: ChannelKey,
    pub frequency_hz: u64,
    pub demod_mode: DemodMode,
    pub bandwidth: f64,
    pub ctcss: Option<sdr_radio::af_chain::CtcssMode>,
    pub voice_squelch: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
    /// 0 = normal rotation, >=1 = priority (checked more often).
    pub priority: u8,
    /// Resolved dwell time in ms (per-channel override folded in).
    pub dwell_ms: u32,
    /// Resolved hang time in ms (per-channel override folded in).
    pub hang_ms: u32,
}
```

- [ ] **Step 2: Compile-check**

Run: `cargo build -p sdr-scanner`
Expected: FAIL — other modules not yet created.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-scanner/src/channel.rs
git commit -m "sdr-scanner: ChannelKey + ScannerChannel types"
```

---

### Task 1.3: Event and command types (`events.rs`, `commands.rs`, `state.rs`)

**Files:**
- Create: `crates/sdr-scanner/src/events.rs`
- Create: `crates/sdr-scanner/src/commands.rs`
- Create: `crates/sdr-scanner/src/state.rs`

- [ ] **Step 1: Write `events.rs`**

```rust
//! Events fed into the scanner by the DSP controller or UI.
//! No wall-clock time anywhere — sample-count is the only timing
//! primitive, matching the `AutoBreakMachine` pattern.

use crate::channel::{ChannelKey, ScannerChannel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SquelchState {
    Open,
    Closed,
}

#[derive(Debug, Clone)]
pub enum ScannerEvent {
    /// Fired by the DSP controller on every IQ block arrival.
    /// `samples_consumed` is block length; `sample_rate_hz`
    /// anchors the ms→sample conversion for dwell/hang/settle.
    SampleTick {
        samples_consumed: u32,
        sample_rate_hz: u32,
    },

    /// Edge-triggered squelch transition, identical to the stream
    /// already fed to the transcription tap for Auto Break.
    SquelchEdge(SquelchState),

    /// User added / removed / edited a scannable bookmark.
    /// Scanner swaps its channel list and recovers a sensible
    /// rotation position.
    ChannelsChanged(Vec<ScannerChannel>),

    /// Master scanner on/off toggle.
    SetEnabled(bool),

    /// Session-scoped lockout — channel is skipped in rotation
    /// until unlocked or scanner is disabled.
    LockoutChannel(ChannelKey),
    UnlockoutChannel(ChannelKey),

    /// Global default dwell/hang changes from the UI sliders.
    SetDefaultDwellMs(u32),
    SetDefaultHangMs(u32),
}
```

- [ ] **Step 2: Write `commands.rs`**

```rust
//! Commands emitted by the scanner in response to events.
//! The DSP controller applies these — scanner itself never
//! touches the source, sink, or radio module directly.

use crate::channel::ChannelKey;
use crate::state::ScannerState;

#[derive(Debug, Clone)]
pub enum ScannerCommand {
    /// Retune the source and reconfigure the radio module to this
    /// channel. Controller dispatches `source.set_center_freq`,
    /// `radio_module.set_demod_mode`, `set_bandwidth`,
    /// `set_ctcss_mode`, `set_voice_squelch_mode` in order.
    Retune {
        freq_hz: u64,
        demod_mode: sdr_types::DemodMode,
        bandwidth: f64,
        ctcss: Option<sdr_radio::af_chain::CtcssMode>,
        voice_squelch: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
    },

    /// Gate the final PCM stream to the audio device. DSP chain
    /// keeps running so squelch edges still fire; only user-
    /// audible output is silenced.
    MuteAudio(bool),

    /// UI-facing: active channel changed. `None` during Idle.
    ActiveChannelChanged(Option<ChannelKey>),

    /// UI-facing: scanner phase indicator updated.
    StateChanged(ScannerState),

    /// Emitted when the active rotation is fully empty — every
    /// channel is either removed, disabled, or locked out.
    /// UI surfaces as a toast; scanner transitions to Idle
    /// afterwards.
    EmptyRotation,
}
```

- [ ] **Step 3: Write `state.rs`**

```rust
//! Scanner phase enum surfaced to the UI + internal state variants
//! carrying per-phase bookkeeping.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScannerState {
    /// Scanner off, or on with no channels enabled.
    Idle,
    /// Retune command emitted, audio muted, waiting for settle
    /// window to close before honoring squelch on the new channel.
    Retuning,
    /// Settled on the target channel, audio still muted,
    /// listening for squelch-open within the dwell window.
    Dwelling,
    /// Squelch open post-settle, audio flowing.
    Listening,
    /// Squelch closed, audio muted, counting down hang window
    /// before advancing to next channel.
    Hanging,
}
```

- [ ] **Step 4: Verify compile**

Run: `cargo build -p sdr-scanner`
Expected: Clean build — all pub names in `lib.rs` now resolve.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-scanner/src/events.rs crates/sdr-scanner/src/commands.rs crates/sdr-scanner/src/state.rs
git commit -m "sdr-scanner: event / command / state types"
```

---

### Task 1.4: Scanner struct skeleton (`scanner.rs`)

**Files:**
- Create: `crates/sdr-scanner/src/scanner.rs`

- [ ] **Step 1: Write scanner skeleton with public API**

```rust
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
```

- [ ] **Step 2: Verify compile**

Run: `cargo build -p sdr-scanner`
Expected: Clean build with several `unused` warnings on the TODO stubs — that's fine for now.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-scanner/src/scanner.rs
git commit -m "sdr-scanner: Scanner skeleton with event dispatch"
```

---

### Task 1.5: Rotation logic + enable/disable

**Files:**
- Modify: `crates/sdr-scanner/src/scanner.rs`

- [ ] **Step 1: Write a failing test for the enable flow**

Append to `scanner.rs`:

```rust
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
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p sdr-scanner --lib enable_with_channels_transitions_to_retuning -- --nocapture`
Expected: FAIL — `assertion failed: matches!(commands[0], ScannerCommand::Retune { .. })` because `handle_set_enabled` stub returns empty.

- [ ] **Step 3: Implement `handle_set_enabled` + helpers**

Replace `handle_set_enabled`, `handle_channels_changed` stub bodies and add new helper methods in the `impl Scanner` block:

```rust
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
```

- [ ] **Step 4: Run test to verify PASS**

Run: `cargo test -p sdr-scanner --lib enable_with_channels_transitions_to_retuning`
Expected: PASS.

- [ ] **Step 5: Add disable test + empty-channels test**

Append in `mod tests`:

```rust
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
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p sdr-scanner --lib`
Expected: All 3 tests PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "sdr-scanner: rotation logic + enable/disable transitions"
```

---

### Task 1.6: Sample-tick countdown + squelch-edge transitions

**Files:**
- Modify: `crates/sdr-scanner/src/scanner.rs`

- [ ] **Step 1: Write failing tests for the settle + dwell + listen flow**

Append to `mod tests`:

```rust
    /// 48 kHz rate, so 30ms settle = 1440 samples, 100ms dwell = 4800 samples.
    const RATE: u32 = 48_000;

    fn tick(samples: u32) -> ScannerEvent {
        ScannerEvent::SampleTick {
            samples_consumed: samples,
            sample_rate_hz: RATE,
        }
    }

    #[test]
    fn settle_window_ignores_squelch_open() {
        let mut s = Scanner::new();
        s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
        s.handle_event(ScannerEvent::SetEnabled(true));
        // Feed a squelch open during the settle window.
        s.handle_event(tick(500));
        let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
        assert_eq!(s.state(), ScannerState::Retuning);
        // No MuteAudio(false) should have been emitted.
        assert!(
            !commands.iter().any(|c| matches!(c, ScannerCommand::MuteAudio(false))),
            "mute was released during settle window"
        );
    }

    #[test]
    fn post_settle_squelch_open_transitions_to_listening() {
        let mut s = Scanner::new();
        s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
        s.handle_event(ScannerEvent::SetEnabled(true));
        // Elapse the settle window (1440 samples for 30ms at 48kHz).
        s.handle_event(tick(1500));
        assert_eq!(s.state(), ScannerState::Dwelling);
        let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
        assert_eq!(s.state(), ScannerState::Listening);
        assert!(commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(false))));
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
        s.handle_event(tick(1500));
        assert_eq!(s.state(), ScannerState::Dwelling);
        // Dwell is 100ms = 4800 samples at 48kHz. Tick past it.
        let commands = s.handle_event(tick(5000));
        assert_eq!(s.state(), ScannerState::Retuning);
        // Should have retuned to channel B (frequency 162_550_000).
        assert!(commands.iter().any(|c| matches!(
            c,
            ScannerCommand::Retune { freq_hz: 162_550_000, .. }
        )));
    }

    #[test]
    fn squelch_close_in_listening_enters_hanging_and_mutes() {
        let mut s = Scanner::new();
        s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
        s.handle_event(ScannerEvent::SetEnabled(true));
        s.handle_event(tick(1500));
        s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
        assert_eq!(s.state(), ScannerState::Listening);
        let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
        assert_eq!(s.state(), ScannerState::Hanging);
        assert!(commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(true))));
    }

    #[test]
    fn squelch_reopen_before_hang_end_returns_to_listening() {
        let mut s = Scanner::new();
        s.handle_event(ScannerEvent::ChannelsChanged(vec![ch("A", 146_520_000, 0)]));
        s.handle_event(ScannerEvent::SetEnabled(true));
        s.handle_event(tick(1500));
        s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
        s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
        assert_eq!(s.state(), ScannerState::Hanging);
        // Advance partway into hang (2000ms hang = 96000 samples).
        s.handle_event(tick(10_000));
        let commands = s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
        assert_eq!(s.state(), ScannerState::Listening);
        assert!(commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::MuteAudio(false))));
    }

    #[test]
    fn hang_elapsed_advances_to_next_channel() {
        let mut s = Scanner::new();
        s.handle_event(ScannerEvent::ChannelsChanged(vec![
            ch("A", 146_520_000, 0),
            ch("B", 162_550_000, 0),
        ]));
        s.handle_event(ScannerEvent::SetEnabled(true));
        s.handle_event(tick(1500));
        s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
        s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Closed));
        let commands = s.handle_event(tick(100_000));
        assert_eq!(s.state(), ScannerState::Retuning);
        assert!(commands.iter().any(|c| matches!(
            c,
            ScannerCommand::Retune { freq_hz: 162_550_000, .. }
        )));
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p sdr-scanner --lib`
Expected: 6 new tests FAIL — sample tick + squelch handlers are still stubs.

- [ ] **Step 3: Implement `handle_sample_tick`**

Replace the stub body:

```rust
    fn handle_sample_tick(
        &mut self,
        samples_consumed: u32,
        sample_rate_hz: u32,
    ) -> Vec<ScannerCommand> {
        let samples = u64::from(samples_consumed);
        let new_phase = match &mut self.phase {
            Phase::Idle | Phase::Listening { .. } => return Vec::new(),
            Phase::Retuning {
                target_idx,
                samples_until_settled,
            } => {
                if *samples_until_settled == 0 {
                    // First tick after retune — seed countdown.
                    *samples_until_settled = ms_to_samples(SETTLE_MS, sample_rate_hz);
                }
                *samples_until_settled = samples_until_settled.saturating_sub(samples);
                if *samples_until_settled == 0 {
                    let idx = *target_idx;
                    let dwell_ms = self.channels[idx].dwell_ms;
                    Some(Phase::Dwelling {
                        idx,
                        samples_until_timeout: ms_to_samples(dwell_ms, sample_rate_hz),
                    })
                } else {
                    None
                }
            }
            Phase::Dwelling {
                idx,
                samples_until_timeout,
            } => {
                *samples_until_timeout = samples_until_timeout.saturating_sub(samples);
                if *samples_until_timeout == 0 {
                    // Dwell elapsed silently — advance rotation.
                    let _ = idx;
                    Some(Phase::__AdvanceFromDwell)
                } else {
                    None
                }
            }
            Phase::Hanging {
                idx,
                samples_until_timeout,
            } => {
                *samples_until_timeout = samples_until_timeout.saturating_sub(samples);
                if *samples_until_timeout == 0 {
                    let _ = idx;
                    Some(Phase::__AdvanceFromHang)
                } else {
                    None
                }
            }
        };

        match new_phase {
            None => Vec::new(),
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
            Some(Phase::__AdvanceFromDwell) | Some(Phase::__AdvanceFromHang) => {
                self.hops_since_priority_sweep += 1;
                self.advance_rotation()
            }
            Some(_) => Vec::new(),
        }
    }

    fn advance_rotation(&mut self) -> Vec<ScannerCommand> {
        match self.pick_next_channel() {
            Some(idx) => self.enter_retuning(idx),
            None => {
                self.phase = Phase::Idle;
                vec![
                    ScannerCommand::EmptyRotation,
                    ScannerCommand::MuteAudio(false),
                    ScannerCommand::ActiveChannelChanged(None),
                    ScannerCommand::StateChanged(ScannerState::Idle),
                ]
            }
        }
    }
```

Also add the two synthetic `Phase` variants used as transition markers at the top of the `Phase` enum (these never appear in `self.phase`, they're just carriers):

```rust
enum Phase {
    // ... existing variants ...
    __AdvanceFromDwell,
    __AdvanceFromHang,
}
```

And add `__AdvanceFromDwell | __AdvanceFromHang` to the wildcard arm in `as_state`:

```rust
    fn as_state(&self) -> ScannerState {
        match self {
            // ... existing arms ...
            Phase::__AdvanceFromDwell | Phase::__AdvanceFromHang => {
                unreachable!("advance markers should never sit as the phase")
            }
        }
    }
```

- [ ] **Step 4: Implement `handle_squelch_edge`**

Replace stub body:

```rust
    fn handle_squelch_edge(&mut self, state: SquelchState) -> Vec<ScannerCommand> {
        match (&self.phase, state) {
            (Phase::Retuning { .. }, _) => {
                // Ignore edges during settle window.
                Vec::new()
            }
            (Phase::Dwelling { idx, .. }, SquelchState::Open) => {
                let idx = *idx;
                self.phase = Phase::Listening { idx };
                vec![
                    ScannerCommand::MuteAudio(false),
                    ScannerCommand::StateChanged(ScannerState::Listening),
                ]
            }
            (Phase::Listening { idx }, SquelchState::Closed) => {
                let idx = *idx;
                let hang_ms = self.channels[idx].hang_ms;
                // We have the rate from the most recent SampleTick,
                // but Hanging is sample-counted so we seed on first
                // tick after entering. Initial value 0 means "seed
                // on next tick" — matches Retuning's pattern.
                self.phase = Phase::Hanging {
                    idx,
                    samples_until_timeout: u64::MAX, // sentinel: seed on first tick
                };
                // Actually: since hang resume-from-Listening also
                // depends on ms, and we need the current rate, we
                // store the _ms_ and convert on first tick. Simpler:
                // add a Hanging field carrying the ms plus a seeded
                // flag. See Step 5 for the clean fix.
                let _ = hang_ms;
                vec![
                    ScannerCommand::MuteAudio(true),
                    ScannerCommand::StateChanged(ScannerState::Hanging),
                ]
            }
            (Phase::Hanging { idx, .. }, SquelchState::Open) => {
                let idx = *idx;
                self.phase = Phase::Listening { idx };
                vec![
                    ScannerCommand::MuteAudio(false),
                    ScannerCommand::StateChanged(ScannerState::Listening),
                ]
            }
            _ => Vec::new(),
        }
    }
```

- [ ] **Step 5: Fix the Hanging seeding**

The `u64::MAX` sentinel is ugly. Replace `Phase::Hanging` with a seed-on-first-tick variant shape:

```rust
enum Phase {
    // ...
    Hanging {
        idx: usize,
        /// None → seed on next SampleTick (we just entered Hanging
        /// from Listening and don't yet know the sample rate).
        /// Some(n) → samples remaining before rotation advances.
        samples_until_timeout: Option<u64>,
    },
}
```

Update the two match sites:
- In `handle_squelch_edge`, entering Hanging: `samples_until_timeout: None`.
- In `handle_sample_tick`, Hanging arm:

```rust
            Phase::Hanging {
                idx,
                samples_until_timeout,
            } => {
                let remaining = match samples_until_timeout {
                    None => {
                        let hang_ms = self.channels[*idx].hang_ms;
                        let seeded = ms_to_samples(hang_ms, sample_rate_hz)
                            .saturating_sub(samples);
                        *samples_until_timeout = Some(seeded);
                        seeded
                    }
                    Some(remaining) => {
                        *remaining = remaining.saturating_sub(samples);
                        *remaining
                    }
                };
                if remaining == 0 {
                    Some(Phase::__AdvanceFromHang)
                } else {
                    None
                }
            }
```

Apply the same `Option<u64>` treatment to `Phase::Retuning::samples_until_settled` for symmetry:

```rust
    Retuning {
        target_idx: usize,
        samples_until_settled: Option<u64>,
    },
```

And update its match in `handle_sample_tick`:

```rust
            Phase::Retuning {
                target_idx,
                samples_until_settled,
            } => {
                let remaining = match samples_until_settled {
                    None => {
                        let seeded = ms_to_samples(SETTLE_MS, sample_rate_hz)
                            .saturating_sub(samples);
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
                    let dwell_ms = self.channels[idx].dwell_ms;
                    Some(Phase::Dwelling {
                        idx,
                        samples_until_timeout: ms_to_samples(dwell_ms, sample_rate_hz),
                    })
                } else {
                    None
                }
            }
```

And update `enter_retuning` initializer from `samples_until_settled: 0` to `samples_until_settled: None`.

- [ ] **Step 6: Run all tests**

Run: `cargo test -p sdr-scanner --lib`
Expected: 9/9 tests PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "sdr-scanner: sample-tick countdown + squelch-edge state transitions"
```

---

### Task 1.7: Priority sweep, lockout, edge cases

**Files:**
- Modify: `crates/sdr-scanner/src/scanner.rs`

- [ ] **Step 1: Add priority sweep test**

Append to `mod tests`:

```rust
    #[test]
    fn priority_sweep_triggers_after_interval_hops() {
        let mut s = Scanner::new();
        s.handle_event(ScannerEvent::ChannelsChanged(vec![
            ch("A", 146_520_000, 0),
            ch("B", 162_550_000, 0),
            ch("P", 121_500_000, 1), // priority
        ]));
        s.handle_event(ScannerEvent::SetEnabled(true));

        // Burn through 5 normal hops. Each hop = Retuning→Dwelling→advance.
        // Need to settle (tick past 30ms), then timeout dwell (tick past 100ms).
        let mut retune_freqs: Vec<u64> = Vec::new();
        for _ in 0..6 {
            s.handle_event(tick(1500)); // settle
            let cmds = s.handle_event(tick(5000)); // dwell timeout → next retune
            for c in &cmds {
                if let ScannerCommand::Retune { freq_hz, .. } = c {
                    retune_freqs.push(*freq_hz);
                }
            }
        }
        // After 5 normal hops, the 6th should be the priority channel.
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
        s.handle_event(ScannerEvent::SetEnabled(true));
        // First retune should skip A and go to B.
        // Check the initial retune command emitted from SetEnabled.
        // We re-derive by inspecting state: the phase should be
        // Retuning targeting B (index 1).
        let mut s2 = Scanner::new();
        s2.handle_event(ScannerEvent::ChannelsChanged(vec![
            ch("A", 146_520_000, 0),
            ch("B", 162_550_000, 0),
        ]));
        s2.handle_event(ScannerEvent::LockoutChannel(ChannelKey {
            name: "A".to_string(),
            frequency_hz: 146_520_000,
        }));
        let commands = s2.handle_event(ScannerEvent::SetEnabled(true));
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
        assert!(commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::EmptyRotation)));
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
        s.handle_event(tick(1500));
        // Default dwell would be 100ms = 4800 samples. Channel
        // overrides to 500ms = 24000 samples. Tick 5000 — should
        // still be Dwelling (not advanced) because override kicks in.
        s.handle_event(tick(5000));
        assert_eq!(s.state(), ScannerState::Dwelling);
        // Tick past 500ms → advance.
        s.handle_event(tick(25_000));
        assert_eq!(s.state(), ScannerState::Retuning);
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p sdr-scanner --lib`
Expected: All 4 new tests PASS (the logic is already in place from Task 1.5/1.6). If one fails, inspect and adjust.

- [ ] **Step 3: Add edge-case test: channels changed during scanning**

```rust
    #[test]
    fn channels_changed_mid_scan_recovers() {
        let mut s = Scanner::new();
        s.handle_event(ScannerEvent::ChannelsChanged(vec![
            ch("A", 146_520_000, 0),
            ch("B", 162_550_000, 0),
        ]));
        s.handle_event(ScannerEvent::SetEnabled(true));
        s.handle_event(tick(1500));
        s.handle_event(ScannerEvent::SquelchEdge(SquelchState::Open));
        assert_eq!(s.state(), ScannerState::Listening);
        // User deletes channel B and adds C.
        let commands = s.handle_event(ScannerEvent::ChannelsChanged(vec![
            ch("A", 146_520_000, 0),
            ch("C", 28_400_000, 0),
        ]));
        // Scanner should recover — we chose to restart rotation at 0.
        assert_eq!(s.state(), ScannerState::Retuning);
        // First retune after list change goes to A.
        assert!(commands
            .iter()
            .any(|c| matches!(c, ScannerCommand::Retune { freq_hz: 146_520_000, .. })));
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
        s.handle_event(ScannerEvent::ChannelsChanged(vec![ch(
            "B",
            162_550_000,
            0,
        )]));
        // Internal set should have pruned.
        assert!(!s.locked_out.contains(&key_a));
    }
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p sdr-scanner --lib`
Expected: 13/13 tests PASS.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p sdr-scanner --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "sdr-scanner: priority sweep + lockout + edge-case tests"
```

---

### Task 1.8: Finalize PR 1 — open PR

- [ ] **Step 1: Verify workspace build**

Run: `cargo build --workspace`
Expected: Clean — the new crate slots in without breaking anything.

- [ ] **Step 2: Run workspace tests**

Run: `cargo test --workspace --features sdr-ui/whisper-cpu`
Expected: All existing tests still pass; new scanner tests pass.

- [ ] **Step 3: Rebase on latest main and push**

```bash
git fetch origin
git rebase origin/main
git push -u origin feature/scanner-engine
```

- [ ] **Step 4: Open PR**

```bash
gh pr create --title "feat(#317): scanner engine — sdr-scanner crate" --body "..."
```

Body should summarize: pure state machine, no I/O, zero integration. Lists the test coverage. References the design doc at `docs/superpowers/specs/2026-04-21-scanner-phase-1-design.md`. Notes that PR 2 wires it into DspController.

- [ ] **Step 5: Address CodeRabbit rounds as they come in**

Follow the same pattern from prior PRs — fix, push, reply to each inline comment.

- [ ] **Step 6: Merge and move to PR 2**

After merge, sync main locally: `git checkout main && git pull --ff-only && git branch -D feature/scanner-engine`.

---

## PR 2 — `DspController` integration + bookmark schema

**Branch:** `feature/scanner-integration`
**Scope:** Wire the scanner into `DspController`, extend `Bookmark` schema, add UiToDsp/DspToUi variants, implement mutex with recording + transcription. No UI panel yet — smoke-testable via debug logs. ~500 lines.

---

### Task 2.1: Extend `Bookmark` schema

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/navigation_panel.rs` (Bookmark struct + tests)

- [ ] **Step 1: Add the four new fields**

In `navigation_panel.rs`, extend the `Bookmark` struct inline after `voice_squelch_mode`:

```rust
    /// Include in scanner rotation. Default false so existing
    /// bookmarks don't start getting scanned without opt-in.
    #[serde(default)]
    pub scan_enabled: bool,
    /// Priority tier. 0 = normal, 1 = priority (checked more
    /// often). Higher tiers reserved for future phases.
    #[serde(default)]
    pub priority: u8,
    /// Per-channel dwell override in ms. None → scanner default.
    #[serde(default)]
    pub dwell_ms_override: Option<u32>,
    /// Per-channel hang override in ms. None → scanner default.
    #[serde(default)]
    pub hang_ms_override: Option<u32>,
```

Initialize them in `Bookmark::new` (the simple constructor) and `Bookmark::with_profile`:

```rust
    // ... existing inits ...
    scan_enabled: false,
    priority: 0,
    dwell_ms_override: None,
    hang_ms_override: None,
```

- [ ] **Step 2: Write a backward-compat serde roundtrip test**

In the `#[cfg(test)] mod tests` at bottom of `navigation_panel.rs`:

```rust
    #[test]
    fn bookmark_scanner_fields_default_on_old_json() {
        // Old pre-scanner bookmark JSON (no scanner fields present).
        let old_json = r#"{"name":"Old","frequency":162550000,"demod_mode":"NFM","bandwidth":12500.0}"#;
        let bm: Bookmark = serde_json::from_str(old_json).unwrap();
        assert!(!bm.scan_enabled);
        assert_eq!(bm.priority, 0);
        assert!(bm.dwell_ms_override.is_none());
        assert!(bm.hang_ms_override.is_none());
    }

    #[test]
    fn bookmark_scanner_fields_roundtrip() {
        let mut bm = Bookmark::new("Test", 146_520_000, DemodMode::Nfm, 12_500.0);
        bm.scan_enabled = true;
        bm.priority = 1;
        bm.dwell_ms_override = Some(200);
        bm.hang_ms_override = Some(3000);
        let json = serde_json::to_string(&bm).unwrap();
        let back: Bookmark = serde_json::from_str(&json).unwrap();
        assert!(back.scan_enabled);
        assert_eq!(back.priority, 1);
        assert_eq!(back.dwell_ms_override, Some(200));
        assert_eq!(back.hang_ms_override, Some(3000));
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p sdr-ui --features whisper-cpu bookmark_scanner`
Expected: Both PASS.

- [ ] **Step 4: Commit**

```bash
git checkout -b feature/scanner-integration
git add -A
git commit -m "bookmark: add scanner fields (#317)"
```

---

### Task 2.2: Add `UiToDsp` variants for scanner

**Files:**
- Modify: `crates/sdr-core/src/messages.rs`

- [ ] **Step 1: Identify insertion point**

Open `crates/sdr-core/src/messages.rs`. Locate `pub enum UiToDsp`. Add new variants before the closing brace (alphabetical isn't enforced elsewhere in this enum; group the scanner variants together).

- [ ] **Step 2: Add the variants**

```rust
    // --- Scanner ---
    /// Master scanner on/off toggle.
    SetScannerEnabled(bool),
    /// Replace the scanner's channel list. UI projects bookmarks
    /// with `scan_enabled = true` into `ScannerChannel`s.
    UpdateScannerChannels(Vec<sdr_scanner::ScannerChannel>),
    /// Session-scoped lockout.
    LockoutScannerChannel(sdr_scanner::ChannelKey),
    UnlockoutScannerChannel(sdr_scanner::ChannelKey),
    /// Global default timings (user moved the sidebar sliders).
    SetScannerDefaultDwellMs(u32),
    SetScannerDefaultHangMs(u32),
```

- [ ] **Step 3: Add `sdr-scanner` dependency on `sdr-core`**

In `crates/sdr-core/Cargo.toml` `[dependencies]`:

```toml
sdr-scanner.workspace = true
```

- [ ] **Step 4: Verify compile**

Run: `cargo build -p sdr-core`
Expected: Clean build.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "sdr-core: add scanner UiToDsp variants"
```

---

### Task 2.3: Add `DspToUi` variants for scanner

**Files:**
- Modify: `crates/sdr-core/src/messages.rs`

- [ ] **Step 1: Add variants near other DspToUi scanner-adjacent events**

In `pub enum DspToUi`:

```rust
    // --- Scanner (#317) ---
    ScannerActiveChannelChanged {
        key: Option<sdr_scanner::ChannelKey>,
        freq_hz: u64,
        demod_mode: sdr_types::DemodMode,
        bandwidth: f64,
        name: String,
    },
    ScannerStateChanged(sdr_scanner::ScannerState),
    /// Scanner emitted EmptyRotation — UI surfaces a toast.
    ScannerEmptyRotation,
    /// Scanner forced recording/transcription off (or vice versa).
    ScannerMutexStopped(ScannerMutexReason),
```

And define the reason enum in the same file:

```rust
#[derive(Debug, Clone, Copy)]
pub enum ScannerMutexReason {
    RecordingStoppedForScanner,
    TranscriptionStoppedForScanner,
    ScannerStoppedForRecording,
    ScannerStoppedForTranscription,
}
```

For the `ScannerActiveChannelChanged::key = None` case, the other fields should carry sensible defaults (0 freq, Nfm, 0 bw, empty name) — UI handler treats `key = None` as "scanner gone idle, clear display."

- [ ] **Step 2: Compile**

Run: `cargo build -p sdr-core`
Expected: Clean.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "sdr-core: add scanner DspToUi variants + mutex reason enum"
```

---

### Task 2.4: Wire scanner into `DspController`

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Add scanner field to the controller struct**

Near the other state fields in `DspController`:

```rust
    /// Scanner state machine. Fed sample ticks from the IQ loop
    /// and squelch edges from the existing transcription edge
    /// detector. Commands applied inline.
    scanner: sdr_scanner::Scanner,
```

Initialize in `DspController::new` (or equivalent constructor) with `sdr_scanner::Scanner::new()`.

- [ ] **Step 2: Handle the new UiToDsp variants**

In the controller's message dispatch (match over `UiToDsp`), add arms forwarding events into the scanner and applying commands. Create a helper `apply_scanner_commands(&mut self, commands: Vec<ScannerCommand>)` that translates each command:

```rust
    fn apply_scanner_commands(&mut self, commands: Vec<sdr_scanner::ScannerCommand>) {
        use sdr_scanner::ScannerCommand;
        for cmd in commands {
            match cmd {
                ScannerCommand::Retune {
                    freq_hz,
                    demod_mode,
                    bandwidth,
                    ctcss,
                    voice_squelch,
                } => {
                    // Apply to source + radio module directly (same
                    // path the Tune / SetBandwidth / SetDemodMode
                    // handlers use).
                    if let Some(source) = self.source.as_mut() {
                        let _ = source.set_center_freq(freq_hz);
                    }
                    self.radio_module.set_demod_mode(demod_mode);
                    self.radio_module.set_bandwidth(bandwidth);
                    if let Some(m) = ctcss {
                        self.radio_module.set_ctcss_mode(m);
                    }
                    if let Some(m) = voice_squelch {
                        self.radio_module.set_voice_squelch_mode(m);
                    }
                    // Emit ScannerActiveChannelChanged so UI syncs.
                    // Name resolved from current channel list; if
                    // scanner just advanced we need the key — pull
                    // from the ActiveChannelChanged emission
                    // elsewhere. Simplest: cache last emitted key
                    // on self and look up name when Retune fires.
                    // See Step 3 for the helper.
                }
                ScannerCommand::MuteAudio(muted) => {
                    self.sink_manager.set_scanner_mute(muted);
                }
                ScannerCommand::ActiveChannelChanged(key) => {
                    self.emit_scanner_active_channel(key);
                }
                ScannerCommand::StateChanged(state) => {
                    let _ = self.dsp_to_ui_tx.send(
                        DspToUi::ScannerStateChanged(state),
                    );
                }
                ScannerCommand::EmptyRotation => {
                    let _ = self
                        .dsp_to_ui_tx
                        .send(DspToUi::ScannerEmptyRotation);
                }
            }
        }
    }
```

- [ ] **Step 3: Write `emit_scanner_active_channel`**

Add as a private method on `DspController`:

```rust
    fn emit_scanner_active_channel(
        &self,
        key: Option<sdr_scanner::ChannelKey>,
    ) {
        // Resolve to full-channel info so the UI can drive its
        // frequency selector / spectrum / status bar without
        // needing its own copy of the scanner channel list.
        let channel = key.as_ref().and_then(|k| {
            self.scanner_channels
                .iter()
                .find(|c| c.key == *k)
                .cloned()
        });
        let msg = DspToUi::ScannerActiveChannelChanged {
            key: key.clone(),
            freq_hz: channel.as_ref().map_or(0, |c| c.frequency_hz),
            demod_mode: channel
                .as_ref()
                .map_or(sdr_types::DemodMode::Nfm, |c| c.demod_mode),
            bandwidth: channel.as_ref().map_or(0.0, |c| c.bandwidth),
            name: channel.map_or_else(String::new, |c| c.key.name),
        };
        let _ = self.dsp_to_ui_tx.send(msg);
    }
```

Also add a parallel field: `scanner_channels: Vec<sdr_scanner::ScannerChannel>` initialized empty, updated on `UpdateScannerChannels`:

```rust
    UiToDsp::UpdateScannerChannels(channels) => {
        self.scanner_channels = channels.clone();
        let cmds = self
            .scanner
            .handle_event(sdr_scanner::ScannerEvent::ChannelsChanged(channels));
        self.apply_scanner_commands(cmds);
    }
```

And arms for the rest:

```rust
    UiToDsp::SetScannerEnabled(enabled) => {
        self.handle_scanner_mutex(enabled);
        let cmds = self
            .scanner
            .handle_event(sdr_scanner::ScannerEvent::SetEnabled(enabled));
        self.apply_scanner_commands(cmds);
    }
    UiToDsp::LockoutScannerChannel(key) => {
        let cmds = self
            .scanner
            .handle_event(sdr_scanner::ScannerEvent::LockoutChannel(key));
        self.apply_scanner_commands(cmds);
    }
    UiToDsp::UnlockoutScannerChannel(key) => {
        let cmds = self
            .scanner
            .handle_event(sdr_scanner::ScannerEvent::UnlockoutChannel(key));
        self.apply_scanner_commands(cmds);
    }
    UiToDsp::SetScannerDefaultDwellMs(ms) => {
        let cmds = self
            .scanner
            .handle_event(sdr_scanner::ScannerEvent::SetDefaultDwellMs(ms));
        self.apply_scanner_commands(cmds);
    }
    UiToDsp::SetScannerDefaultHangMs(ms) => {
        let cmds = self
            .scanner
            .handle_event(sdr_scanner::ScannerEvent::SetDefaultHangMs(ms));
        self.apply_scanner_commands(cmds);
    }
```

- [ ] **Step 4: Feed SampleTick on every IQ block**

Locate the IQ-block processing loop (where `radio_module.process` is called). After that call, add:

```rust
    let tick_cmds = self.scanner.handle_event(
        sdr_scanner::ScannerEvent::SampleTick {
            samples_consumed: block_samples as u32,
            sample_rate_hz: source_sample_rate as u32,
        },
    );
    self.apply_scanner_commands(tick_cmds);
```

- [ ] **Step 5: Feed SquelchEdge alongside existing transcription edge emission**

Find the existing `SquelchOpened / SquelchClosed` emission site (line ~1846 of controller.rs per prior grep). Add scanner feed alongside:

```rust
    // Existing transcription emission:
    sdr_transcription::TranscriptionInput::SquelchOpened
    // New scanner feed:
    let scan_cmds = self.scanner.handle_event(
        sdr_scanner::ScannerEvent::SquelchEdge(
            sdr_scanner::SquelchState::Open,
        ),
    );
    self.apply_scanner_commands(scan_cmds);
```

Same pattern for the `SquelchClosed` branch.

- [ ] **Step 6: Compile**

Run: `cargo build -p sdr-core`
Expected: Clean.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "sdr-core: wire scanner into DspController"
```

---

### Task 2.5: Implement scanner ↔ recording / transcription mutex

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Add mutex enforcement in `handle_scanner_mutex`**

```rust
    fn handle_scanner_mutex(&mut self, scanner_becoming_active: bool) {
        if !scanner_becoming_active {
            return;
        }
        // Stop active recording.
        if self.recording_active {
            self.stop_recording(); // existing method
            let _ = self
                .dsp_to_ui_tx
                .send(DspToUi::ScannerMutexStopped(
                    ScannerMutexReason::RecordingStoppedForScanner,
                ));
        }
        // Stop active transcription.
        if self.transcription_active() {
            self.stop_transcription(); // existing method
            let _ = self
                .dsp_to_ui_tx
                .send(DspToUi::ScannerMutexStopped(
                    ScannerMutexReason::TranscriptionStoppedForScanner,
                ));
        }
    }
```

- [ ] **Step 2: Reject recording/transcription start while scanner is on**

In the existing `UiToDsp::StartRecording` / transcription start handlers, add a pre-check:

```rust
    UiToDsp::StartRecording { .. } => {
        if self.scanner.state() != sdr_scanner::ScannerState::Idle {
            // Scanner is active — stop it before starting recording.
            let cmds = self
                .scanner
                .handle_event(sdr_scanner::ScannerEvent::SetEnabled(false));
            self.apply_scanner_commands(cmds);
            let _ = self.dsp_to_ui_tx.send(DspToUi::ScannerMutexStopped(
                ScannerMutexReason::ScannerStoppedForRecording,
            ));
        }
        // ... existing recording start logic ...
    }
```

Similar for transcription start.

- [ ] **Step 3: Compile**

Run: `cargo build -p sdr-core`
Expected: Clean.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "sdr-core: scanner ↔ recording/transcription mutex"
```

---

### Task 2.6: Sink mute plumbing

**Files:**
- Modify: `crates/sdr-core/src/sink_manager.rs` (or wherever `SinkManager` lives)

- [ ] **Step 1: Add `scanner_mute: bool` + reusable silence scratch buffer**

```rust
pub struct SinkManager {
    // ... existing fields ...
    scanner_mute: bool,
    /// Reusable silence buffer grown-in-place during mute windows.
    /// `Vec` initialized empty and only resized on the mute path;
    /// capacity is sticky across calls so the steady-state has no
    /// allocation. Only touched on the audio thread via `&mut self`.
    scanner_silence_scratch: Vec<f32>,
}

impl SinkManager {
    pub fn set_scanner_mute(&mut self, muted: bool) {
        self.scanner_mute = muted;
    }
}
```

- [ ] **Step 2: Gate in the audio write path**

Wherever the manager writes PCM to the audio device, swap the buffer on mute. Do NOT allocate a fresh `vec![0.0; len]` per block — reuse the scratch buffer:

```rust
    pub fn write_audio(&mut self, audio: &[f32]) {
        let out = if self.scanner_mute {
            // Ensure capacity matches; `resize` reuses existing
            // allocation when the buffer is already large enough,
            // so steady-state mute is allocation-free.
            self.scanner_silence_scratch.resize(audio.len(), 0.0);
            &self.scanner_silence_scratch[..]
        } else {
            audio
        };
        // ... write `out` to current sink ...
    }
```

If there's a single hot path, hoist the branch out of the per-sample loop. Never allocate per block — real-time audio pipelines can't tolerate heap churn on every buffer.

- [ ] **Step 3: Compile**

Run: `cargo build --workspace`
Expected: Clean.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "sink-manager: scanner_mute gate at audio output"
```

---

### Task 2.7: Verify + open PR 2

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace --features sdr-ui/whisper-cpu`
Expected: All tests pass. Scanner crate tests (13) + bookmark roundtrip tests (2 new) + everything existing.

- [ ] **Step 2: Clippy check**

Run: `cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 3: Smoke-test via debug-build logs**

Add temporary `tracing::info!` calls in `apply_scanner_commands` if they're not already there. Run: `make install && sdr-rs`. Observe log stream while triggering scanner via a dev-only UiToDsp dispatch (no UI panel yet — can poke via a test hook or defer to PR 3 smoke).

If no easy way to trigger scanner without UI, document as a known constraint of PR 2 — actual end-to-end smoke happens in PR 3.

- [ ] **Step 4: Rebase + push + PR**

```bash
git fetch origin
git rebase origin/main
git push -u origin feature/scanner-integration
gh pr create --title "feat(#317): scanner ↔ DspController integration + bookmark schema" --body "..."
```

Body: references PR 1 (scanner engine), notes bookmark schema extension, mutex with recording/transcription, UI surface still deferred to PR 3.

- [ ] **Step 5: CR rounds → merge**

---

## PR 3 — UI surface

**Branch:** `feature/scanner-ui`
**Scope:** Sidebar scanner panel at the bottom of the left column, bookmark row Scan checkbox + Priority toggle, full UI sync wiring, mutex UI behavior, manual-tune-force-disables-scanner. User smoke test before merge. Closes #317.

---

### Task 3.1: Create scanner panel module

**Files:**
- Create: `crates/sdr-ui/src/sidebar/scanner_panel.rs`
- Modify: `crates/sdr-ui/src/sidebar/mod.rs`

- [ ] **Step 1: Write `scanner_panel.rs`**

```rust
//! Scanner control panel at the bottom of the left sidebar.
//!
//! Master switch, active-channel / state display, default
//! dwell/hang sliders (collapsed expander), and session lockout
//! button (visible only when scanner is on an active channel).
//! UI wiring of user actions → `UiToDsp::*` commands lives in
//! `window.rs::connect_scanner_panel`.

use gtk4::prelude::*;
use libadwaita as adw;

pub struct ScannerPanel {
    pub widget: gtk4::Box,
    pub master_switch: gtk4::Switch,
    pub active_channel_label: gtk4::Label,
    pub state_label: gtk4::Label,
    pub default_dwell_row: adw::SpinRow,
    pub default_hang_row: adw::SpinRow,
    pub lockout_button: gtk4::Button,
}

pub const DWELL_MIN_MS: f64 = 50.0;
pub const DWELL_MAX_MS: f64 = 500.0;
pub const HANG_MIN_MS: f64 = 500.0;
pub const HANG_MAX_MS: f64 = 5000.0;

pub const CONFIG_KEY_DEFAULT_DWELL_MS: &str = "scanner_default_dwell_ms";
pub const CONFIG_KEY_DEFAULT_HANG_MS: &str = "scanner_default_hang_ms";

#[must_use]
pub fn build_scanner_panel() -> ScannerPanel {
    let widget = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(8)
        .build();

    let heading = gtk4::Label::builder()
        .label("Scanner")
        .css_classes(["heading"])
        .halign(gtk4::Align::Start)
        .build();
    widget.append(&heading);

    // Master switch row.
    let switch_row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .build();
    let switch_label = gtk4::Label::builder()
        .label("Scanner")
        .hexpand(true)
        .halign(gtk4::Align::Start)
        .build();
    let master_switch = gtk4::Switch::builder().halign(gtk4::Align::End).build();
    switch_row.append(&switch_label);
    switch_row.append(&master_switch);
    widget.append(&switch_row);

    // Active channel label.
    let active_channel_label = gtk4::Label::builder()
        .label("Active: —")
        .halign(gtk4::Align::Start)
        .css_classes(["caption"])
        .build();
    widget.append(&active_channel_label);

    // State label.
    let state_label = gtk4::Label::builder()
        .label("State: Off")
        .halign(gtk4::Align::Start)
        .css_classes(["caption", "dim-label"])
        .build();
    widget.append(&state_label);

    // Lockout button (hidden until listening/hanging).
    let lockout_button = gtk4::Button::builder()
        .label("Lockout current channel")
        .css_classes(["destructive-action", "flat"])
        .visible(false)
        .build();
    widget.append(&lockout_button);

    // Settings expander.
    let expander = adw::ExpanderRow::builder().title("Settings").build();
    let default_dwell_row = adw::SpinRow::builder()
        .title("Default dwell (ms)")
        .adjustment(&gtk4::Adjustment::new(
            100.0,
            DWELL_MIN_MS,
            DWELL_MAX_MS,
            10.0,
            50.0,
            0.0,
        ))
        .digits(0)
        .build();
    let default_hang_row = adw::SpinRow::builder()
        .title("Default hang (ms)")
        .adjustment(&gtk4::Adjustment::new(
            2000.0,
            HANG_MIN_MS,
            HANG_MAX_MS,
            100.0,
            500.0,
            0.0,
        ))
        .digits(0)
        .build();
    expander.add_row(&default_dwell_row);
    expander.add_row(&default_hang_row);

    // Wrap expander in a ListBox for proper styling.
    let settings_group = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    settings_group.append(&expander);
    widget.append(&settings_group);

    ScannerPanel {
        widget,
        master_switch,
        active_channel_label,
        state_label,
        default_dwell_row,
        default_hang_row,
        lockout_button,
    }
}
```

- [ ] **Step 2: Register in `sidebar/mod.rs`**

Add `pub mod scanner_panel;` near the other `pub mod` lines, and `pub use scanner_panel::{ScannerPanel, build_scanner_panel};`.

Add field to `SidebarPanels`:

```rust
    /// Scanner control panel at bottom of left sidebar.
    pub scanner: ScannerPanel,
```

In `build_sidebar`, construct and append to `sidebar_box` after `display.widget`:

```rust
    let scanner = build_scanner_panel();
    sidebar_box.append(&scanner.widget);
```

And include in the returned struct.

- [ ] **Step 3: Compile**

Run: `cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 4: Commit**

```bash
git checkout -b feature/scanner-ui
git add -A
git commit -m "sdr-ui: scanner panel scaffolding"
```

---

### Task 3.2: Wire master switch + default sliders → UiToDsp

**Files:**
- Modify: `crates/sdr-ui/src/window.rs` (new `connect_scanner_panel` fn)

- [ ] **Step 1: Write `connect_scanner_panel`**

```rust
fn connect_scanner_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let scanner = &panels.scanner;

    // Master switch → SetScannerEnabled.
    let state_switch = Rc::clone(state);
    scanner.master_switch.connect_state_set(move |_, active| {
        state_switch.send_dsp(UiToDsp::SetScannerEnabled(active));
        glib::Propagation::Proceed
    });

    // Default dwell slider.
    let state_dwell = Rc::clone(state);
    let cfg_dwell = std::sync::Arc::clone(config);
    scanner
        .default_dwell_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = row.value() as u32;
            state_dwell.send_dsp(UiToDsp::SetScannerDefaultDwellMs(ms));
            cfg_dwell.write(|v| {
                v[sidebar::scanner_panel::CONFIG_KEY_DEFAULT_DWELL_MS] =
                    serde_json::json!(ms);
            });
        });

    // Default hang slider.
    let state_hang = Rc::clone(state);
    let cfg_hang = std::sync::Arc::clone(config);
    scanner
        .default_hang_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = row.value() as u32;
            state_hang.send_dsp(UiToDsp::SetScannerDefaultHangMs(ms));
            cfg_hang.write(|v| {
                v[sidebar::scanner_panel::CONFIG_KEY_DEFAULT_HANG_MS] =
                    serde_json::json!(ms);
            });
        });

    // Restore persisted defaults.
    let saved_dwell = config.read(|v| {
        v.get(sidebar::scanner_panel::CONFIG_KEY_DEFAULT_DWELL_MS)
            .and_then(serde_json::Value::as_u64)
            .map_or(100.0_f64, |v| v as f64)
    });
    scanner.default_dwell_row.set_value(saved_dwell);

    let saved_hang = config.read(|v| {
        v.get(sidebar::scanner_panel::CONFIG_KEY_DEFAULT_HANG_MS)
            .and_then(serde_json::Value::as_u64)
            .map_or(2000.0_f64, |v| v as f64)
    });
    scanner.default_hang_row.set_value(saved_hang);
}
```

- [ ] **Step 2: Call from `build_window`**

In the main wiring block:

```rust
    connect_scanner_panel(&panels, &state, config);
```

- [ ] **Step 3: Compile + lint**

Run: `cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "sdr-ui: scanner master switch + default sliders wired"
```

---

### Task 3.3: Handle `DspToUi::Scanner*` events → UI updates

**Files:**
- Modify: `crates/sdr-ui/src/window.rs` (event dispatch)

- [ ] **Step 1: Add arms to the DspToUi handler**

In the existing match over `DspToUi` events, add:

```rust
    DspToUi::ScannerActiveChannelChanged {
        key,
        freq_hz,
        demod_mode,
        bandwidth,
        name,
    } => {
        let scanner = &panels.scanner;
        if key.is_some() {
            scanner.active_channel_label.set_text(&format!(
                "Active: {} — {}",
                name,
                sidebar::navigation_panel::format_frequency(freq_hz),
            ));
            // Sync all existing UI surfaces to the new tune.
            freq_selector.set_frequency(freq_hz);
            #[allow(clippy::cast_precision_loss)]
            spectrum_handle.set_center_frequency(freq_hz as f64);
            #[allow(clippy::cast_precision_loss)]
            status_bar_demod.update_frequency(freq_hz as f64);
            let label = header::demod_mode_label(demod_mode);
            status_bar_demod.update_demod(label, bandwidth);
            // Demod dropdown + bandwidth row updates (see Task 3.4
            // for the suppress-notify guard pattern).
            state.suppress_demod_notify.set(true);
            if let Some(idx) =
                header::demod_selector::demod_mode_to_index(demod_mode)
            {
                demod_dropdown.set_selected(idx);
            }
            state.suppress_demod_notify.set(false);
            state.suppress_bandwidth_notify.set(true);
            panels.radio.bandwidth_row.set_value(bandwidth);
            state.suppress_bandwidth_notify.set(false);
            // Show lockout button (hidden while Idle/Retuning).
            panels.scanner.lockout_button.set_visible(true);
        } else {
            scanner.active_channel_label.set_text("Active: —");
            panels.scanner.lockout_button.set_visible(false);
        }
    }
    DspToUi::ScannerStateChanged(scanner_state) => {
        let label = match scanner_state {
            sdr_scanner::ScannerState::Idle => "Off",
            sdr_scanner::ScannerState::Retuning => "Scanning…",
            sdr_scanner::ScannerState::Dwelling => "Listening…",
            sdr_scanner::ScannerState::Listening => "Listening",
            sdr_scanner::ScannerState::Hanging => "Hang…",
        };
        panels
            .scanner
            .state_label
            .set_text(&format!("State: {}", label));
    }
    DspToUi::ScannerEmptyRotation => {
        toast_overlay.add_toast(
            adw::Toast::builder()
                .title("Scanner has no active channels")
                .timeout(3)
                .build(),
        );
        panels.scanner.master_switch.set_state(false);
    }
    DspToUi::ScannerMutexStopped(reason) => {
        let msg = match reason {
            sdr_core::messages::ScannerMutexReason::RecordingStoppedForScanner =>
                "Recording stopped — scanner activated",
            sdr_core::messages::ScannerMutexReason::TranscriptionStoppedForScanner =>
                "Transcription stopped — scanner activated",
            sdr_core::messages::ScannerMutexReason::ScannerStoppedForRecording =>
                "Scanner stopped — recording started",
            sdr_core::messages::ScannerMutexReason::ScannerStoppedForTranscription =>
                "Scanner stopped — transcription started",
        };
        toast_overlay.add_toast(
            adw::Toast::builder().title(msg).timeout(3).build(),
        );
    }
```

- [ ] **Step 2: Add `suppress_demod_notify: Cell<bool>` to `AppState`**

In `crates/sdr-ui/src/state.rs`:

```rust
    /// Mirror of `suppress_bandwidth_notify` for the demod dropdown.
    /// Set true when we're programmatically changing the selected
    /// demod mode so the dropdown's `connect_selected_notify`
    /// doesn't bounce a `SetDemodMode` command back to DSP.
    pub suppress_demod_notify: Cell<bool>,
```

Initialize to `Cell::new(false)` in constructor. Add a guard check in `demod_dropdown.connect_selected_notify` at wiring time:

```rust
    dd.connect_selected_notify(move |row| {
        if state.suppress_demod_notify.get() {
            return;
        }
        // ... existing dispatch logic ...
    });
```

- [ ] **Step 3: Compile**

Run: `cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "sdr-ui: handle scanner DspToUi events + sync UI surfaces"
```

---

### Task 3.4: Bookmark row Scan + Priority toggles

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/navigation_panel.rs` (row builder)

- [ ] **Step 1: Extend `build_bookmark_row` with two new suffix widgets**

In `build_bookmark_row` (after the existing `delete_btn` append):

```rust
    // Scan checkbox — binds to bookmark.scan_enabled.
    let scan_check = gtk4::CheckButton::builder()
        .tooltip_text("Include in scanner")
        .valign(gtk4::Align::Center)
        .active(bm.scan_enabled)
        .build();
    scan_check.update_property(&[gtk4::accessible::Property::Label(
        "Include in scanner",
    )]);
    let bm_rc_scan = std::rc::Rc::clone(bookmarks);
    let entry_scan = name_entry.clone();
    let sig_name = bm.name.clone();
    let sig_freq = bm.frequency;
    let state_scan = state.clone();
    scan_check.connect_toggled(move |btn| {
        let enabled = btn.is_active();
        let mut bms = bm_rc_scan.borrow_mut();
        if let Some(b) = bms
            .iter_mut()
            .find(|b| b.name == sig_name && b.frequency == sig_freq)
        {
            b.scan_enabled = enabled;
        }
        save_bookmarks(&bms);
        // Push the new channel list to the scanner.
        let channels = project_scanner_channels(&bms);
        drop(bms);
        state_scan.send_dsp(UiToDsp::UpdateScannerChannels(channels));
    });
    row.add_suffix(&scan_check);

    // Priority star — binds to bookmark.priority (0 or 1 in Phase 1).
    let pri_btn = gtk4::ToggleButton::builder()
        .icon_name(if bm.priority >= 1 {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        })
        .tooltip_text("Scanner priority channel")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .active(bm.priority >= 1)
        .build();
    pri_btn.update_property(&[gtk4::accessible::Property::Label(
        "Scanner priority channel",
    )]);
    let bm_rc_pri = std::rc::Rc::clone(bookmarks);
    let pri_name = bm.name.clone();
    let pri_freq = bm.frequency;
    let state_pri = state.clone();
    pri_btn.connect_toggled(move |btn| {
        let priority = if btn.is_active() { 1 } else { 0 };
        btn.set_icon_name(if btn.is_active() {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        let mut bms = bm_rc_pri.borrow_mut();
        if let Some(b) = bms
            .iter_mut()
            .find(|b| b.name == pri_name && b.frequency == pri_freq)
        {
            b.priority = priority;
        }
        save_bookmarks(&bms);
        let channels = project_scanner_channels(&bms);
        drop(bms);
        state_pri.send_dsp(UiToDsp::UpdateScannerChannels(channels));
    });
    row.add_suffix(&pri_btn);
```

Update `build_bookmark_row`'s signature to take `state: &Rc<AppState>` so the toggles can dispatch. Threads through `rebuild_bookmark_list` as another parameter. Update all call sites.

- [ ] **Step 2: Write `project_scanner_channels` in navigation_panel.rs**

```rust
/// Project the bookmark list into `ScannerChannel`s. Only
/// includes bookmarks with `scan_enabled = true`. Override
/// fields are folded against the scanner defaults at projection
/// time so the scanner state machine doesn't need to know about
/// Options.
pub fn project_scanner_channels(
    bookmarks: &[Bookmark],
) -> Vec<sdr_scanner::ScannerChannel> {
    bookmarks
        .iter()
        .filter(|b| b.scan_enabled)
        .map(|b| sdr_scanner::ScannerChannel {
            key: sdr_scanner::ChannelKey {
                name: b.name.clone(),
                frequency_hz: b.frequency,
            },
            frequency_hz: b.frequency,
            demod_mode: parse_demod_mode(&b.demod_mode),
            bandwidth: b.bandwidth,
            ctcss: b.ctcss_mode.clone(),
            voice_squelch: b.voice_squelch_mode,
            priority: b.priority,
            dwell_ms: b
                .dwell_ms_override
                .unwrap_or(sdr_scanner::DEFAULT_DWELL_MS),
            hang_ms: b
                .hang_ms_override
                .unwrap_or(sdr_scanner::DEFAULT_HANG_MS),
        })
        .collect()
}
```

- [ ] **Step 3: Also push `UpdateScannerChannels` on Add / Delete / RR import paths**

Locate the add-bookmark click handler, delete-button click handler (inside `build_bookmark_row`), and the RR browse post-import callback. After each `save_bookmarks` call, add:

```rust
    let channels = project_scanner_channels(&bm_rc.borrow());
    state_clone.send_dsp(UiToDsp::UpdateScannerChannels(channels));
```

- [ ] **Step 4: Compile**

Run: `cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "sdr-ui: bookmark row scan checkbox + priority toggle"
```

---

### Task 3.5: Lockout button + manual-tune-force-disables-scanner

**Files:**
- Modify: `crates/sdr-ui/src/window.rs` (lockout click + manual tune hook)

- [ ] **Step 1: Wire lockout button**

In `connect_scanner_panel`:

```rust
    // Lockout button. The active channel's key is captured via
    // the most recent ScannerActiveChannelChanged — stash it on
    // AppState for the click handler to read.
    let state_lockout = Rc::clone(state);
    scanner.lockout_button.connect_clicked(move |_| {
        if let Some(key) = state_lockout.scanner_active_key.borrow().clone() {
            state_lockout.send_dsp(UiToDsp::LockoutScannerChannel(key));
        }
    });
```

- [ ] **Step 2: Add `scanner_active_key` field to `AppState`**

```rust
    /// Scanner's currently-active channel key (if any). Updated
    /// on every `ScannerActiveChannelChanged` event. Read by the
    /// lockout button to know what to lock out.
    pub scanner_active_key: RefCell<Option<sdr_scanner::ChannelKey>>,
```

Update the `ScannerActiveChannelChanged` handler from Task 3.3 to write this field:

```rust
    *state.scanner_active_key.borrow_mut() = key.clone();
```

- [ ] **Step 3: Force-disable scanner on manual tune**

Locate the `frequency_selector.connect_frequency_changed` handler (or whatever signal fires on manual tune). Add a pre-dispatch:

```rust
    if panels.scanner.master_switch.state() {
        panels.scanner.master_switch.set_state(false);
        toast_overlay.add_toast(
            adw::Toast::builder()
                .title("Scanner stopped — manual tune")
                .timeout(3)
                .build(),
        );
    }
```

Same pattern for demod dropdown changes, bandwidth changes, and preset selection.

- [ ] **Step 4: Compile**

Run: `cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "sdr-ui: lockout button + manual-tune force-disables scanner"
```

---

### Task 3.6: Initial channel list push + keyboard shortcut

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`
- Modify: `crates/sdr-ui/src/shortcuts.rs`

- [ ] **Step 1: Push scanner channels on app startup**

In `build_window` after the panels are wired up, push the initial channel list to the scanner:

```rust
    // Seed the scanner with the persisted bookmark list — scanner
    // starts Idle so no retune happens, but the channels are in
    // place if the user flips the switch on.
    let initial_channels =
        sidebar::navigation_panel::project_scanner_channels(
            &panels.bookmarks.bookmarks.borrow(),
        );
    state.send_dsp(UiToDsp::UpdateScannerChannels(initial_channels));
```

- [ ] **Step 2: Add F8 shortcut to toggle scanner**

In `crates/sdr-ui/src/shortcuts.rs` `setup_shortcuts`, add a new block mirroring the F9 sidebar-toggle block:

```rust
    // F8: Toggle scanner master switch.
    let scanner_switch_weak = scanner_switch.downgrade();
    let trigger_f8 = gtk4::ShortcutTrigger::parse_string("F8");
    if let Some(trigger) = trigger_f8 {
        let action = gtk4::CallbackAction::new(move |_widget, _args| {
            if let Some(sw) = scanner_switch_weak.upgrade() {
                sw.set_state(!sw.state());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
        controller.add_shortcut(shortcut);
    }
```

Update `setup_shortcuts`'s signature to take `scanner_switch: &gtk4::Switch`.

Update the `SHORTCUT_CATALOG` to include the new shortcut:

```rust
    (
        "Navigation",
        &[
            ("F9", "Toggle sidebar"),
            ("Ctrl+B", "Toggle bookmarks panel"),
            ("F8", "Toggle scanner"),
        ],
    ),
```

Update the call site in `window.rs`:

```rust
    shortcuts::setup_shortcuts(
        &window,
        &play_button,
        &sidebar_toggle,
        &bookmarks_toggle,
        &panels.scanner.master_switch,
        &demod_dropdown,
    );
```

- [ ] **Step 3: Compile + lint**

Run: `cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings`
Expected: Clean.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "sdr-ui: initial channel seed + F8 scanner shortcut"
```

---

### Task 3.7: Final build, smoke test, open PR

- [ ] **Step 1: Full workspace build + tests**

Run:
```bash
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings
cargo test --workspace --features sdr-ui/whisper-cpu
```
Expected: All clean.

- [ ] **Step 2: Install and hand off to user for smoke test**

```bash
make install CARGO_FLAGS="--release --features whisper-cuda"
```

Give the user the smoke test checklist:
- Enable scan on 5+ bookmarks (NFM mix across bands).
- Mark 1-2 as priority.
- Flip scanner switch on.
- Verify: frequency selector + spectrum flick through channels; state label cycles Scanning / Listening / Hanging; audio silent during retune, active during listening; priority channels get checked more often.
- Lockout current channel during Listening; verify it's skipped next cycle.
- Try to start recording while scanner on → verify scanner stops with toast.
- Try to start transcription while scanner on → same.
- Manually tune via spectrum click → verify scanner stops with toast.
- F8 toggles scanner.
- Persistence: set default dwell to 200ms, restart, verify slider shows 200.

- [ ] **Step 3: Rebase + push + PR**

```bash
git fetch origin
git rebase origin/main
git push -u origin feature/scanner-ui
gh pr create --title "feat(#317): scanner UI + closes scanner Phase 1" --body "..."
```

Body: Closes #317. References PR 1 + PR 2. Lists smoke-test checklist, notes what's in scope vs. what's deferred to follow-up issues.

- [ ] **Step 4: CR rounds → merge**

---

## After all three PRs merge

- File follow-up tickets (per spec "Out of scope" + "Follow-up issues" sections):
  - Per-hit recording
  - Per-hit transcription log
  - Band / category scanning
  - Mac-side SwiftUI scanner

- Update `memory/project_current_state.md` with a new "Scanner Phase 1 — shipped" section summarizing the three PRs, mutex behavior, UI surface, deferred follow-ups. Mirror the format used for #361, #359, etc.

- If priority-interrupt-during-listening (#365) becomes relevant, re-read the state machine and plan it as an extension.
