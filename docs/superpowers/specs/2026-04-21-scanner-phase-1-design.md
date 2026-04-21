# Scanner Phase 1 — Sequential Scanner Design

**Issue**: #317 (Phase 1 of scanner epic #316)
**Status**: Design + plan approved; execution in progress (PR 1 of 3 filed as #368)
**Date**: 2026-04-21

---

## Goal

Ship the 80% use case: retune through N scannable favorites in sequence, dwell on each long enough to evaluate squelch, stop-and-play on squelch-open, resume sequencing when the transmission ends and a hang window elapses. Classic police-scanner behavior built on top of the existing single-source pipeline — no new DSP infrastructure required for Phase 1.

Phase 2 (simultaneous channelizer) and Phase 3 (hybrid band-grouped) build on the config / state / UI plumbing this PR establishes. Not in scope here.

---

## Architecture

### New crate: `sdr-scanner`

A pure state machine with zero I/O, zero threading, no GTK, no audio, no USB. Consumes events, emits commands. Mirrors the `AutoBreakMachine` pattern established in #273 — trivially unit-testable in isolation, which pays for itself on first correctness bug.

**Workspace entry**:

```text
crates/sdr-scanner/
  Cargo.toml
  src/
    lib.rs
    scanner.rs         -- state machine + Scanner struct
    channel.rs         -- ScannerChannel + ChannelKey types
    events.rs          -- ScannerEvent enum
    commands.rs        -- ScannerCommand enum
    state.rs           -- ScannerState enum (Idle / Retuning / Dwelling / Listening / Hanging)
```

Dependencies (workspace-pinned): `sdr-types` (for `DemodMode`), `sdr-radio` (for `CtcssMode`, `VoiceSquelchMode`), `thiserror`. No GTK, no tokio, no I/O crates. Workspace lints inherited.

### Integration via `sdr-core::DspController`

`DspController` owns a `Scanner` instance. On every IQ block arrival, the controller feeds it `SampleTick { samples, sample_rate_hz }`. On every squelch edge (already emitted for Auto Break), the controller feeds `SquelchEdge(SquelchState::{Open,Closed})`. UI commands (enable/disable scanner, update channel list, lockout channel) arrive via new `UiToDsp::*` variants and are forwarded to the scanner. The scanner's emitted commands (retune, mute, active-channel-changed) are applied by the controller — `source.set_center_freq`, `sink_manager.set_scanner_mute`, and `DspToUi::ScannerActiveChannelChanged` event emission respectively.

The scanner **does not** own the source or the sink — it tells the controller what to do. This keeps the crate boundary clean and lets the same state machine run in the Mac-side FFI controller if/when that path wants a scanner.

---

## State machine

### Events in (inputs)

```rust
pub enum ScannerEvent {
    /// Fired on every IQ block from the source. `samples_consumed` is the
    /// block length; `sample_rate_hz` lets the scanner convert dwell/hang
    /// targets in ms to sample counts. No wall-clock used anywhere.
    SampleTick { samples_consumed: u32, sample_rate_hz: u32 },

    /// Edge-triggered squelch transition, already emitted by the DSP
    /// controller for Auto Break transcription. Scanner consumes the
    /// same stream.
    SquelchEdge(SquelchState),

    /// User added / removed / edited a scannable bookmark. Scanner
    /// snapshots the new list and resumes from a sensible position.
    ChannelsChanged(Vec<ScannerChannel>),

    /// Master scanner on/off toggle.
    SetEnabled(bool),

    /// Session-scoped lockout (not persisted to config or bookmarks).
    LockoutChannel(ChannelKey),
    UnlockoutChannel(ChannelKey),
}

pub enum SquelchState { Open, Closed }
```

### Commands out (outputs)

```rust
pub enum ScannerCommand {
    /// Retune the source to this channel. Controller translates to
    /// source.set_center_freq(freq_hz), demod/bandwidth/ctcss/vsq updates
    /// on the radio module.
    Retune {
        freq_hz: u64,
        demod_mode: DemodMode,
        bandwidth: f64,
        ctcss: Option<CtcssMode>,
        voice_squelch: Option<VoiceSquelchMode>,
    },

    /// Gate audio output at the sink. DSP chain keeps running so squelch
    /// state stays live; only the final PCM stream to the audio device
    /// is silenced.
    MuteAudio(bool),

    /// UI-facing: which channel (if any) the scanner currently considers
    /// the "active" channel. None during Idle, Some during all other
    /// phases. Emitted on every phase transition so the UI can update
    /// frequency selector / spectrum / status bar / demod dropdown.
    ActiveChannelChanged(Option<ChannelKey>),

    /// UI-facing: scanner phase indicator ("Scanning" / "Listening" /
    /// "Hanging" / "Off"). Emitted on every phase transition.
    StateChanged(ScannerState),
}
```

### Phases

```text
                +--------+
                |  Idle  |
                +---+----+
                    | SetEnabled(true) + channel list non-empty
                    v
            +-------+-------+
            |   Retuning    |<--------------------+
            |   target_idx  |                     |
            +-------+-------+                     |
                    | sample_counter >= SETTLE_MS |
                    v                             |
            +-------+-------+                     |
            |   Dwelling    |                     |
            |   idx         |                     |
            +-------+-------+                     |
                    | squelch OPEN after settle   |
                    v                             |
            +-------+-------+                     |
            |   Listening   |                     |
            |   idx         |                     |
            +-------+-------+                     |
                    | squelch CLOSE               |
                    v                             |
            +-------+-------+                     |
            |    Hanging    |                     |
            |    idx        |                     |
            +-------+-------+                     |
           hang     |           squelch OPEN      |
          elapsed   |           before hang end   |
       (next ch.)   |           (back to Listen)  |
                    +-----------------------------+

          dwell elapsed silently → next channel (Retuning)
```

### Phase behaviors

**`Idle`**: Scanner off, or enabled but no channels. Mute off (user might be listening to manually-tuned audio). No retunes, no commands emitted except state/active-channel transitions on entry.

**`Retuning { target_idx, samples_until_settled }`**: Emit `Retune(...)` command on entry, `MuteAudio(true)`, `ActiveChannelChanged(Some(target))`, `StateChanged(Retuning)`. Count down `SETTLE_MS`-worth of samples. Ignore all `SquelchEdge` events during this window — retune transients produce spurious squelch behavior we don't want to act on. Transition to `Dwelling` when settled.

**`Dwelling { idx, samples_until_timeout }`**: Audio still muted. Listen for `SquelchEdge(Open)` to transition to `Listening`. If `DEFAULT_DWELL_MS` (or channel's `dwell_ms_override`) elapses without an open, hop to next channel via `Retuning`.

**`Listening { idx }`**: Emit `MuteAudio(false)` on entry, `StateChanged(Listening)`. No sample counter running — we stay here as long as the squelch holds open. On `SquelchEdge(Closed)` transition to `Hanging`.

**`Hanging { idx, samples_until_timeout }`**: Emit `MuteAudio(true)` on entry (user said cut immediately on squelch close — classic scanner). `StateChanged(Hanging)`. Count down hang window. If squelch reopens before timeout → back to `Listening`. If hang elapses → `Retuning` to next channel.

### Priority cycling (single tier)

Scanner maintains a `hop_counter: u32` incremented on every `Retuning → Dwelling` transition. Every `PRIORITY_CHECK_INTERVAL` hops (constant, 5), the next rotation pass inserts all `priority >= 1` channels before resuming normal rotation. No interruption of active `Listening` — that's deferred to follow-up #365.

Example with channels [A(p=0), B(p=1), C(p=0), D(p=0), E(p=1), F(p=0)] and interval=5:
- Normal hops (priority 0 only): A → C → D → F → A → C → D → F → A → ... every hop on the outer loop.
- After 5 normal hops, insert a priority sweep: A → C → D → F → A → **B → E** → C → D → F → ...

The scanner internally maintains the rotation index in two sub-lists (`normal_indices`, `priority_indices`) rather than one, advancing them independently. Simple implementation.

### Channel lockout

`LockoutChannel(key)` adds the key to an in-memory `HashSet<ChannelKey>`. The rotation step skips locked channels. `UnlockoutChannel(key)` removes it. State resets on `SetEnabled(false)` or when `ChannelsChanged` fires (a channel gone from the bookmark list can't be locked out of a list that doesn't include it anymore).

### Edge cases

- **Channel list becomes empty while scanning**: transition to `Idle`, emit `ActiveChannelChanged(None)`, `MuteAudio(false)`, `StateChanged(Idle)`. User's manual frequency / mode is preserved (the controller doesn't un-tune on scanner shutdown).
- **Active channel removed**: if user deletes the currently-`Listening`-on bookmark, treat as squelch close → move to `Hanging` with shortened hang? Or just transition directly to next `Retuning`? Pick the latter — cleaner model, no surprise silence after a bookmark delete.
- **All remaining channels locked out**: transition to `Idle` with a `ScannerCommand::EmptyRotation` signal (UI can toast "All scannable channels are locked out").
- **SampleTick before Retune acknowledged**: scanner accumulates the tick but emits no command. SETTLE_MS accumulates from the first tick after entering `Retuning`.
- **SquelchEdge during Retuning**: ignored (documented; covered by a specific unit test).

---

## UI sync contract

Scanner retunes must update every sync surface a bookmark recall already touches. When `DspController` applies a scanner-emitted `Retune` it also emits `DspToUi::ScannerActiveChannelChanged { freq_hz, demod_mode, bandwidth, name }` to the UI event loop.

UI handler for this event fans out to:
- `FrequencySelector::set_frequency(freq_hz)` — no callback fire
- `spectrum_handle.set_center_frequency(freq_hz as f64)` — updates plot axis + VFO marker
- `status_bar.update_frequency(freq_hz as f64)` + `update_demod(label, bandwidth)`
- `demod_dropdown.set_selected(idx)` — with a `suppress_demod_notify: Cell<bool>` guard on AppState mirroring the existing `suppress_bandwidth_notify` pattern (#342)
- `radio_panel.bandwidth_row.set_value(bandwidth)` — existing `suppress_bandwidth_notify` guard handles this
- `panels.bookmarks.active_bookmark` is **not** updated — scanner is a separate mode from manual recall, the active-bookmark highlight in the flyout should stay on the last manually-selected channel (if any) so the user knows where they were before scanning. UI shows scanner-active state via the scanner panel's "active channel" label, not by highlighting in the bookmark flyout.

During rapid retune hops (~100 ms apart), the frequency selector and spectrum will flick visibly through each channel — exactly what classic scanners do. During `Listening`, the display parks on the active channel until hang elapses.

---

## Scanner ↔ recording / transcription mutex

Scanner is mutually exclusive with recording and transcription. Activating scanner stops any active session of either. Activating recording or transcription stops scanner. Symmetric mutex at the controller level.

Rationale: per-hit recording and channel-tagged transcription are deferred Phase 2+ concerns. For Phase 1, users who want to scan don't need mid-scan recordings (and vice versa). Cleaner to make the mutex explicit than leave recording/transcription running on a frequency that's about to retune 100 ms later.

UI behavior:
- Scanner master switch ON → recording button grays out (with "Scanner active" tooltip). If a recording session was active, stop it with a toast *"Recording stopped — scanner activated"*. Same for transcription.
- Recording button click while scanner is on → toast *"Stop scanner to record"* and no-op (or offer a "Stop scanner and start recording" flow — defer to UX iteration).
- Same pattern reversed: transcription start while scanner is on is rejected with toast guidance.

Implementation: new `DspController` state field `scanner_active: bool`, gated inside the existing `start_recording`, `start_transcription` handlers and the new `UiToDsp::SetScannerEnabled` handler. Emission of `DspToUi::ScannerStateChanged` lets the UI gray/ungray the mutex'd controls.

---

## Schema changes to `Bookmark`

Extend the existing `Bookmark` struct with four scanner-related fields. All optional / defaulted for backward compat — any existing `bookmarks.json` file deserializes cleanly.

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    // ... existing fields ...

    /// Include this bookmark in the scanner's rotation when scanner is on.
    /// Default false so existing bookmarks don't suddenly start getting
    /// scanned without explicit opt-in.
    #[serde(default)]
    pub scan_enabled: bool,

    /// Priority tier. 0 = normal, 1 = priority (checked more often).
    /// Higher tiers reserved for later phases.
    #[serde(default)]
    pub priority: u8,

    /// Per-channel dwell override in ms. None → scanner uses its configured
    /// default (100 ms at ship). Useful for bands where PLL settle is slower
    /// or user wants longer sampling.
    #[serde(default)]
    pub dwell_ms_override: Option<u32>,

    /// Per-channel hang override in ms. None → scanner default (2000 ms).
    /// Useful for channels with irregular transmission patterns.
    #[serde(default)]
    pub hang_ms_override: Option<u32>,
}
```

Scanner projects each scannable bookmark into a `ScannerChannel`:

```rust
pub struct ScannerChannel {
    pub key: ChannelKey,           // (name, freq_hz) — sole owner of freq + name
    pub demod_mode: DemodMode,
    pub bandwidth: f64,
    pub ctcss: Option<CtcssMode>,
    pub voice_squelch: Option<VoiceSquelchMode>,
    pub priority: u8,
    pub dwell_ms: u32,             // resolved: override or UI default
    pub hang_ms: u32,
}

pub struct ChannelKey {
    pub name: String,
    pub frequency_hz: u64,
}
```

Frequency is NOT duplicated on `ScannerChannel` — it lives only on `key`, so identity (used for lockout + active-channel tracking) and the retune target can't drift apart. `ScannerChannel::frequency_hz()` is an accessor on the struct that reads through to the key.

Projection happens in `sdr-ui` at scanner-start and on `ChannelsChanged` pushes. Scanner is decoupled from bookmark persistence format; future non-bookmark channel sources slot in at the same projection boundary.

Lockout state lives inside the scanner (`HashSet<ChannelKey>`), never serialized.

---

## UI surface

### Sidebar scanner panel (bottom of left sidebar)

Rough layout (Adwaita widgets):

```text
┌─ Scanner ──────────────────────────┐
│  [•] Scanner On/Off      (Switch)  │
│                                    │
│  Active: 98.1 MHz — FM Broadcast   │
│  State: Listening                  │
│                                    │
│  ▽ Settings                        │
│    Default dwell:   [100] ms       │
│    Default hang:    [2000] ms      │
│                                    │
│  [Lockout current channel]         │
└────────────────────────────────────┘
```

Master switch drives `UiToDsp::SetScannerEnabled`. Active channel + state labels update from `DspToUi::ScannerActiveChannelChanged` + `ScannerStateChanged`. Settings expander holds the two default sliders (persisted via new config keys `scanner_default_dwell_ms`, `scanner_default_hang_ms`). Lockout button appears visible only when state is `Listening` or `Hanging` — locks out the active channel for the session.

Panel goes at the **bottom** of the existing left sidebar (below Display panel). Layout redesign deferred per user — this placement is intentionally provisional.

### Bookmark row additions

In the right-side bookmarks flyout (#361), each `AdwActionRow` grows two suffix widgets:

- **Scan checkbox** (`gtk4::CheckButton`, small/compact css class) — binds to `bookmark.scan_enabled`, toggles via the same `save_bookmarks` path used for other bookmark edits.
- **Priority star** (`gtk4::ToggleButton` with `starred-symbolic` / `non-starred-symbolic` icon swap) — toggles priority between 0 and 1 (Phase 1 single-tier).

Both are persistent bookmark-level settings. Saving re-writes the bookmarks.json and calls `UpdateScannerChannels` so the running scanner sees the change immediately.

Accessibility labels on both controls ("Include in scanner" / "Scanner priority channel"), matching the icon-only control pattern established in #340 and reinforced in #361.

### Header bar

No header-bar indicator in v1. The sidebar panel is the canonical scanner display surface. Follow-up if user wants a persistent header presence.

---

## Defaults

All hardcoded module-level constants in `sdr-scanner`:

```rust
pub const DEFAULT_DWELL_MS: u32 = 100;
pub const DEFAULT_HANG_MS: u32 = 2000;
pub const SETTLE_MS: u32 = 30;                 // not user-configurable
pub const PRIORITY_CHECK_INTERVAL: u32 = 5;    // not user-configurable
```

Slider ranges in UI:
- Default dwell: 50–500 ms
- Default hang: 500–5000 ms

Rationale docstrings on each constant tying the value to scanner-retune timing reality (I2C command ~1-5ms + PLL lock ~10-20ms + squelch estimate ~30-50ms).

---

## PR split

### PR 1 — `sdr-scanner` crate (engine only)

- New crate scaffolding (Cargo.toml, workspace entry, lints).
- `Scanner`, `ScannerChannel`, `ChannelKey`, `ScannerEvent`, `ScannerCommand`, `ScannerState` types.
- Complete state machine with unit tests (12+ cases per testing strategy below).
- Zero integration, zero dependencies on `sdr-core` or `sdr-ui`.

Estimate: ~600 lines including tests. One commit per logical piece (types, state machine, priority sweep, lockout, edge cases).

### PR 2 — `DspController` integration + bookmark schema

- Extend `Bookmark` with 4 new fields + backward-compat tests.
- New `UiToDsp` variants: `SetScannerEnabled(bool)`, `UpdateScannerChannels(Vec<ScannerChannel>)`, `LockoutScannerChannel(ChannelKey)`, `UnlockoutScannerChannel(ChannelKey)`. Timing defaults are UI-side only — slider changes re-project bookmarks into `ScannerChannel`s with resolved `dwell_ms` / `hang_ms` and dispatch `UpdateScannerChannels`; the scanner itself has no "set default" event.
- New `DspToUi` variants: `ScannerActiveChannelChanged { ... }`, `ScannerStateChanged(ScannerState)`.
- `DspController` adds `scanner: Scanner` field, wires sample ticks + squelch edges + UI commands into it, applies scanner-emitted commands.
- Mutex enforcement: `SetScannerEnabled(true)` stops recording + transcription with toasts. Recording / transcription start rejected when scanner is active.
- Smoke test via debug-build log inspection — scanner can be toggled, retunes happen, mute works, state transitions emit.

Estimate: ~500 lines. One commit per logical piece (schema, new enum variants, controller integration, mutex).

### PR 3 — UI surface + wiring

- New `sidebar/scanner_panel.rs` with the layout above. Master switch, active-channel + state labels, settings expander, lockout button.
- Existing `sidebar/bookmarks_panel.rs` / `navigation_panel.rs`: add Scan checkbox + priority star to each bookmark row. Persistence via existing `save_bookmarks` path. Project bookmarks → `Vec<ScannerChannel>` on change, dispatch `UpdateScannerChannels`.
- UI handlers for `ScannerActiveChannelChanged` / `ScannerStateChanged` — fan out to frequency selector / spectrum / status bar / demod dropdown / bandwidth row with re-entrancy guards (`suppress_demod_notify` added mirror of `suppress_bandwidth_notify`).
- Config persistence for `scanner_default_dwell_ms`, `scanner_default_hang_ms`.
- Toast messages for scanner/recording/transcription mutex events.
- Closes #317.

Estimate: ~800 lines including the UI panel + row extensions. Multi-commit, smoke-tested by user before merge.

---

## Testing strategy

### PR 1 unit tests (target ~12–14 covering the state machine)

1. `idle_stays_idle_with_no_channels_enabled` — turning scanner on with empty list is a no-op.
2. `enable_with_channels_transitions_to_retuning` — sets first channel as target.
3. `settle_window_ignores_squelch_open` — feed `SquelchEdge(Open)` during `Retuning`; state must stay `Retuning`.
4. `post_settle_squelch_open_transitions_to_listening` — open after settle → `Listening`, `MuteAudio(false)` emitted.
5. `dwell_elapsed_without_squelch_advances_to_next_channel` — dwell samples accumulate past `DEFAULT_DWELL_MS * rate / 1000`, no squelch open → `Retuning` next channel.
6. `squelch_close_in_listening_enters_hanging_and_mutes` — `MuteAudio(true)` emitted immediately on squelch close.
7. `squelch_reopen_before_hang_end_returns_to_listening` — un-mute re-emitted.
8. `hang_elapsed_without_reopen_advances_to_next_channel` — hop counter increments.
9. `priority_sweep_triggers_every_five_hops` — after 5 normal hops, priority channels scheduled next.
10. `lockout_skips_channel_in_rotation` — locked channel doesn't appear in `Retuning` sequence.
11. `all_channels_locked_transitions_to_idle_with_signal` — drains rotation → `Idle` + toast signal.
12. `channel_removed_mid_listening_transitions_to_retuning_next` — active-channel vanish recovers cleanly.
13. `disable_while_listening_transitions_to_idle_with_mute_release` — scanner off clears mute, emits `ActiveChannelChanged(None)`.
14. `dwell_ms_override_on_channel_respected` — channel-specific override used instead of default.

### PR 2 integration verification

No automated integration tests (would need a full DSP harness). Smoke-test path:
- Debug-build log at scanner enable, expect `Retune` dispatched.
- Manual bookmark table with 2–3 entries, toggle `scan_enabled` via in-memory edit, toggle scanner on, observe log sequence.
- Verify mutex: start recording → try to enable scanner → expect rejection or recording-stop.

### PR 3 smoke test (manual)

User runs real scanner workflow:
- Populate 5–10 scannable bookmarks across 2m / FM broadcast / NOAA.
- Enable scanner. Verify: frequency selector flickering through channels; spectrum center updating; state indicator cycling Scanning / Listening / Hanging appropriately; audio silent during retune, active during Listening.
- Mark one channel as priority, observe it gets checked more often.
- Lockout a channel while it's listening, verify next cycle skips it.
- Verify recording + transcription disabled while scanner is on.

---

## Out of scope (explicitly)

- Per-hit recording (per-channel WAV files with timestamp filenames).
- Per-hit transcription log with channel tagging.
- Band/category-based scanning (use existing bookmarks only; category filter is a later enhancement).
- Multi-tier priority (priority-1 + priority-2 with separate intervals).
- Priority interrupt during listening — filed as follow-up #365.
- Scheduled scanning (scan only at certain times).
- Multi-dongle scanning.
- Trunked radio protocols (P25, DMR, etc.) — separate epic.
- Mac-side SwiftUI scanner — file as follow-up once Linux scanner stabilizes.

---

## Risks + mitigations

- **Rapid retune CPU cost**: 100 ms dwell × 10 channels = 10 retunes/sec. Each retune is a single libusb command; negligible CPU. RTL-SDR tolerates this rate well (tested in original librtlsdr). Mitigation: if user scales to 100 channels and hits I/O pressure, the scanner will self-throttle via natural latency of retunes. No pre-emptive limits.
- **Spectrum flicker**: user-requested, matches classic scanner UX. Not a bug.
- **Transient audio pops on mute/unmute edges**: sink already has click-suppression via the audio envelope from #343. Scanner mute piggybacks on it — envelope ramp smooths the transition.
- **Squelch behavior variation across demod modes**: NFM with CTCSS, WFM broadcast, SSB all have different squelch characteristics. Scanner reads the AND-gate output of the existing squelch stack (power + CTCSS + voice), so it inherits whatever the user has configured per-channel. Mitigation: document that mixing CTCSS-gated channels with non-CTCSS in a scan rotation works fine as long as each channel's squelch is set up correctly.
- **Manual retune during scan**: user clicks a frequency on the spectrum while scanner is on. Current design: this would fight the scanner — scanner would retune back on its next hop. Mitigation: manual frequency/demod/bandwidth changes during scan-active should force-disable scanner with a toast *"Scanner stopped — manual tune"*. Add to PR 3 UI wiring.

---

## Follow-up issues (to file after Phase 1 ships)

- Per-hit recording (WAV per hit).
- Per-hit transcription with channel-tagged entries.
- Priority interrupt during listening (already filed as #365).
- Band / category-based scanner channel filtering.
- Mac-side SwiftUI scanner panel (file once Linux scanner is stable).
- Scheduled scanning (file if demand exists).
