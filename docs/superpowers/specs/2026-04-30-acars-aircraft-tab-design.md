# ACARS Aircraft-Grouped Viewer Tab + Label Names (issue #579, v1)

> Add a "By Aircraft" tab to the ACARS viewer alongside the
> existing "Stream" tab — one row per tail number with last-seen,
> message count, and last label. Bundle in the long-stub label-
> name lookup table so labels render as `H1 (Crew message)`
> instead of cryptic 2-char codes. Plus a separate ACK column
> on the Stream tab so `0x15` shows as `NAK` etc.

## Goal

Make the ACARS viewer dramatically more useful for a long-
running session by:

1. Letting the user see *which aircraft are active* at a glance
   (currently buried in the firehose Stream view).
2. Decoding the label codes so a user without an ACARS
   protocol cheat sheet can read the firehose.
3. Surfacing the ACK byte so downlink protocol state is visible.

All three are user-visible readability improvements that
together turn the viewer from "raw protocol dump" to "scannable
ops dashboard."

## Non-goals

- **Tree-list expand/collapse** — rejected in favor of click-to-
  filter (single click on aircraft row → filter set + switch to
  Stream tab). Simpler state model, same user goal.
- **ADS-B cross-correlation** — owned by issue #582, blocked on
  ADS-B not yet shipping.
- **Per-aircraft routing or aggregator views** (the way
  `airframes.io` does it). Out of scope for v1.
- **Custom user-tagged aircraft** — saved tail filters, watch-
  lists, etc. Future-issue territory if requested.

## Architecture

### Module layout

```text
crates/sdr-acars/src/
├── label.rs                         ← MODIFY: populate ~80-entry lookup table
crates/sdr-ui/src/
├── acars_viewer.rs                  ← MODIFY: stack, aircraft tab, ACK column
├── window.rs                        ← MODIFY: aircraft-index update on append
```

No new files. The new `AircraftEntryObject` glib subclass lives
in a private `mod imp_aircraft` inside `acars_viewer.rs` to mirror
how `AcarsMessageObject` is already structured.

### Component 1 — Stack switcher (acars_viewer.rs)

Replace the single `ScrolledWindow` in the viewer's content area
with a `GtkStack` holding two pages:

- `"stream"` — existing `GtkColumnView` of messages
- `"aircraft"` — new `GtkColumnView` of aircraft entries

A `GtkStackSwitcher` goes in the header bar between the
existing buttons and the filter entry:

```text
[⏸][🗑][≡]    [Stream | By Aircraft]    [filter…]    [3 / 12 messages]
```

Each page wraps its own `ScrolledWindow`, so each tab's
`GtkAdjustment` retains its scroll position independently — no
manual saving/restoring needed.

### Component 2 — `AircraftEntryObject` glib subclass

```rust
mod imp_aircraft {
    pub struct AircraftEntryObject {
        pub inner: RefCell<Option<AircraftEntry>>,
    }
}

#[derive(Clone, Debug)]
pub struct AircraftEntry {
    pub tail: ArrayString<8>,
    pub last_seen: SystemTime,
    pub msg_count: u32,
    pub last_label: [u8; 2],
}

impl AircraftEntryObject {
    pub fn new(entry: AircraftEntry) -> Self;
    pub fn entry(&self) -> Option<AircraftEntry>;
    pub fn record_message(&self, msg: &AcarsMessage);
}
```

`record_message` mutates in place: bumps `msg_count`, advances
`last_seen` to `max(last_seen, msg.timestamp)`, sets
`last_label` to the message's label. Same monotonic-update
discipline as `AcarsMessageObject::record_duplicate` (CR round 2
on PR #591).

### Component 3 — Aircraft store + index in `ViewerHandles`

```rust
pub struct ViewerHandles {
    /* existing fields */
    pub aircraft_store: gtk4::gio::ListStore,
    pub aircraft_filter: gtk4::CustomFilter,
    pub aircraft_filter_model: gtk4::FilterListModel,
    pub aircraft_index: RefCell<HashMap<ArrayString<8>, AircraftEntryObject>>,
    pub stack: gtk4::Stack,
}
```

`aircraft_store` holds the `AircraftEntryObject`s.
`aircraft_filter_model` wraps it for the column view.
`aircraft_index` is the `HashMap<tail, AircraftEntryObject>`
that gives the message-append site O(1) lookup-or-insert; the
hashmap holds clones of the same objects that live in the
store, so updates flow through to the column view via the
shared glib refcount.

### Component 4 — Lifecycle integration

#### Viewer open (acars_viewer.rs::build_acars_viewer_window)

Same hydration loop as the existing message store, with a
second pass that populates the aircraft index from the same
ring:

```rust
let aircraft_index: RefCell<HashMap<ArrayString<8>, AircraftEntryObject>> =
    RefCell::new(HashMap::new());
{
    let recent = state.acars_recent.borrow();
    for msg in recent.iter() {
        let mut idx = aircraft_index.borrow_mut();
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

#### Message append (window.rs::handle_dsp_message — `DspToUi::AcarsMessage` arm)

The existing append path:

1. Pushes the message into `state.acars_recent` (ring trim)
2. Pushes into `viewer.store` if open + not paused

Extends with:

3. If viewer open + not paused:
   - Find or insert in `viewer.aircraft_index`
   - Call `obj.record_message(msg)` to bump count/timestamp/label
   - The store auto-redraws because `record_message` mutates
     the object that's already in the store (glib's notify is
     not strictly required for `GListStore` items but the
     filter/sort models pick up changes via items_changed
     when the count changes — see Edge Cases below)

#### Clear (existing handler)

The existing `clear_button.connect_clicked` clears `store` +
`acars_recent`. Extend to also clear `aircraft_store` and
`aircraft_index`.

#### Pause

Existing pause-toggle gates the message-append site via
`viewer.pause_button.is_active()`. Extend the same gate to skip
the aircraft-index update.

#### Viewer close

`window.connect_close_request` already clears the AppState weak
ref slot and the per-viewer handles. The `aircraft_index` and
`aircraft_store` drop with the handles via Rc refcount — no
explicit cleanup needed.

### Component 5 — Click-to-filter

```rust
column_view_aircraft.connect_activate(move |_view, position| {
    let Some(obj) = aircraft_filter_model
        .item(position)
        .and_then(|o| o.downcast::<AircraftEntryObject>().ok())
    else { return; };
    let Some(entry) = obj.entry() else { return };
    filter_entry.set_text(&entry.tail);
    stack.set_visible_child_name("stream");
});
```

`connect_activate` fires on:
- Double-click row
- Enter key
- `<Space>` if the focused row is selected

Single-click does NOT activate (that's `connect_row_activated`
or equivalent on a `GtkListView`; `GtkColumnView` activation is
double-click for legibility).

### Component 6 — Filter semantics

Single `filter_entry` shared across both tabs. Two
`CustomFilter`s, both updated by the same
`connect_search_changed` handler:

```rust
filter_entry.connect_search_changed(move |entry| {
    let needle = entry.text().as_str().to_lowercase();
    // Stream filter: existing aircraft + label + text substring
    stream_filter.set_filter_func(/* existing logic */);
    // Aircraft filter: tail substring only
    let needle_clone = needle.clone();
    aircraft_filter.set_filter_func(move |obj| {
        let Some(obj) = obj.downcast_ref::<AircraftEntryObject>() else {
            return false;
        };
        let inner = obj.imp().inner.borrow();
        let Some(entry) = inner.as_ref() else { return false };
        if needle_clone.is_empty() {
            return true;
        }
        entry.tail.to_lowercase().contains(&needle_clone)
    });
});
```

### Component 7 — Status label switching

Status label currently shows `"{filtered} / {total} messages"`.
On the aircraft tab, show `"{filtered} / {total} aircraft"`.
Switch the wording based on `stack.visible_child_name()`:

```rust
stack.connect_visible_child_notify(move |stack| {
    let label = stack.visible_child_name().as_deref();
    refresh_status_label(label);
});
```

The two `connect_items_changed` handlers (one per filter model)
each refresh from their own counts, scoped to whichever tab is
visible.

### Component 8 — Aircraft-tab columns

```text
| Aircraft | Last Seen | Count | Last Label              |
|----------|-----------|-------|-------------------------|
| .N12345  | 14:32:11  |     8 | M1 (Position report)    |
| .C-FYKW  | 14:31:45  |     3 | H1 (Crew message)       |
```

4 columns. All resizable. All sortable via `CustomSorter`s on
`AircraftEntryObject`'s inner fields. Default sort: Last Seen
descending (newest active aircraft at top).

Column-render fns mirror the existing `render_*` pattern from
the Stream tab — read inner via the `imp` borrow, format,
return `String`.

### Component 9 — Label-name lookup table

Populate `crates/sdr-acars/src/label.rs::lookup`. Currently
returns `None` for all inputs; replace with a static match
covering ~80 known ACARS labels.

#### Source-list outline (curated during implementation)

| Code | Name |
|------|------|
| `H1` | Crew message |
| `Q0` | Link test |
| `M1` | Position report |
| `B1` | Weather request |
| `B2` | Weather acknowledge |
| `_D` | General downlink |
| `_E` | General uplink |
| `10` | Arrival |
| `11` | Out (gate) |
| `12` | Off (wheels) |
| `13` | On (wheels) |
| `14` | In (gate) |
| `15` | Departure |
| `17` | Arrival info |
| `1G` | Gate request |
| `20` | Departure clearance |
| `21` | Departure clearance reply |
| `26` | Schedule |
| `2N` | Takeoff time |
| `2Z` | Destination update |
| `33` | Fuel report |
| `39` | Maintenance ground report |
| `44` | Position |
| `45` | Position |
| `5Y` | OOOI report |
| `7B` | Test message |
| `80` | Departure |
| `83` | Pre-departure clearance |
| `8D` | Dispatch reply |
| `8E` | ETA report |
| `8S` | Schedule |
| `Q1` – `QT` | OOOI events (out / off / on / in / etc.) |
| `RB` | Schedule (alias for 26) |

(The full ~80-entry table will be populated from ARINC 618 +
sigidwiki + community wikis during implementation. Per
[feedback memory] the project favors port-fidelity, but ARINC
618 is a paid spec — acarsdec doesn't ship a name table either,
so we curate from public sources.)

#### Output format

`Option<&'static str>` per the existing signature. Names are
1-3 words, terse, no trailing punctuation. Aliases (e.g. RB →
"Schedule (alias for 26)") get a parenthetical for clarity.

#### Tests

Per-label `#[test]`s for ~5 canonical labels (don't test all 80;
that's just a copy of the table). Plus an `unknown_returns_none`
test that hits two known-bogus codes.

### Component 10 — ACK column on the Stream tab

New 8th column between Block and Text:

```text
| Time | Freq | Aircraft | Mode | Label | Block | Ack | Text |
```

`render_ack(obj)` body:

```rust
fn render_ack(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| match m.ack {
        b'\x15' => "NAK".to_string(),
        b'!'    => "!".to_string(),
        c if c.is_ascii_graphic() => char::from(c).to_string(),
        c       => format!("0x{c:02X}"),
    })
}
```

`NAK` (0x15, the ACARS negative-ack) is the most common case
and previously rendered as a nonprintable control char glyph
or empty cell. Now legible.

Sortable on `m.ack` byte. Resizable, narrow default width.

## Data flow

```text
DspToUi::AcarsMessage
        │
        ▼
window.rs::handle_dsp_message
        │
        ├── push to state.acars_recent (always)
        ├── push to viewer.store (if open + not paused)
        └── update viewer.aircraft_index (if open + not paused)   ← NEW
                │
                ▼
        AircraftEntryObject::record_message
                │
                ▼
        store mutates → filter/sort models invalidate → column view redraws
```

## Edge cases

### Aircraft store doesn't auto-redraw on field change

`GListStore::items_changed` only fires on insert/remove, not on
field mutation of an existing item. The aircraft column view's
sort by Last Seen would freeze on an existing entry's bumped
timestamp.

**Mitigation:** when `record_message` updates an existing
entry, look up the entry's current position via
`aircraft_store.find(&obj)` (gtk4-rs `gio::ListStore::find`,
O(n) over ~50 aircraft is fine), then call
`aircraft_store.items_changed(idx, 1, 1)` to nudge the
filter/sort models. The `find`-based lookup keeps
`aircraft_index` simple (no position field on the object, no
`(obj, idx)` tuple) and stays correct even if items get
removed via Clear and re-inserted on the next message. The
filter/sort models pick up the items_changed signal and
re-evaluate.

Alternative considered: re-sort manually on every update.
Rejected — GTK's CustomSorter will re-sort if we call
`items_changed`, no manual sort needed.

Alternative considered: track `(obj, Cell<u32>)` in the index
to skip the `find`. Rejected — Clear invalidates positions and
the bookkeeping risk outweighs the O(n) win at this scale.

### Aircraft index drift on Clear

When user clicks Clear, both `store` and `acars_recent` are
emptied. The aircraft entries remain stale unless we also
clear `aircraft_index` and `aircraft_store`. Handled in the
existing `clear_button` handler — adds 2 lines.

### Pause-while-aircraft-active

Pause currently skips `viewer.store` appends but lets
`acars_recent` keep filling. Aircraft index/store should follow
the same pattern: pause skips the index update, the ring keeps
growing. Resume picks up new aircraft + new messages from there
forward. Deliberately does not retroactively backfill the gap
(matches existing message-append behavior; same user-intuition
trade-off).

### Click-activation on a filtered-out row

If the user has typed a filter and an aircraft row is hidden,
they can't click it. Non-issue.

### Click-activation when stack is already on Stream tab

If `stack.visible_child_name() == "stream"`, the
`set_visible_child_name("stream")` call is a no-op. Non-issue.

## Testing

### Unit tests

- `label.rs::tests`:
  - `lookup_known_labels` — 5 spot-checks for canonical labels
  - `lookup_unknown_returns_none` — 2 known-bogus codes
- `acars_viewer.rs::tests`:
  - `aircraft_entry_object_record_message_bumps_count` — round-trip
  - `aircraft_entry_object_last_seen_monotonic_under_record_message` — out-of-order timestamps don't regress
  - `aircraft_entry_record_message_updates_label`

The aircraft-index lifecycle (insert/update/clear) and the
column view rendering can't be unit-tested without a GTK
display — covered by smoke.

### Smoke (USER ONLY)

Manual GTK smoke checklist (provided to user after `make install`):

1. **Stream tab unchanged**
   - Open viewer with ACARS engaged. Stream tab shows messages.
   - Label column now shows `H1 (Crew message)` for known labels,
     bare code for unknowns.
   - New ACK column shows `NAK` for `0x15`, printable char or
     `0xNN` for others.
2. **By Aircraft tab populates**
   - Switch to By Aircraft tab via stack switcher.
   - One row per unique tail seen since session start.
   - Sorted Last Seen descending by default.
3. **By Aircraft live updates**
   - Stay on aircraft tab. Confirm Count and Last Seen fields
     bump on new messages from already-seen aircraft.
   - New aircraft → new row appears.
4. **Click-to-filter**
   - Double-click an aircraft row.
   - Filter entry gets the tail. Stack switches to Stream tab.
     Stream tab shows only that aircraft's messages.
5. **Filter applies to both tabs**
   - Type "UA" in filter. Stream tab shows messages from all
     aircraft with "UA" in the tail (or label/text).
   - Switch to By Aircraft tab. Aircraft list shows only tails
     containing "UA" (no label/text match on this tab —
     intentional).
6. **Pause works on both tabs**
   - Pause. Confirm both tabs freeze (no new entries / no
     count bumps on existing aircraft).
   - Resume. Both tabs resume from current state.
7. **Clear works on both tabs**
   - Clear. Both tabs empty.
8. **Sort each aircraft column**
   - Click each header — alphabetical by tail, descending by
     Last Seen, descending by Count, alphabetical by Last
     Label.
9. **Reopen retains aircraft state**
   - Close and reopen viewer mid-session. Both tabs hydrate
     from `acars_recent` (the existing message-side hydration
     already does this; aircraft side mirrors it).

## File budget

| File | LOC |
|------|-----|
| `crates/sdr-acars/src/label.rs` | +120 (table + tests) |
| `crates/sdr-ui/src/acars_viewer.rs` | +280 (stack + aircraft tab + ACK column + glib subclass) |
| `crates/sdr-ui/src/window.rs` | +30 (aircraft-index update at append site) |
| **Total** | **~430 LOC** |

Single bundled PR.

## Out-of-scope items (deferred)

- ADS-B integration (#582 — blocked on ADS-B)
- Custom user-tagged aircraft / watchlists
- Per-aircraft routing or aggregator-style views
- Tree-list expand/collapse (rejected in design phase)
- ACK rendering improvements beyond the basic NAK / printable
  / hex split (e.g. mapping printable acks to a sequence-number
  display)

## References

- Existing viewer: `crates/sdr-ui/src/acars_viewer.rs`
- Existing label-stub:
  `crates/sdr-acars/src/label.rs::lookup`
- ACARS protocol references:
  `docs/research/07-acars-aviation-datalink.md`
- Issue #579 acceptance
- Sibling specs:
  - Sub-project 1 (DSP/parser) — PR #583
  - Sub-project 2 (controller) — PR #584
  - Sub-project 3 (viewer scaffold) — PR #587
  - #577 (label parsers) — PR #594
  - #578 (output formatters) — PR #595
