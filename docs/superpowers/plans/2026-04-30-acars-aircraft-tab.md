# ACARS Aircraft-Grouped Viewer Tab + Label Names + ACK Column Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a "By Aircraft" tab to the ACARS viewer, populate the label-name lookup table so labels render as `H1 (Crew message)`, and add an ACK column to the Stream tab so `0x15` reads as `NAK` — three readability improvements bundled into a single PR.

**Architecture:** A `GtkStack` with `GtkStackSwitcher` replaces the single content area in the ACARS viewer. The new "aircraft" page wraps a second `GtkColumnView` over a parallel `gio::ListStore` of `AircraftEntryObject`s, kept in sync with the message store via a `HashMap<tail, AircraftEntryObject>` index updated at the message-append site in `window.rs`. Click-to-filter (vs tree-list expand) keeps state simple. Label-name lookup gets populated; ACK column slots between Block and Text on the Stream tab.

**Tech Stack:** Rust 1.x, GTK4 v4.10 + libadwaita via `gtk4-rs`, `glib::Object` subclassing, `gio::ListStore` + `FilterListModel` + `SortListModel`, `CustomFilter` + `CustomSorter`. Pure-Rust unit tests via `cargo test`; GTK widget code verified via manual smoke (per project workflow).

**Branch:** `feat/acars-aircraft-tab` (already off `main`, spec already committed at `de39218`).

**Out of scope:** ADS-B cross-correlation (#582), tree-list expand/collapse, custom user-tagged aircraft. See spec § "Out-of-scope items".

---

## File Structure

| File | Role |
|------|------|
| `crates/sdr-acars/src/label.rs` | MODIFY — populate `lookup` from empty stub to ~80-entry static match table; expand tests. |
| `crates/sdr-ui/src/acars_viewer.rs` | MODIFY — add `AircraftEntryObject` glib subclass + `mod imp_aircraft`; build `GtkStack` with Stream + By Aircraft pages; add aircraft column view, sorters, click-to-filter, ACK column on Stream tab; extend `ViewerHandles`; update Clear handler. |
| `crates/sdr-ui/src/window.rs` | MODIFY — extend `DspToUi::AcarsMessage` handler arm to update `aircraft_index` (find-or-insert + `record_message` + `items_changed`) when viewer is open and not paused. |

No new files. The new `AircraftEntryObject` lives in a private `mod imp_aircraft` inside `acars_viewer.rs` mirroring the existing `mod imp` for `AcarsMessageObject`.

---

## Workspace Gates

Run after each task that touches Rust source. Per-crate gates are sufficient when a task is isolated to one crate.

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

---

## Task 1: Populate `label.rs` lookup table

**Files:**
- Modify: `crates/sdr-acars/src/label.rs`

The stub at lines 21-23 always returns `None`. Replace with a static match arm covering ~80 known ACARS labels sourced from sigidwiki, airframes.io public docs, and acarsdeco2 / vdlm2dec public name tables. Names are 1-3 words, terse, no trailing punctuation. Aliases (RB → 26) get a parenthetical hint.

- [ ] **Step 1: Write the failing positive-lookup tests**

Replace the existing `mod tests` body in `crates/sdr-acars/src/label.rs` (currently lines 25-39) with:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_labels() {
        // Spot-check 5 canonical labels across the major code
        // families (alphanumeric pair, numeric, underscore prefix,
        // alias). Adding all 80 here would just be a copy of the
        // table in `lookup` itself.
        assert_eq!(lookup(*b"H1"), Some("Crew message"));
        assert_eq!(lookup(*b"Q0"), Some("Link test"));
        assert_eq!(lookup(*b"M1"), Some("Position report"));
        assert_eq!(lookup(*b"_d"), Some("General downlink"));
        assert_eq!(lookup(*b"RB"), Some("Schedule (alias for 26)"));
    }

    #[test]
    fn lookup_numeric_labels() {
        // OOOI events (10-15) are particularly important — the
        // viewer's UI uses them to highlight gate/wheels events.
        assert_eq!(lookup(*b"10"), Some("Arrival info"));
        assert_eq!(lookup(*b"11"), Some("Out (gate)"));
        assert_eq!(lookup(*b"12"), Some("Off (wheels)"));
        assert_eq!(lookup(*b"13"), Some("On (wheels)"));
        assert_eq!(lookup(*b"14"), Some("In (gate)"));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        // Bogus codes outside the known table should still
        // resolve to `None` so the viewer falls back to the bare
        // 2-char code in the column display.
        assert_eq!(lookup([0xFF, 0xFF]), None);
        assert_eq!(lookup(*b"ZZ"), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sdr-acars label::tests --features sdr-transcription/whisper-cpu`
Expected: FAIL with ``assertion `left == right` failed`` for the positive lookups (the stub returns `None` for everything).

- [ ] **Step 3: Replace `lookup` body with the populated table**

Replace `pub fn lookup(_code: [u8; 2]) -> Option<&'static str>` (currently at lines 20-23) with:

```rust
/// Look up the human-readable name for a 2-byte label code.
/// Returns `Some(name)` for known ACARS labels (~80 entries
/// covering OOOI events, position reports, weather, ATC, and
/// ACMS) or `None` for unknown codes. Names are sourced from
/// the sigidwiki ACARS labels page, airframes.io public docs,
/// and the open-source `acarsdeco2` / `vdlm2dec` projects'
/// name tables. ARINC 618 itself is paywalled; `acarsdec`
/// doesn't ship a name table either, so this is a curated
/// best-effort list rather than a verbatim port.
#[must_use]
pub fn lookup(code: [u8; 2]) -> Option<&'static str> {
    Some(match &code {
        // OOOI events (numeric pairs)
        b"10" => "Arrival info",
        b"11" => "Out (gate)",
        b"12" => "Off (wheels)",
        b"13" => "On (wheels)",
        b"14" => "In (gate)",
        b"15" => "Departure",
        b"16" => "Position",
        b"17" => "Arrival info",
        b"1F" => "Free text",
        b"1G" => "Gate request",
        b"1H" => "Gate assignment",
        b"1L" => "Position report",
        b"1S" => "Schedule",
        // Departure clearance + datalink (2x)
        b"20" => "Departure clearance",
        b"21" => "Departure clearance reply",
        b"22" => "Pre-departure clearance",
        b"23" => "Datalink expedite",
        b"25" => "Position",
        b"26" => "Schedule",
        b"27" => "Schedule (revision)",
        b"2C" => "Position report",
        b"2N" => "Takeoff time",
        b"2Z" => "Destination update",
        // 3x — fuel + maintenance
        b"30" => "Position report",
        b"32" => "Position",
        b"33" => "Fuel report",
        b"35" => "Position",
        b"39" => "Maintenance ground report",
        // 4x — position
        b"40" => "Position",
        b"44" => "Position",
        b"45" => "Position",
        b"4M" => "Position",
        b"4N" => "Position",
        // 5x — OOOI summary
        b"51" => "Ground service request",
        b"52" => "Engine maintenance",
        b"57" => "Position report",
        b"5U" => "Position",
        b"5Y" => "OOOI report",
        b"5Z" => "Schedule",
        // 7x — voice + tests
        b"70" => "Voice contact request",
        b"7A" => "Test message",
        b"7B" => "Test message",
        b"7C" => "Test message",
        // 8x — dispatch + clearance
        b"80" => "Departure",
        b"81" => "Position",
        b"82" => "Free text",
        b"83" => "Pre-departure clearance",
        b"8A" => "Engine maintenance",
        b"8D" => "Dispatch reply",
        b"8E" => "ETA report",
        b"8S" => "Schedule",
        // A-family
        b"A0" => "Test",
        b"A6" => "Position",
        b"A7" => "Pre-departure clearance request",
        b"A8" => "Position",
        b"A9" => "Pre-departure clearance reply",
        b"AA" => "Engine data",
        // B-family — weather
        b"B1" => "Weather request",
        b"B2" => "Weather information",
        b"B3" => "Weather (text)",
        b"B4" => "Weather (route)",
        b"B5" => "Weather (terminal)",
        b"B6" => "Weather (en-route)",
        b"B7" => "Weather (clearance)",
        b"B8" => "Weather (SIGMET)",
        b"B9" => "Weather (other)",
        b"BA" => "Weather request",
        // C-family — ATC
        b"C0" => "Uplink command",
        b"C1" => "ATC",
        b"C2" => "ATC clearance request",
        b"C3" => "ATC reply",
        // H-family — free text
        b"H1" => "Crew message",
        b"H2" => "Free text uplink",
        b"H3" => "Free text downlink",
        // M-family
        b"M1" => "Position report",
        // Q-family — control + position + OOOI
        b"Q0" => "Link test",
        b"Q1" => "ATIS",
        b"Q2" => "ACARS network test",
        b"Q3" => "Voice circuit test",
        b"Q4" => "Navaids",
        b"Q5" => "Engine data",
        b"Q6" => "Engine display data",
        b"Q7" => "Component maintenance",
        b"QA" => "Out (gate)",
        b"QB" => "Off (wheels)",
        b"QC" => "On (wheels)",
        b"QD" => "In (gate)",
        b"QE" => "OOOI summary",
        b"QF" => "OOOI (extended)",
        b"QG" => "Position report",
        b"QH" => "Position",
        b"QK" => "Voice request",
        b"QL" => "ATIS (alt)",
        b"QM" => "Position",
        b"QN" => "Diversion",
        b"QP" => "Position",
        b"QQ" => "Position",
        b"QR" => "Position",
        b"QS" => "Diversion",
        b"QT" => "ACARS request",
        // RB — alias dispatched the same as 26 in label_parsers
        b"RB" => "Schedule (alias for 26)",
        // Underscore prefix — generic up/down link
        b"_d" => "General downlink",
        b"_e" => "General uplink",
        // Unknown
        _ => return None,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sdr-acars label::tests --features sdr-transcription/whisper-cpu`
Expected: PASS — `running 3 tests ... test result: ok. 3 passed; 0 failed`.

- [ ] **Step 5: Run clippy + fmt for the crate**

```bash
cargo clippy -p sdr-acars --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean (no warnings, no diff).

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-acars/src/label.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): #579 populate label-name lookup table

Replace the empty v1 stub with a curated ~80-entry static match
covering OOOI events, position reports, weather, ATC, ACMS, and
free-text labels. Sources: sigidwiki ACARS labels page,
airframes.io public docs, acarsdeco2 / vdlm2dec name tables.
ARINC 618 itself is paywalled; this is a best-effort table
rather than a verbatim port.

The viewer's `render_label` already calls `label::lookup` and
formats results as `H1 (Crew message)`, so this populates the
display end-to-end with no viewer change needed.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Define `AircraftEntry` + `AircraftEntryObject` glib subclass

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

Add the glib subclass scaffold next to `AcarsMessageObject` (existing `mod imp` at lines 70-111). New module is `mod imp_aircraft`. The wrapper type itself goes after the existing `glib::wrapper!` block and exposes `new(entry: AircraftEntry)`, `entry()` getter, and `record_message(&self, msg: &AcarsMessage)`. Pure data — no GTK widget code yet.

Tests live inline in `crates/sdr-ui/src/acars_viewer.rs::tests` (new module added at file bottom). The existing test convention in this crate uses GTK init via `gtk4::init()` for tests that touch widgets, but `glib::Object` subclasses can be constructed directly without an `Application`.

- [ ] **Step 1: Add the failing tests**

Append to `crates/sdr-ui/src/acars_viewer.rs` (at the end of the file, after `render_text`):

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use arrayvec::ArrayString;
    use sdr_acars::AcarsMessage;

    fn fixture_message(tail: &str, label: [u8; 2], ts: SystemTime) -> AcarsMessage {
        let mut aircraft = ArrayString::<8>::new();
        aircraft.push_str(tail);
        AcarsMessage {
            timestamp: ts,
            freq_hz: 131_550_000.0,
            channel: 0,
            mode: b'2',
            aircraft,
            label,
            block_id: b'5',
            ack: b'!',
            text: String::new(),
            msn: ArrayString::new(),
            flight_id: ArrayString::new(),
            reassembled_block_count: 1,
            end_of_message: true,
            parsed: None,
        }
    }

    #[test]
    fn aircraft_entry_object_record_message_bumps_count() {
        // GTK glib::Object subclasses can be constructed without
        // a running GTK Application — `glib::Object::new` works
        // as long as the type was registered (which happens via
        // the `#[glib::object_subclass]` macro at module load).
        gtk4::glib::MainContext::default();
        let ts = SystemTime::now();
        let entry = AircraftEntry {
            tail: {
                let mut s = ArrayString::<8>::new();
                s.push_str(".N12345");
                s
            },
            last_seen: ts,
            msg_count: 0,
            last_label: *b"H1",
        };
        let obj = AircraftEntryObject::new(entry);
        assert_eq!(obj.entry().unwrap().msg_count, 0);

        let msg = fixture_message(".N12345", *b"M1", ts + Duration::from_secs(1));
        obj.record_message(&msg);
        assert_eq!(obj.entry().unwrap().msg_count, 1);

        obj.record_message(&msg);
        assert_eq!(obj.entry().unwrap().msg_count, 2);
    }

    #[test]
    fn aircraft_entry_object_last_seen_monotonic() {
        gtk4::glib::MainContext::default();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let entry = AircraftEntry {
            tail: ArrayString::new(),
            last_seen: t0,
            msg_count: 0,
            last_label: *b"  ",
        };
        let obj = AircraftEntryObject::new(entry);

        // Out-of-order timestamps must not regress last_seen.
        let later = fixture_message("X", *b"H1", t0 + Duration::from_secs(60));
        let earlier = fixture_message("X", *b"H1", t0 + Duration::from_secs(30));
        obj.record_message(&later);
        obj.record_message(&earlier);
        assert_eq!(obj.entry().unwrap().last_seen, t0 + Duration::from_secs(60));
    }

    #[test]
    fn aircraft_entry_object_record_message_updates_label() {
        gtk4::glib::MainContext::default();
        let ts = SystemTime::now();
        let entry = AircraftEntry {
            tail: ArrayString::new(),
            last_seen: ts,
            msg_count: 0,
            last_label: *b"H1",
        };
        let obj = AircraftEntryObject::new(entry);
        let msg = fixture_message("X", *b"M1", ts);
        obj.record_message(&msg);
        assert_eq!(obj.entry().unwrap().last_label, *b"M1");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sdr-ui acars_viewer::tests --features sdr-transcription/whisper-cpu --no-run`
Expected: FAIL with `cannot find type 'AircraftEntryObject' in this scope` and similar for `AircraftEntry`.

- [ ] **Step 3: Add `mod imp_aircraft`, the wrapper type, and the impl block**

Add after the existing `mod imp` block (after line 111 in `crates/sdr-ui/src/acars_viewer.rs`):

```rust
// ── glib::Object wrapper around an AircraftEntry (issue #579) ──

mod imp_aircraft {
    use std::cell::RefCell;

    use gtk4::glib;
    use gtk4::glib::subclass::prelude::{ObjectImpl, ObjectSubclass};

    pub struct AircraftEntryObject {
        pub inner: RefCell<Option<super::AircraftEntry>>,
    }

    impl Default for AircraftEntryObject {
        fn default() -> Self {
            Self {
                inner: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AircraftEntryObject {
        const NAME: &'static str = "AircraftEntryObject";
        type Type = super::AircraftEntryObject;
    }

    impl ObjectImpl for AircraftEntryObject {}
}

glib::wrapper! {
    /// Glib subclass wrapping an `AircraftEntry`. Used as the
    /// row model for the "By Aircraft" tab in the ACARS viewer.
    /// The aircraft column view's factories + sorters borrow
    /// the inner `AircraftEntry` via `obj.imp().inner.borrow()`.
    pub struct AircraftEntryObject(ObjectSubclass<imp_aircraft::AircraftEntryObject>);
}

/// Per-aircraft summary backing one row of the "By Aircraft"
/// tab. Mutated in place via [`AircraftEntryObject::record_message`]
/// — `last_seen` advances monotonically (`max(prev, msg.timestamp)`)
/// to mirror the same out-of-order discipline as
/// `AcarsMessageObject::record_duplicate` (CR round 2 on PR #591).
#[derive(Clone, Debug)]
pub struct AircraftEntry {
    pub tail: arrayvec::ArrayString<8>,
    pub last_seen: std::time::SystemTime,
    pub msg_count: u32,
    pub last_label: [u8; 2],
}

impl AircraftEntryObject {
    /// Wrap an `AircraftEntry` for insertion into the aircraft
    /// `gio::ListStore`. Caller should typically seed `msg_count`
    /// to 0 and immediately invoke [`Self::record_message`] for
    /// the message that triggered the insert; that gives the new
    /// row a `msg_count` of 1 with `last_seen` and `last_label`
    /// taken from the message.
    #[must_use]
    pub fn new(entry: AircraftEntry) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().inner.borrow_mut() = Some(entry);
        obj
    }

    /// Borrow-clone of the wrapped entry. Returns `None` only if
    /// a caller called `take()` (we don't); column-view factories
    /// can `expect` in their bind closures.
    #[must_use]
    pub fn entry(&self) -> Option<AircraftEntry> {
        self.imp().inner.borrow().clone()
    }

    /// Record a new message for this aircraft: bumps `msg_count`,
    /// advances `last_seen` monotonically to
    /// `max(last_seen, msg.timestamp)`, and overwrites
    /// `last_label`. Same out-of-order discipline as
    /// `AcarsMessageObject::record_duplicate`.
    pub fn record_message(&self, msg: &sdr_acars::AcarsMessage) {
        let imp = self.imp();
        let mut slot = imp.inner.borrow_mut();
        let Some(entry) = slot.as_mut() else { return };
        entry.msg_count = entry.msg_count.saturating_add(1);
        entry.last_seen = std::cmp::max(entry.last_seen, msg.timestamp);
        entry.last_label = msg.label;
    }
}
```

You also need to import `glib::subclass::prelude::ObjectSubclassIsExt` if not already in scope (it's already imported at the top via line 13 — verify).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sdr-ui acars_viewer::tests --features sdr-transcription/whisper-cpu`
Expected: PASS — 3 tests pass.

- [ ] **Step 5: Run clippy + fmt**

```bash
cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 add AircraftEntryObject glib subclass

Backs the upcoming "By Aircraft" tab in the ACARS viewer with a
glib::Object subclass wrapping an AircraftEntry summary (tail,
last_seen, msg_count, last_label). Mirrors the AcarsMessageObject
pattern in the same file: a private mod imp_aircraft holds the
RefCell<Option<…>> slot, the wrapper type exposes new/entry/
record_message, and last_seen updates monotonically the same way
record_duplicate does on the message wrapper.

Pure data — column view + store wiring lands in subsequent tasks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Extend `ViewerHandles` with aircraft store + index + stack

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs:25-46`

Add the new fields to `ViewerHandles` so the message-append site in `window.rs` can reach them via `state.acars_viewer_handles`.

- [ ] **Step 1: Extend the struct**

Replace the existing `pub struct ViewerHandles { … }` (lines 25-46 in `acars_viewer.rs`) with:

```rust
pub struct ViewerHandles {
    pub store: gtk4::gio::ListStore,
    pub filter: gtk4::CustomFilter,
    pub filter_model: gtk4::FilterListModel,
    pub status_label: gtk4::Label,
    pub pause_button: gtk4::ToggleButton,
    pub filter_entry: gtk4::SearchEntry,
    /// Collapse-duplicates toggle (issue #586). When active,
    /// the message-append site walks the most recent rows for
    /// a `(aircraft, mode, label, text)` key match within the
    /// recency window and bumps the existing row's count + last
    /// seen instead of appending a new one.
    pub collapse_button: gtk4::ToggleButton,
    /// `ScrolledWindow` for the auto-scroll-to-top behavior on
    /// new arrivals. The append site checks whether the user is
    /// at the top via `vadjustment().value()` and resets the
    /// adjustment back to its lower bound — if they've manually
    /// scrolled down to read older rows, auto-follow freezes
    /// until they scroll back. Bypasses `ColumnView::scroll_to`
    /// which needs gtk4 `v4_12` (workspace is on `v4_10`).
    pub scrolled_window: gtk4::ScrolledWindow,
    /// "By Aircraft" tab parallel store, one row per unique
    /// tail seen since session start. Issue #579.
    pub aircraft_store: gtk4::gio::ListStore,
    /// Filter applied to `aircraft_store`. Same `filter_entry`
    /// drives both this and the stream `filter`; this one
    /// matches tail substring only (no label/text — those don't
    /// exist on aircraft rows).
    pub aircraft_filter: gtk4::CustomFilter,
    /// FilterListModel wrapping `aircraft_store` for the
    /// aircraft column view.
    pub aircraft_filter_model: gtk4::FilterListModel,
    /// O(1) tail → AircraftEntryObject lookup so the message-
    /// append site in `window.rs` can find-or-insert without
    /// scanning the store. Holds clones of the same glib
    /// objects that live in `aircraft_store`; updates flow
    /// through to the column view via the shared refcount.
    pub aircraft_index: std::cell::RefCell<
        std::collections::HashMap<arrayvec::ArrayString<8>, AircraftEntryObject>,
    >,
    /// `GtkStack` switching between the "stream" page (existing
    /// chronological view) and "aircraft" page (per-tail
    /// summary). Cloned into the click-to-filter handler on the
    /// aircraft column view to switch back to the stream tab.
    pub stack: gtk4::Stack,
}
```

- [ ] **Step 2: Verify the struct still compiles (no consumers updated yet)**

Run: `cargo check -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: FAIL with `missing fields aircraft_store, aircraft_filter, …` at the existing `ViewerHandles { store, … }` initializer at line 406.

- [ ] **Step 3: Stub the new fields at the construction site**

In `crates/sdr-ui/src/acars_viewer.rs`, find the `let handles = Rc::new(ViewerHandles { … })` block (currently around line 406). Modify it to:

```rust
    // Placeholder aircraft store + filter + stack for the
    // "By Aircraft" tab. Wired to a real column view + click
    // handlers in subsequent tasks; this stub keeps the
    // struct buildable so the field additions land cleanly.
    let aircraft_store = gtk4::gio::ListStore::new::<AircraftEntryObject>();
    let aircraft_filter = gtk4::CustomFilter::new(|_obj| true);
    let aircraft_filter_model =
        gtk4::FilterListModel::new(Some(aircraft_store.clone()), Some(aircraft_filter.clone()));
    let stack = gtk4::Stack::new();

    // Hoist handles so all signal handlers below can clone from it.
    let handles = Rc::new(ViewerHandles {
        store,
        filter,
        filter_model,
        status_label: status_label.clone(),
        pause_button: pause_button.clone(),
        filter_entry: filter_entry.clone(),
        collapse_button: collapse_button.clone(),
        scrolled_window: scroll.clone(),
        aircraft_store,
        aircraft_filter,
        aircraft_filter_model,
        aircraft_index: std::cell::RefCell::new(std::collections::HashMap::new()),
        stack,
    });
```

- [ ] **Step 4: Verify it builds**

Run: `cargo check -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: PASS (warnings about `aircraft_store` etc. being unused are OK at this checkpoint — they'll be wired up in later tasks; suppress with `#[allow(dead_code)]` only if clippy fails).

Run: `cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings`
Expected: PASS. Clippy may flag unused fields on the stub — if so, those fields are about to be used in later tasks, but for this commit we want clippy clean. Add `#[allow(dead_code)]` to the new fields if needed. Do NOT add any other allow attributes.

- [ ] **Step 5: Run tests**

Run: `cargo test -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: PASS — 3 tests from Task 2 still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 extend ViewerHandles with aircraft store + index + stack

Adds the fields the message-append site in window.rs will use to
find-or-insert an AircraftEntryObject and bump the matching row's
count/timestamp. Stubs the construction site with empty store +
no-op filter so subsequent tasks can wire the column view
incrementally without breaking compilation between commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Add ACK column to Stream tab

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs` — `columns` array + `sorters` array + new `render_ack` function

The ACK byte is currently invisible in the viewer — `0x15` (NAK) renders as a control char glyph or empty cell on most fonts. New 8th column slots between Block and Text:

```text
| Time | Freq | Aircraft | Mode | Label | Block | Ack | Text |
```

`render_ack`: `0x15` → `"NAK"`, `b'!'` → `"!"`, printable ASCII (`0x20..=0x7E`, including space) → that char as a string, others → `0xNN` hex.

- [ ] **Step 1: Add the `render_ack` function**

Insert after `render_block` (currently at line 587-589 in `acars_viewer.rs`):

```rust
fn render_ack(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| match m.ack {
        b'\x15' => "NAK".to_string(),
        b'!' => "!".to_string(),
        // Printable ASCII range 0x20..=0x7E (inclusive of
        // space, exclusive of DEL). `is_ascii_graphic`
        // excludes space, which is a legitimate ACK byte.
        c if (0x20..=0x7E).contains(&c) => char::from(c).to_string(),
        c => format!("0x{c:02X}"),
    })
}
```

- [ ] **Step 2: Extend the columns + sorters arrays**

In `build_acars_viewer_window` (currently around lines 296-340), update the `ColumnSpec` count from 7 to 8 and add the Ack column between Block and Text:

```rust
    // Eight columns per spec section "Content" (issue #579 adds Ack):
    //   Time | Freq | Aircraft | Mode | Label | Block | Ack | Text
    let columns: [ColumnSpec; 8] = [
        ("Time", render_time, false),
        ("Freq", render_freq, false),
        ("Aircraft", render_aircraft, false),
        ("Mode", render_mode, false),
        ("Label", render_label, false),
        ("Block", render_block, false),
        ("Ack", render_ack, false),
        ("Text", render_text, true),
    ];

    // Per-column sorters. (See header comment on the previous
    // sorters block — same `make_message_sorter` helper.)
    let sorters: [gtk4::CustomSorter; 8] = [
        // Time column sorts on the wrapper's `last_seen`, not
        // the original frame timestamp. After a collapse hit,
        // `record_duplicate` advances `last_seen` in place
        // without moving the row in the store; sorting on
        // `inner.timestamp` would leave the refreshed row
        // stranded at its original position. Issue #586 / CR
        // round 1 on PR #591.
        gtk4::CustomSorter::new(|a, b| {
            let Some(a_obj) = a.downcast_ref::<AcarsMessageObject>() else {
                return gtk4::Ordering::Equal;
            };
            let Some(b_obj) = b.downcast_ref::<AcarsMessageObject>() else {
                return gtk4::Ordering::Equal;
            };
            a_obj.last_seen().cmp(&b_obj.last_seen()).into()
        }),
        make_message_sorter(|a, b| {
            a.freq_hz
                .partial_cmp(&b.freq_hz)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        make_message_sorter(|a, b| cmp_case_insensitive(&a.aircraft, &b.aircraft)),
        make_message_sorter(|a, b| a.mode.cmp(&b.mode)),
        make_message_sorter(|a, b| a.label.cmp(&b.label)),
        make_message_sorter(|a, b| a.block_id.cmp(&b.block_id)),
        make_message_sorter(|a, b| a.ack.cmp(&b.ack)),
        make_message_sorter(|a, b| cmp_case_insensitive(&a.text, &b.text)),
    ];
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: PASS.

Run: `cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings`
Expected: clean.

Run: `cargo test -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: PASS (existing 3 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 add Ack column to ACARS Stream tab

Slots between Block and Text. Renders 0x15 as "NAK" (the most
common case — ACARS negative-ack), '!' as itself, other printable
ASCII as the char, and everything else as a 0xNN hex escape.
Sortable on the raw byte. Closes the gap where the ACK byte was
rendering as a control-char glyph or empty cell on the default
column font.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Build the aircraft column view

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

Build a second `GtkColumnView` over `aircraft_filter_model` with 4 columns: Aircraft, Last Seen, Count, Last Label. Default sort: Last Seen descending. The column view goes inside its own `ScrolledWindow` (so each tab keeps its own scroll position). Stack switcher and stack itself wired in Task 6.

- [ ] **Step 1: Add a helper for building the aircraft column view**

Add to `crates/sdr-ui/src/acars_viewer.rs` (place right after `make_message_sorter`, around line 538):

```rust
/// Aircraft-tab column descriptor. Same shape as `ColumnSpec`
/// but the render closure operates on `AircraftEntryObject`.
type AircraftColumnSpec = (&'static str, fn(&AircraftEntryObject) -> String, bool);

/// Build the aircraft-tab column view + scrolled window. Returns
/// the `ScrolledWindow` so the caller can pack it into the stack.
/// The column view emits `connect_activate` for click-to-filter;
/// the caller wires that handler so the activate closure can hold
/// `Rc`s of the relevant viewer state.
fn build_aircraft_column_view(
    filter_model: &gtk4::FilterListModel,
) -> (gtk4::ScrolledWindow, gtk4::ColumnView) {
    let columns: [AircraftColumnSpec; 4] = [
        ("Aircraft", render_aircraft_tail, false),
        ("Last Seen", render_aircraft_last_seen, false),
        ("Count", render_aircraft_count, false),
        ("Last Label", render_aircraft_last_label, true),
    ];

    let sorters: [gtk4::CustomSorter; 4] = [
        // Aircraft tail — case-insensitive alphabetical
        gtk4::CustomSorter::new(|a, b| {
            let Some(a_obj) = a.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let Some(b_obj) = b.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let a_inner = a_obj.imp().inner.borrow();
            let b_inner = b_obj.imp().inner.borrow();
            match (a_inner.as_ref(), b_inner.as_ref()) {
                (Some(a), Some(b)) => cmp_case_insensitive(&a.tail, &b.tail).into(),
                _ => gtk4::Ordering::Equal,
            }
        }),
        // Last Seen — newest first wins descending sort
        gtk4::CustomSorter::new(|a, b| {
            let Some(a_obj) = a.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let Some(b_obj) = b.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let a_inner = a_obj.imp().inner.borrow();
            let b_inner = b_obj.imp().inner.borrow();
            match (a_inner.as_ref(), b_inner.as_ref()) {
                (Some(a), Some(b)) => a.last_seen.cmp(&b.last_seen).into(),
                _ => gtk4::Ordering::Equal,
            }
        }),
        // Count — numeric
        gtk4::CustomSorter::new(|a, b| {
            let Some(a_obj) = a.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let Some(b_obj) = b.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let a_inner = a_obj.imp().inner.borrow();
            let b_inner = b_obj.imp().inner.borrow();
            match (a_inner.as_ref(), b_inner.as_ref()) {
                (Some(a), Some(b)) => a.msg_count.cmp(&b.msg_count).into(),
                _ => gtk4::Ordering::Equal,
            }
        }),
        // Last Label — byte ordering on the 2-char code
        gtk4::CustomSorter::new(|a, b| {
            let Some(a_obj) = a.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let Some(b_obj) = b.downcast_ref::<AircraftEntryObject>() else {
                return gtk4::Ordering::Equal;
            };
            let a_inner = a_obj.imp().inner.borrow();
            let b_inner = b_obj.imp().inner.borrow();
            match (a_inner.as_ref(), b_inner.as_ref()) {
                (Some(a), Some(b)) => a.last_label.cmp(&b.last_label).into(),
                _ => gtk4::Ordering::Equal,
            }
        }),
    ];

    // SortListModel in the chain so column-header clicks reorder
    // visible rows. Sorter starts as None; bound to the column
    // view's sorter once it exists.
    let sort_model =
        gtk4::SortListModel::new(Some(filter_model.clone()), Option::<gtk4::Sorter>::None);
    // SingleSelection (vs NoSelection on the Stream tab) so the
    // column view's `activate` signal fires. Click-to-filter
    // wiring depends on `connect_activate`; `single_click_activate
    // (true)` on the ColumnView makes a single click both select
    // and emit `activate`, matching the "click an aircraft to drill
    // in" UX. NoSelection suppresses the activate signal entirely.
    let selection = gtk4::SingleSelection::new(Some(sort_model.clone()));
    let column_view = gtk4::ColumnView::builder()
        .model(&selection)
        .show_column_separators(true)
        .show_row_separators(true)
        .single_click_activate(true)
        .build();
    sort_model.set_sorter(column_view.sorter().as_ref());

    let mut last_seen_column: Option<gtk4::ColumnViewColumn> = None;

    for (idx, (title, render, expand)) in columns.into_iter().enumerate() {
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(move |_factory, item| {
            let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
                return;
            };
            let label = gtk4::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            item.set_child(Some(&label));
        });
        factory.connect_bind(move |_factory, item| {
            let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
                return;
            };
            let Some(label) = item.child().and_then(|w| w.downcast::<gtk4::Label>().ok()) else {
                return;
            };
            let Some(obj) = item
                .item()
                .and_then(|o| o.downcast::<AircraftEntryObject>().ok())
            else {
                return;
            };
            label.set_text(&render(&obj));
        });
        let column = gtk4::ColumnViewColumn::builder()
            .title(title)
            .factory(&factory)
            .resizable(true)
            .expand(expand)
            .build();
        column.set_sorter(Some(&sorters[idx]));
        if idx == 1 {
            last_seen_column = Some(column.clone());
        }
        column_view.append_column(&column);
    }
    // Default sort: Last Seen descending — newest active aircraft
    // at top, stale entries drift down.
    if let Some(col) = last_seen_column {
        column_view.sort_by_column(Some(&col), gtk4::SortType::Descending);
    }

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .hexpand(true)
        .build();

    (scrolled, column_view)
}

fn render_aircraft_tail(obj: &AircraftEntryObject) -> String {
    obj.entry().map(|e| e.tail.to_string()).unwrap_or_default()
}

fn render_aircraft_last_seen(obj: &AircraftEntryObject) -> String {
    let Some(entry) = obj.entry() else { return String::new() };
    let dt: chrono::DateTime<chrono::Local> = entry.last_seen.into();
    dt.format("%H:%M:%S").to_string()
}

fn render_aircraft_count(obj: &AircraftEntryObject) -> String {
    obj.entry().map(|e| e.msg_count.to_string()).unwrap_or_default()
}

fn render_aircraft_last_label(obj: &AircraftEntryObject) -> String {
    let Some(entry) = obj.entry() else { return String::new() };
    let raw = std::str::from_utf8(&entry.last_label)
        .unwrap_or("??")
        .to_string();
    match sdr_acars::label::lookup(entry.last_label) {
        Some(name) => format!("{raw} ({name})"),
        None => raw,
    }
}
```

- [ ] **Step 2: Verify it builds (helper not yet called)**

Run: `cargo build -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: PASS, but with a `dead_code` warning on `build_aircraft_column_view` if it isn't called yet. This is intentional — Task 6 wires it up. If clippy fails on this, add a temporary `#[allow(dead_code)]` to the helper that you'll remove in Task 6. **Do not commit until Task 6 is done** so the column view actually shows up in the UI in this commit pair.

- [ ] **Step 3: Skip commit — bundle with Task 6**

Continue to Task 6 without committing. The column view helper is meaningless without the stack wiring.

---

## Task 6: Build the GtkStack with Stream + By Aircraft pages

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

Replace the single-`ScrolledWindow` content area with a `GtkStack` containing both pages and a `GtkStackSwitcher` in the header bar. The stack reuses the `stack` field of `ViewerHandles` (currently a placeholder built in Task 3).

- [ ] **Step 1: Wire the stack into the window content area**

In `build_acars_viewer_window`, find the existing block (currently around lines 394-403):

```rust
    let scroll = gtk4::ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .hexpand(true)
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&scroll);
    window.set_content(Some(&content));
```

Replace with:

```rust
    let scroll = gtk4::ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .hexpand(true)
        .build();

    // Build the aircraft column view. Helper returns its own
    // ScrolledWindow so each tab retains its own GtkAdjustment
    // and scroll position independently.
    let (aircraft_scroll, aircraft_column_view) =
        build_aircraft_column_view(&aircraft_filter_model);

    // Stack with two pages — Stream (existing) and By Aircraft
    // (issue #579). Switcher in the header bar between the
    // existing buttons and the filter entry.
    let stack = gtk4::Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    stack.add_titled(&scroll, Some("stream"), "Stream");
    stack.add_titled(&aircraft_scroll, Some("aircraft"), "By Aircraft");

    let stack_switcher = gtk4::StackSwitcher::builder()
        .stack(&stack)
        .build();
    // Pack switcher between the action buttons (already
    // pack_start'd) and the filter entry (set as title widget).
    // pack_start preserves declaration order so the switcher
    // sits to the right of the collapse button.
    header.pack_start(&stack_switcher);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&stack);
    window.set_content(Some(&content));
```

- [ ] **Step 2: Update the `ViewerHandles { … }` initializer to use the real stack**

The block from Task 3 currently constructs an empty placeholder `let stack = gtk4::Stack::new();`. Remove that placeholder line and the placeholder `aircraft_store` / `aircraft_filter` / `aircraft_filter_model` declarations — they need to be hoisted to BEFORE `build_aircraft_column_view` is called so the helper can reference `aircraft_filter_model`.

Concretely: move the four `let aircraft_store = …; let aircraft_filter = …; let aircraft_filter_model = …;` declarations from the (Task 3) placeholder block to right before `build_aircraft_column_view(&aircraft_filter_model)`. Drop the placeholder `let stack = gtk4::Stack::new();` (the real stack from Step 1 above is what gets stored).

The final `let handles = Rc::new(ViewerHandles { … })` block should now reference the real `stack` variable.

- [ ] **Step 3: Wire click-to-filter on the aircraft column view**

Add right after the `*state.acars_viewer_handles.borrow_mut() = Some(Rc::clone(&handles));` line:

```rust
    // Click-to-filter (issue #579): single-click an aircraft row
    // → set filter entry to the tail and switch to Stream tab.
    // `single_click_activate(true)` on the ColumnView makes a
    // single click both select AND emit `activate`.
    {
        let handles = Rc::clone(&handles);
        aircraft_column_view.connect_activate(move |view, position| {
            // `position` is the column view's row index, which
            // maps to the immediate model (SingleSelection) wrapping
            // sort + filter. Look up via `view.model()` so the
            // resolved row matches the visible ordering regardless
            // of current sort + filter; resolving through
            // `aircraft_filter_model` directly would diverge once
            // a non-default sort is active.
            let Some(model) = view.model() else { return };
            let Some(obj) = model
                .item(position)
                .and_then(|o| o.downcast::<AircraftEntryObject>().ok())
            else {
                return;
            };
            let Some(entry) = obj.entry() else { return };
            handles.filter_entry.set_text(&entry.tail);
            handles.stack.set_visible_child_name("stream");
        });
    }
```

- [ ] **Step 4: Verify build**

Run: `cargo build -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: PASS.

Run: `cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings`
Expected: clean. If you added `#[allow(dead_code)]` to `build_aircraft_column_view` in Task 5, remove it now — the function is called.

- [ ] **Step 5: Run tests**

Run: `cargo test -p sdr-ui --features sdr-transcription/whisper-cpu`
Expected: PASS — 3 existing tests still pass.

- [ ] **Step 6: Commit Task 5 + Task 6 together**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 add "By Aircraft" tab to ACARS viewer

Replaces the single ScrolledWindow content area with a GtkStack
holding two pages:
  - "stream" — existing chronological message column view
  - "aircraft" — new per-tail summary (Aircraft, Last Seen,
    Count, Last Label)

GtkStackSwitcher in the header bar between the action buttons
and the filter entry. Default aircraft-tab sort is Last Seen
descending; all 4 columns are resizable + sortable. Each tab
wraps its own ScrolledWindow so per-tab scroll position is
preserved automatically by GTK.

Click-to-filter: single-click / Enter on an aircraft row sets
the filter entry to the tail and switches to the Stream tab.

Aircraft store/filter/index population happens in subsequent
tasks (hydration on viewer open, message-append site update,
filter handler split).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Hydrate `aircraft_index` from `acars_recent` on viewer open

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

The viewer currently hydrates the message store from `state.acars_recent` on open (lines 263-268). Add a parallel pass that walks the same ring and populates `aircraft_store` + `aircraft_index`.

- [ ] **Step 1: Replace the hydration block**

Find the existing block (around lines 263-268):

```rust
    {
        let recent = state.acars_recent.borrow();
        for msg in recent.iter().cloned() {
            store.append(&AcarsMessageObject::new(msg));
        }
    }
```

Replace with:

```rust
    let aircraft_store = gtk4::gio::ListStore::new::<AircraftEntryObject>();
    let aircraft_index_initial: std::collections::HashMap<
        arrayvec::ArrayString<8>,
        AircraftEntryObject,
    > = std::collections::HashMap::new();
    let aircraft_index_initial =
        std::cell::RefCell::new(aircraft_index_initial);
    {
        let recent = state.acars_recent.borrow();
        for msg in recent.iter() {
            store.append(&AcarsMessageObject::new(msg.clone()));

            // Mirror into aircraft_index + aircraft_store. New
            // tail → seed an entry with msg_count=0 and let
            // record_message bring it to 1; existing tail →
            // record_message bumps in place.
            let mut idx = aircraft_index_initial.borrow_mut();
            let obj = idx.entry(msg.aircraft).or_insert_with(|| {
                let entry = AircraftEntry {
                    tail: msg.aircraft,
                    last_seen: msg.timestamp,
                    msg_count: 0,
                    last_label: msg.label,
                };
                let obj = AircraftEntryObject::new(entry);
                aircraft_store.append(&obj);
                obj
            });
            obj.record_message(msg);
        }
    }
```

- [ ] **Step 2: Update the placeholder declaration block to use the hydrated values**

The placeholder block from Tasks 3 + 6 currently constructs `aircraft_store` + `aircraft_filter` + `aircraft_filter_model`. Replace its `let aircraft_store = gtk4::gio::ListStore::new::<AircraftEntryObject>();` with code that consumes the hydrated values. Concretely, remove that line — we already declared and populated `aircraft_store` in the hydration block above. Move the `aircraft_filter` + `aircraft_filter_model` declarations to remain after hydration.

The final layout in `build_acars_viewer_window` should be:

```rust
    // (existing message store + aircraft hydration block from Step 1)
    let initial_count = store.n_items();
    // (existing status_label + header pack_* lines)
    // (existing filter / filter_model / sort_model / column_view block)

    // Build aircraft filter + model AFTER aircraft_store is
    // populated — FilterListModel ::new takes a model reference
    // so this is order-flexible, but keeping the declaration
    // close to the consumer (build_aircraft_column_view) makes
    // the data flow obvious.
    let aircraft_filter = gtk4::CustomFilter::new(|_obj| true);
    let aircraft_filter_model =
        gtk4::FilterListModel::new(Some(aircraft_store.clone()), Some(aircraft_filter.clone()));

    let (aircraft_scroll, aircraft_column_view) =
        build_aircraft_column_view(&aircraft_filter_model);

    // (existing stack construction)

    // (existing handles construction — using the now-real
    // aircraft_store, aircraft_filter, aircraft_filter_model,
    // and aircraft_index_initial.into_inner())
```

In the `let handles = Rc::new(ViewerHandles { … })` block, set:

```rust
        aircraft_index: aircraft_index_initial,
```

(The `aircraft_index_initial` `RefCell` becomes the `ViewerHandles::aircraft_index` field directly — no `into_inner()` needed because the field type is `RefCell<HashMap<…, …>>`.)

- [ ] **Step 3: Verify build + tests + clippy**

```bash
cargo build -p sdr-ui --features sdr-transcription/whisper-cpu
cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-ui --features sdr-transcription/whisper-cpu
cargo fmt --all -- --check
```

Expected: all clean.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 hydrate aircraft index from acars_recent on open

Mirrors the existing message-store hydration: walks the bounded
ACARS ring and seeds the per-tail aircraft_index + aircraft_store
so reopening the viewer mid-session shows the retained backlog
on the By Aircraft tab instead of an empty table. Same loop as
the messages — single pass, hash-map find-or-insert, record_message
to bump count + monotonic last_seen.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Update filter handler to drive both Stream + Aircraft filters

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

The existing `connect_search_changed` handler (lines 437-462) updates only the message `filter`. Extend it to also update `aircraft_filter` — same `needle`, but matches tail substring only.

- [ ] **Step 1: Replace the filter handler block**

Find the `// ── Filter: live substring match on aircraft + label + text ──` block (currently lines 434-463). Replace with:

```rust
    // ── Filter: live substring match across both tabs ────────────
    // Stream tab: aircraft + label + text. Aircraft tab: tail only
    // (label/text don't exist on aircraft rows).
    {
        let filter = handles.filter.clone();
        let aircraft_filter = handles.aircraft_filter.clone();
        let entry = handles.filter_entry.clone();
        entry.connect_search_changed(move |entry| {
            let needle_str: String = entry.text().as_str().to_lowercase();

            // Stream filter — existing logic unchanged.
            let stream_needle = needle_str.clone();
            filter.set_filter_func(move |obj| {
                let Some(obj) = obj.downcast_ref::<AcarsMessageObject>() else {
                    return false;
                };
                let inner = obj.imp().inner.borrow();
                let Some(msg) = inner.as_ref() else {
                    return false;
                };
                if stream_needle.is_empty() {
                    return true;
                }
                let needle = &stream_needle;
                msg.aircraft.to_lowercase().contains(needle)
                    || std::str::from_utf8(&msg.label)
                        .is_ok_and(|s| s.to_lowercase().contains(needle))
                    || msg.text.to_lowercase().contains(needle)
            });

            // Aircraft filter — tail substring only.
            let aircraft_needle = needle_str;
            aircraft_filter.set_filter_func(move |obj| {
                let Some(obj) = obj.downcast_ref::<AircraftEntryObject>() else {
                    return false;
                };
                let inner = obj.imp().inner.borrow();
                let Some(entry) = inner.as_ref() else {
                    return false;
                };
                if aircraft_needle.is_empty() {
                    return true;
                }
                entry.tail.to_lowercase().contains(&aircraft_needle)
            });
        });
    }
```

- [ ] **Step 2: Verify build + clippy + tests**

```bash
cargo build -p sdr-ui --features sdr-transcription/whisper-cpu
cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-ui --features sdr-transcription/whisper-cpu
cargo fmt --all -- --check
```

Expected: all clean.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 share filter entry across Stream + Aircraft tabs

Extends the existing connect_search_changed handler to update both
filter and aircraft_filter from the same needle. Stream filter
matches aircraft + label + text (existing semantics); aircraft
filter matches tail substring only (label/text don't exist on
aircraft rows).

Empty filter shows everything on both tabs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Switch status-label wording based on visible tab

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

Status label currently shows `"{filtered} / {total} messages"`. On the aircraft tab, it should show `"{filtered} / {total} aircraft"`. Wire the existing `connect_items_changed` to also re-evaluate when the stack swaps tabs.

- [ ] **Step 1: Add a helper that refreshes status from whichever tab is visible**

Find the existing status-label block (currently lines 465-484). Replace it with:

```rust
    // ── Status label: <filtered> / <total> on the visible tab ────
    // Switches wording between "messages" and "aircraft" based on
    // stack.visible_child_name(). Re-evaluated on:
    //   - either filter model's items-changed signal
    //   - stack visible-child-notify
    {
        let stack = handles.stack.clone();
        let status = handles.status_label.clone();
        let store = handles.store.clone();
        let aircraft_store = handles.aircraft_store.clone();
        let filter_model = handles.filter_model.clone();
        let aircraft_filter_model = handles.aircraft_filter_model.clone();
        let refresh = move || {
            let on_aircraft = stack.visible_child_name().as_deref() == Some("aircraft");
            if on_aircraft {
                let filtered = aircraft_filter_model.n_items();
                let total = aircraft_store.n_items();
                status.set_label(&format!("{filtered} / {total} aircraft"));
            } else {
                let filtered = filter_model.n_items();
                let total = store.n_items();
                status.set_label(&format!("{filtered} / {total} messages"));
            }
        };

        // Wire the same refresh closure to all three signal
        // sources. Each clone is its own move-into-closure so
        // lifetimes work out.
        {
            let refresh = refresh.clone();
            handles
                .filter_model
                .connect_items_changed(move |_, _, _, _| refresh());
        }
        {
            let refresh = refresh.clone();
            handles
                .aircraft_filter_model
                .connect_items_changed(move |_, _, _, _| refresh());
        }
        {
            let refresh = refresh.clone();
            handles.stack.connect_visible_child_notify(move |_| refresh());
        }
    }
```

- [ ] **Step 2: Verify build + clippy + tests**

```bash
cargo build -p sdr-ui --features sdr-transcription/whisper-cpu
cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-ui --features sdr-transcription/whisper-cpu
cargo fmt --all -- --check
```

Expected: all clean. Note: the closure captures both `store` and `filter_model` — make sure these are clones (not moves) so the existing append site can still see them. The code above uses `.clone()` on each before the move.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 switch status-label wording per ACARS viewer tab

Shows "filtered / total messages" on Stream, "filtered / total
aircraft" on By Aircraft. Wires a single refresh closure to
items_changed on both filter models and visible-child-notify on
the stack so the wording updates immediately on tab swap and on
message/aircraft arrival.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Extend Clear button to clear aircraft store + index

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

The existing Clear handler (lines 419-432) wipes `store` + `acars_recent` and updates the status label. Extend to also wipe `aircraft_store` + `aircraft_index`.

- [ ] **Step 1: Update the Clear button block**

Replace the existing Clear-button block (lines 418-432) with:

```rust
    // ── Clear button ──────────────────────────────────────────────
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(&handles);
        clear_button.connect_clicked(move |_| {
            handles.store.remove_all();
            handles.aircraft_store.remove_all();
            handles.aircraft_index.borrow_mut().clear();
            state.acars_recent.borrow_mut().clear();
            // Status label is recomputed by the existing
            // items_changed / visible-child refresh wiring —
            // both `store.remove_all()` and `aircraft_store
            // .remove_all()` fire items_changed, which the
            // tab-aware refresh closure handles. Don't hard-
            // code wording here; that would leave the aircraft
            // tab saying "0 / 0 messages" until the next event.
        });
    }
```

- [ ] **Step 2: Verify build + clippy + tests**

```bash
cargo build -p sdr-ui --features sdr-transcription/whisper-cpu
cargo clippy -p sdr-ui --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-ui --features sdr-transcription/whisper-cpu
cargo fmt --all -- --check
```

Expected: all clean.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 clear aircraft store + index on Clear button

Extends the existing Clear handler to also wipe aircraft_store
and aircraft_index. Without this, clicking Clear would empty
the message list but leave stale aircraft rows visible on the
By Aircraft tab — including aircraft last seen before the click
that have since been "forgotten" from the message ring.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Wire the message-append site in `window.rs` to update the aircraft index

**Files:**
- Modify: `crates/sdr-ui/src/window.rs:2005-2050`

The existing `DspToUi::AcarsMessage` arm pushes into `state.acars_recent` and (if open + not paused) into `viewer.store`. Add a parallel block that updates `viewer.aircraft_index` + `viewer.aircraft_store`.

- [ ] **Step 1: Locate the existing append block**

Read `crates/sdr-ui/src/window.rs` around lines 2005-2050 to confirm the append block layout. It's the `if let Some(handles) = state.acars_viewer_handles.borrow().as_ref() && !handles.pause_button.is_active() { … }` block.

- [ ] **Step 2: Add aircraft-index update inside the gate**

Inside the same `if let Some(handles) = … && !handles.pause_button.is_active() { … }` block, AFTER the existing `// Auto-scroll-to-top` block (after the `adj.set_value(adj.lower());` line, around line 2049), insert:

```rust
                // Aircraft-index update (issue #579). Find or
                // insert the AircraftEntryObject for this tail,
                // then call record_message to bump count +
                // monotonic last_seen + last_label. On an
                // existing-entry hit, manually nudge the
                // filter/sort models via items_changed since
                // GListStore doesn't fire that signal on field
                // mutation of an already-stored object.
                {
                    let mut idx = handles.aircraft_index.borrow_mut();
                    let inserted = !idx.contains_key(&msg.aircraft);
                    let obj = idx.entry(msg.aircraft).or_insert_with(|| {
                        let entry = crate::acars_viewer::AircraftEntry {
                            tail: msg.aircraft,
                            last_seen: msg.timestamp,
                            msg_count: 0,
                            last_label: msg.label,
                        };
                        let obj = crate::acars_viewer::AircraftEntryObject::new(entry);
                        handles.aircraft_store.append(&obj);
                        obj
                    });
                    obj.record_message(&msg);
                    if !inserted {
                        // O(n) over ~50 aircraft is fine; Clear
                        // invalidates positions otherwise so we
                        // re-find each time rather than tracking
                        // a position field on the object. Spec
                        // edge-case "Aircraft store doesn't
                        // auto-redraw on field change".
                        if let Some(pos) = handles.aircraft_store.find(obj) {
                            handles.aircraft_store.items_changed(pos, 1, 1);
                        }
                    }
                }
```

Note: `gio::ListStore::find` returns `Option<u32>`. The bound name `obj` is the inserted-or-existing `AircraftEntryObject` reference inside `idx`.

- [ ] **Step 3: Verify build + clippy + tests**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo fmt --all -- --check
```

Expected: all clean. If clippy flags `redundant_closure` or similar, prefer the explicit form shown above.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #579 update aircraft index on each ACARS message

Extends the DspToUi::AcarsMessage handler arm: when the viewer is
open and not paused, find-or-insert into aircraft_index and call
record_message to bump count + monotonic last_seen + last_label.
On an existing-entry hit, look up the position via gio::ListStore
::find and emit items_changed(pos, 1, 1) so the filter/sort models
re-evaluate (GListStore doesn't fire items-changed on field
mutation by itself).

Pause and Clear semantics inherit from the surrounding gate +
the existing Clear handler — both now cover aircraft state too.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Workspace gates verification

**Files:** none modified — gate run only.

- [ ] **Step 1: Run the full workspace gate set**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: all clean. Total runtime ~3-5 minutes on a developer workstation.

- [ ] **Step 2: Skim the diff for obvious issues**

```bash
git log --oneline main..HEAD
git diff main...HEAD --stat
git diff main...HEAD -- crates/sdr-acars/src/label.rs | head -200
```

Expected: ~10 commits, ~3 files changed, ~430 LOC. Spot-check that:
- `label.rs` lookup table covers ~80 entries
- `acars_viewer.rs` has the new `AircraftEntryObject` + `imp_aircraft` module + `build_aircraft_column_view` + the stack wiring
- `window.rs` has the aircraft-index update block inside the existing pause-gated branch

- [ ] **Step 3: No commit (verification only)**

If any gate fails, fix in-place on the appropriate task's commit (use `git rebase -i` only if you understand the history rewrite; otherwise add a follow-up commit on the same task's commit message theme).

---

## Task 13: Manual GTK smoke (USER ONLY)

**Files:** none modified.

GTK widget code (stack, columns, factories, click handlers) is not unit-testable without a display server. Per project convention (`feedback_smoke_test_workflow.md`): Claude installs the binary; the user runs the smoke checklist manually. Claude does NOT launch the binary.

- [ ] **Step 1 (Claude): Install the binary**

```bash
make install CARGO_FLAGS="--release --features whisper-cuda"
```

Expected: `make install` builds with `--release` and `whisper-cuda` features and copies the binary to `$(BINDIR)/sdr-rs`. Per `feedback_make_install_release_flag.md`, the `--release` flag is required — without it, an old release binary stays in place. Verify with:

```bash
strings $BINDIR/sdr-rs | grep -i "by aircraft" | head -3
```

Expected: at least one match for the new tab title (`"By Aircraft"` from the `add_titled` call). If no match, the install copied a stale binary — re-run `make install` with the flag.

- [ ] **Step 2 (USER ONLY): Run the smoke checklist**

Hand off to user with:

> Build installed at `$(BINDIR)/sdr-rs`. Please run through the smoke checklist below and report which steps pass / fail before I push the branch.

**Smoke checklist (USER ONLY, copy verbatim into your pre-push report):**

1. **Stream tab unchanged**
   - [ ] Open the app, engage ACARS, open the ACARS viewer.
   - [ ] Stream tab shows messages with the existing column layout PLUS a new `Ack` column between Block and Text.
   - [ ] Label column shows `H1 (Crew message)` for known labels and just `H1` (or whatever code) for unknowns.
   - [ ] ACK column shows `NAK` for `0x15`, the printable char for bytes `0x20..=0x7E` (including space), `0xNN` for other bytes.
2. **By Aircraft tab populates**
   - [ ] Switch to By Aircraft tab via the GtkStackSwitcher in the header.
   - [ ] Status label changes wording from "messages" to "aircraft".
   - [ ] One row per unique tail, sorted Last Seen descending by default.
3. **By Aircraft live updates**
   - [ ] Stay on aircraft tab. Confirm Count and Last Seen fields bump when new messages arrive from already-seen aircraft.
   - [ ] New aircraft → new row appears (with Count=1).
4. **Click-to-filter**
   - [ ] Single-click an aircraft row.
   - [ ] Filter entry gets the tail. Stack switches to Stream tab. Stream tab shows only that aircraft's messages.
5. **Filter applies to both tabs**
   - [ ] Type a substring in the filter (e.g. `UA` or `.N`).
   - [ ] Stream tab shows messages from any aircraft whose tail / label / text contains the substring.
   - [ ] Switch to By Aircraft tab. Aircraft list shows only tails containing the substring (no label/text match — intentional).
6. **Pause works on both tabs**
   - [ ] Pause. Confirm both tabs freeze: no new entries on Stream, no count bumps on existing aircraft, no new aircraft rows.
   - [ ] Resume. Both tabs resume from current state.
7. **Clear works on both tabs**
   - [ ] Click Clear. Both tabs empty.
   - [ ] Status label resets to `0 / 0 messages` on Stream or `0 / 0 aircraft` on By Aircraft (tab-aware).
8. **Sort each aircraft column**
   - [ ] Click each header. Confirm: Aircraft column sorts case-insensitively alphabetical, Last Seen descending newest-first / ascending oldest-first, Count descending highest-first / ascending lowest-first, Last Label byte-ordered.
9. **Reopen retains aircraft state**
   - [ ] Close the viewer mid-session. Reopen.
   - [ ] Both tabs hydrate from the in-memory ACARS ring.

- [ ] **Step 3: Wait for user smoke pass**

Do NOT proceed to Task 14 until the user reports the smoke checklist passing. If any step fails, file as a follow-up issue on the appropriate task's commit and fix before proceeding.

---

## Task 14: Final pre-push sweep + push branch

**Files:** none modified.

- [ ] **Step 1: Re-run gates immediately before push**

Per `feedback_fmt_check_immediately_before_push.md`, fmt is the LAST gate before push:

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: all clean. If `cargo fmt --check` fails, run `cargo fmt --all`, re-stage, re-commit (or amend the last commit if it's a fmt-only fix), then re-run the check.

- [ ] **Step 2: Confirm branch state**

```bash
git status
git log --oneline main..HEAD
git diff main...HEAD --stat
```

Expected: clean working tree, ~10 commits ahead of main, 3 files changed (`label.rs`, `acars_viewer.rs`, `window.rs`), ~430 LOC.

- [ ] **Step 3: Push branch**

```bash
git push -u origin feat/acars-aircraft-tab
```

Expected: success. **DO NOT open the PR — the user will do that.** The plan ends here; user opens the PR via GitHub UI or `gh pr create` themselves.

---

## Spec coverage matrix

| Spec section | Task |
|------|------|
| Component 1 — Stack switcher | Task 6 |
| Component 2 — `AircraftEntryObject` | Task 2 |
| Component 3 — Aircraft store + index in `ViewerHandles` | Task 3 |
| Component 4 — Lifecycle integration (open / append / clear / pause) | Tasks 7, 10, 11 |
| Component 5 — Click-to-filter | Task 6 |
| Component 6 — Filter semantics | Task 8 |
| Component 7 — Status label switching | Task 9 |
| Component 8 — Aircraft-tab columns | Task 5 |
| Component 9 — Label-name lookup table | Task 1 |
| Component 10 — ACK column on Stream tab | Task 4 |
| Edge case: store doesn't auto-redraw | Task 11 (items_changed nudge) |
| Edge case: aircraft index drift on Clear | Task 10 |
| Edge case: pause-while-aircraft-active | Task 11 (gated by pause_button.is_active) |
| Edge case: click-on-filtered-out / click-on-stream-tab | Task 6 (no-op fall-through) |
| Testing — unit tests | Tasks 1, 2 |
| Testing — smoke (USER ONLY) | Task 13 |
