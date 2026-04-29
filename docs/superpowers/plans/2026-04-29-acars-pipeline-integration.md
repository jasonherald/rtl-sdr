# ACARS Pipeline Integration + Airband Lock Implementation Plan (sub-project 2 of epic #474)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the shipped `sdr-acars::ChannelBank` into the live SDR pipeline so flipping a `SetAcarsEnabled(true)` UI command engages an "airband lock" (forces source rate to 2.5 MSps, center to 130.3375 MHz, IqFrontend decimation to 1, snapshots prior config), instantiates a `ChannelBank`, taps post-IqFrontend IQ at source rate to feed it, and emits decoded `AcarsMessage`s + per-channel `ChannelStats` back to the UI thread. Toggle off restores the snapshot. No UI rendering work — sub-project 3 ships the Aviation activity panel + viewer window. End state: the controller round-trips ACARS messages headlessly and AppState holds them in a bounded ring + counter.

**Architecture:** Mirror the existing `lrpt_decode_tap` pattern in `crates/sdr-core/src/controller.rs` — separate-parameter init/process function (`&mut Option<ChannelBank>`, `&[Complex]`, `&mut bool` for one-shot init guard) that the borrow checker can keep disjoint from a borrow on `state.processed_buf`. Tap point is **post-IqFrontend, pre-VFO** (different from LRPT which is post-VFO at 144 ksps): with decim=1 forced, `state.processed_buf[..processed_count]` is the raw 2.5 MSps source IQ. Lifecycle owned by `DspState`: instantiating the bank on `SetAcarsEnabled(true)`, dropping it on toggle-off / source-stop / source-type change. All wire-up state lives in `DspState`; the airband-lock snapshot/restore math is extracted into a pure `acars_airband_lock` module so we can TDD it without spinning up a controller.

**Tech Stack:** Rust 2024, `sdr-acars::{ChannelBank, AcarsMessage, ChannelStats}` (sub-project 1 API), `sdr-types::Complex`, `sdr-config::ConfigManager`, `thiserror` for the new `AcarsEnableError`, `tracing` for structured logs (no `println!`). All edits stay in `sdr-core`, `sdr-ui`, and a thin `sdr-types` addition for shared message variants. No new crates.

---

## File structure

| Path | Responsibility |
|---|---|
| `crates/sdr-core/src/acars_airband_lock.rs` | **NEW.** `PreLockSnapshot` struct + pure `engage` / `disengage` functions (compute the deltas; don't apply them). `AcarsEnableError` enum. Fully unit-testable without a controller. |
| `crates/sdr-core/src/controller.rs` | **MODIFIED.** New fields in `DspState` (`acars_bank`, `acars_pre_lock`, `acars_init_failed`, `acars_stats_emitted_at`). New `acars_decode_tap` function (mirrors `lrpt_decode_tap` shape). New match arm for `UiToDsp::SetAcarsEnabled`. New tap call inside `process_iq_block` between `frontend.process` and the VFO branch. Source-type-change detection that auto-disables ACARS. |
| `crates/sdr-core/src/messages.rs` | **MODIFIED.** Add `UiToDsp::SetAcarsEnabled(bool)`, `DspToUi::AcarsMessage(Box<AcarsMessage>)`, `DspToUi::AcarsChannelStats(Box<[ChannelStats; 6]>)`, `DspToUi::AcarsEnabledChanged(Result<bool, AcarsEnableError>)`. |
| `crates/sdr-core/src/lib.rs` | **MODIFIED.** Re-export the new `acars_airband_lock` module (gives external test access). |
| `crates/sdr-core/Cargo.toml` | **MODIFIED.** Add `sdr-acars = { path = "../sdr-acars" }`. |
| `crates/sdr-core/tests/acars_pipeline_integration.rs` | **NEW.** Headless harness: build a fixture `DspState`, dispatch `UiToDsp::SetAcarsEnabled(true)`, feed synthetic IQ through `process_iq_block`, assert `DspToUi::AcarsMessage` arrives + `acars_bank` lifecycle is correct. |
| `crates/sdr-ui/src/state.rs` | **MODIFIED.** New AppState fields: `acars_enabled`, `acars_recent`, `acars_total_count`, `acars_channel_stats`, `acars_pre_lock_state`. (Defer `acars_viewer_window` to sub-project 3.) |
| `crates/sdr-ui/src/dsp_messages.rs` (or wherever DspToUi is dispatched on the UI side — confirm in Task 0) | **MODIFIED.** New match arms for the three new `DspToUi` variants — push `AcarsMessage` into the bounded ring, increment counter, store stats, log toast on `AcarsEnabledChanged(Err)`. |
| `crates/sdr-ui/src/sidebar/acars_panel.rs` | **NEW (config-only stub).** Holds `KEY_ACARS_ENABLED`, `KEY_ACARS_CHANNEL_SET`, `KEY_ACARS_RECENT_KEEP_COUNT` constants + `read_acars_enabled` / `save_acars_enabled` helpers. **No GTK widgets in this sub-project** — the Aviation activity panel is sub-project 3. We just need the config keys defined so startup persistence works. |
| `crates/sdr-ui/src/app.rs` (or wherever the post-DSP-ready UiToDsp dispatch lives — confirm in Task 0) | **MODIFIED.** On startup, after the DSP thread is ready, read `acars_enabled` and dispatch `SetAcarsEnabled(true)` if set. On `AcarsEnabledChanged(Err)`, clear the persisted flag. |

---

## Constants (locked across the plan — do NOT redefine in any task)

```rust
// In crates/sdr-core/src/acars_airband_lock.rs
pub const ACARS_SOURCE_RATE_HZ: f64 = 2_500_000.0;
pub const ACARS_CENTER_HZ: f64 = 130_337_500.0;
pub const ACARS_FRONTEND_DECIM: u32 = 1;
pub const ACARS_RECENT_DEFAULT_KEEP: u32 = 500;

/// US-6: the canonical six-channel airband list (Hz). Spec
/// `acars_channel_set` enum has only this value in v1.
pub const US_SIX_CHANNELS_HZ: [f64; 6] = [
    131_550_000.0,
    131_525_000.0,
    130_025_000.0,
    130_425_000.0,
    130_450_000.0,
    129_125_000.0,
];

/// Minimum interval between `DspToUi::AcarsChannelStats`
/// emissions. Spec calls out ~1 Hz cadence so the stats
/// ring doesn't flood the channel.
pub const ACARS_STATS_EMIT_INTERVAL_MS: u64 = 1_000;
```

---

## Task 0: Branch verification + UI dispatch site discovery

**Files:** none (sanity check)

- [ ] **Step 1: Confirm we're on the right branch with main merged**

```bash
git rev-parse --abbrev-ref HEAD
# Expected: feat/acars-pipeline-integration

git log --oneline main -1
# Expected: 377107e Merge pull request #583 ... (sub-project 1 merge)

git log --oneline -3
# Should be the same — fresh branch off main.
```

- [ ] **Step 2: Confirm sub-project 1 API is reachable**

```bash
grep -E "^pub (fn|struct|enum)" crates/sdr-acars/src/lib.rs crates/sdr-acars/src/channel.rs
# Must show: ChannelBank, AcarsMessage, ChannelStats, ChannelLockState
```

- [ ] **Step 3: Locate the UI-side DspToUi dispatcher**

```bash
grep -rn "DspToUi::FftData\|match.*DspToUi" crates/sdr-ui/src/ | head -10
```

Record the file path of the file whose `match cmd` arm currently handles `DspToUi::FftData` — this is where the new `AcarsMessage` / `AcarsChannelStats` / `AcarsEnabledChanged` arms must be added in Task 11. Likely `crates/sdr-ui/src/dsp_messages.rs` or `crates/sdr-ui/src/app.rs`. Note the file path and line range.

- [ ] **Step 4: Locate the UI-side post-DSP-ready dispatch site**

```bash
grep -rn "UiToDsp::Start\|ui_to_dsp.*send\|send.*UiToDsp" crates/sdr-ui/src/ | head -10
```

Record the file path of the function called once after the DSP thread finishes initialization — this is where Task 12 dispatches `SetAcarsEnabled(true)` if the config flag is set.

If either site is unclear, stop and ask the user before proceeding. The plan's later tasks reference these exact paths.

---

## Task 1: New module `acars_airband_lock` — types + error enum

**Files:**
- Create: `crates/sdr-core/src/acars_airband_lock.rs`
- Modify: `crates/sdr-core/src/lib.rs` (one new `pub mod acars_airband_lock;` line)

- [ ] **Step 1: Add the module declaration to `lib.rs`**

Open `crates/sdr-core/src/lib.rs`. Find the existing `pub mod ...;` block (near the top). Add a new line in alphabetical position:

```rust
pub mod acars_airband_lock;
```

- [ ] **Step 2: Create the new module with constants + types**

Create `crates/sdr-core/src/acars_airband_lock.rs`:

```rust
//! Airband-lock state machine for ACARS reception.
//!
//! ACARS sub-project 2 (epic #474). When `SetAcarsEnabled(true)`
//! arrives, the controller snapshots the prior source config and
//! forces airband geometry (2.5 MSps, 130.3375 MHz center,
//! IqFrontend decimation = 1). Toggle off restores the snapshot.
//!
//! This module is pure (no controller, no I/O, no GTK). It
//! reports what should change; the controller applies the
//! changes to its `DspState`. That split lets us TDD the
//! engage/disengage math without spinning up the full DSP
//! thread.

use crate::messages::SourceType;
use thiserror::Error;

/// Locked source rate when ACARS is on. Spec section
/// "Airband-lock mechanism".
pub const ACARS_SOURCE_RATE_HZ: f64 = 2_500_000.0;

/// Locked source center frequency when ACARS is on. Midpoint
/// of the US-6 cluster (129.125–131.550 MHz).
pub const ACARS_CENTER_HZ: f64 = 130_337_500.0;

/// IqFrontend decimation when ACARS is on. Forces the
/// post-frontend buffer to carry the full source rate so
/// the ACARS tap reads 2.5 MSps IQ unchanged.
pub const ACARS_FRONTEND_DECIM: u32 = 1;

/// Default ring-buffer cap for the recent-message AppState
/// ring. Spec config key `acars_recent_keep_count`.
pub const ACARS_RECENT_DEFAULT_KEEP: u32 = 500;

/// US-6 channel set (Hz). The only `acars_channel_set` value
/// supported in v1.
pub const US_SIX_CHANNELS_HZ: [f64; 6] = [
    131_550_000.0,
    131_525_000.0,
    130_025_000.0,
    130_425_000.0,
    130_450_000.0,
    129_125_000.0,
];

/// Minimum interval between `DspToUi::AcarsChannelStats`
/// emissions. Spec calls out ~1 Hz cadence.
pub const ACARS_STATS_EMIT_INTERVAL_MS: u64 = 1_000;

/// Pre-lock config snapshot. Captured on `SetAcarsEnabled(true)`,
/// applied verbatim on `SetAcarsEnabled(false)` to restore the
/// user's prior tuning.
#[derive(Clone, Debug, PartialEq)]
pub struct PreLockSnapshot {
    /// Source sample rate before the lock engaged (Hz).
    pub source_rate_hz: f64,
    /// Source center frequency before the lock engaged (Hz).
    pub center_freq_hz: f64,
    /// VFO offset (relative to center) before the lock (Hz).
    pub vfo_offset_hz: f64,
    /// Source type at the moment of engage. Used by
    /// source-type-change auto-disable to verify the user
    /// is restoring to the same kind of source.
    pub source_type: SourceType,
    /// Frontend decimation ratio prior to engage. Restored
    /// verbatim on disengage; the controller's auto-decim
    /// logic re-derives a fresh value if the user toggles
    /// the demod mode after disengage.
    pub frontend_decim: u32,
}

/// Failure modes for `SetAcarsEnabled(true)`. Sent back to
/// the UI inside `DspToUi::AcarsEnabledChanged(Err(...))`.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum AcarsEnableError {
    /// Active source isn't an RTL-SDR (or rtl_tcp) — ACARS
    /// is dongle-only in v1. Spec section "Source-type gate".
    #[error("ACARS reception requires an RTL-SDR source (current: {0:?})")]
    UnsupportedSourceType(SourceType),

    /// `ChannelBank::new` rejected the channel list. Wraps
    /// the lower-layer error message so the UI can surface
    /// it to the user.
    #[error("ChannelBank construction failed: {0}")]
    ChannelBankInit(String),

    /// Source backend rejected `set_sample_rate` or `tune`
    /// while engaging the lock.
    #[error("source rejected airband-lock retune: {0}")]
    SourceRetuneFailed(String),

    /// Frontend rejected the forced decimation factor.
    #[error("frontend rejected decim={ACARS_FRONTEND_DECIM}: {0}")]
    FrontendDecimFailed(String),
}
```

- [ ] **Step 3: Build to confirm types compile**

```bash
cargo build -p sdr-core 2>&1 | tail -5
```

Expected: clean build, no errors. (No tests yet — we add those in Task 2.)

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-core/src/acars_airband_lock.rs crates/sdr-core/src/lib.rs
git commit -m "feat(sdr-core): scaffold acars_airband_lock module + AcarsEnableError"
```

---

## Task 2: Pure `engage` / `disengage` functions (TDD)

**Files:**
- Modify: `crates/sdr-core/src/acars_airband_lock.rs`

The two functions are deliberately pure: they take the *current* source config, return what the controller should *do*. The controller's `DspState` mutation logic lives in Task 7; this task just gets the math right.

- [ ] **Step 1: Write failing tests at the bottom of `acars_airband_lock.rs`**

Append to `crates/sdr-core/src/acars_airband_lock.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn rtl_state() -> CurrentSourceState {
        CurrentSourceState {
            source_rate_hz: 1_024_000.0,
            center_freq_hz: 162_550_000.0,
            vfo_offset_hz: -25_000.0,
            source_type: SourceType::RtlSdr,
            frontend_decim: 4,
        }
    }

    #[test]
    fn engage_snapshots_and_emits_target_geometry() {
        let plan = engage(&rtl_state()).expect("RTL-SDR engage should succeed");
        assert_eq!(plan.target_source_rate_hz, ACARS_SOURCE_RATE_HZ);
        assert_eq!(plan.target_center_hz, ACARS_CENTER_HZ);
        assert_eq!(plan.target_frontend_decim, ACARS_FRONTEND_DECIM);
        assert_eq!(plan.snapshot.source_rate_hz, 1_024_000.0);
        assert_eq!(plan.snapshot.center_freq_hz, 162_550_000.0);
        assert_eq!(plan.snapshot.vfo_offset_hz, -25_000.0);
        assert_eq!(plan.snapshot.source_type, SourceType::RtlSdr);
        assert_eq!(plan.snapshot.frontend_decim, 4);
    }

    #[test]
    fn engage_rejects_non_rtl_sources() {
        for bad in [SourceType::Network, SourceType::File, SourceType::RtlTcp] {
            let mut state = rtl_state();
            state.source_type = bad;
            match engage(&state) {
                Err(AcarsEnableError::UnsupportedSourceType(t)) => assert_eq!(t, bad),
                other => panic!("source={bad:?} expected UnsupportedSourceType, got {other:?}"),
            }
        }
    }

    #[test]
    fn disengage_returns_snapshotted_geometry_verbatim() {
        let plan = engage(&rtl_state()).unwrap();
        let restore = disengage(&plan.snapshot);
        assert_eq!(restore.target_source_rate_hz, 1_024_000.0);
        assert_eq!(restore.target_center_hz, 162_550_000.0);
        assert_eq!(restore.target_frontend_decim, 4);
        assert_eq!(restore.target_vfo_offset_hz, -25_000.0);
    }

    #[test]
    fn engage_then_disengage_is_a_round_trip() {
        let original = rtl_state();
        let plan = engage(&original).unwrap();
        let restore = disengage(&plan.snapshot);
        assert_eq!(restore.target_source_rate_hz, original.source_rate_hz);
        assert_eq!(restore.target_center_hz, original.center_freq_hz);
        assert_eq!(restore.target_frontend_decim, original.frontend_decim);
        assert_eq!(restore.target_vfo_offset_hz, original.vfo_offset_hz);
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail (no `engage`/`disengage` yet)**

```bash
cargo test -p sdr-core --lib acars_airband_lock 2>&1 | tail -15
```

Expected: compile error, "cannot find function `engage` in this scope" or similar.

- [ ] **Step 3: Add the types and the two pure functions**

Insert directly above the `#[cfg(test)] mod tests` block (still in `acars_airband_lock.rs`):

```rust
/// Current source-side configuration the airband-lock state
/// machine reads to compute what to change. The controller
/// fills this from `DspState` at the moment of toggle.
#[derive(Clone, Debug, PartialEq)]
pub struct CurrentSourceState {
    pub source_rate_hz: f64,
    pub center_freq_hz: f64,
    pub vfo_offset_hz: f64,
    pub source_type: SourceType,
    pub frontend_decim: u32,
}

/// What `engage` decides should happen. The controller
/// applies these and stores the snapshot in `DspState`.
#[derive(Clone, Debug, PartialEq)]
pub struct EngagePlan {
    pub target_source_rate_hz: f64,
    pub target_center_hz: f64,
    pub target_frontend_decim: u32,
    pub snapshot: PreLockSnapshot,
}

/// What `disengage` decides should happen.
#[derive(Clone, Debug, PartialEq)]
pub struct DisengagePlan {
    pub target_source_rate_hz: f64,
    pub target_center_hz: f64,
    pub target_vfo_offset_hz: f64,
    pub target_frontend_decim: u32,
}

/// Compute the changes that engage the airband lock. Pure —
/// the controller calls this BEFORE touching any source state.
///
/// # Errors
///
/// Returns [`AcarsEnableError::UnsupportedSourceType`] if the
/// active source isn't `SourceType::RtlSdr`. Source-type gate
/// in v1 — rtl_tcp / network / file sources are not supported.
pub fn engage(current: &CurrentSourceState) -> Result<EngagePlan, AcarsEnableError> {
    if current.source_type != SourceType::RtlSdr {
        return Err(AcarsEnableError::UnsupportedSourceType(current.source_type));
    }
    Ok(EngagePlan {
        target_source_rate_hz: ACARS_SOURCE_RATE_HZ,
        target_center_hz: ACARS_CENTER_HZ,
        target_frontend_decim: ACARS_FRONTEND_DECIM,
        snapshot: PreLockSnapshot {
            source_rate_hz: current.source_rate_hz,
            center_freq_hz: current.center_freq_hz,
            vfo_offset_hz: current.vfo_offset_hz,
            source_type: current.source_type,
            frontend_decim: current.frontend_decim,
        },
    })
}

/// Compute the changes that release the airband lock and
/// restore the user's prior config. Pure.
#[must_use]
pub fn disengage(snapshot: &PreLockSnapshot) -> DisengagePlan {
    DisengagePlan {
        target_source_rate_hz: snapshot.source_rate_hz,
        target_center_hz: snapshot.center_freq_hz,
        target_vfo_offset_hz: snapshot.vfo_offset_hz,
        target_frontend_decim: snapshot.frontend_decim,
    }
}
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
cargo test -p sdr-core --lib acars_airband_lock 2>&1 | tail -10
```

Expected: `test result: ok. 4 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-core/src/acars_airband_lock.rs
git commit -m "feat(sdr-core): pure engage/disengage airband-lock state machine"
```

---

## Task 3: Add new message variants

**Files:**
- Modify: `crates/sdr-core/src/messages.rs`
- Modify: `crates/sdr-core/Cargo.toml` (add `sdr-acars` path dep)

- [ ] **Step 1: Add the dependency**

In `crates/sdr-core/Cargo.toml`, find the `[dependencies]` section. Add (alphabetical order with siblings):

```toml
sdr-acars = { path = "../sdr-acars" }
```

- [ ] **Step 2: Add the new variants to `UiToDsp` and `DspToUi`**

Open `crates/sdr-core/src/messages.rs`. Find the `pub enum UiToDsp` definition. Add at the end of the variant list (immediately before the closing brace):

```rust
    /// Engage or release the ACARS airband lock. `true` snapshots
    /// the prior source config and forces (2.5 MSps, 130.3375 MHz,
    /// frontend decim=1); `false` restores the snapshot.
    SetAcarsEnabled(bool),
```

Find `pub enum DspToUi`. Add at the end of the variant list:

```rust
    /// One decoded ACARS frame. Boxed because `AcarsMessage`
    /// holds an inline `String` body and `arrayvec` fields,
    /// so the enum's stack footprint stays small.
    AcarsMessage(Box<sdr_acars::AcarsMessage>),
    /// Per-channel ACARS stats. Emitted no more than once per
    /// `ACARS_STATS_EMIT_INTERVAL_MS` while ACARS is on.
    AcarsChannelStats(Box<[sdr_acars::ChannelStats; 6]>),
    /// Ack for `UiToDsp::SetAcarsEnabled`. `Ok(true)` after a
    /// successful engage; `Ok(false)` after disengage; `Err`
    /// on any failure (bank init, source retune, etc).
    AcarsEnabledChanged(Result<bool, crate::acars_airband_lock::AcarsEnableError>),
```

- [ ] **Step 3: Add the imports at the top of `messages.rs`**

If `sdr_acars` is not already imported in `messages.rs`, add:

```rust
use sdr_acars::{AcarsMessage, ChannelStats};
```

(The variants reference these types via fully-qualified paths in the suggested code above; if you prefer direct names, adjust the variant declarations to use the imported names.)

- [ ] **Step 4: Verify Debug + Clone derivation still compiles**

`UiToDsp` and `DspToUi` likely derive `Debug`. `AcarsMessage` and `ChannelStats` derive `Debug`+`Clone` (per sub-project 1). `AcarsEnableError` derives `Debug`+`Clone`. So nothing extra needed on the enum-level derives.

```bash
cargo build -p sdr-core 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 5: Smoke test that the variants round-trip Debug**

Append to `crates/sdr-core/src/messages.rs`'s existing `#[cfg(test)] mod tests` block (or create one if none exists):

```rust
    #[test]
    fn acars_set_enabled_round_trips_debug() {
        let cmd = UiToDsp::SetAcarsEnabled(true);
        let s = format!("{cmd:?}");
        assert!(s.contains("SetAcarsEnabled"), "got {s}");
        assert!(s.contains("true"), "got {s}");
    }

    #[test]
    fn acars_enabled_changed_carries_error() {
        use crate::acars_airband_lock::AcarsEnableError;
        let msg = DspToUi::AcarsEnabledChanged(Err(
            AcarsEnableError::UnsupportedSourceType(crate::messages::SourceType::File),
        ));
        let s = format!("{msg:?}");
        assert!(s.contains("AcarsEnabledChanged"), "got {s}");
        assert!(s.contains("UnsupportedSourceType"), "got {s}");
    }
```

```bash
cargo test -p sdr-core --lib messages 2>&1 | tail -10
```

Expected: 2 new tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/Cargo.toml crates/sdr-core/src/messages.rs
git commit -m "feat(sdr-core): UiToDsp::SetAcarsEnabled + 3 DspToUi ACARS variants"
```

---

## Task 4: Config keys for ACARS

**Files:**
- Create: `crates/sdr-ui/src/sidebar/acars_panel.rs`
- Modify: `crates/sdr-ui/src/sidebar/mod.rs` (add `pub mod acars_panel;`)

This task adds **only** the config-key constants and read/save helpers — no GTK widgets. The Aviation activity panel is sub-project 3.

- [ ] **Step 1: Add the module declaration**

Open `crates/sdr-ui/src/sidebar/mod.rs`. Find the existing `pub mod ...;` block. Add (alphabetical):

```rust
pub mod acars_panel;
```

- [ ] **Step 2: Create the new module**

Create `crates/sdr-ui/src/sidebar/acars_panel.rs`:

```rust
//! ACARS config-key holders + read/save helpers.
//!
//! This module deliberately holds no GTK widgets — the
//! Aviation activity panel ships in sub-project 3 of epic
//! #474. Sub-project 2 (pipeline integration) only needs
//! the keys + helpers so app startup persistence works.

use sdr_config::ConfigManager;

/// Persisted ACARS toggle. Default `false`.
pub const KEY_ACARS_ENABLED: &str = "acars_enabled";

/// Channel-set selector. Spec enum has only `"us-6"` in v1.
pub const KEY_ACARS_CHANNEL_SET: &str = "acars_channel_set";

/// Cap on the in-memory `acars_recent` ring buffer. Default
/// 500. Not exposed in the UI in v1; documented here so the
/// constant has one home.
pub const KEY_ACARS_RECENT_KEEP_COUNT: &str = "acars_recent_keep_count";

/// Default value used when a key is missing from the config.
const DEFAULT_ACARS_ENABLED: bool = false;
const DEFAULT_ACARS_CHANNEL_SET: &str = "us-6";

/// Read the persisted ACARS-enabled flag, defaulting to
/// `DEFAULT_ACARS_ENABLED` if absent.
#[must_use]
pub fn read_acars_enabled(config: &ConfigManager) -> bool {
    config
        .get_bool(KEY_ACARS_ENABLED)
        .unwrap_or(DEFAULT_ACARS_ENABLED)
}

/// Persist the ACARS-enabled flag.
pub fn save_acars_enabled(config: &ConfigManager, value: bool) {
    config.set_bool(KEY_ACARS_ENABLED, value);
}

/// Read the persisted channel-set string. Returns the default
/// (`"us-6"`) if absent or empty.
#[must_use]
pub fn read_acars_channel_set(config: &ConfigManager) -> String {
    config
        .get_string(KEY_ACARS_CHANNEL_SET)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_ACARS_CHANNEL_SET.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_config() -> ConfigManager {
        // Mirror the in-tree pattern (see
        // `crates/sdr-ui/src/sidebar/activity_bar.rs` tests):
        // `ConfigManager::in_memory(&serde_json::json!({}))`.
        // serde_json is already a workspace dep of sdr-ui.
        ConfigManager::in_memory(&serde_json::json!({}))
    }

    #[test]
    fn defaults_when_unset() {
        let cfg = fresh_config();
        assert!(!read_acars_enabled(&cfg));
        assert_eq!(read_acars_channel_set(&cfg), "us-6");
    }

    #[test]
    fn round_trip_enabled() {
        let cfg = fresh_config();
        save_acars_enabled(&cfg, true);
        assert!(read_acars_enabled(&cfg));
        save_acars_enabled(&cfg, false);
        assert!(!read_acars_enabled(&cfg));
    }
}
```

- [ ] **Step 3: Verify the `ConfigManager` test constructor matches**

```bash
grep -rn "ConfigManager::in_memory\|ConfigManager::new" crates/sdr-ui/src/sidebar/ crates/sdr-config/src/ | head -10
```

If the rest of the workspace uses a different test constructor (e.g. `ConfigManager::test()` or `ConfigManager::with_path`), update the `fresh_config()` helper to match. Do not invent a new constructor — use whatever `crates/sdr-ui/src/sidebar/general_page.rs` or `audio_panel.rs` use in their tests.

- [ ] **Step 4: Run tests**

```bash
cargo test -p sdr-ui --lib sidebar::acars_panel 2>&1 | tail -10
```

Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/sidebar/acars_panel.rs crates/sdr-ui/src/sidebar/mod.rs
git commit -m "feat(sdr-ui): ACARS config keys (acars_enabled / channel_set / keep_count)"
```

---

## Task 5: DspState fields

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Add the four new fields to `DspState`**

In `crates/sdr-core/src/controller.rs`, find the `struct DspState { ... }` definition. Group the new fields together near the existing imaging-decoder fields (`lrpt_decoder`, `lrpt_init_failed`, etc) for cohesion. Add:

```rust
    /// Active ACARS bank. `Some` while ACARS is on, `None`
    /// otherwise. Instantiated by the `SetAcarsEnabled(true)`
    /// arm; dropped by `SetAcarsEnabled(false)`, source-stop,
    /// and source-type-change auto-disable.
    acars_bank: Option<sdr_acars::ChannelBank>,
    /// Snapshot of the prior source config taken at engage.
    /// Used by disengage to restore the user's tuning.
    acars_pre_lock: Option<crate::acars_airband_lock::PreLockSnapshot>,
    /// One-shot guard: a previous `ChannelBank::new` failed.
    /// Mirrors `lrpt_init_failed` — prevents warn-spam on
    /// every subsequent IQ block. Cleared on source-stop.
    acars_init_failed: bool,
    /// Last `DspToUi::AcarsChannelStats` emission timestamp.
    /// Throttles stats emission to ~1 Hz per spec.
    acars_stats_emitted_at: std::time::Instant,
```

- [ ] **Step 2: Initialize the fields in the `DspState::new` (or wherever the struct is constructed)**

Find where the existing `lrpt_decoder: None`, `lrpt_init_failed: false` defaults live. Add alongside:

```rust
    acars_bank: None,
    acars_pre_lock: None,
    acars_init_failed: false,
    acars_stats_emitted_at: std::time::Instant::now(),
```

- [ ] **Step 3: Build to confirm compile**

```bash
cargo build -p sdr-core 2>&1 | tail -5
```

Expected: clean build. (If the `Default` derive on `DspState` complains about `Instant`, this is fine — `DspState` is constructed explicitly, not via `Default`. Confirm by reading the existing `lrpt_init_failed` pattern.)

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): DspState fields for ACARS bank + airband-lock snapshot"
```

---

## Task 6: `acars_decode_tap` function (TDD)

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

This is the per-block hot-path tap. Mirrors `lrpt_decode_tap` exactly: takes parameters separately (not `&mut DspState`) so the call site can hold a live borrow on `state.processed_buf`.

- [ ] **Step 1: Write the failing test as a new integration test**

Create `crates/sdr-core/tests/acars_decode_tap.rs`:

```rust
//! Tests for the per-block `acars_decode_tap` function.
//! These cover the init-on-first-call lifecycle and the
//! one-shot init-failed guard. End-to-end frame decoding is
//! covered by the sub-project 1 e2e test (`sdr-acars`); this
//! suite is purely about the controller-side wiring.

use std::sync::mpsc;

use sdr_core::acars_airband_lock::{
    AcarsEnableError, ACARS_CENTER_HZ, ACARS_SOURCE_RATE_HZ, US_SIX_CHANNELS_HZ,
};
use sdr_core::messages::DspToUi;
use sdr_core::testing::acars_decode_tap;
use sdr_types::Complex;

#[test]
fn tap_is_a_no_op_when_bank_slot_is_none_and_stays_silent() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = false;
    let (tx, rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];

    // No bank yet, init_failed not set — tap must lazily
    // initialize. Successful init at airband geometry.
    acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &US_SIX_CHANNELS_HZ,
        &iq,
        &tx,
    );
    assert!(bank.is_some(), "first call should initialize the bank");
    assert!(!init_failed);
    // Silent IQ produces no messages.
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

#[test]
fn tap_skips_processing_after_init_failure() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = true; // Simulate prior failure.
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];

    acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &US_SIX_CHANNELS_HZ,
        &iq,
        &tx,
    );
    assert!(bank.is_none(), "init_failed=true must short-circuit");
    assert!(init_failed);
}

#[test]
fn tap_records_init_failure_on_invalid_channel_list() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = false;
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];
    let bad_channels: [f64; 6] = [0.0; 6]; // outside source bandwidth

    acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &bad_channels,
        &iq,
        &tx,
    );
    assert!(bank.is_none());
    assert!(init_failed, "bad channels should set init_failed");
}
```

- [ ] **Step 2: Expose `acars_decode_tap` for tests**

In `crates/sdr-core/src/lib.rs`, add a `pub mod testing` re-export (only behind `#[cfg(any(test, feature = "testing"))]` if the crate has that feature; otherwise just `pub` — matching whatever sibling tests like LRPT use):

```rust
/// Test-only re-exports for integration tests in `tests/`.
#[doc(hidden)]
pub mod testing {
    pub use crate::controller::acars_decode_tap;
}
pub mod messages;
```

(If `controller` is private, you may need to make `acars_decode_tap` `pub(crate)` and re-export through this `testing` module. Match the pattern the LRPT tests use; if there are no LRPT integration tests, this is the simplest path.)

- [ ] **Step 3: Run the test to confirm it fails**

```bash
cargo test -p sdr-core --test acars_decode_tap 2>&1 | tail -10
```

Expected: compile error, "cannot find function `acars_decode_tap`".

- [ ] **Step 4: Implement `acars_decode_tap`**

In `crates/sdr-core/src/controller.rs`, immediately after the `lrpt_decode_tap` function (the existing one around line 724), add:

```rust
/// ACARS decode tap. Mirrors `lrpt_decode_tap`'s shape: takes
/// the bank slot, init-failed flag, current geometry, IQ
/// buffer, and dsp_tx as separate parameters so the call
/// site can hold a live borrow of `state.processed_buf`.
///
/// Lazy-init: on the first call with `bank.is_none()` and
/// `*init_failed == false`, builds the `ChannelBank` from
/// `(source_rate_hz, center_hz, channels)`. If construction
/// fails, sets `*init_failed = true` and skips subsequent
/// calls until source-stop clears the flag (matching the
/// LRPT pattern).
///
/// Per-block: feeds `iq` through `bank.process(...)` and
/// forwards each decoded `AcarsMessage` to `dsp_tx`. The
/// caller is responsible for periodic `AcarsChannelStats`
/// emission (handled in `process_iq_block` via the
/// throttle in `state.acars_stats_emitted_at`).
pub(crate) fn acars_decode_tap(
    bank: &mut Option<sdr_acars::ChannelBank>,
    init_failed: &mut bool,
    source_rate_hz: f64,
    center_hz: f64,
    channels: &[f64],
    iq: &[sdr_types::Complex],
    dsp_tx: &std::sync::mpsc::Sender<crate::messages::DspToUi>,
) {
    if *init_failed {
        return;
    }
    if bank.is_none() {
        match sdr_acars::ChannelBank::new(source_rate_hz, center_hz, channels) {
            Ok(b) => {
                tracing::info!(
                    "ACARS bank initialised: source_rate={source_rate_hz} \
                     center={center_hz} n_channels={}",
                    channels.len()
                );
                *bank = Some(b);
            }
            Err(e) => {
                tracing::warn!("ACARS bank init failed: {e}");
                *init_failed = true;
                return;
            }
        }
    }
    let Some(bank) = bank.as_mut() else { return };
    bank.process(iq, |msg| {
        // Boxed because AcarsMessage is large enough that
        // unboxed would inflate the DspToUi enum's footprint.
        let _ = dsp_tx.send(crate::messages::DspToUi::AcarsMessage(Box::new(msg)));
    });
}
```

- [ ] **Step 5: Run the test to confirm it passes**

```bash
cargo test -p sdr-core --test acars_decode_tap 2>&1 | tail -10
```

Expected: 3 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/src/controller.rs crates/sdr-core/src/lib.rs crates/sdr-core/tests/acars_decode_tap.rs
git commit -m "feat(sdr-core): acars_decode_tap (mirror of lrpt_decode_tap)"
```

---

## Task 7: Wire `UiToDsp::SetAcarsEnabled` arm

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Locate the `handle_command` match block**

```bash
grep -n "fn handle_command\|UiToDsp::Tune\|UiToDsp::SetSampleRate" crates/sdr-core/src/controller.rs | head -10
```

Note the line range — you'll add the new match arm at the end of the existing arms (just before the closing brace).

- [ ] **Step 2: Add the new arm**

Add at the end of the `match cmd { ... }` block in `handle_command`:

```rust
        UiToDsp::SetAcarsEnabled(enable) => {
            handle_set_acars_enabled(state, enable, dsp_tx);
        }
```

- [ ] **Step 3: Add the implementation function below `handle_command`**

Append (a new top-level fn, near the existing decode-tap helpers):

```rust
/// Handler for `UiToDsp::SetAcarsEnabled`. Engages or
/// releases the airband lock, instantiates / drops the
/// `ChannelBank`, and emits an ack via `DspToUi`.
fn handle_set_acars_enabled(
    state: &mut DspState,
    enable: bool,
    dsp_tx: &std::sync::mpsc::Sender<crate::messages::DspToUi>,
) {
    use crate::acars_airband_lock::{
        disengage, engage, AcarsEnableError, CurrentSourceState, US_SIX_CHANNELS_HZ,
    };
    use crate::messages::DspToUi;

    if enable {
        if state.acars_bank.is_some() {
            // Idempotent: already on. Re-ack with current state.
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(true)));
            return;
        }

        let current = CurrentSourceState {
            source_rate_hz: state.sample_rate,
            center_freq_hz: state.center_freq,
            vfo_offset_hz: state.vfo_offset,
            source_type: state.source_type,
            frontend_decim: state.frontend.decim_ratio(),
        };
        let plan = match engage(&current) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("ACARS engage rejected: {e}");
                let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(e)));
                return;
            }
        };

        // Apply target geometry to the source. ANY failure
        // here triggers a full rollback: don't half-engage.
        if let Some(source) = state.source.as_mut()
            && let Err(e) = source.set_sample_rate(plan.target_source_rate_hz)
        {
            let err = AcarsEnableError::SourceRetuneFailed(e.to_string());
            tracing::warn!("ACARS engage source-rate failed: {err}");
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(err)));
            return;
        }
        state.sample_rate = plan.target_source_rate_hz;
        state.center_freq = plan.target_center_hz;
        if let Some(source) = state.source.as_mut()
            && let Err(e) = source.tune(plan.target_center_hz)
        {
            // Roll back the rate so we don't leave the source
            // half-tuned. (`Source::tune` is the trait method
            // for center-freq retune — see
            // `crates/sdr-pipeline/src/source_manager.rs`.)
            if let Some(s) = state.source.as_mut() {
                let _ = s.set_sample_rate(plan.snapshot.source_rate_hz);
            }
            state.sample_rate = plan.snapshot.source_rate_hz;
            state.center_freq = plan.snapshot.center_freq_hz;
            let err = AcarsEnableError::SourceRetuneFailed(e.to_string());
            tracing::warn!("ACARS engage center-freq failed: {err}");
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(err)));
            return;
        }
        if let Err(e) = state.frontend.set_decimation(plan.target_frontend_decim) {
            // Roll back rate + center.
            if let Some(s) = state.source.as_mut() {
                let _ = s.set_sample_rate(plan.snapshot.source_rate_hz);
                let _ = s.tune(plan.snapshot.center_freq_hz);
            }
            state.sample_rate = plan.snapshot.source_rate_hz;
            state.center_freq = plan.snapshot.center_freq_hz;
            let err = AcarsEnableError::FrontendDecimFailed(e.to_string());
            tracing::warn!("ACARS engage decim failed: {err}");
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(err)));
            return;
        }

        // Geometry locked. Pre-build the ChannelBank now (rather
        // than on first IQ block) so init failure surfaces in
        // the engage ack rather than as a quiet `init_failed=true`
        // state the UI never finds out about.
        match sdr_acars::ChannelBank::new(
            plan.target_source_rate_hz,
            plan.target_center_hz,
            &US_SIX_CHANNELS_HZ,
        ) {
            Ok(bank) => {
                state.acars_bank = Some(bank);
                state.acars_init_failed = false;
                state.acars_pre_lock = Some(plan.snapshot);
                state.acars_stats_emitted_at = std::time::Instant::now();
                tracing::info!("ACARS engaged: airband lock active");
                let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(true)));
            }
            Err(e) => {
                // Roll back source + decim.
                if let Some(s) = state.source.as_mut() {
                    let _ = s.set_sample_rate(plan.snapshot.source_rate_hz);
                    let _ = s.tune(plan.snapshot.center_freq_hz);
                }
                let _ = state.frontend.set_decimation(plan.snapshot.frontend_decim);
                state.sample_rate = plan.snapshot.source_rate_hz;
                state.center_freq = plan.snapshot.center_freq_hz;
                let err = AcarsEnableError::ChannelBankInit(e.to_string());
                tracing::warn!("ACARS bank init failed: {err}");
                let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(err)));
            }
        }
    } else {
        // Disengage. Idempotent: silently OK if already off.
        let Some(snapshot) = state.acars_pre_lock.take() else {
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(false)));
            return;
        };
        let restore = disengage(&snapshot);
        // Drop the bank first so any in-flight messages drain.
        state.acars_bank = None;
        state.acars_init_failed = false;

        if let Some(source) = state.source.as_mut() {
            let _ = source.set_sample_rate(restore.target_source_rate_hz);
            let _ = source.tune(restore.target_center_hz);
        }
        let _ = state.frontend.set_decimation(restore.target_frontend_decim);
        state.sample_rate = restore.target_source_rate_hz;
        state.center_freq = restore.target_center_hz;
        // VFO offset restore. The controller stores it on
        // `state.vfo_offset` (read by `rebuild_vfo` and the
        // VFO struct). Mirror the `UiToDsp::SetVfoOffset` arm
        // (around `controller.rs:1219`).
        state.vfo_offset = restore.target_vfo_offset_hz;
        if let Some(vfo) = state.vfo.as_mut() {
            vfo.set_offset(restore.target_vfo_offset_hz);
        }
        tracing::info!("ACARS disengaged: source restored to snapshot");
        let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(false)));
    }
}
```

> **Verified method signatures** (from `crates/sdr-pipeline/src/source_manager.rs` and `vfo_manager.rs`, fresh as of `377107e`):
> - `Source::tune(&mut self, frequency_hz: f64) -> Result<(), SourceError>` — center freq retune.
> - `Source::set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError>` — source rate change.
> - `RxVfo::set_offset(offset: f64)` — VFO offset setter; `state.vfo_offset: f64` is the controller-side mirror.
> - `IqFrontend::set_decimation(ratio: u32) -> Result<(), DspError>`.
>
> If any of these no longer exist (e.g. due to a refactor between plan-write and execution), grep, adjust the call sites in this task to whatever the controller's `UiToDsp::Tune` / `SetSampleRate` / `SetVfoOffset` arms now use, and document the deviation in the commit message. Do NOT invent a new method.

- [ ] **Step 4: Build to confirm everything compiles**

```bash
cargo build -p sdr-core 2>&1 | tail -10
```

Fix any signature mismatches surfaced by the compiler (these will be method-name nits per the note above).

- [ ] **Step 5: Run all sdr-core tests**

```bash
cargo test -p sdr-core 2>&1 | tail -10
```

Expected: existing tests pass, new tests from Tasks 2/3/6 pass. No regressions.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): handle UiToDsp::SetAcarsEnabled (engage/disengage)"
```

---

## Task 8: Wire `acars_decode_tap` into `process_iq_block`

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 1: Find the post-IqFrontend, pre-VFO seam**

In `process_iq_block`, locate the block:

```rust
match state.frontend.process(
    &state.iq_buf[..iq_count],
    &mut state.processed_buf,
    &mut state.fft_buf,
) {
    Ok((processed_count, fft_ready)) => {
        // ... fft handling ...
        if processed_count > 0 {
            // Pass through RxVfo: ...
            let radio_input = if let Some(vfo) = ...
```

The ACARS tap goes inside the `if processed_count > 0` block, **before** the `let radio_input = ...` VFO branch — `state.processed_buf[..processed_count]` carries the frontend's effective-rate IQ, which equals source rate when ACARS forces decim=1.

- [ ] **Step 2: Insert the tap call**

Add directly inside `if processed_count > 0 { ... }`, before the `let radio_input = ...` line:

```rust
                // ACARS decode tap (#474). Runs at source rate
                // (ACARS forces frontend decim=1). Tapped BEFORE
                // the VFO so we read the full 2.5 MHz airband
                // window unchanged. Mirror of `lrpt_decode_tap`
                // but at source rate vs post-VFO 144 ksps.
                if state.acars_bank.is_some() {
                    acars_decode_tap(
                        &mut state.acars_bank,
                        &mut state.acars_init_failed,
                        state.sample_rate,
                        state.center_freq,
                        &crate::acars_airband_lock::US_SIX_CHANNELS_HZ,
                        &state.processed_buf[..processed_count],
                        dsp_tx,
                    );

                    // ~1 Hz channel-stats emission throttle.
                    let now = std::time::Instant::now();
                    let elapsed = now.duration_since(state.acars_stats_emitted_at);
                    if elapsed >= std::time::Duration::from_millis(
                        crate::acars_airband_lock::ACARS_STATS_EMIT_INTERVAL_MS,
                    ) && let Some(bank) = state.acars_bank.as_ref()
                    {
                        let stats = bank.channels();
                        if stats.len() == 6 {
                            let arr: [sdr_acars::ChannelStats; 6] = [
                                stats[0], stats[1], stats[2],
                                stats[3], stats[4], stats[5],
                            ];
                            let _ = dsp_tx.send(
                                crate::messages::DspToUi::AcarsChannelStats(Box::new(arr)),
                            );
                            state.acars_stats_emitted_at = now;
                        }
                    }
                }
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-core 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Run all sdr-core tests**

```bash
cargo test -p sdr-core 2>&1 | tail -10
```

Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): wire acars_decode_tap into process_iq_block"
```

---

## Task 9: Source-type-change auto-disable + source-stop cleanup

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

When the source type changes while ACARS is on (e.g. user switches to network source), the spec says: auto-disable ACARS and emit a one-line toast. Source-stop should also drop the bank and clear `init_failed` (matching the LRPT pattern).

- [ ] **Step 1: Find the source-type-change site**

```bash
grep -n "state.source_type =\|source_type = SourceType::" crates/sdr-core/src/controller.rs | head -10
```

There's typically a `UiToDsp::SetSourceType` (or similar) handler. Locate it.

- [ ] **Step 2: Add the auto-disable call at the top of the source-type-change handler**

Just BEFORE `state.source_type = new_type;` (or wherever the type is reassigned), add:

```rust
            if state.acars_bank.is_some() && new_type != SourceType::RtlSdr {
                tracing::info!(
                    "ACARS auto-disabling: source type changing to {new_type:?}"
                );
                // Reuse the disengage path. Note: we issue a
                // synthetic disengage rather than "just drop the
                // bank" so the source-rate/center are restored
                // and the UI gets the ack.
                handle_set_acars_enabled(state, false, dsp_tx);
            }
```

- [ ] **Step 3: Find the source-stop / cleanup site**

```bash
grep -n "fn cleanup\|state.lrpt_decoder = None\|state.lrpt_init_failed = false" crates/sdr-core/src/controller.rs | head -10
```

Find where LRPT state is cleared on source-stop.

- [ ] **Step 4: Add ACARS cleanup alongside LRPT**

In the same function, immediately after the LRPT cleanup lines, add:

```rust
    // Match the LRPT pattern: source-stop drops the bank and
    // clears the one-shot init flag. Snapshot stays so a
    // subsequent re-enable can read it (the user re-toggling
    // ACARS after a source restart should still restore prior
    // tuning).
    state.acars_bank = None;
    state.acars_init_failed = false;
```

- [ ] **Step 5: Build + test**

```bash
cargo build -p sdr-core 2>&1 | tail -5
cargo test -p sdr-core 2>&1 | tail -10
```

Expected: clean, green.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): auto-disable ACARS on source-type change + cleanup hook"
```

---

## Task 10: AppState ACARS fields

**Files:**
- Modify: `crates/sdr-ui/src/state.rs`

Spec lists six AppState additions; `acars_viewer_window` is sub-project 3 territory and is deferred. The other five land here.

- [ ] **Step 1: Add the imports if needed**

Near the top of `crates/sdr-ui/src/state.rs`, ensure these are imported (likely already are, but check):

```rust
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use sdr_acars::{AcarsMessage, ChannelStats};
use sdr_core::acars_airband_lock::PreLockSnapshot;
```

- [ ] **Step 2: Add the fields to `AppState`**

In the `pub struct AppState { ... }` definition, group near the existing imaging-decoder fields:

```rust
    /// ACARS toggle (mirrors persisted `acars_enabled`).
    pub acars_enabled: Cell<bool>,
    /// Bounded ring of recent decoded messages. Cap is set
    /// from `acars_recent_keep_count` config (default 500).
    pub acars_recent: RefCell<VecDeque<AcarsMessage>>,
    /// Cumulative decoded-message count since toggle-on.
    /// Reset by `SetAcarsEnabled(true)` — gives the UI a
    /// running counter without scanning the bounded ring.
    pub acars_total_count: Cell<u64>,
    /// Latest per-channel stats, populated by the
    /// `DspToUi::AcarsChannelStats` arm. Defaulted on init.
    pub acars_channel_stats: RefCell<[ChannelStats; 6]>,
    /// Mirror of the DSP-side snapshot, populated when the
    /// engage ack arrives. Lets the UI display "restoring
    /// to {prior_freq}" hints on disengage.
    pub acars_pre_lock_state: RefCell<Option<PreLockSnapshot>>,
```

- [ ] **Step 3: Initialize in the `AppState::new` constructor**

Find the `impl AppState { pub fn new(...) -> Self { Self { ... } } }`. Add to the field initializers:

```rust
            acars_enabled: Cell::new(false),
            acars_recent: RefCell::new(VecDeque::with_capacity(512)),
            acars_total_count: Cell::new(0),
            acars_channel_stats: RefCell::new([ChannelStats::default(); 6]),
            acars_pre_lock_state: RefCell::new(None),
```

- [ ] **Step 4: Confirm `ChannelStats: Default`**

```bash
grep -n "Default\|impl.*ChannelStats\|#\[derive" crates/sdr-acars/src/channel.rs | head -5
```

If `ChannelStats` does NOT derive `Default`, that's a sub-project 1 oversight to fix here:

In `crates/sdr-acars/src/channel.rs`, locate the `pub struct ChannelStats { ... }` definition. Change the derive line from:

```rust
#[derive(Clone, Copy, Debug)]
```

to:

```rust
#[derive(Clone, Copy, Debug, Default)]
```

Then for each field that doesn't have a sensible `Default` automatically, ensure it does. (`Option<SystemTime>` defaults to `None`; `f64`, `u32`, `f32` default to 0. The enum `ChannelLockState` needs `#[derive(Default)]` with `#[default] Idle` — likely already there since `Idle` is the natural default; verify and add if missing.)

If you needed to add `Default` here, smoke-test:

```bash
cargo build -p sdr-acars 2>&1 | tail -5
cargo test -p sdr-acars 2>&1 | tail -10
```

- [ ] **Step 5: Build to confirm AppState compiles**

```bash
cargo build -p sdr-ui 2>&1 | tail -10
```

Expected: clean. (If `freq_hz: 0.0` in `Default::default()` doesn't match the channel-set we want, that's fine — these are placeholders the first `AcarsChannelStats` message overwrites.)

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/state.rs crates/sdr-acars/src/channel.rs
git commit -m "feat(sdr-ui): AppState ACARS fields (toggle, ring, count, stats, snapshot)"
```

---

## Task 11: UI-side DspToUi dispatch arms

**Files:**
- Modify: the file noted in Task 0 Step 3 (the UI-side `DspToUi` matcher; likely `crates/sdr-ui/src/dsp_messages.rs` or `crates/sdr-ui/src/app.rs`)

- [ ] **Step 1: Add the three new arms**

In the `match cmd { ... }` block that handles `DspToUi`, add at the end:

```rust
        DspToUi::AcarsMessage(msg) => {
            // Bounded ring: pop oldest if at cap.
            let cap = sidebar::acars_panel::default_recent_keep() as usize;
            let mut ring = state.acars_recent.borrow_mut();
            if ring.len() >= cap {
                ring.pop_front();
            }
            ring.push_back((*msg).clone());
            drop(ring);
            state
                .acars_total_count
                .set(state.acars_total_count.get().saturating_add(1));
            // No UI rendering yet — sub-project 3 wires the
            // viewer + panel summary off these fields.
            tracing::trace!(
                "ACARS msg {} ({}, label {:?})",
                state.acars_total_count.get(),
                msg.aircraft.as_str(),
                msg.label
            );
        }
        DspToUi::AcarsChannelStats(stats) => {
            *state.acars_channel_stats.borrow_mut() = *stats;
        }
        DspToUi::AcarsEnabledChanged(result) => {
            match result {
                Ok(true) => {
                    state.acars_enabled.set(true);
                    sidebar::acars_panel::save_acars_enabled(&state.config, true);
                    tracing::info!("ACARS engaged");
                }
                Ok(false) => {
                    state.acars_enabled.set(false);
                    sidebar::acars_panel::save_acars_enabled(&state.config, false);
                    state.acars_recent.borrow_mut().clear();
                    state.acars_total_count.set(0);
                    *state.acars_channel_stats.borrow_mut() =
                        [sdr_acars::ChannelStats::default(); 6];
                    tracing::info!("ACARS disengaged");
                }
                Err(err) => {
                    tracing::warn!("ACARS enable failed: {err}");
                    state.acars_enabled.set(false);
                    // Clear the persisted flag so a startup
                    // attempt won't retry the failing config.
                    sidebar::acars_panel::save_acars_enabled(&state.config, false);
                    // Sub-project 3 wires a toast off this; for
                    // sub-project 2 the warn-log is sufficient.
                }
            }
        }
```

- [ ] **Step 2: Add a helper for the keep-count default**

In `crates/sdr-ui/src/sidebar/acars_panel.rs`, append:

```rust
/// Default ring-buffer cap. Honors a config override of
/// `acars_recent_keep_count` if present; otherwise returns
/// the spec default (500).
#[must_use]
pub fn default_recent_keep() -> u32 {
    // Read the override once at module load; for sub-project 2
    // we don't need a setter.
    sdr_core::acars_airband_lock::ACARS_RECENT_DEFAULT_KEEP
}
```

(Sub-project 3 may extend this to consult `ConfigManager`. For now, the constant is enough.)

- [ ] **Step 3: Add the imports needed in the UI file**

At the top of the file noted in Task 0 Step 3, ensure:

```rust
use crate::sidebar;
use sdr_core::messages::DspToUi;
```

(These likely already exist; only add if missing.)

- [ ] **Step 4: Build**

```bash
cargo build -p sdr-ui 2>&1 | tail -10
```

Fix any compile errors related to `state.config` field access (the actual field name on `AppState` may differ — match what `save_close_to_tray` callers use; grep if unsure).

- [ ] **Step 5: Commit**

```bash
# Stage every file modified in this task. The DspToUi-handler
# file path was recorded in Task 0 Step 3; if everything is
# already staged via `git add -u`, that's fine.
git add -A
git status   # sanity-check: only files for this task should be staged
git commit -m "feat(sdr-ui): DspToUi arms for AcarsMessage/Stats/EnabledChanged"
```

---

## Task 12: Startup config replay

**Files:**
- Modify: the file noted in Task 0 Step 4 (the post-DSP-ready dispatch site)

- [ ] **Step 1: Find the right callback**

The plan's "post-DSP-ready" path is wherever the UI sends `UiToDsp::Start` after the DSP thread is initialized. After (or alongside) that send, replay the persisted ACARS state.

- [ ] **Step 2: Add the replay**

```rust
// Replay persisted ACARS state. If `acars_enabled = true`
// in config, dispatch SetAcarsEnabled(true) so the DSP
// re-engages on app start. On failure, the
// `AcarsEnabledChanged(Err(_))` arm clears the persisted
// flag so a subsequent restart won't retry.
if sidebar::acars_panel::read_acars_enabled(&state.config) {
    let _ = ui_to_dsp_tx.send(UiToDsp::SetAcarsEnabled(true));
    tracing::info!("ACARS startup-replay: dispatching SetAcarsEnabled(true)");
}
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
# The startup-replay file path was recorded in Task 0 Step 4.
git add -A
git status
git commit -m "feat(sdr-ui): replay persisted acars_enabled on app startup"
```

---

## Task 13: End-to-end controller integration test

**Files:**
- Create: `crates/sdr-core/tests/acars_pipeline_integration.rs`

This is the headless harness the project doesn't yet have. Build it small: just enough to assert that `SetAcarsEnabled(true)` → DSP processes IQ → emits `AcarsMessage`.

- [ ] **Step 1: Locate any existing integration-test scaffolding**

```bash
ls crates/sdr-core/tests/ 2>/dev/null
grep -l "DspState::new\|spawn_dsp_thread" crates/sdr-core/tests/ 2>/dev/null | head -3
```

If a fixture builder exists (e.g. `make_test_dsp_state()`), use it. If not, the test below constructs `DspState` directly.

- [ ] **Step 2: Write the integration test**

Create `crates/sdr-core/tests/acars_pipeline_integration.rs`:

```rust
//! Headless ACARS controller integration test. Exercises the
//! full SetAcarsEnabled → process IQ → AcarsMessage round-trip
//! without spinning up a real source thread or GTK loop.
//!
//! IQ source: a zero-IQ buffer (no decodable signal). The test
//! asserts the lifecycle (bank instantiated on engage, dropped
//! on disengage) and that the engage ack arrives. End-to-end
//! frame decoding is covered by `sdr-acars`'s e2e tests.

use std::sync::mpsc;

use sdr_core::acars_airband_lock::ACARS_SOURCE_RATE_HZ;
use sdr_core::messages::{DspToUi, SourceType, UiToDsp};

#[test]
fn engage_disengage_round_trip_emits_acks_only() {
    // Build a minimal DspState. The exact constructor depends
    // on what's exposed by sdr-core; if there's no public
    // constructor, this test should live as `#[cfg(test)] mod
    // tests` inside `controller.rs` instead. Match whichever
    // is consistent with the LRPT integration tests (or, if
    // none exist, with the engine.rs / sink_slot.rs tests).
    let (ui_tx, _ui_rx) = mpsc::channel::<UiToDsp>();
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = sdr_core::testing::make_dsp_state_for_tests(
        SourceType::RtlSdr,
        ACARS_SOURCE_RATE_HZ,
        130_000_000.0,
    );

    // Drive engage.
    sdr_core::testing::handle_command(
        &mut state,
        UiToDsp::SetAcarsEnabled(true),
        &dsp_tx,
    );
    let ack = dsp_rx.recv_timeout(std::time::Duration::from_millis(100))
        .expect("engage ack");
    assert!(matches!(ack, DspToUi::AcarsEnabledChanged(Ok(true))));
    assert!(state.acars_bank_is_some_for_tests());

    // Drive disengage.
    sdr_core::testing::handle_command(
        &mut state,
        UiToDsp::SetAcarsEnabled(false),
        &dsp_tx,
    );
    let ack = dsp_rx.recv_timeout(std::time::Duration::from_millis(100))
        .expect("disengage ack");
    assert!(matches!(ack, DspToUi::AcarsEnabledChanged(Ok(false))));
    assert!(!state.acars_bank_is_some_for_tests());

    drop(ui_tx);
}
```

- [ ] **Step 3: Add the test-only re-exports**

In `crates/sdr-core/src/lib.rs`, extend the `pub mod testing` block (created in Task 6):

```rust
#[doc(hidden)]
pub mod testing {
    pub use crate::controller::{acars_decode_tap, handle_command};
    pub use crate::controller::test_helpers::{
        make_dsp_state_for_tests, DspStateExt,
    };
}
```

In `crates/sdr-core/src/controller.rs`, add at the bottom (outside any existing `mod tests`):

```rust
/// Helpers for integration tests that need to construct a
/// `DspState` without going through the full `spawn_dsp_thread`
/// path. Behind `#[cfg(any(test, feature = "testing"))]` if the
/// crate has that feature; otherwise unconditional + `#[doc(hidden)]`.
pub mod test_helpers {
    use super::*;

    /// Build a minimal `DspState` for headless integration tests.
    /// Caller specifies source type + initial rate/center; the
    /// rest is defaulted to whatever a real `DspState::new` would
    /// produce.
    pub fn make_dsp_state_for_tests(
        source_type: crate::messages::SourceType,
        sample_rate: f64,
        center_freq: f64,
    ) -> DspState {
        // Implementer: copy the existing DspState construction
        // path used by `spawn_dsp_thread` but inject the values.
        // If DspState has many private fields, gate this entire
        // module behind `#[cfg(test)]` and accept that the
        // integration test must live inside controller.rs as
        // `#[cfg(test)] mod tests`. Match whichever path the
        // existing engine.rs tests use.
        unimplemented!("see plan task 13 step 3 — match the engine.rs test pattern")
    }

    pub trait DspStateExt {
        fn acars_bank_is_some_for_tests(&self) -> bool;
    }

    impl DspStateExt for DspState {
        fn acars_bank_is_some_for_tests(&self) -> bool {
            self.acars_bank.is_some()
        }
    }
}
```

> **Implementer note:** The `unimplemented!` is a tripwire. If the controller has no public way to build a `DspState`, you have two choices:
> 1. Move the test from `tests/acars_pipeline_integration.rs` to `#[cfg(test)] mod tests { ... }` inside `controller.rs` so it can access private fields directly.
> 2. Add a real test-only constructor that builds a `DspState` with the same initialization the existing `spawn_dsp_thread` does (probably copy-paste a small subset).
>
> Choose option 1 if the existing engine.rs tests live in-module. Option 2 if they live in `tests/`. Match the established pattern.

- [ ] **Step 4: Run the new integration test**

```bash
cargo test -p sdr-core --test acars_pipeline_integration 2>&1 | tail -10
```

Expected: 1 test passes (after wiring the helper per the implementer note above).

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-core/tests/acars_pipeline_integration.rs crates/sdr-core/src/lib.rs crates/sdr-core/src/controller.rs
git commit -m "test(sdr-core): headless ACARS engage/disengage round-trip"
```

---

## Task 14: Workspace lint + format gates

**Files:** none (verification only)

- [ ] **Step 1: Run the workspace test suite**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: all tests pass. No `# failed`, no `# ignored` regressions.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --all-targets --workspace -- -D warnings 2>&1 | tail -10
```

Expected: clean. Fix any new lints (the `#[allow(...)]` patterns documented earlier are precedent — match style; don't add new allows without a code comment explaining why).

- [ ] **Step 3: Run fmt check**

```bash
cargo fmt --all -- --check 2>&1 | tail -5
```

Expected: silent (no diff). If anything is off:

```bash
cargo fmt --all
git add -u
git commit -m "chore: cargo fmt"
```

- [ ] **Step 4: Run cargo deny / cargo audit (skip if these aren't in `make lint`)**

```bash
make lint 2>&1 | tail -10
```

Expected: clean.

If `make lint` is impractical (slow or has flaky deps), at minimum re-run clippy + fmt before push (per `feedback_fmt_check_immediately_before_push.md`).

---

## Task 15: Smoke test — manual GTK pass

Per `feedback_smoke_test_workflow.md`: the user runs the GTK smoke test manually. Claude installs and provides the checklist; never launches the binary itself.

**Files:** none (manual verification)

- [ ] **Step 1: Build + install**

Per `feedback_make_install_release_flag.md`, the `--release` flag is required:

```bash
make install CARGO_FLAGS="--release"
```

(If the user is normally on the `whisper-cuda` flavor per their daily-driver memory, use `make install CARGO_FLAGS="--release --features whisper-cuda"`.)

- [ ] **Step 2: Confirm the new binary actually contains the changes**

```bash
strings ~/.local/bin/sdr-rs 2>/dev/null | grep -E "acars_decode_tap|airband_lock" | head -3
# Or wherever BINDIR points; per the memory the install copies into $HOME/.local/bin or /usr/local/bin
```

Expected: at least one match. If empty, the install is stale — investigate before asking the user to smoke.

- [ ] **Step 3: Provide the smoke checklist for the user to run**

Show the user this checklist (do NOT launch the binary yourself):

```
ACARS pipeline-integration smoke checklist:

1. Launch app. Confirm no panic on startup.
2. With an RTL-SDR connected and `acars_enabled` NOT set in config:
     - App should launch normally; no ACARS-related toasts.
     - tracing logs should NOT contain "ACARS engaged".
3. Manually flip `acars_enabled = true` in
   ~/.config/sdr-rs/config.json (or wherever the config lives),
   restart the app:
     - Logs should show "ACARS startup-replay: dispatching
       SetAcarsEnabled(true)" then "ACARS engaged".
     - Source rate should be 2.5 MSps (visible in Source panel).
     - Center freq 130.3375 MHz (visible in header / freq selector).
4. Without an RTL-SDR (e.g. switch to network source):
     - Engage attempt should produce "ACARS auto-disabling: source
       type changing" log entry, no panic.
     - acars_enabled should be cleared from the persisted config
       on a subsequent restart.
5. Quit cleanly while ACARS is on. Restart. Should re-engage.
6. Stop the source while ACARS is on. ACARS bank should drop;
   re-starting the source should re-engage cleanly via
   SetAcarsEnabled startup-replay.
```

- [ ] **Step 4: Wait for the user to report results**

Do NOT proceed to push until the user signs off on the smoke. If they report failures, the loop is: read tracing logs → identify root cause → fix → re-install → re-smoke. Per `feedback_pre_commit_cr_review.md`, run the CR-pattern checklist before each commit.

---

## Task 16: Final pre-push sweep + push

**Files:** none

- [ ] **Step 1: Re-run fmt check (this is the LAST gate, per `feedback_fmt_check_immediately_before_push.md`)**

```bash
cargo fmt --all -- --check 2>&1 | tail -3
```

Expected: silent.

- [ ] **Step 2: Confirm branch is clean**

```bash
git status
```

Expected: nothing to commit.

- [ ] **Step 3: Push**

```bash
git push -u origin feat/acars-pipeline-integration 2>&1 | tail -5
```

- [ ] **Step 4: Open the PR**

Use the `gh pr create` workflow per CLAUDE.md / project conventions. PR title: `feat(sdr-core,sdr-ui): ACARS pipeline integration + airband lock (#474)`. Body: 1-3 bullet summary + test plan checklist mirroring Task 15's smoke checklist.

- [ ] **Step 5: Wait for CodeRabbit**

Per `feedback_coderabbit_workflow.md`: do NOT start sub-project 3 (Aviation activity + viewer) until CR has reviewed and any rounds are addressed.

---

## Spec coverage cross-check

| Spec section | Tasks |
|---|---|
| Tap point post-IqFrontend, pre-VFO at source rate | Task 6, 8 |
| Airband-lock geometry (2.5 MSps, 130.3375 MHz, decim=1) | Task 1, 2, 7 |
| Source-rate / center / decim snapshot + restore | Task 1, 2, 7 |
| VFO disabled in UI | Sub-project 3 (DSP just doesn't touch VFO when ACARS on; nothing to do here) |
| Source-type gate (RTL-SDR only) | Task 2, 7, 9 |
| `UiToDsp::SetAcarsEnabled` + 3 `DspToUi` variants | Task 3 |
| `AppState` additions | Task 10 |
| `controller::DspState::acars_bank` | Task 5 |
| Lifecycle: engage / disengage / failure rollback / source-stop / app quit / startup replay | Task 5, 7, 9, 11, 12 |
| Config keys (`acars_enabled`, `acars_channel_set`, `acars_recent_keep_count`) | Task 4 |
| ~700–800 LOC PR sizing | Plan target — verify on push |
| `AcarsEnableError` | Task 1 |
| Headless test infrastructure | Task 13 |

If a spec requirement is missing from this table, stop and add a task before implementation.

---

## Implementation notes

- **Fresh subagent per task** is the recommended execution mode. Each task is self-contained; the implementer doesn't need to read prior task code (the plan repeats every constant + signature it needs).
- **No backwards-compat shims.** Per CLAUDE.md / port-fidelity memory: don't add deprecated-rename re-exports or feature-flag guards for the new types. They land or they don't.
- **Don't invent error variants.** The error set in `AcarsEnableError` is locked at 4 variants per Task 1. If the implementer hits a 5th failure mode, stop and discuss with the user before adding a variant — premature taxonomy is a mess to revisit.
- **Sub-project 3 hooks.** The spec assumes sub-project 3 will surface toasts on `AcarsEnabledChanged(Err)`, render the message ring in a viewer, and add the Aviation activity entry. None of that is in this plan. If you find yourself writing a GTK widget, you've drifted into sub-project 3.
