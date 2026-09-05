# Orbcomm Activity Panel — Design (epic #867, Orbcomm slice)

Date: 2026-09-05
Status: approved (design discussion in-session)

## Goal

Give Orbcomm a first-class, approachable home in the UI: a dedicated
**left activity-bar panel** that owns the decoder enable toggle, live
channel health, a "what am I hearing" per-spacecraft list, a
packet-type breakdown, and the raw packet / reassembled-message log —
all in one place. This is the Orbcomm slice of epic #867 ("dedicated
per-satellite-type surfaces"); APT/LRPT/SSTV surfaces stay future work.

Second half — "finish the decode": surface everything `sdr-orbcomm`
already decodes but the UI currently discards or under-shows
(per-spacecraft **velocity** and **satellite-reported time**,
**packet-type counts**, reassembled **messages**), without interpreting
the proprietary application-level payloads and without inventing
spacecraft names.

## Motivation / context

Today Orbcomm lives in a floating `adw::Window` (`orbcomm_viewer.rs`,
`Ctrl+Shift+O`) plus a "Heard via Orbcomm" group grafted onto the
already-overloaded Satellites activity panel. The user's direction
(from the #865 smoke session and this brainstorm): a new **activity-bar
entry** makes it obvious what the user is doing, puts all Orbcomm
functionality in one panel for less-experienced users, and makes it
easy to just *start* Orbcomm. The working precedent for "one mode, one
place" is the Aviation activity (which hosts the ACARS viewer).

## Decode boundary (what "finish the decode" can and cannot do)

Established from a full inventory of `sdr-orbcomm` (see the crate for
authority; key facts captured here so implementation needs no
re-research):

**Decoded and identity-bearing (carry `sat_id`):**

- `OrbcommPacket::Sync { code, sat_id }`
- `OrbcommPacket::Ephemeris(Ephemeris)` where `Ephemeris` =
  `{ sat_id: u8, sat_time_unix: i64, lat_deg, lon_deg, alt_m, vel_ms: f64 }`.
  (The three ECEF velocity components are decoded internally but
  collapsed to the scalar `vel_ms` magnitude — only the magnitude is
  retained.) Ephemeris is range-gated: implausible altitude
  (outside 400–1000 km) or time-of-week ≥ one week falls back to a raw
  `Other` packet.

**Decoded but opaque (raw bytes, no further structure):**

- `OrbcommPacket::Other { packet_type, bytes }` — this is where
  `Message`, `UplinkInfo`, `DownlinkInfo`, `Network`, `Fill`, `Orbital`
  all land. **These carry no `sat_id`.**

**Reassembly:** `OrbcommEventKind::MessageComplete { bytes, partial }`
stitches consecutive `Message`-type fragments (per-channel, one
in-flight sequence at a time, identity-free). Payload stays
application-encoded binary — rendered as hex + printable-ASCII, never
interpreted.

**Hard V1 walls (stated in-UI, not worked around):**

1. Completed messages have **no `sat_id`** → they appear in the log,
   **not** attributed to any spacecraft in the By-Spacecraft list.
2. No real spacecraft **name** exists in the downlink — spacecraft show
   as `Sat 0xNN` via `sat_label`. Matching self-broadcast ephemeris
   against an Orbcomm TLE set for real names is the V2 path (#866).

**Consequence: `sdr-orbcomm` is not modified by this work.** The
DSP↔UI message set is unchanged (`SetOrbcommEnabled`, `OrbcommEvent`,
`OrbcommChannelStats`, `OrbcommEnabledChanged`). All changes are in
`sdr-ui` plus the activity-bar append and `window.rs` wiring.

## Architecture

### 1. Activity-bar entry

Append one entry to `LEFT_ACTIVITIES` in
`crates/sdr-ui/src/sidebar/activity_bar.rs` (append-only — existing
`name` strings are config keys and must not move):

```rust
ActivityBarEntry {
    name: "orbcomm",
    icon_name: "network-cellular-signal-excellent-symbolic", // finalize during impl; must be distinct from find-location (Satellites) and the wireless/signal icons already in the bar
    display_name: "Orbcomm",
    shortcut_label: "Ctrl+9",
    accelerator: "<Ctrl>9",
}
```

This is the **9th** left activity (current last is `aviation` at
`Ctrl+8`). Shortcut registration, help-dialog row, click-handler state,
and session persistence (`ui_sidebar_left_selected`) all derive from
the slice automatically.

In `window.rs::build_layout`, add the stack child:
`left_stack.add_named(&orbcomm_panel.widget, Some("orbcomm"))`, and add
a `connect_orbcomm_panel(panels, state)` call in
`connect_sidebar_panels` matching the existing per-panel pattern.

### 2. Panel widget — `crates/sdr-ui/src/sidebar/orbcomm_panel.rs` (new)

**Deliberate deviation from the panel-layout convention, documented in
the module header:** activity panels are normally an `AdwPreferencesPage`
of flat `AdwPreferencesGroup`s. This panel is a **data surface**, not a
controls page, and it hosts a scrolling log that must `vexpand` to fill
remaining height. An `AdwPreferencesPage` self-scrolls, so nesting a
scrolling log inside it produces nested-scroll conflict. The panel is
therefore a custom vertical `GtkBox`: a compact dashboard region on top
(fixed height, using `AdwPreferencesGroup`-styled sections for chrome
consistency where practical) and the packet log filling the rest.

Top → bottom:

1. **Enable toggle** — the "Decode" switch. Ack-driven exactly as the
   current window: state is set only from `OrbcommEnabledChanged`, never
   optimistically; a `suppress_switch_notify` re-entrancy guard wraps
   the programmatic `set_active`. (Lift the current window's
   `apply_enabled_ack` / switch-wiring logic verbatim.)
2. **Channel activity** — the 9 `ORBCOMM_CHANNELS_HZ` channels as a
   **3×3 `GtkGrid`** of compact cells (freq in MHz + `N ok / M err`),
   which fits the narrow docked panel far better than a 9-wide
   horizontal strip. Fed from `OrbcommChannelStats`. Out-of-span
   channels get `dim-label` + `set_sensitive(false)` (existing
   convention). Cell index → `ORBCOMM_CHANNELS_HZ` index mapping is the
   authoritative one; `ChannelStats.freq_hz` remains the source of truth
   for the displayed frequency once a stats entry exists (same guard as
   `refresh_channel_strip` today).
3. **By Spacecraft** — one row per heard `sat_id` (from `Sync` /
   `Ephemeris` only). Columns/fields: `Sat 0xNN`, last-heard age,
   position (`lat`/`lon`/`alt`), **velocity**, **sat-time (UTC)**,
   packet count. Most-recently-heard first; ages out at 20 min. This
   **absorbs and replaces** the "Heard via Orbcomm" group in
   `satellites_panel.rs`.
4. **Packet-type breakdown** — session counts per `PacketType`
   (Sync / Message / UplinkInfo / DownlinkInfo / Network / Fill /
   Ephemeris / Orbital), plus repaired + checksum-fail totals. Answers
   "what am I actually getting" at a glance.
5. **Packet / message log** — scrolling monospace `TextView`,
   oldest→newest, bounded ring (`MAX_LOG_ENTRIES`), auto-follow to
   bottom when the user is already at bottom. `MessageComplete` renders
   as the hex + printable-ASCII hexdump (the #865 "payoff" view). This
   is the current window's log, moved into the panel. Full 16-byte rows
   become legible when the user widens the sidebar via the existing drag
   handle.

### 3. Shared render module — `crates/sdr-ui/src/orbcomm_render.rs` (new)

Lift the pure, already-tested formatting helpers out of
`orbcomm_viewer.rs` into a shared module the panel consumes:
`format_packet_row`, `format_packet_line`, `format_ephemeris_line`,
`format_message_complete`, `format_hexdump`, `format_hex_inline`,
`format_lat`, `format_lon`, `format_utc_hms`, `packet_type_name`, and
the unit constants. Move their `tests.rs` alongside. No behavior change
— this is a pure move so the panel doesn't re-derive formatting.

### 4. Enriched heard-state — `crates/sdr-ui/src/sidebar/satellites_heard.rs`

Extend the per-spacecraft `Entry` (currently `position` +
`last_heard`) to retain `vel_ms: Option<f64>` and
`sat_time_unix: Option<i64>`, and surface them on `HeardRow`. `record`
gains velocity/time parameters (populated from `Ephemeris`, left `None`
for `Sync`-only sightings). The 20-min `HEARD_EXPIRY_SECS` aging and
most-recent-first `rows()` ordering are unchanged. This module keeps its
unit tests; add cases for velocity/time retention and for a `Sync`-only
entry leaving them `None`.

### 5. Packet-type tally (UI-side)

A small session counter (on `AppState` beside `orbcomm_channel_stats`)
tallies parsed packets by `PacketType` as `OrbcommEvent`s arrive in
`on_orbcomm_event`. This is pure UI classification from the event
stream — no decoder change. Checksum-fail / repaired totals are summed
from the latest `ChannelStats` slice (the decoder already tracks those;
they are not per-type because rejects happen before parse).

### 6. Wiring migration

- `window/dsp_events/orbcomm_events.rs`: the three handlers
  (`on_orbcomm_event`, `on_orbcomm_channel_stats`,
  `on_orbcomm_enabled_changed`) retarget from the window's
  `ViewerHandles` to the panel's handle struct (packet log + channel
  grid + by-spacecraft list + type-breakdown labels + enable switch).
- `AppState`: `orbcomm_viewer_window` is removed; `orbcomm_viewer_handles`
  becomes the panel's handle struct (built once at panel construction,
  lives for the app lifetime — no open/close teardown, unlike a window).
- `orbcomm_viewer.rs` window scaffolding is deleted; the
  `orbcomm-open` action (`Ctrl+Shift+O`) is re-pointed to **select the
  Orbcomm activity** (activate the left bar entry) rather than open a
  window, preserving the existing shortcut.
- `satellites_panel.rs`: remove the "Heard via Orbcomm" group (moved
  into the Orbcomm panel).

## Data flow

```text
DSP: orbcomm_decode_tap ──DspToUi::OrbcommEvent────────┐
     (unchanged)         ──DspToUi::OrbcommChannelStats─┤
                         ──DspToUi::OrbcommEnabledChanged┤
                                                         ▼
UI: window/dsp_events/orbcomm_events.rs
     ├── on_orbcomm_event ──► append to packet log (orbcomm_render)
     │                    ──► HeardSatellites.record (pos+vel+time)  ──► By-Spacecraft list
     │                    ──► PacketType tally                       ──► type breakdown
     ├── on_orbcomm_channel_stats ──► 3×3 channel grid + fail/repair totals
     └── on_orbcomm_enabled_changed ──► ack-drive enable switch
```

## Behavioral decisions

- **Reset on disable:** the packet-type tally and channel counters reset
  when the decoder is disabled (fresh each session). The By-Spacecraft
  list uses its existing 20-min age-out rather than a hard reset.
- **Panel handles built once:** unlike the window (open/close lifecycle),
  the panel exists for the app lifetime, so there is no handle
  teardown/rebuild path and no weak-ref window slot.
- **Log bounding:** the packet log keeps the current `MAX_LOG_ENTRIES`
  bounded ring so a multi-hour session can't grow UI memory without
  bound.

## Error handling

Unchanged from today: bank-init failure surfaces once via a
`DspToUi::Error`; the enable switch tracks the `OrbcommEnabledChanged`
ack (refused if the scanner is running or decim-force fails). Checksum
failures are counters, never log spam.

## Testing

TDD on the pure/logic pieces:

- Shared render module: keeps its lifted `tests.rs` (pure move — tests
  must pass unchanged before and after the move).
- `satellites_heard.rs`: new cases for velocity/time retention from
  `Ephemeris`, `None` for `Sync`-only, and unchanged aging/order.
- Packet-type tally: counts by type from a synthetic `OrbcommEvent`
  sequence; reset-on-disable.
- Channel-grid indexing: 9 channels map to the 3×3 grid in
  `ORBCOMM_CHANNELS_HZ` order; out-of-span dimming.

GTK panel wiring is smoke-tested by the user per the standard workflow
(`make install` with `--release` + a checklist); Claude does not launch
the binary.

## Non-goals (V1)

- Any change to `sdr-orbcomm` (decoder is untouched).
- Interpreting proprietary application-level message payloads.
- Real spacecraft names / ephemeris↔TLE matching (#866, V2).
- Per-satellite message attribution (messages carry no `sat_id`).
- Extending the activity-bar restructure to APT/LRPT/SSTV (rest of #867).
- Persisting heard spacecraft or the log across sessions; JSONL/UDP
  export.

## Files touched

New:
- `crates/sdr-ui/src/sidebar/orbcomm_panel.rs` — the panel.
- `crates/sdr-ui/src/orbcomm_render.rs` — lifted render helpers + tests.

Modified:
- `crates/sdr-ui/src/sidebar/activity_bar.rs` — append `orbcomm` entry.
- `crates/sdr-ui/src/window.rs` — stack child + `connect_orbcomm_panel`.
- `crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs` — retarget handlers.
- `crates/sdr-ui/src/sidebar/satellites_heard.rs` — enrich `Entry` / `HeardRow`.
- `crates/sdr-ui/src/sidebar/satellites_panel.rs` — remove "Heard via Orbcomm" group.
- `crates/sdr-ui/src/state.rs` — panel handles + type tally; drop window slot.
- `crates/sdr-ui/src/orbcomm_viewer.rs` — deleted (helpers moved; window retired).

## Sequencing note

Epic #867 flagged that this work touches `window.rs` wiring the
refactor-era queue (#845–#847, #818–#820) was about to move. Those
refactors have since merged, so the path is clear.
