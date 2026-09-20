# WEFAX Auto-Catch — Design

**Status:** approved design, pre-implementation
**Parent enhancement issue:** #913
**Depends on:** #911 (WEFAX subcarrier auto-offset, merged) — auto-tuning to a
published fax frequency now lands the subcarrier in the decode window, which
makes unattended tuning correct in the first place.

## Goal

Turn WEFAX from a demod mode the user hand-tunes into a **turn-it-on-and-forget
decoder** (like Orbcomm/ACARS): the user enables it, and the app scans the
receivable HF fax channels, finds one that is actually transmitting a chart,
and decodes a full chart — without hand-tuning, and **without drawing static**.

Three user-facing wins, one cohesive mechanism:
1. A **WEFAX activity tab** you enable, rather than selecting a demod mode.
2. **Auto-scan** across a station's multiple channels to find the one that is
   propagating right now.
3. **No static** — never start an image until a real fax signal is present.

## Non-goals (v1) → deferred to #914

- Per-product schedule timetables ("0600 Surface Analysis"). v1 is block-level.
- FM-sweep confirmation in the detector. v1 is a subcarrier-band energy ratio.
- User-editable / custom stations. v1 is a fixed built-in catalog.
- "Draw a partial chart mid-transmission" toggle. v1 holds for the next chart
  start so the image is complete.
- Day/night band-propagation hint for geo-ranking.
- Embedded thumbnail preview, session "recent captures" list, manual "scan now"
  button in the panel.

## Architecture: a dedicated `WefaxCatcher`, not the scanner engine

The core is a **pure state machine** mirroring the satellite auto-recorder
(`crates/sdr-ui/src/sidebar/satellites_recorder.rs`):

```
WefaxCatcher::tick(now, wefax_state, fax_present, schedule_ctx, saved_tune)
    -> Vec<Action>
```

Pure — inputs in, `Action`s out, no I/O — so the whole control logic is
unit-tested without a GTK harness. A thin `interpret_wefax_action` layer in
`crates/sdr-ui/src/window/` turns each `Action` into `UiToDsp` messages / viewer
calls / toasts, exactly like `window/satellites/recorder.rs`.

**Why not reuse `sdr-scanner`:** the scanner decides "signal present" from the
audio squelch (`SquelchEdge` fed from `if_chain().squelch_open()`) and refuses
to run unless a gating squelch is active. WEFAX is a constant FM subcarrier with
no speech-squelch behavior, so the scanner's dwell/hang/"Listening" logic would
not correspond to "fax present." A dedicated catcher lets the **fax-presence
detector** be the keeper signal directly.

### Flow

```
Idle       — Decode toggle off, or on with nothing to do
Scanning   — rotate candidate channels (geo + schedule prioritized):
             Tune + SetDemodMode(Wefax), dwell ~2-3 s each, watch the detector
   | fax_present fires
Locked     — hold this channel; wait for the decoder's phasing lock
             (WefaxState -> Imaging). If presence drops for ~10 s with no
             imaging -> false lock, back to Scanning
   | WefaxState = Imaging
Imaging    — chart draws; viewer auto-opens; detector keeps it honest
             (sustained presence loss -> abort back to Scanning)
   | chart complete (WefaxState = Stopped / take_chart_complete)
Finalizing — SavePng, then STAY on this channel (proven receivable) for the
             next chart; only re-scan when it goes dead
```

## Components

### 1. Station catalog (`sdr-sat`)

A pure, built-in catalog (mirrors `KNOWN_SATELLITES`), living in `sdr-sat`
alongside the existing coordinate / ground-station machinery:

```rust
pub struct WefaxStation {
    pub name: &'static str,          // "NMG New Orleans"
    pub channels_hz: &'static [u64], // published fax freqs (real RF)
    pub coords: (f64, f64),          // transmitter lat/lon, for geo-ranking
    pub schedule: &'static [DailyWindow], // recurring UTC broadcast windows
}

pub struct DailyWindow { pub start_min_utc: u16, pub end_min_utc: u16 }
```

- **Initial catalog:** NMG New Orleans, NMF Boston, plus a couple of other US
  stations (NMC Point Reyes, NOJ Kodiak). The geo-filter naturally hides the
  far ones for a given user.
- **Geo-filter:** great-circle distance from the user's ground station (already
  configured in the Satellites panel) to each transmitter -> **rank by
  distance**; hide only the truly hopeless (very far). Distance is a
  prioritization heuristic, not a hard gate — the detector makes the final call.
- **Schedule:** block-level recurring daily UTC windows, hardcoded from the
  published NWS/RFAX schedules (like the frequencies). Two jobs: (a) prioritize
  channels whose station is "active now" and skip known-dead periods; (b) drive
  a "next chart ~HH:MMZ" readout. The schedule never *blocks* a catch — it
  focuses the search; the detector still confirms reality.

Pure helpers (unit-tested): `receivable_stations(user_coords) -> ranked Vec`,
`is_active(station, now) -> bool`, `next_window(station, now) -> Option<time>`.

### 2. Fax-presence detector (`sdr-dsp`)

A pure DSP block that answers "is a WEFAX signal on this channel right now?",
run on the same pre-gate WEFAX demod audio the decode tap already uses.

- **Method (v1):** a **subcarrier-band energy ratio** — Goertzel power summed
  across the 1500–2300 Hz band (bins at ~1500/1700/1900/2100/2300) vs power
  outside it, with **hysteresis** (sustained in-band dominance for ~1–2 s to
  declare "present"; drops after a gap). Patterned on the existing Goertzel
  `ToneDetector` in `wefax/tones.rs`. Real fax concentrates energy in that band;
  noise is flat; an off-band carrier fails the ratio.
- **Output:** a boolean surfaced to the UI as a new edge-triggered
  `DspToUi::WefaxPresence(bool)`, so the catcher can watch it. (An FM-sweep
  refinement is deferred; the ratio is sufficient unless a steady 1900 Hz tone
  false-triggers.)

**Separation of concerns:** the *detector* selects the channel ("fax activity
here → commit"); the *decoder's existing phasing lock* starts the image ("a real
chart's opening → draw from the top"). So the gated decoder still guarantees a
full chart, never a mid-chart fragment.

### 3. `WefaxCatcher` state machine (`sdr-ui/src/sidebar/`)

Pure `tick` + `State`/`Action` enums as above. Driven by a glib timer (~1–2 Hz,
like the satellites recorder). `Action`s:

```rust
enum Action {
    Tune(u64),               // center on a candidate channel
    SetDemodMode,            // DemodMode::Wefax
    ResetDecoder,            // clear phasing/image between channels
    OpenViewer,              // on imaging start
    SavePng(PathBuf),        // on chart complete (path via existing helper)
    RestoreTune(SavedTune),  // on disable
    Toast { message, kind },
}
```

Contracts:
- **On enable:** snapshot the user's tune (`SavedTune`), build the prioritized
  candidate list, → Scanning. **On disable:** `RestoreTune`, → Idle.
- **Catcher owns the VFO while enabled** — a manual tune/mode change means the
  user takes Decode off first (same spirit as the scanner refusing geometry
  changes while running).
- **Mutually exclusive with the scanner** — enabling one while the other runs is
  refused with a toast (Orbcomm already does this vs the scanner).
- **Stay on a working channel** post-chart; re-scan only when the detector drops
  for a sustained period.
- **Reset the decoder between channels** so channel A's phasing/partial image
  never bleeds into channel B.
- **Empty rotation:** if all candidates are dead, idle-wait and re-scan
  periodically, respecting the schedule (don't hammer known-dead windows).

### 4. WEFAX activity panel (`sdr-ui/src/sidebar/`)

A new **left-bar activity** (append one `ActivityBarEntry` to `LEFT_ACTIVITIES`
in `activity_bar.rs`) with a fresh accelerator (Ctrl+9 is the current max — use
the next free one, e.g. `Ctrl+0`). Standard `AdwPreferencesPage` /
`AdwPreferencesGroup` layout (not Orbcomm's atypical `ScrolledWindow`):

- **Decode** — the enable `Switch` (mirror `orbcomm_panel::build_enable_group`).
- **Station** — selector: "Auto (nearest receivable)" (default) or pin a
  specific station; list is the geo-ranked catalog with distances shown.
- **Status** — current state (Scanning / Signal found — waiting for chart /
  Imaging / Idle), current station + channel, a signal-present indicator, and
  "next chart ~HH:MMZ".
- **Viewer/captures** — the live `wefax_viewer` auto-opens on imaging start;
  PNGs auto-save on completion via the existing path; an "Open viewer" button.

The demod-mode WEFAX selection stays for manual/power use; the tab is the
turn-on-and-forget path layered on top.

## Data flow

```
DSP thread:  WEFAX decode tap (existing) + NEW fax-presence detector on the
             mono audio -> DspToUi::WefaxState (existing) + WefaxPresence (new)
UI thread:   glib timer -> WefaxCatcher.tick(...) -> Vec<Action>
             -> interpret_wefax_action(deps) -> UiToDsp (Tune / SetDemodMode /
                Reset) + viewer calls + toasts
             -> panel status from WefaxState / WefaxPresence / catcher state
```

## Error handling

- Detector never present / all channels dead → stay Scanning/idle-wait; status
  shows "searching"; no crash.
- Decoder init failure at a rate → existing `wefax_init_failed_at_rate` latch.
- Manual tune/mode while catcher running → catcher owns the VFO (see contracts).
- Scanner already running → refuse to enable the catcher (toast), and vice
  versa.

## Testing

- **`WefaxCatcher`** (pure): assert `Action`s for enable→scan, present→locked,
  imaging, complete→save+stay, dead→rescan, false-lock timeout, empty rotation,
  disable→restore. The bulk of the logic.
- **Presence detector** (pure): synthetic fax sweep → present; white noise →
  absent; off-band tone → absent; hysteresis (blip vs sustained). **Real-data
  gate:** the recovered NMF audio → present; a static segment → absent.
- **Catalog/geo/schedule** (pure): distance ranking; "active now"/"next window"
  at a fixed `now`.
- **Panel/GTK wiring:** manual smoke test (the user runs it).

## Wiring checklist (from the existing-code map)

- `crates/sdr-dsp/` — new fax-presence detector block (+ unit tests).
- `crates/sdr-sat/` — `WefaxStation` catalog + geo/schedule pure helpers (+ tests).
- `crates/sdr-core/` — `DspToUi::WefaxPresence(bool)` message; run the detector in
  the WEFAX decode-tap path (`controller/wefax.rs`) and emit it edge-triggered.
- `crates/sdr-ui/src/sidebar/` — `wefax_catcher.rs` (state machine) + `wefax_panel.rs`.
- `crates/sdr-ui/src/sidebar/activity_bar.rs` — new `LEFT_ACTIVITIES` entry + accelerator + persistence keys.
- `crates/sdr-ui/src/sidebar/mod.rs` — `WefaxPanel` field + `build_panels`.
- `crates/sdr-ui/src/window/layout.rs` — `left_stack.add_named(&panels.wefax.widget, Some("wefax"))`.
- `crates/sdr-ui/src/window.rs` — `connect_wefax_panel`.
- `crates/sdr-ui/src/window/wefax/catcher.rs` — `interpret_wefax_action` + a glib-timer driver.
- `crates/sdr-ui/src/state.rs` — `AppState` fields for the catcher + panel handles.

The `WefaxDecoder` DSP is unchanged — the gated path already does "no draw until
phasing lock."
