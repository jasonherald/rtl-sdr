# ACARS (VHF Aircraft Datalink) Reception — Design

**Date:** 2026-04-28
**Issue:** [#474](https://github.com/jasonherald/rtl-sdr/issues/474) (epic)
**Status:** Approved by user, ready for implementation planning

## Goal

Receive and decode VHF ACARS (Plain Old ACARS) text messages from aircraft on the six US frequencies simultaneously, surfacing them in a streaming log inside a new "Aviation" sidebar activity.

## Decisions made during brainstorming

1. **Multi-channel from day one** (vs single-channel-first). Hardware supports 2.5 MSps over the 2.425 MHz US ACARS cluster (129.125–131.550 MHz centered at 130.3375 MHz — the midpoint of the channel extremes). Lays the IQ-fork → N-decimator → N-demod pattern that future ADS-B / VDL2 / Iridium epics will reuse.

   > **Geometry note (corrected 2026-04-28).** An earlier draft of this spec said 2.4 MSps centered on 130.45 MHz. That doesn't fit: the cluster span (131.550 − 129.125 = 2.425 MHz) is wider than the 2.4 MHz Nyquist window, so 129.125 MHz would fall outside ±1.2 MHz at any center. The Task 7 implementer caught this when verifying the test config and corrected to 2.5 MSps + 130.3375 MHz, which gives ±1.2125 MHz offsets that fit comfortably in ±1.25 MHz Nyquist. RTL-SDR supports 2.5 MSps stably; that's the canonical airband-mode rate.
2. **Hybrid decomposition** (vs DSP-first or walking-skeleton). Sub-project 1 ships the full multi-channel `sdr-acars` crate plus a CLI validation tool. Sub-projects 2 and 3 wire it into the live pipeline and UI. Build the right architecture once.
3. **Source-tap with airband lock** (vs takeover or always-on). When ACARS is on, the dongle is locked to airband config (2.5 MSps, 130.3375 MHz center) and the existing radio chain stays functional inside the airband window for voice listening. VFO is fully disabled while ACARS is on (chosen over clamp-and-toast for simplicity).
4. **Streaming log viewer** (vs aircraft-grouped tab). v1 ships a chronological floating "ACARS" window opened from the Aviation activity. Aircraft grouping is deferred — see deferred-items list.
5. **Activity placement.** New "Aviation" activity (airplane icon) in the left sidebar. Future ADS-B / VDL2 / Iridium each get their own viewer windows under the same Aviation activity.

## Architecture overview

```text
                          ┌────────────────────────────────────────┐
RTL-SDR @ 2.5 MSps        │ sdr-acars (NEW crate)                  │
center 130.3375 MHz       │  ┌──────────────────────────────────┐  │
        │                 │  │ ChannelBank (N channel oscs +    │  │
        ▼                 │  │  decimators, source 2.5M → IF)   │  │
   Source IQ ─────────────┤  └────────┬───┬───┬───┬───┬──────┬──┘  │
        │                 │           ▼   ▼   ▼   ▼   ▼      ▼     │
        │                 │       ┌─────────────────────────────┐  │
        ▼                 │       │ MskDemod ×N (PLL + matched  │  │
   IqFrontend             │       │  filter — port of msk.c)    │  │
   (FFT, decim=1 in       │       └────────┬─┬─┬─┬─┬──────┬─────┘  │
    airband mode)         │                ▼ ▼ ▼ ▼ ▼      ▼        │
        │                 │       ┌─────────────────────────────┐  │
        │                 │       │ FrameParser ×N (state mach, │  │
        │                 │       │  parity + CRC + FEC)        │  │
        ├─[TAP]─acars_decode_tap──┴────────┬┬┬┬┬───────────┬────┘  │
        │                 │                ▼▼▼▼▼           ▼       │
        ▼                 │       AcarsMessage stream (per-chan)   │
   VFO (disabled when     └────────────────┬───────────────────────┘
        ACARS on)                          │
        │                                  ▼
        ▼                       UiToDsp / DspToUi channel
   RadioModule                              │
        │                                   ▼
        ▼                       ┌────────────────────────────────────────┐
   Audio sink                   │ sdr-ui                                 │
   (works in airband)           │   Aviation activity (✈ icon)           │
                                │     → AcarsPanel (toggle + summary)    │
                                │     → ACARS viewer window (log)        │
                                └────────────────────────────────────────┘
```

## Crate layout

| Crate | Status | Role |
|---|---|---|
| `sdr-acars` | **NEW** | DSP + frame parser + label name table. Pure logic, no GTK, no rtlsdr dependency. Ports `original/acarsdec/{msk.c, acars.c, label.c, syndrom.h}`. |
| `sdr-acars` (bin `sdr-acars-cli`) | **NEW** | Takes a WAV or IQ file, prints decoded messages in acarsdec text format. Diff-test harness against C reference. |
| `sdr-core` / `sdr-pipeline` | **MODIFIED** (sub-project 2) | Controller gains `acars_decode_tap` analogous to `lrpt_decode_tap`. |
| `sdr-radio` | unchanged | RadioModule stays single-VFO. ACARS doesn't go through it. |
| `sdr-ui` | **MODIFIED** (sub-project 3) | New `aviation_panel.rs` + `acars_viewer.rs`, AppState fields, config keys. |
| `sdr-config` | unchanged | New keys via the existing API. |

## Sub-project 1 — `sdr-acars` crate + CLI

### Module structure

```text
crates/sdr-acars/
  Cargo.toml
  src/
    lib.rs           — public API (re-exports + entry types)
    channel.rs       — IQ-fork: source-rate IQ → N narrowband IF streams
    msk.rs           — MskDemod: narrowband IQ → bits (PLL + matched filter)
    frame.rs         — FrameParser: bits → AcarsMessage (state machine)
    crc.rs           — CRC-CCITT-16 verification
    syndrom.rs       — parity-error FEC table + correction logic
    label.rs         — label code → human-readable name
    error.rs         — AcarsError variants (thiserror — library-crate rule)
    bin/
      sdr-acars-cli.rs  — WAV / IQ file → acarsdec-format text output
  tests/
    e2e_acarsdec_compat.rs  — diff-test against acarsdec on test.wav + recorded IQ
```

### Public API

```rust
pub struct ChannelBank { /* per-channel oscillators + decim state + msk + frame */ }

impl ChannelBank {
    pub fn new(source_rate_hz: f64, center_hz: f64, channels: &[f64])
        -> Result<Self, AcarsError>;

    /// Hot path. Drains all channels' state forward. Decoded messages are
    /// emitted via `on_message` to avoid allocation when nothing decoded.
    pub fn process<F: FnMut(AcarsMessage)>(&mut self, iq: &[Complex32], on_message: F);

    pub fn channels(&self) -> &[ChannelStats];
}

pub struct ChannelStats {
    pub freq_hz: f64,
    pub last_msg_at: Option<SystemTime>,
    pub msg_count: u32,
    pub level_db: f32,
    pub lock_state: ChannelLockState,  // Idle | Signal | Locked
}

pub struct AcarsMessage {
    pub timestamp: SystemTime,
    pub channel_idx: u8,
    pub freq_hz: f64,
    pub level_db: f32,
    pub error_count: u8,        // bytes corrected by FEC
    pub mode: u8,                    // ASCII mode byte (e.g. b'2')
    pub label: [u8; 2],
    pub block_id: u8,
    pub ack: u8,
    pub aircraft: ArrayString<8>,    // ".N12345" — leading dot per protocol
    pub flight_id: Option<ArrayString<7>>,
    pub message_no: Option<ArrayString<5>>,
    pub text: String,                // up to ~220 chars
    pub end_of_message: bool,        // ETX vs ETB
}
```

### Scope of the port (v1 vs deferred)

| Item | acarsdec source | v1? |
|---|---|---|
| MSK demod (PLL + matched filter) | `msk.c` ~138 LOC | ✅ |
| Bit timing recovery (Gardner-style PLL) | inline in `msk.c` | ✅ |
| Frame state machine + parity check | `acars.c` ~250 LOC | ✅ |
| CRC-CCITT-16 verify | `acars.c` ~30 LOC | ✅ |
| **FEC parity correction** (syndrom + fixprerr/fixdberr) | `acars.c` + `syndrom.h` ~400 LOC | ✅ |
| **Multi-channel parallel decode** (the IQ-fork pattern) | `air.c` | ✅ |
| Label name lookup (code → "Crew message") | `label.c` `Lbl[]` table, ~150 entries | ✅ — port the `Lbl[]` table verbatim as a static `phf::Map` or const slice |
| Per-label field parsers (~40, extracting structured fields) | `label.c` ~340 LOC | ❌ deferred |
| Output formatters (JSON, MQTT, network feeders) | `output.c`, `netout.c` | ❌ deferred |

### CLI binary

```text
sdr-acars-cli original/acarsdec/test.wav                 # WAV input
sdr-acars-cli --iq capture.cs16 --rate 2500000 \         # IQ input
              --center 130337500 \
              --channels 131.550,131.525,130.025,130.425,130.450,129.125
```

Output **byte-for-byte matches `acarsdec`'s text mode** (header + Mode/Label/Aircraft/Flight/MsgNo lines + body) modulo volatile fields, which are stripped before diffing. The volatile field set is: wall-clock timestamp, signal level (`L:` field), error count (`E:` field), and the per-channel sequence number (`#N` in the header). All other bytes — Mode, Label, Aircraft, Flight, Block ID, MsgNo, ACK, body, ETX/ETB suffix — must match exactly. That diff is the acceptance test for the port.

### Test plan

- Unit tests per module: synthetic MSK tones, hand-crafted frame bytes, CRC roundtrip, syndrom table lookups.
- Property tests for CRC and parity helpers.
- Integration test (`tests/e2e_acarsdec_compat.rs`): runs `sdr-acars-cli` and the C `acarsdec` on `original/acarsdec/test.wav`, strips volatile fields, asserts byte-equal output.
- Multi-channel test: synthesize a 2.5 MSps IQ buffer with two MSK signals at known offsets; confirm both channels decode their respective messages independently with no cross-talk.

### PR sizing

One PR. Estimated ~2,000 LOC of Rust + tests, all in the new crate, no churn elsewhere. Reviewer focuses on faithfulness of the port without UI/pipeline distractions.

## Sub-project 2 — Pipeline integration + airband lock

### Tap point

`acars_decode_tap` runs **post-IqFrontend, pre-VFO** (analog of the existing `lrpt_decode_tap` in `crates/sdr-core/src/controller.rs:705`, but at source rate instead of post-VFO 144 ksps). When ACARS is enabled the IqFrontend's decimation factor is forced to 1 so the post-IqFrontend buffer carries the full 2.5 MHz of source IQ.

### Airband-lock mechanism

When `acars_enabled = true`, the DSP thread enforces:

| Setting | Locked value |
|---|---|
| Source sample rate | 2.5 MSps |
| Source center frequency | 130.3375 MHz |
| `IqFrontend` decimation | 1 (pass-through) |
| VFO | **Fully disabled** (greyed in UI) |

Toggle ON snapshots the prior `(source_rate, center_freq, vfo_freq, source_type)`. Toggle OFF restores them.

**Source-type gate.** ACARS toggle is disabled (greyed) for non-RTL-SDR sources, with tooltip *"ACARS reception requires an RTL-SDR source."* Switching source while ACARS is active auto-disables ACARS and shows a one-line toast.

### New messages on the UiToDsp / DspToUi channels

```rust
// UiToDsp (existing enum, new variant):
SetAcarsEnabled(bool),

// DspToUi (existing enum, new variants):
AcarsMessage(Box<sdr_acars::AcarsMessage>),
AcarsChannelStats(Box<[ChannelStats; 6]>),   // ~1 Hz: lock state, lvl, msg count
AcarsEnabledChanged(Result<bool, AcarsEnableError>),  // ack including failure cause
```

### AppState additions

```rust
// crates/sdr-ui/src/state.rs
acars_enabled: Cell<bool>,
acars_recent: RefCell<VecDeque<AcarsMessage>>,   // bounded ring (500)
acars_total_count: Cell<u64>,
acars_channel_stats: RefCell<[ChannelStats; 6]>,
acars_viewer_window: RefCell<Option<glib::WeakRef<adw::Window>>>,
acars_pre_lock_state: RefCell<Option<PreLockSnapshot>>,  // saved state for restore on toggle off
```

DSP-side:

```rust
// controller::DspState
acars_bank: Option<sdr_acars::ChannelBank>,  // None when disabled
```

### Lifecycle

| Trigger | DSP thread | UI thread |
|---|---|---|
| Toggle ON | Snapshot prior config; force airband config; instantiate ChannelBank; tap fires every block | Set airband-lock; disable VFO + source/rate controls |
| Toggle ON failure | Drop ChannelBank attempt; emit `AcarsEnabledChanged(Err)` | Show error toast; revert toggle to off; log via tracing |
| Toggle OFF | Drop ChannelBank; restore snapshotted config | Clear airband-lock; re-enable controls |
| Source-type changed while on | Auto-disable ACARS (treat as user toggle off) | Show one-line toast explaining auto-disable |
| App quit while ACARS on | Drop ChannelBank cleanly via existing Drop path | Persist `acars_enabled = true` to config |
| App startup with `acars_enabled = true` | After DSP ready, attempt enable; on failure, clear persisted flag | Show error toast on first paint if failure |

### Persistence (config keys)

```text
acars_enabled                   bool, default false
acars_channel_set               enum string, default "us-6" (only value supported in v1)
acars_recent_keep_count         u32, default 500 (no UI exposure in v1)
```

No persistence of decoded messages in v1 — they live in the bounded in-memory ring. Persistent message logging is a deferred sub-project.

### PR sizing

One PR. Estimated ~700–800 LOC across `sdr-core/src/controller.rs`, `sdr-ui/src/state.rs`, `sdr-ui/src/window.rs`, the DSP message enums, source/freq-control sites for the lock, and config keys.

## Sub-project 3 — Aviation activity + ACARS viewer window

### Sidebar registration

One new entry in `LEFT_ACTIVITIES` (`crates/sdr-ui/src/sidebar/activity_bar.rs`):

```rust
ActivityBarEntry {
    name: "aviation",
    display_name: "Aviation",
    icon_name: "airplane-mode-symbolic",  // verify availability in Adwaita
    shortcut: "<Ctrl>7",                  // next free Ctrl+N slot
    config_key: "ui_sidebar_left_aviation_open",
}
```

### AcarsPanel structure

`crates/sdr-ui/src/sidebar/aviation_panel.rs` — two flat `AdwPreferencesGroup`s, no expanders (per CLAUDE.md convention):

**Group 1 — ACARS:**

- `AdwSwitchRow` "Enable ACARS" → drives `SetAcarsEnabled`
- `AdwActionRow` status: "Decoded 1,247 · Last: 12s ago" (subtitle live-updated, ~4 Hz)
- Button "Open ACARS Window"

**Group 2 — Channels** (read-only status):

Per-channel `AdwActionRow`s showing `<glyph> <freq> <msg count> <level dB> <last msg time>`. Glyphs:

```text
●  Locked   — receiving valid frames within last 30s
○  Idle     — no signal detected
⚠  Signal   — RF energy present but no valid frames decoded
```

Legend strip directly under the group caption.

Updates throttled to 1 Hz via `DspToUi::AcarsChannelStats`.

### ACARS viewer window

`crates/sdr-ui/src/acars_viewer.rs` — `adw::Window`, transient on the main window. Same lifecycle pattern as APT/LRPT viewers.

**Header bar:**

- Pause/Resume toggle (pause buffers — see lifecycle below)
- Clear button (empties both ListStore and AppState ring; doesn't disable ACARS)
- Filter entry (live substring match on aircraft + label + text)
- Status label: "1,247 / 1,247 messages" (filtered / total)

**Content:**

`GtkColumnView` with seven columns: `Time | Freq | Aircraft | Mode | Label | Block | Text`.

- Backed by `GListStore` of `glib::Object` wrappers around `AcarsMessage`.
- Filter: `GtkFilterListModel` + `GtkCustomFilter` for substring match. Re-evaluates on every keystroke.
- Label column shows `H1 (Crew)` style with full label name from the lookup table; tooltip on hover for the longer description.
- Text column truncates with ellipsis; hover/click reveals full text.

**Lifecycle of a single decoded message:**

```text
DSP thread: ChannelBank emits AcarsMessage
  → controller dispatches DspToUi::AcarsMessage(boxed)
  → main loop receives on glib channel
  → AppState handler:
      • push to acars_recent ring (drop oldest if full)
      • increment acars_total_count
      • if viewer window open AND not paused: append to GListStore
      • if viewer paused: leave in ring; resume drains the gap
      • update sidebar status row subtitle (throttled, ~4 Hz)
```

### Persistence (sub-project 3 contributions)

```text
acars_enabled                   already covered in sub-project 2
ui_sidebar_left_aviation_open   standard activity-panel persistence
```

### PR sizing

One PR. Estimated ~700–1,000 LOC: `aviation_panel.rs` (~250), `acars_viewer.rs` (~400), AppState wiring + `window.rs` handlers (~150), activity-bar entry (~10), config keys + tests (~50).

## Edge cases (consolidated)

1. **`ChannelBank::new` failure.** DSP thread sends `AcarsEnabledChanged(Err)`; UI shows toast, reverts toggle, doesn't apply airband lock. Logged via `tracing::error!` with cause.
2. **Startup with persisted `acars_enabled = true`.** Auto-resume after DSP ready signal. On failure, clear persisted flag and show error toast on first paint.
3. **Source-type switch while ACARS on.** Auto-disable ACARS, show one-line toast. ACARS toggle is disabled for non-RTL-SDR sources with tooltip explanation.
4. **Saved-state restore on toggle-off.** Toggle ON snapshots `(source_rate, center_freq, vfo_freq, source_type)`. Toggle OFF restores them. Controls are locked while ACARS is on, so no in-flight user changes to reconcile.
5. **Rapid toggle.** `SetAcarsEnabled(x)` is idempotent on the DSP side. UI trusts `AcarsEnabledChanged` ack to update visible state.
6. **Multi-block messages.** ACARS messages > ~220 chars span multiple blocks marked `ETB` instead of `ETX`. v1 displays each block as its own row with `block_id` shown — readers can mentally chain. Reassembly is deferred (issue filed).
7. **CPU/memory budget.** 6 parallel MSK demods at 12.5 ksps + decimation runs comfortably under 5% of one core (acarsdec runs on a Pi 3). Ring buffer = 500 messages × ~500 bytes = ~250 KB. Negligible.

## Deferred items (filed as issues under epic #474)

| Issue | Topic | Source pointer |
|---|---|---|
| [#577](https://github.com/jasonherald/rtl-sdr/issues/577) | Per-label field parsers (~40 from `label.c`) extracting structured fields per label | acarsdec `label.c` |
| [#578](https://github.com/jasonherald/rtl-sdr/issues/578) | Output formatters / network feeders (JSON file log, airframes.io feeding) | acarsdec `output.c`, `netout.c` |
| [#579](https://github.com/jasonherald/rtl-sdr/issues/579) | Aircraft-grouped viewer tab (collapsed-rows-per-tail-number, expandable) | this spec, Section 1 option B |
| [#580](https://github.com/jasonherald/rtl-sdr/issues/580) | Multi-block message reassembly (ETB → ETX chaining) | this spec, edge case 6 |
| [#581](https://github.com/jasonherald/rtl-sdr/issues/581) | International channel-set support (Europe, configurable lists) | hardcoded `us-6` in v1 |
| [#582](https://github.com/jasonherald/rtl-sdr/issues/582) | ADS-B integration / aircraft enrichment (cross-correlate tail numbers) | future epic |

## Decomposition summary

| # | Title | Crate(s) touched | Estimated LOC | Acceptance |
|---|---|---|---|---|
| 1 | `sdr-acars` crate + CLI | new `sdr-acars` | ~2,000 | `sdr-acars-cli test.wav` byte-equals `acarsdec test.wav` (volatile fields stripped) |
| 2 | Pipeline integration + airband lock | `sdr-core`, `sdr-pipeline`, `sdr-ui` (state, messages) | ~700 | Toggle ACARS in headless test → DSP loop produces `AcarsMessage`s; airband lock honored end-to-end |
| 3 | Aviation activity + ACARS viewer window | `sdr-ui` (panels, viewer, config) | ~900 | Manual smoke (per workflow): viewer opens, messages stream, pause/clear/filter work, panel persists across restart |

Total epic: ~3,600 LOC across 3 PRs.

## References

- Research doc: `docs/research/07-acars-aviation-datalink.md`
- C reference: `original/acarsdec/` (cloned from <https://github.com/TLeconte/acarsdec>)
- LRPT integration pattern (closest in-tree analog): `crates/sdr-core/src/controller.rs:705` (`lrpt_decode_tap`)
- Activity-bar pattern: `crates/sdr-ui/src/sidebar/activity_bar.rs` (epic #420 redesign)
- Project conventions: `CLAUDE.md`
