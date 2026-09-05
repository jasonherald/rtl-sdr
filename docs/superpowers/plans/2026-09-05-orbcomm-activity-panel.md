# Orbcomm Activity Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the floating Orbcomm viewer window with a dedicated left activity-bar panel that owns the decoder enable toggle, a 3×3 channel-activity grid, a "By Spacecraft" list, a packet-type breakdown, and the raw packet/message log — and surface already-decoded-but-discarded data (per-spacecraft velocity + satellite-time, packet-type counts).

**Architecture:** All work is in `sdr-ui`; `sdr-orbcomm` is not touched and the DSP↔UI message set is unchanged. Pure formatting/logic is extracted into small unit-tested modules (`orbcomm_render`, `orbcomm_tally`, enriched `satellites_heard`); the GTK panel (`sidebar/orbcomm_panel.rs`) is a new activity registered in `LEFT_ACTIVITIES`. The three `DspToUi::Orbcomm*` handlers retarget from the window to panel handles stored on `AppState`.

**Tech Stack:** Rust, GTK4 (`gtk4` crate, `v4_10`), libadwaita (`adw`), the workspace `sdr-orbcomm` decoder crate.

**Spec:** `docs/superpowers/specs/2026-09-05-orbcomm-activity-panel-design.md`

## Global Constraints

- `sdr-orbcomm` is **NOT modified**. No decoder/DSP/message-enum changes.
- No `unwrap()` / `panic!()` / `println!()` in library code; use `tracing`. GTK callbacks fail closed (early-return) rather than panic — match existing `acars_viewer.rs` / `orbcomm_viewer.rs` style.
- Workspace clippy pedantic is enabled. Match the CI invocation exactly before push: `cargo clippy --all-targets --workspace -- -D warnings` (no features).
- Run each cargo gate as its own bare, unpiped Bash command. `cargo fmt --all -- --check` is the **last** gate before any push; any edit after it re-runs it.
- **Never** `git add -A` / `git add .` in this repo (it carries untracked `build/` + `.vscode/`). Stage explicit paths only; verify with `git status --short`.
- Branch is `feature/orbcomm-activity-panel` (already created; spec already committed). One PR. Push once at the end (Task 8); if any interim push happens, **wait for CodeRabbit to post its review for that exact SHA before the next push** (never outrun the rabbit).
- Activity-bar entry `name` strings and `LEFT_ACTIVITIES` order are config-persistence keys — **append only**, never reorder or rename existing entries.
- No `sdr-transcription` changes → the triple-build rule does **not** apply here.
- Author/attribution: commits are authored `Jason Herald <392+jasonherald@users.noreply.github.com>` (repo default; do not override). End each commit message with the two trailer lines shown in Task steps.

---

### Task 1: Extract pure render helpers into `orbcomm_render.rs` (pure move)

Move the GTK-free formatting helpers out of `orbcomm_viewer.rs` so the new panel can reuse them without depending on the window. This is a **pure move** — no behavior change; the moved tests must pass unchanged.

**Files:**
- Create: `crates/sdr-ui/src/orbcomm_render.rs`
- Create: `crates/sdr-ui/src/orbcomm_render/tests.rs` (moved from `orbcomm_viewer/tests.rs`)
- Modify: `crates/sdr-ui/src/orbcomm_viewer.rs` (delete the moved fns/consts; `use crate::orbcomm_render::…`)
- Modify: `crates/sdr-ui/src/lib.rs` (add `pub mod orbcomm_render;`)
- Modify: `crates/sdr-ui/src/window/satellites/heard.rs` (repoint `crate::orbcomm_viewer::format_lat/lon` → `crate::orbcomm_render::format_lat/lon`)
- Delete: `crates/sdr-ui/src/orbcomm_viewer/tests.rs` (its contents move)

**Interfaces:**
- Produces (all in `crate::orbcomm_render`):
  - `pub fn format_packet_row(event: &sdr_orbcomm::OrbcommEvent) -> String`
  - `pub fn format_hexdump(bytes: &[u8]) -> String`
  - `pub(crate) fn format_lat(lat_deg: f64) -> String`
  - `pub(crate) fn format_lon(lon_deg: f64) -> String`
  - `pub(crate) fn format_utc_hms(unix_secs: i64) -> String`
  - `pub(crate) fn channel_label_text(freq_hz: f64, stats: Option<&sdr_orbcomm::ChannelStats>) -> String`
  - `pub(crate) const HEXDUMP_BYTES_PER_ROW: usize = 16;`
  - `pub(crate) const HZ_PER_MHZ: f64 = 1_000_000.0;`
  - `pub(crate) const METERS_PER_KM: f64 = 1_000.0;`
  - private (module-internal): `format_packet_line`, `format_ephemeris_line`, `format_message_complete`, `format_hex_inline`, `packet_type_name`

- [ ] **Step 1: Create `orbcomm_render.rs` with the moved helpers**

Move these items **verbatim** from `orbcomm_viewer.rs` (lines ~35–219 and 450–462): the three consts, `format_packet_row`, `format_packet_line`, `format_ephemeris_line`, `format_lat`, `format_lon`, `format_utc_hms`, `packet_type_name`, `format_hex_inline`, `format_message_complete`, `format_hexdump`, and `channel_label_text`. Change `format_utc_hms` and `channel_label_text` from private to `pub(crate)` (the panel will call them). Add the module header and the test hook:

```rust
//! Pure, GTK-free rendering helpers for the Orbcomm surface: packet /
//! ephemeris / message-complete log lines, the classic hexdump block,
//! coordinate + UTC formatting, and channel-cell label text. Extracted
//! from the former `orbcomm_viewer` window so the Orbcomm activity
//! panel (`sidebar/orbcomm_panel.rs`) reuses one tested formatter set.

use sdr_orbcomm::channelizer::ChannelStats;
use sdr_orbcomm::packet::{Ephemeris, OrbcommPacket, PacketType};
use sdr_orbcomm::{OrbcommEvent, OrbcommEventKind};

// … moved consts + fns here, unchanged bodies …

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
```

- [ ] **Step 2: Move the tests**

`git mv crates/sdr-ui/src/orbcomm_viewer/tests.rs crates/sdr-ui/src/orbcomm_render/tests.rs`. The file opens with `use super::*;` — leave it. Its 10 tests (`hexdump_exact_layout_for_20_bytes`, `hexdump_printable_ascii_gutter`, `ephemeris_row_*`, `sync_row_format`, `other_packet_row_format`, `message_complete_*`) only exercise the moved pure fns, so they compile against `orbcomm_render` unchanged.

- [ ] **Step 3: Trim `orbcomm_viewer.rs` to consume the extracted module**

Delete the moved consts/fns from `orbcomm_viewer.rs`. Its remaining code (`ViewerHandles`, `append_log_entry`, `refresh_channel_strip`, `apply_enabled_ack`, `open_orbcomm_viewer_if_needed`, `connect_orbcomm_action`, `build_*`) references `format_packet_row`, `format_hexdump`, `channel_label_text`, `HZ_PER_MHZ` — repoint those to `crate::orbcomm_render::…`. Remove the now-empty `#[cfg(test)] mod tests;` line at the bottom (the tests moved). Add `pub mod orbcomm_render;` to `crates/sdr-ui/src/lib.rs` near the other `pub mod` viewer declarations.

- [ ] **Step 4: Repoint the heard-subtitle formatter**

In `crates/sdr-ui/src/window/satellites/heard.rs`, change the two `crate::orbcomm_viewer::format_lat` / `format_lon` calls in `format_heard_subtitle` to `crate::orbcomm_render::format_lat` / `format_lon`.

- [ ] **Step 5: Run the moved tests — expect PASS (pure move)**

Run: `cargo test -p sdr-ui orbcomm_render`
Expected: the 10 moved tests PASS. (A pure move keeps them green; if any fail, the move changed behavior — fix the move.)

- [ ] **Step 6: Build + clippy**

Run: `cargo build -p sdr-ui`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Expected: clean.

- [ ] **Step 7: fmt + commit**

Run: `cargo fmt --all -- --check`

```bash
git add crates/sdr-ui/src/orbcomm_render.rs crates/sdr-ui/src/orbcomm_render/tests.rs \
        crates/sdr-ui/src/orbcomm_viewer.rs crates/sdr-ui/src/lib.rs \
        crates/sdr-ui/src/window/satellites/heard.rs
git status --short   # confirm no build/ or .vscode/ staged
git commit -m "$(cat <<'EOF'
refactor(ui): extract Orbcomm render helpers into orbcomm_render (pure move)

Lift the GTK-free formatting helpers (packet/ephemeris/message log
lines, hexdump, coordinate + UTC + channel-cell formatters) out of the
orbcomm_viewer window into a shared orbcomm_render module so the coming
Orbcomm activity panel reuses one tested formatter set. Tests move with
them and pass unchanged.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 2: Enrich the heard-spacecraft model with velocity + satellite-time

`Ephemeris` decodes `vel_ms` and `sat_time_unix`, but `satellites_heard.rs` keeps only position + last-heard. Retain them so the panel's By-Spacecraft row can show speed and the satellite's own clock. TDD — this module is pure and GTK-free.

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/satellites_heard.rs`
- Modify: `crates/sdr-ui/src/sidebar/satellites_heard/tests.rs` (add cases)
- Modify: `crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs` (update the one `record(...)` caller to the new signature — pass `None, None` for now; Task 5 passes real values)

**Interfaces:**
- Produces:
  - `pub struct HeardRow { pub label: String, pub age_secs: u64, pub position: Option<(f64, f64, f64)>, pub vel_ms: Option<f64>, pub sat_time_unix: Option<i64>, pub packet_count: u64 }`
  - `pub fn record(&mut self, sat_id: u8, position: Option<(f64, f64, f64)>, vel_ms: Option<f64>, sat_time_unix: Option<i64>, now: Instant)` (increments `packet_count` on every call, not just position/velocity updates)
  - `pub fn rows(&self, now: Instant) -> Vec<HeardRow>` (unchanged signature; richer rows, `packet_count` copied straight from the entry)

- [ ] **Step 1: Write the failing tests**

Add to `crates/sdr-ui/src/sidebar/satellites_heard/tests.rs`:

```rust
#[test]
fn ephemeris_record_retains_velocity_and_sat_time() {
    let now = Instant::now();
    let mut heard = HeardSatellites::new();
    heard.record(0x2C, Some((51.2, 7.4, 715_000.0)), Some(7450.0), Some(1_600_000_000), now);
    let rows = heard.rows(now);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].vel_ms, Some(7450.0));
    assert_eq!(rows[0].sat_time_unix, Some(1_600_000_000));
}

#[test]
fn sync_only_record_leaves_velocity_and_time_none() {
    let now = Instant::now();
    let mut heard = HeardSatellites::new();
    heard.record(0x2C, None, None, None, now);
    let rows = heard.rows(now);
    assert_eq!(rows[0].vel_ms, None);
    assert_eq!(rows[0].sat_time_unix, None);
    assert_eq!(rows[0].position, None);
}

#[test]
fn sync_after_ephemeris_preserves_last_velocity_and_time() {
    let now = Instant::now();
    let mut heard = HeardSatellites::new();
    heard.record(0x2C, Some((1.0, 2.0, 700_000.0)), Some(7400.0), Some(111), now);
    heard.record(0x2C, None, None, None, now); // Sync beacon after a fix
    let rows = heard.rows(now);
    assert_eq!(rows[0].vel_ms, Some(7400.0));
    assert_eq!(rows[0].sat_time_unix, Some(111));
    assert_eq!(rows[0].position, Some((1.0, 2.0, 700_000.0)));
}
```

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p sdr-ui satellites_heard`
Expected: FAIL to compile (`record` arity wrong; `vel_ms`/`sat_time_unix` fields missing).

- [ ] **Step 3: Implement**

In `satellites_heard.rs`: add `vel_ms: Option<f64>` and `sat_time_unix: Option<i64>` to both the private `Entry` and public `HeardRow`. Update `record`:

```rust
pub fn record(
    &mut self,
    sat_id: u8,
    position: Option<(f64, f64, f64)>,
    vel_ms: Option<f64>,
    sat_time_unix: Option<i64>,
    now: Instant,
) {
    self.entries.retain(|_, entry| {
        now.saturating_duration_since(entry.last_heard).as_secs() < HEARD_EXPIRY_SECS
    });
    let entry = self.entries.entry(sat_id).or_insert(Entry {
        position: None,
        vel_ms: None,
        sat_time_unix: None,
        last_heard: now,
    });
    if position.is_some() {
        entry.position = position;
    }
    if vel_ms.is_some() {
        entry.vel_ms = vel_ms;
    }
    if sat_time_unix.is_some() {
        entry.sat_time_unix = sat_time_unix;
    }
    entry.last_heard = now;
}
```

In `rows`, populate the two new `HeardRow` fields from `entry.vel_ms` / `entry.sat_time_unix`. Update the `HeardRow` doc comments. Keep the existing tests' `record(...)` calls compiling by updating them to the new 5-arg form (`None, None` where they only passed position).

- [ ] **Step 4: Update the DSP-side caller (keep it compiling)**

In `crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs`, `record_heard_satellite` currently calls `record(sat_id, position, Instant::now())`. Change to `record(sat_id, position, None, None, Instant::now())` for now (Task 5 replaces this with real velocity/time).

- [ ] **Step 5: Run — expect PASS**

Run: `cargo test -p sdr-ui satellites_heard`
Expected: all PASS (new + existing).

- [ ] **Step 6: Build + clippy + fmt + commit**

Run: `cargo build -p sdr-ui`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `cargo fmt --all -- --check`

```bash
git add crates/sdr-ui/src/sidebar/satellites_heard.rs \
        crates/sdr-ui/src/sidebar/satellites_heard/tests.rs \
        crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): retain Orbcomm ephemeris velocity + satellite-time per spacecraft

HeardSatellites now keeps vel_ms and sat_time_unix (already decoded, so
far discarded) alongside position, surfaced on HeardRow for the coming
By-Spacecraft panel row. Velocity/time update on an Ephemeris fix and
are preserved across Sync-only beacons.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 3: Packet-type tally module (`orbcomm_tally.rs`)

A pure, unit-tested counter of parsed packets by `PacketType`, plus a formatter for the panel's breakdown label. Checksum-fail / repaired totals are passed in from `ChannelStats` at format time (not tracked here — rejects happen before parse).

**Files:**
- Create: `crates/sdr-ui/src/orbcomm_tally.rs`
- Create: `crates/sdr-ui/src/orbcomm_tally/tests.rs`
- Modify: `crates/sdr-ui/src/lib.rs` (add `pub mod orbcomm_tally;`)

**Interfaces:**
- Produces (all in `crate::orbcomm_tally`):
  - `#[derive(Default)] pub struct OrbcommTally { counts: [u64; 8] }`
  - `pub fn record(&mut self, event: &sdr_orbcomm::OrbcommEvent)`
  - `pub fn reset(&mut self)`
  - `pub fn format_breakdown(&self, checksum_fail: u64, repaired: u64) -> String`

- [ ] **Step 1: Write the failing tests**

`crates/sdr-ui/src/orbcomm_tally/tests.rs`:

```rust
use super::*;
use sdr_orbcomm::packet::{OrbcommPacket, PacketType};
use sdr_orbcomm::{OrbcommEvent, OrbcommEventKind};

fn packet_event(packet: OrbcommPacket) -> OrbcommEvent {
    OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet { packet, repaired: false },
    }
}

#[test]
fn counts_each_packet_type() {
    let mut t = OrbcommTally::default();
    t.record(&packet_event(OrbcommPacket::Sync { code: 0, sat_id: 0x2C }));
    t.record(&packet_event(OrbcommPacket::Sync { code: 0, sat_id: 0x2C }));
    t.record(&packet_event(OrbcommPacket::Other {
        packet_type: PacketType::Message,
        bytes: vec![],
    }));
    let out = t.format_breakdown(0, 0);
    assert!(out.contains("Sync"));
    assert!(out.contains("2"));
    assert!(out.contains("Message"));
}

#[test]
fn message_complete_events_are_not_tallied() {
    let mut t = OrbcommTally::default();
    t.record(&OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::MessageComplete { bytes: vec![1, 2, 3], partial: false },
    });
    // No parsed packet ⇒ all type counts stay zero.
    assert_eq!(t.format_breakdown(0, 0), OrbcommTally::default().format_breakdown(0, 0));
}

#[test]
fn reset_zeroes_counts() {
    let mut t = OrbcommTally::default();
    t.record(&packet_event(OrbcommPacket::Sync { code: 0, sat_id: 1 }));
    t.reset();
    assert_eq!(t.format_breakdown(0, 0), OrbcommTally::default().format_breakdown(0, 0));
}

#[test]
fn breakdown_includes_checksum_fail_and_repaired() {
    let t = OrbcommTally::default();
    let out = t.format_breakdown(7, 3);
    assert!(out.contains("checksum"));
    assert!(out.contains('7'));
    assert!(out.contains("repaired"));
    assert!(out.contains('3'));
}
```

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p sdr-ui orbcomm_tally`
Expected: FAIL (module/type not defined).

- [ ] **Step 3: Implement**

`crates/sdr-ui/src/orbcomm_tally.rs`:

```rust
//! Session packet-type tally for the Orbcomm panel's "what am I
//! getting" breakdown. Pure UI-side classification from the decoded
//! OrbcommEvent stream — the decoder does not emit per-type counts.
//! Checksum-fail / repaired totals live in ChannelStats and are passed
//! in at format time rather than tracked here (rejects occur before a
//! packet is parsed to a type).

use sdr_orbcomm::packet::{OrbcommPacket, PacketType};
use sdr_orbcomm::{OrbcommEvent, OrbcommEventKind};

/// Fixed display order of the eight packet types (index = tally slot).
const TYPE_ORDER: [(PacketType, &str); 8] = [
    (PacketType::Sync, "Sync"),
    (PacketType::Message, "Message"),
    (PacketType::UplinkInfo, "Uplink"),
    (PacketType::DownlinkInfo, "Downlink"),
    (PacketType::Network, "Network"),
    (PacketType::Fill, "Fill"),
    (PacketType::Ephemeris, "Ephemeris"),
    (PacketType::Orbital, "Orbital"),
];

fn type_index(ty: PacketType) -> usize {
    // Small linear match; TYPE_ORDER is the single source of order.
    TYPE_ORDER.iter().position(|&(t, _)| t == ty).unwrap_or(0)
}

#[derive(Default)]
pub struct OrbcommTally {
    counts: [u64; 8],
}

impl OrbcommTally {
    /// Tally one decoded event. Only `Packet` events carry a type;
    /// `MessageComplete` (a reassembly of already-counted Message
    /// packets) is ignored.
    pub fn record(&mut self, event: &OrbcommEvent) {
        let OrbcommEventKind::Packet { packet, .. } = &event.kind else {
            return;
        };
        let ty = match packet {
            OrbcommPacket::Sync { .. } => PacketType::Sync,
            OrbcommPacket::Ephemeris(_) => PacketType::Ephemeris,
            OrbcommPacket::Other { packet_type, .. } => *packet_type,
        };
        self.counts[type_index(ty)] = self.counts[type_index(ty)].saturating_add(1);
    }

    /// Zero every count (called on decoder disable).
    pub fn reset(&mut self) {
        self.counts = [0; 8];
    }

    /// Multi-line breakdown for the panel label. `checksum_fail` and
    /// `repaired` are summed from the latest ChannelStats slice by the
    /// caller.
    #[must_use]
    pub fn format_breakdown(&self, checksum_fail: u64, repaired: u64) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (i, (_, name)) in TYPE_ORDER.iter().enumerate() {
            let _ = writeln!(out, "{name:<9} {}", self.counts[i]);
        }
        let _ = write!(out, "checksum-fail {checksum_fail}  ·  repaired {repaired}");
        out
    }
}

#[cfg(test)]
mod tests;
```

Add `pub mod orbcomm_tally;` to `crates/sdr-ui/src/lib.rs`.

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p sdr-ui orbcomm_tally`
Expected: all PASS.

- [ ] **Step 5: Build + clippy + fmt + commit**

Run: `cargo build -p sdr-ui`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `cargo fmt --all -- --check`

```bash
git add crates/sdr-ui/src/orbcomm_tally.rs crates/sdr-ui/src/orbcomm_tally/tests.rs crates/sdr-ui/src/lib.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): add Orbcomm packet-type tally + breakdown formatter

Pure UI-side classifier counting decoded packets by PacketType from the
event stream, with a multi-line breakdown formatter (checksum-fail /
repaired totals passed in from ChannelStats). Backs the Orbcomm panel's
"what am I getting" section.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 4: Build the Orbcomm panel widget (display-only) + register the activity

Create the panel widget and its runtime handles, register it as the 9th left activity, and add it to the stack. At the end of this task the Orbcomm activity **appears in the bar and shows an (empty) dashboard**, but it is not yet wired to the DSP and the old window still exists. This keeps the tree compiling and lets you smoke-test the layout in isolation.

**Files:**
- Create: `crates/sdr-ui/src/sidebar/orbcomm_panel.rs`
- Modify: `crates/sdr-ui/src/sidebar/mod.rs` (`mod orbcomm_panel;`, re-export, `SidebarPanels.orbcomm` field, `build_panels` wiring)
- Modify: `crates/sdr-ui/src/sidebar/activity_bar.rs` (append `orbcomm` entry to `LEFT_ACTIVITIES`)
- Modify: `crates/sdr-ui/src/window/layout.rs` (`left_stack.add_named` for `"orbcomm"`)

**Interfaces:**
- Consumes: `crate::orbcomm_render::channel_label_text`, `sdr_orbcomm::ORBCOMM_CHANNELS_HZ`, `HeardRow` (Task 2), `OrbcommTally` (Task 3).
- Produces:
  - `pub struct OrbcommPanel { pub widget: gtk4::Box, pub handles: std::rc::Rc<OrbcommPanelHandles> }`
  - `pub struct OrbcommPanelHandles { … }` (fields listed in Step 1)
  - `pub fn build_orbcomm_panel() -> OrbcommPanel`
  - impl methods (used by Task 5): `append_log_entry`, `refresh_channel_grid`, `apply_enabled_ack`, `rebuild_heard_list`, `set_breakdown` (signatures in Step 2)
  - `pub(crate) fn format_heard_subtitle(row: &crate::sidebar::satellites_heard::HeardRow) -> String`

- [ ] **Step 1: Create the panel module — struct + handles + `build_orbcomm_panel`**

Follow `crates/sdr-ui/src/sidebar/display_panel.rs` as the construction template, and lift the switch/log/grid mechanics from the old `orbcomm_viewer.rs` (`build_header_bar`, `build_log_view`, `append_log_entry`, `refresh_channel_strip`, `apply_enabled_ack`). Key differences from the window: the root is a vertical `gtk4::Box` (not an `AdwPreferencesPage`), the channel strip becomes a **3×3 `GtkGrid`**, and it gains By-Spacecraft + breakdown groups.

```rust
//! Orbcomm activity panel (epic #867, Orbcomm slice).
//!
//! Docked left-activity surface that replaces the former floating
//! orbcomm_viewer window: enable toggle, a 3×3 channel-activity grid,
//! a "By Spacecraft" list, a packet-type breakdown, and the raw
//! packet/message log.
//!
//! Layout deviation (deliberate): activity panels are normally an
//! `AdwPreferencesPage` of flat groups. This one is a data surface
//! hosting a scrolling log that must vexpand-fill, and an
//! `AdwPreferencesPage` self-scrolls — nesting a scrolling log inside
//! it fights itself. So the root is a vertical `gtk4::Box`: compact
//! dashboard groups at natural height on top, the packet log
//! (vexpand) filling the rest. Widen the sidebar via the drag handle
//! for full 16-byte hexdump rows.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{glib, gio};
use libadwaita as adw;

use crate::orbcomm_render::{HZ_PER_MHZ, METERS_PER_KM, channel_label_text, format_lat, format_lon, format_utc_hms};
use crate::sidebar::satellites_heard::HeardRow;

const MAX_LOG_ENTRIES: usize = 500;
const SCROLL_BOTTOM_TOLERANCE_PX: f64 = 1.0;

pub struct OrbcommPanelHandles {
    pub enable_switch: gtk4::Switch,
    pub suppress_switch_notify: Cell<bool>,
    /// One label per ORBCOMM_CHANNELS_HZ entry, same order, laid out
    /// row-major in the 3×3 grid.
    pub channel_cells: Vec<gtk4::Label>,
    pub heard_group: adw::PreferencesGroup,
    pub heard_rows: RefCell<Vec<adw::ActionRow>>,
    pub breakdown_label: gtk4::Label,
    pub log_view: gtk4::TextView,
    pub scrolled_window: gtk4::ScrolledWindow,
    pub log_entries: RefCell<VecDeque<String>>,
}

pub struct OrbcommPanel {
    pub widget: gtk4::Box,
    pub handles: Rc<OrbcommPanelHandles>,
}

pub fn build_orbcomm_panel() -> OrbcommPanel {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    // ── Enable toggle ("Decode") ──
    let enable_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    enable_switch.update_property(&[gtk4::accessible::Property::Label("Enable Orbcomm decoding")]);
    let enable_row = adw::ActionRow::builder()
        .title("Decode")
        .subtitle("9 fixed 137 MHz Orbcomm downlink channels")
        .build();
    enable_row.add_suffix(&enable_switch);
    let enable_group = adw::PreferencesGroup::new();
    enable_group.add(&enable_row);

    // ── 3×3 channel grid ──
    let grid = gtk4::Grid::builder().row_spacing(6).column_spacing(12)
        .margin_start(12).margin_end(12).margin_top(6).margin_bottom(6).build();
    let mut channel_cells = Vec::with_capacity(sdr_orbcomm::ORBCOMM_CHANNELS_HZ.len());
    for (i, &hz) in sdr_orbcomm::ORBCOMM_CHANNELS_HZ.iter().enumerate() {
        let label = gtk4::Label::builder()
            .label(channel_label_text(hz, None))
            .justify(gtk4::Justification::Center)
            .build();
        let (col, row) = ((i % 3) as i32, (i / 3) as i32);
        grid.attach(&label, col, row, 1, 1);
        channel_cells.push(label);
    }
    let channel_group = adw::PreferencesGroup::builder().title("Channels").build();
    channel_group.add(&grid);

    // ── By Spacecraft ──
    let heard_group = adw::PreferencesGroup::builder()
        .title("By Spacecraft")
        .description("Spacecraft decoded from the 137 MHz downlink this session.")
        .visible(false)
        .build();

    // ── Packet-type breakdown ──
    let breakdown_label = gtk4::Label::builder()
        .xalign(0.0).monospace(true)
        .margin_start(12).margin_end(12).margin_top(6).margin_bottom(6).build();
    let breakdown_group = adw::PreferencesGroup::builder().title("Packet types").build();
    breakdown_group.add(&breakdown_label);

    // ── Packet / message log ──
    let log_view = gtk4::TextView::builder()
        .editable(false).cursor_visible(false).monospace(true)
        .wrap_mode(gtk4::WrapMode::None)
        .top_margin(4).bottom_margin(4).left_margin(6).right_margin(6).build();
    let scrolled_window = gtk4::ScrolledWindow::builder()
        .child(&log_view).vexpand(true).hexpand(true).build();

    root.append(&enable_group);
    root.append(&channel_group);
    root.append(&heard_group);
    root.append(&breakdown_group);
    root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    root.append(&scrolled_window);

    let handles = Rc::new(OrbcommPanelHandles {
        enable_switch,
        suppress_switch_notify: Cell::new(false),
        channel_cells,
        heard_group,
        heard_rows: RefCell::new(Vec::new()),
        breakdown_label,
        log_view,
        scrolled_window,
        log_entries: RefCell::new(VecDeque::new()),
    });

    OrbcommPanel { widget: root, handles }
}
```

- [ ] **Step 2: Add the handle methods (log append, grid refresh, ack, heard list, breakdown)**

Port `append_log_entry` / `refresh_channel_strip` / `apply_enabled_ack` from `orbcomm_viewer.rs` **verbatim in behavior**, as methods on `OrbcommPanelHandles`, plus two new ones. Add `format_heard_subtitle`:

```rust
impl OrbcommPanelHandles {
    pub fn append_log_entry(&self, entry: &str) {
        // (identical logic to orbcomm_viewer::append_log_entry:
        // was-at-bottom check, push_back + trim to MAX_LOG_ENTRIES,
        // join borrowed entries, set_text, deferred idle scroll)
        // … port body unchanged, using self.scrolled_window /
        //   self.log_entries / self.log_view …
    }

    pub fn refresh_channel_grid(&self, stats: &[sdr_orbcomm::ChannelStats]) {
        for (i, label) in self.channel_cells.iter().enumerate() {
            let Some(s) = stats.get(i) else {
                let hz = sdr_orbcomm::ORBCOMM_CHANNELS_HZ[i];
                label.set_label(&channel_label_text(hz, None));
                continue;
            };
            label.set_label(&channel_label_text(s.freq_hz, Some(s)));
            if s.in_span {
                label.remove_css_class("dim-label");
                label.set_sensitive(true);
            } else {
                label.add_css_class("dim-label");
                label.set_sensitive(false);
            }
        }
    }

    pub fn apply_enabled_ack(&self, enabled: bool) {
        if self.enable_switch.is_active() == enabled {
            return;
        }
        self.suppress_switch_notify.set(true);
        self.enable_switch.set_active(enabled);
        self.suppress_switch_notify.set(false);
    }

    /// Rebuild the By-Spacecraft rows from a HeardRow snapshot.
    pub fn rebuild_heard_list(&self, rows: &[HeardRow], visible: bool) {
        let mut displayed = self.heard_rows.borrow_mut();
        for row in displayed.drain(..) {
            self.heard_group.remove(&row);
        }
        self.heard_group.set_visible(visible);
        for row in rows {
            let action_row = adw::ActionRow::builder()
                .title(&row.label)
                .subtitle(format_heard_subtitle(row))
                .build();
            self.heard_group.add(&action_row);
            displayed.push(action_row);
        }
    }

    pub fn set_breakdown(&self, text: &str) {
        self.breakdown_label.set_label(text);
    }
}

/// One By-Spacecraft subtitle: position · alt · speed · sat-clock · age.
pub(crate) fn format_heard_subtitle(row: &HeardRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some((lat, lon, alt_m)) = row.position {
        parts.push(format!("{} {}", format_lat(lat), format_lon(lon)));
        parts.push(format!("{:.0} km", alt_m / METERS_PER_KM));
    }
    if let Some(v) = row.vel_ms {
        parts.push(format!("{:.2} km/s", v / METERS_PER_KM));
    }
    if let Some(t) = row.sat_time_unix {
        parts.push(format_utc_hms(t));
    }
    parts.push(format!("{}s ago", row.age_secs));
    parts.push(format!("{} pkts", row.packet_count));
    parts.join(" · ")
}
```

(`row.packet_count` renders as `N pkts`, appended after the trailing age.)

Note: `HZ_PER_MHZ` import is used by `channel_label_text` internally; keep the `use` only if referenced (drop unused imports to satisfy clippy).

- [ ] **Step 3: Register the panel in `SidebarPanels`**

In `crates/sdr-ui/src/sidebar/mod.rs`:
- add `mod orbcomm_panel;`
- add `pub use orbcomm_panel::{OrbcommPanel, build_orbcomm_panel};`
- add field `pub orbcomm: OrbcommPanel,` to `SidebarPanels`
- in `build_panels()`, add `let orbcomm = build_orbcomm_panel();` and include `orbcomm,` in the returned struct literal.

- [ ] **Step 4: Append the activity entry**

In `crates/sdr-ui/src/sidebar/activity_bar.rs`, append to `LEFT_ACTIVITIES` (after the `aviation` entry — append only):

```rust
ActivityBarEntry {
    name: "orbcomm",
    icon_name: "network-cellular-signal-excellent-symbolic",
    display_name: "Orbcomm",
    shortcut_label: "Ctrl+9",
    accelerator: "<Ctrl>9",
},
```

(If GTK logs a missing-icon warning at smoke time, fall back to `network-wireless-symbolic` — but confirm it reads distinctly from the Satellites `find-location-symbolic` and the Share `network-transmit-receive-symbolic` icons.)

- [ ] **Step 5: Add the stack child**

In `crates/sdr-ui/src/window/layout.rs`, in `build_panel_stacks` after the `aviation` line:

```rust
left_stack.add_named(&panels.orbcomm.widget, Some("orbcomm"));
```

- [ ] **Step 6: Build + clippy + fmt**

Run: `cargo build -p sdr-ui`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: clean. (`build_orbcomm_panel`'s handle methods are unused until Task 5 — if clippy flags dead_code on a method, that's expected; do NOT add `#[allow]` blindly. They become used in Task 5, so instead reorder: it is acceptable to land Task 4 + Task 5 as one push. If executing inline, proceed to Task 5 before the clippy gate rather than suppressing.)

- [ ] **Step 7: Smoke test (user)**

`make install CARGO_FLAGS="--release"`, then verify: the left activity bar shows a new **Orbcomm** icon (Ctrl+9); clicking it opens a panel with a Decode toggle, a 3×3 channel grid (frequencies, no counts yet), empty "By Spacecraft" (hidden) and "Packet types" sections, and a blank log area. The old Ctrl+Shift+O window still opens (removed next task). No layout breakage of other activities.

- [ ] **Step 8: Commit**

```bash
git add crates/sdr-ui/src/sidebar/orbcomm_panel.rs crates/sdr-ui/src/sidebar/mod.rs \
        crates/sdr-ui/src/sidebar/activity_bar.rs crates/sdr-ui/src/window/layout.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): add Orbcomm activity panel widget + register the activity

New docked left-activity Orbcomm panel (Ctrl+9): Decode toggle, 3×3
channel-activity grid, By-Spacecraft group, packet-type breakdown, and
the packet/message log. Display-only in this commit; DSP wiring and
window retirement follow. Root is a custom vertical Box (documented
deviation) so the log can vexpand-fill.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 5: Wire the panel to the DSP and retire the floating window

Flip Orbcomm from the window + Satellites-panel group to the dedicated panel: swap the `AppState` fields, wire the enable switch + heard-aging tick, rewrite the three DSP handlers to drive the panel, delete the window scaffolding, remove the "Heard via Orbcomm" group from the Satellites panel, and re-point the `Ctrl+Shift+O` action to select the Orbcomm activity.

**Files:**
- Modify: `crates/sdr-ui/src/state.rs` (field surgery)
- Modify: `crates/sdr-ui/src/sidebar/orbcomm_panel.rs` (add `connect_orbcomm_panel`)
- Modify: `crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs` (rewrite handlers)
- Modify: `crates/sdr-ui/src/window.rs` (call `connect_orbcomm_panel`; re-point/relocate `orbcomm-open` action)
- Modify: `crates/sdr-ui/src/window/layout.rs` (add `select_left_activity` helper; call the action wiring with layout handles)
- Modify: `crates/sdr-ui/src/window/satellites.rs` (drop the `wire_heard_group` call)
- Delete: `crates/sdr-ui/src/orbcomm_viewer.rs` and `crates/sdr-ui/src/window/satellites/heard.rs`
- Modify: `crates/sdr-ui/src/lib.rs` (`mod orbcomm_viewer;` removal) and `crates/sdr-ui/src/window/satellites.rs` (`mod heard;` removal)
- Modify: `crates/sdr-ui/src/sidebar/satellites_panel.rs` + `satellites_panel/build.rs` (remove `heard_group`)

**Interfaces:**
- Consumes: `OrbcommPanelHandles` methods (Task 4), `OrbcommTally` (Task 3), enriched `HeardSatellites::record`/`rows` (Task 2).
- Produces:
  - `crate::sidebar::orbcomm_panel::connect_orbcomm_panel(panels: &SidebarPanels, state: &Rc<AppState>)`
  - `crate::window::layout::select_left_activity(name: &str, stack: &gtk4::Stack, bar: &crate::sidebar::activity_bar::ActivityBar, split: &libadwaita::OverlaySplitView)`
  - `AppState.orbcomm_panel_handles: RefCell<Option<Rc<OrbcommPanelHandles>>>`, `AppState.orbcomm_tally: RefCell<OrbcommTally>`

- [ ] **Step 1: `AppState` field surgery**

In `crates/sdr-ui/src/state.rs`:
- Remove fields `orbcomm_viewer_window`, `orbcomm_viewer_handles`, `orbcomm_heard_render` and their `AppState::new` initializers.
- Add:
  ```rust
  pub orbcomm_panel_handles:
      RefCell<Option<Rc<crate::sidebar::orbcomm_panel::OrbcommPanelHandles>>>,
  pub orbcomm_tally: RefCell<crate::orbcomm_tally::OrbcommTally>,
  ```
  Initializers: `orbcomm_panel_handles: RefCell::new(None),` and `orbcomm_tally: RefCell::new(crate::orbcomm_tally::OrbcommTally::default()),`.
- If `Weak` is now unused in `state.rs`, drop the import.

- [ ] **Step 2: `connect_orbcomm_panel` — switch dispatch + heard tick + stash handles**

Append to `orbcomm_panel.rs`:

```rust
use crate::state::AppState;

/// Heard-list aging tick (seconds) — matches the old heard-group tick.
const HEARD_TICK_SECS: u32 = 5;

pub fn connect_orbcomm_panel(panels: &crate::sidebar::SidebarPanels, state: &Rc<AppState>) {
    let handles = Rc::clone(&panels.orbcomm.handles);
    *state.orbcomm_panel_handles.borrow_mut() = Some(Rc::clone(&handles));

    // Enable switch → SetOrbcommEnabled (ack-driven state; guard the
    // programmatic set_active in apply_enabled_ack).
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(&handles);
        handles.enable_switch.clone().connect_active_notify(move |sw| {
            if handles.suppress_switch_notify.get() {
                return;
            }
            state.send_dsp(crate::messages::UiToDsp::SetOrbcommEnabled(sw.is_active()));
        });
    }

    // 5 s heard-aging tick: repaint the By-Spacecraft list so ages
    // advance and expired birds drop even without new packets. The
    // panel lives for the app lifetime, so this never needs to stop.
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(&handles);
        glib::timeout_add_seconds_local(HEARD_TICK_SECS, move || {
            repaint_heard(&handles, &state);
            glib::ControlFlow::Continue
        });
    }
}

/// Rebuild the By-Spacecraft list from the current model + enable flag.
pub(crate) fn repaint_heard(handles: &OrbcommPanelHandles, state: &Rc<AppState>) {
    let rows = state.orbcomm_heard.borrow().rows(std::time::Instant::now());
    let visible = state.orbcomm_enabled.get() && !rows.is_empty();
    handles.rebuild_heard_list(&rows, visible);
}
```

- [ ] **Step 3: Rewrite the three DSP handlers**

Replace the body of `crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs` with panel-driven handlers (drop all `orbcomm_viewer_handles` / `orbcomm_heard_render` references):

```rust
//! Orbcomm-side `DspToUi` handlers: drive the Orbcomm activity panel
//! (packet log, channel grid, By-Spacecraft list, packet-type
//! breakdown, enable-switch ack) plus the pure heard-spacecraft and
//! tally models on `AppState`.

use std::rc::Rc;
use std::time::Instant;

use super::DspEventCtx;
use crate::sidebar::orbcomm_panel::{OrbcommPanelHandles, repaint_heard};
use crate::state::AppState;

pub(super) fn on_orbcomm_event(ctx: &DspEventCtx, event: &sdr_orbcomm::OrbcommEvent) {
    let DspEventCtx { state, .. } = ctx;
    state.orbcomm_tally.borrow_mut().record(event);
    record_heard_satellite(state, event);
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.append_log_entry(&crate::orbcomm_render::format_packet_row(event));
        refresh_breakdown(handles, state);
        repaint_heard(handles, state);
    }
}

fn record_heard_satellite(state: &Rc<AppState>, event: &sdr_orbcomm::OrbcommEvent) {
    use sdr_orbcomm::OrbcommEventKind;
    use sdr_orbcomm::packet::OrbcommPacket;

    let (sat_id, position, vel, time) = match &event.kind {
        OrbcommEventKind::Packet { packet: OrbcommPacket::Sync { sat_id, .. }, .. } => {
            (*sat_id, None, None, None)
        }
        OrbcommEventKind::Packet { packet: OrbcommPacket::Ephemeris(eph), .. } => (
            eph.sat_id,
            Some((eph.lat_deg, eph.lon_deg, eph.alt_m)),
            Some(eph.vel_ms),
            Some(eph.sat_time_unix),
        ),
        _ => return,
    };
    state.orbcomm_heard.borrow_mut().record(sat_id, position, vel, time, Instant::now());
}

pub(super) fn on_orbcomm_channel_stats(
    ctx: &DspEventCtx,
    stats: Box<[sdr_orbcomm::ChannelStats]>,
) {
    let DspEventCtx { state, .. } = ctx;
    let stats = stats.into_vec();
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.refresh_channel_grid(&stats);
        refresh_breakdown(handles, state); // checksum/repaired totals live here
    }
    *state.orbcomm_channel_stats.borrow_mut() = stats;
}

pub(super) fn on_orbcomm_enabled_changed(ctx: &DspEventCtx, enabled: bool) {
    let DspEventCtx { state, .. } = ctx;
    state.orbcomm_enabled.set(enabled);
    if !enabled {
        state.orbcomm_tally.borrow_mut().reset();
    }
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.apply_enabled_ack(enabled);
        refresh_breakdown(handles, state);
        repaint_heard(handles, state);
    }
}

/// Sum checksum-fail + repaired across channels and repaint the
/// packet-type breakdown label.
fn refresh_breakdown(handles: &OrbcommPanelHandles, state: &Rc<AppState>) {
    let (fail, repaired) = state
        .orbcomm_channel_stats
        .borrow()
        .iter()
        .fold((0u64, 0u64), |(f, r), s| (f + s.checksum_fail, r + s.repaired));
    let text = state.orbcomm_tally.borrow().format_breakdown(fail, repaired);
    handles.set_breakdown(&text);
}
```

- [ ] **Step 4: Delete the window; re-point the action**

- Delete `crates/sdr-ui/src/orbcomm_viewer.rs`; remove its `pub mod orbcomm_viewer;` from `lib.rs`.
- In `crates/sdr-ui/src/window/layout.rs`, add the shared selector (also usable by session-restore/click later, but only the action uses it now):

```rust
pub(crate) fn select_left_activity(
    name: &str,
    stack: &gtk4::Stack,
    bar: &crate::sidebar::activity_bar::ActivityBar,
    split: &libadwaita::OverlaySplitView,
) {
    for (n, btn) in &bar.buttons {
        btn.set_active(*n == name);
    }
    stack.set_visible_child_name(name);
    split.set_show_sidebar(true);
}
```

- In `crates/sdr-ui/src/window.rs`, replace the `crate::orbcomm_viewer::connect_orbcomm_action(app, &state);` call (line ~697) with an inline action that selects the activity, using the layout handles already in scope (`left_stack`, `left_activity_bar`, `left_split_view` from the `LayoutHandles` destructure):

```rust
{
    let action = gio::SimpleAction::new("orbcomm-open", None);
    let stack = left_stack.clone();
    let bar_buttons = left_activity_bar.buttons.clone();
    let split = left_split_view.clone();
    action.connect_activate(move |_, _| {
        for (n, btn) in &bar_buttons {
            btn.set_active(*n == "orbcomm");
        }
        stack.set_visible_child_name("orbcomm");
        split.set_show_sidebar(true);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.orbcomm-open", &["<Ctrl><Shift>o"]);
}
```

(Inline rather than via `select_left_activity` because `ActivityBar` isn't `Clone`; capture the `buttons` map — its `ToggleButton`s are cheap glib refs. Keep `select_left_activity` for future callers per the spec, or drop it if executing minimally — it is not otherwise wired here.)

- Add `connect_orbcomm_panel(panels, state)` to `connect_sidebar_panels` in `window.rs` (beside `connect_aviation_panel`): `crate::sidebar::orbcomm_panel::connect_orbcomm_panel(panels, state);`.

- [ ] **Step 5: Remove the "Heard via Orbcomm" group from the Satellites panel**

- Delete `crates/sdr-ui/src/window/satellites/heard.rs` and its `mod heard;` declaration; remove the `heard::wire_heard_group(&panel_weak, state);` call in `crates/sdr-ui/src/window/satellites.rs` (line ~335).
- In `crates/sdr-ui/src/sidebar/satellites_panel/build.rs`: delete `build_heard_group` and its call (`let heard_group = build_heard_group(&page);`), and remove `heard_group` from the returned `SatellitesPanel` literal.
- In `crates/sdr-ui/src/sidebar/satellites_panel.rs`: remove the `heard_group` field from `SatellitesPanel` and `SatellitesPanelWeak`, and the two `downgrade()`/`upgrade()` mappings.

- [ ] **Step 6: Build + clippy**

Run: `cargo build -p sdr-ui`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Expected: clean. Fix any now-unused imports (e.g. `Weak` in state.rs, `format_lat/lon` in the deleted heard.rs path).

- [ ] **Step 7: Run the UI unit tests**

Run: `cargo test -p sdr-ui`
Expected: all PASS (render, tally, heard tests; no test referenced the deleted window).

- [ ] **Step 8: Smoke test (user)**

`make install CARGO_FLAGS="--release"`. Verify with a live or replayed Orbcomm signal (or at least the enable path):
- Ctrl+9 (and Ctrl+Shift+O) both open/select the Orbcomm activity.
- Toggling **Decode** on sends enable; the switch settles from the DSP ack (not optimistically).
- With signal: the 3×3 grid shows per-channel `ok/err`; the log fills with packet/ephemeris lines and message hexdumps; "By Spacecraft" appears with `Sat 0xNN` rows showing position, **km/s**, **sat-clock**, and age; "Packet types" shows per-type counts + checksum-fail/repaired.
- Toggling Decode off zeroes the packet-type breakdown; the Satellites panel no longer shows a "Heard via Orbcomm" group.

- [ ] **Step 9: Commit**

```bash
git add crates/sdr-ui/src/state.rs crates/sdr-ui/src/sidebar/orbcomm_panel.rs \
        crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs crates/sdr-ui/src/window.rs \
        crates/sdr-ui/src/window/layout.rs crates/sdr-ui/src/window/satellites.rs \
        crates/sdr-ui/src/sidebar/satellites_panel.rs \
        crates/sdr-ui/src/sidebar/satellites_panel/build.rs crates/sdr-ui/src/lib.rs
git rm crates/sdr-ui/src/orbcomm_viewer.rs crates/sdr-ui/src/window/satellites/heard.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): wire Orbcomm panel to DSP; retire the floating viewer window

The three DspToUi::Orbcomm* handlers now drive the Orbcomm activity
panel (log, 3×3 channel grid, By-Spacecraft list with velocity +
sat-clock, packet-type breakdown) plus the pure heard/tally models on
AppState. The floating orbcomm_viewer window and the Satellites panel's
"Heard via Orbcomm" group are removed; Ctrl+Shift+O now selects the
Orbcomm activity. sdr-orbcomm is untouched.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 6: Docs — update CLAUDE.md Orbcomm pointers

Keep the architecture guide accurate: the Orbcomm surface is now an activity panel, not a floating window.

**Files:**
- Modify: `/data/source/rtl-sdr/CLAUDE.md` (the `crates/sdr-ui/src/orbcomm_viewer.rs` bullet under "Satellite reception")

- [ ] **Step 1: Edit the CLAUDE.md Orbcomm bullet**

Replace the `orbcomm_viewer.rs` line so it reads (adjust wording to fit the surrounding list):

```text
- `crates/sdr-ui/src/sidebar/orbcomm_panel.rs` — Orbcomm activity panel
  (left bar, Ctrl+9): Decode toggle, 3×3 channel-activity grid,
  By-Spacecraft list (position/velocity/sat-clock/age), packet-type
  breakdown, and the packet/message log. `crates/sdr-ui/src/orbcomm_render.rs`
  holds the shared GTK-free formatters; `orbcomm_tally.rs` the packet-type
  counter. Ctrl+Shift+O selects the activity. (The former floating
  orbcomm_viewer window and the Satellites-panel "Heard via Orbcomm" group
  were folded into this panel.)
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git status --short
git commit -m "$(cat <<'EOF'
docs: point CLAUDE.md at the Orbcomm activity panel

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 7: Full workspace gates

Before pushing, run the full gate set the CI runs, as separate bare commands.

- [ ] **Step 1: Tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 2: Clippy (CI form, no features)**

Run: `cargo clippy --all-targets --workspace -- -D warnings`
Expected: clean.

- [ ] **Step 3: Locked check (Cargo.toml/lock unchanged, but verify)**

Run: `cargo check --workspace --locked`
Expected: clean (no dependency edges were added — this catches accidental drift).

- [ ] **Step 4: deny + audit**

Run: `make lint`
Expected: clean (includes cargo-deny + cargo-audit; if `make lint` re-runs fmt/clippy that's fine).

- [ ] **Step 5: fmt (LAST gate)**

Run: `cargo fmt --all -- --check`
Expected: clean. Any fix here re-runs this step.

---

### Task 8: Push + PR + CodeRabbit

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feature/orbcomm-activity-panel
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create --title "Orbcomm activity panel + finish-the-decode surfacing (epic #867)" --body "$(cat <<'EOF'
Replaces the floating Orbcomm viewer window with a dedicated left
activity-bar panel (Ctrl+9): Decode toggle, 3×3 channel-activity grid,
By-Spacecraft list, packet-type breakdown, and the packet/message log.

"Finish the decode" = surface already-decoded-but-discarded data:
per-spacecraft velocity + satellite-clock, packet-type counts, and the
reassembled-message hexdump. `sdr-orbcomm` is untouched; the DSP↔UI
message set is unchanged. Proprietary message payloads stay raw hex;
spacecraft remain `Sat 0xNN` (real names = TLE matching, #866/V2).

Design: docs/superpowers/specs/2026-09-05-orbcomm-activity-panel-design.md
Plan: docs/superpowers/plans/2026-09-05-orbcomm-activity-panel.md

Part of epic #867 (Orbcomm slice). Removes the Satellites-panel "Heard
via Orbcomm" group (folded into the new panel).

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

- [ ] **Step 3: Wait for CodeRabbit, then address**

Arm a Monitor on the PR for CodeRabbit's posted review (not the walkthrough placeholder) for the pushed SHA. **Do not push again until CR has posted its review for the exact HEAD SHA.** Batch all fixes locally, reply to each CR inline comment, re-run the Task 7 gates (fmt last), then push once. Also check Codacy's "Test suggestions" / "Low confidence findings" sections and address or reply. Repeat until CR + Codacy are at zero actionable findings.

---

## Self-Review

**Spec coverage:**
- Activity-bar entry (§Architecture 1) → Task 4 Step 4. ✓
- Custom-Box panel with documented deviation (§Architecture 2) → Task 4 Step 1 (module header). ✓
- Enable toggle / 3×3 grid / By-Spacecraft / breakdown / log (§Architecture 2 items 1–5) → Task 4 (widgets) + Task 5 (wiring). ✓
- Shared render module (§Architecture 3) → Task 1. ✓
- Enriched heard-state (§Architecture 4) → Task 2. ✓
- UI-side packet-type tally (§Architecture 5) → Task 3. ✓
- Wiring migration / window retirement / action re-point / heard-group removal (§Architecture 6) → Task 5. ✓
- Reset-on-disable + 20-min age-out + panel-lifetime handles (§Behavioral decisions) → Task 5 Steps 2–3 (`reset()` on disable; `rows()` uses existing `HEARD_EXPIRY_SECS`; handles built once, never torn down). ✓
- Log bounding (§Behavioral) → `MAX_LOG_ENTRIES` in Task 4 Step 1. ✓
- `sdr-orbcomm` untouched (§Decode boundary / Non-goals) → no task edits that crate. ✓
- Testing plan (§Testing) → Tasks 1–3 unit tests; Tasks 4/5 smoke. ✓
- Docs currency → Task 6. ✓

**Placeholder scan:** No "TBD"/"handle edge cases"/"similar to Task N". The one intentional forward-reference is Task 2 Step 4 (caller passes `None, None`, replaced in Task 5 Step 3) — explicit, with the replacement located. Icon name has a named fallback (Task 4 Step 4), not a placeholder.

**Type consistency:** `record(sat_id, position, vel_ms, sat_time_unix, now)` — defined Task 2, called Task 5 Step 3 (5 args) ✓. `format_breakdown(checksum_fail, repaired)` — defined Task 3, called Task 5 `refresh_breakdown` ✓. `OrbcommPanelHandles` methods (`append_log_entry`, `refresh_channel_grid`, `apply_enabled_ack`, `rebuild_heard_list`, `set_breakdown`) — defined Task 4 Step 2, called Task 5 Steps 2–3 ✓. `repaint_heard` / `connect_orbcomm_panel` — defined Task 5 Step 2, called Task 5 Step 3 + window.rs ✓. `channel_label_text` visibility raised to `pub(crate)` in Task 1, consumed in Task 4 ✓.

**Note on Task 4↔5 clippy:** Task 4's handle methods are dead until Task 5. Land them together in one push (they're one PR anyway); do not suppress with `#[allow(dead_code)]`. Flagged in Task 4 Step 6.
