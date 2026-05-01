//! ACARS viewer window (epic #474, sub-project 3).
//!
//! Floating top-level `adw::Window` showing decoded ACARS
//! messages in a scrollable `GtkColumnView`. Same lifecycle
//! pattern as `lrpt_viewer` / `apt_viewer`: opened from the
//! Aviation panel button, weakly held in
//! `AppState::acars_viewer_window` so a second click presents
//! the existing window rather than spawning a duplicate.

use std::rc::Rc;

use gtk4::glib;
use gtk4::glib::subclass::prelude::ObjectSubclassIsExt;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::AdwWindowExt;

use crate::state::AppState;

/// Per-viewer handles needed by the `DspToUi::AcarsMessage`
/// append site in `window.rs::handle_dsp_message`. Stored on
/// `AppState` (a sibling field of `acars_viewer_window`) so the
/// append site can fetch them without re-walking the widget
/// tree. Cleared on the window's close-request.
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
    /// `FilterListModel` wrapping `aircraft_store` for the
    /// aircraft column view.
    pub aircraft_filter_model: gtk4::FilterListModel,
    /// O(1) tail → `AircraftEntryObject` lookup so the message-
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

/// Recency window length in seconds (10 minutes). Split into a
/// named intermediate so the `60 * 10` factor reads as
/// "ten minutes" without tripping the duration-unit lint that
/// would otherwise fire on `from_secs(600)`.
const ACARS_COLLAPSE_WINDOW_SECS: u64 = 60 * 10;

/// Window of "recent" rows the collapse-duplicates path scans
/// for a key match (issue #586). 10 minutes per the spec; rows
/// older than this stay as their own entry even if the same
/// `(aircraft, mode, label, text)` reappears later. Walking
/// the store from the most recent row backwards typically hits
/// this cutoff before exhausting many rows in a busy session,
/// so the linear scan is fine in practice.
pub const ACARS_COLLAPSE_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(ACARS_COLLAPSE_WINDOW_SECS);

/// Default window dimensions (per spec `acars_viewer.rs` budget).
const ACARS_VIEWER_WINDOW_WIDTH: i32 = 1100;
const ACARS_VIEWER_WINDOW_HEIGHT: i32 = 600;

// ── glib::Object wrapper around an AcarsMessage ────────────────

mod imp {
    use std::cell::{Cell, RefCell};
    use std::time::SystemTime;

    use gtk4::glib;
    use gtk4::glib::subclass::prelude::{ObjectImpl, ObjectSubclass};

    pub struct AcarsMessageObject {
        pub inner: RefCell<Option<sdr_acars::AcarsMessage>>,
        /// Number of times this `(aircraft, mode, label, text)`
        /// has been seen since the row was inserted. `1` for a
        /// fresh row; bumped by the duplicate-collapse path
        /// (issue #586) when the toggle is active.
        pub count: Cell<u32>,
        /// Last-seen timestamp. Equal to `inner.timestamp` for a
        /// fresh row; updated on each duplicate hit so the Time
        /// column displays the most recent occurrence rather
        /// than the first. `SystemTime::UNIX_EPOCH` as a
        /// safe-default sentinel before a real value is set
        /// (callers populate it via `AcarsMessageObject::new`
        /// from the wrapped message's own timestamp).
        pub last_seen: Cell<SystemTime>,
    }

    impl Default for AcarsMessageObject {
        fn default() -> Self {
            Self {
                inner: RefCell::new(None),
                count: Cell::new(1),
                last_seen: Cell::new(SystemTime::UNIX_EPOCH),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AcarsMessageObject {
        const NAME: &'static str = "AcarsMessageObject";
        type Type = super::AcarsMessageObject;
    }

    impl ObjectImpl for AcarsMessageObject {}
}

glib::wrapper! {
    /// Glib subclass wrapping an `AcarsMessage`. `GListStore`
    /// requires a `glib::Object` model type; the viewer's
    /// column-view factories + filter predicate read the inner
    /// `AcarsMessage` via `obj.imp().inner.borrow()` (no-clone
    /// hot path) and fail closed if the slot is empty.
    pub struct AcarsMessageObject(ObjectSubclass<imp::AcarsMessageObject>);
}

impl AcarsMessageObject {
    /// Wrap an `AcarsMessage` for insertion into a `GListStore`.
    /// `count` is initialised to 1 and `last_seen` to the
    /// message's own timestamp; the duplicate-collapse path
    /// (issue #586) bumps both via [`Self::record_duplicate`]
    /// when the viewer toggle is active.
    #[must_use]
    pub fn new(msg: sdr_acars::AcarsMessage) -> Self {
        let obj: Self = glib::Object::new();
        let timestamp = msg.timestamp;
        *obj.imp().inner.borrow_mut() = Some(msg);
        obj.imp().last_seen.set(timestamp);
        obj
    }

    /// Borrow the wrapped message. Returns `None` only if a
    /// caller called `take()` (we don't); callers may
    /// `expect()` in factory closures.
    #[must_use]
    pub fn message(&self) -> Option<sdr_acars::AcarsMessage> {
        self.imp().inner.borrow().clone()
    }

    /// Current duplicate count (1 for a fresh row).
    #[must_use]
    pub fn count(&self) -> u32 {
        self.imp().count.get()
    }

    /// Last-seen timestamp.
    #[must_use]
    pub fn last_seen(&self) -> std::time::SystemTime {
        self.imp().last_seen.get()
    }

    /// Bump the duplicate count and advance `last_seen` to the
    /// later of the current value and `ts`. Called by the
    /// append site in `window.rs` when the collapse toggle is
    /// active and an incoming message matches an existing
    /// row's `(aircraft, mode, label, text)` key within the
    /// recency window. The `max` keeps the field monotonic
    /// even if duplicate frames arrive slightly out of order
    /// (CR round 2 on PR #591) — without it, the displayed
    /// time and the time-column sort key could regress.
    pub fn record_duplicate(&self, ts: std::time::SystemTime) {
        let imp = self.imp();
        imp.count.set(imp.count.get().saturating_add(1));
        imp.last_seen.set(std::cmp::max(imp.last_seen.get(), ts));
    }
}

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
    /// `gio::ListStore`. Callers should set `msg_count` to 1 in
    /// the seed entry (already counting the inserting message)
    /// so the column view's first bind reads the correct value.
    /// Subsequent messages are recorded via [`Self::record_message`].
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

// ── Public API: open / present-if-already-open ─────────────────

/// Open the ACARS viewer window if not already open. If a
/// viewer window already exists (held weakly in
/// `state.acars_viewer_window`), present it instead of opening
/// a second one. Mirror of `open_lrpt_viewer_if_needed` in
/// `lrpt_viewer.rs`.
pub fn open_acars_viewer_if_needed(state: &Rc<AppState>) {
    // If a viewer is already open, present it.
    if let Some(weak) = state.acars_viewer_window.borrow().as_ref()
        && let Some(window) = weak.upgrade()
    {
        window.present();
        return;
    }
    // First-open path: build a new window, stash a weak ref,
    // and connect the close-request handler to clear the slot.
    let window = build_acars_viewer_window(state);
    *state.acars_viewer_window.borrow_mut() = Some(window.downgrade());
    window.present();
}

/// Column descriptor: (title, render function, expand).
/// Render fn takes `&AcarsMessageObject` so the Time column can
/// read `count` + `last_seen` from the wrapper for the
/// duplicate-collapse `(×N)` prefix; non-time columns borrow
/// `obj.imp().inner` themselves.
type ColumnSpec = (&'static str, fn(&AcarsMessageObject) -> String, bool);

#[allow(clippy::too_many_lines)]
fn build_acars_viewer_window(state: &Rc<AppState>) -> adw::Window {
    let window = adw::Window::builder()
        .title("ACARS")
        .default_width(ACARS_VIEWER_WINDOW_WIDTH)
        .default_height(ACARS_VIEWER_WINDOW_HEIGHT)
        .modal(false)
        .build();

    // ─── Header bar ───
    let header = adw::HeaderBar::new();
    let pause_button = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause appending new messages (existing rows stay visible)")
        .build();
    // Icon-only buttons need an explicit accessible label —
    // tooltips are not surfaced to assistive tech (project
    // convention; same pattern as `apt_viewer.rs`,
    // `server_panel.rs`).
    pause_button.update_property(&[gtk4::accessible::Property::Label(
        "Pause appending new ACARS messages",
    )]);
    // PAUSE SEMANTIC: when active, the message-append site in
    // `window.rs::handle_dsp_message` skips pushing into `store`.
    // The bounded ring (`AppState::acars_recent`) keeps growing
    // — pausing the view does NOT pause the DSP. Resume appends
    // from that point forward; we deliberately do NOT drain
    // gap messages from the ring (simpler + matches user
    // intuition; deferred-item issue if drain-on-resume is
    // wanted later).
    let clear_button = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Clear all messages from the view (does not disable ACARS)")
        .build();
    clear_button.update_property(&[gtk4::accessible::Property::Label(
        "Clear ACARS message list",
    )]);
    let collapse_button = gtk4::ToggleButton::builder()
        .icon_name("view-list-bullet-symbolic")
        .tooltip_text(
            "Collapse adjacent duplicates within a 10-minute window. \
             Repeated messages from the same aircraft (e.g. Q0 link tests) \
             stack into a single row with a (×N) prefix.",
        )
        .build();
    collapse_button.update_property(&[gtk4::accessible::Property::Label(
        "Collapse duplicate ACARS messages",
    )]);
    let filter_entry = gtk4::SearchEntry::builder()
        .placeholder_text("Filter aircraft / label / text…")
        .build();

    // ─── Column view ───
    let store = gtk4::gio::ListStore::new::<AcarsMessageObject>();
    // Hydrate from the running ACARS ring so reopening the
    // viewer mid-session shows the retained backlog instead of
    // an empty table. Window.rs keeps `state.acars_recent`
    // populated regardless of viewer lifetime; the ring is
    // already bounded by `default_recent_keep`, which is the
    // same cap the append site enforces on `store` (CR round 1
    // on PR #587), so this can't push the store past its cap.
    //
    // A parallel pass seeds the aircraft_store + aircraft_index
    // in the same walk so the By Aircraft tab also shows the
    // retained backlog on reopen (issue #579).
    let aircraft_store = gtk4::gio::ListStore::new::<AircraftEntryObject>();
    let aircraft_index_initial: std::cell::RefCell<
        std::collections::HashMap<arrayvec::ArrayString<8>, AircraftEntryObject>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    {
        let recent = state.acars_recent.borrow();
        for msg in recent.iter() {
            store.append(&AcarsMessageObject::new(msg.clone()));

            // Mirror into aircraft_index + aircraft_store.
            // New tail → initialize with msg_count=1 (already
            // counting this message) so the column view's first
            // bind reads the correct value, not zero. Existing
            // tail → record_message bumps in place. No
            // items_changed needed in the hydration loop; the
            // column view binds once after all hydration is done,
            // reading the final state.
            let mut idx = aircraft_index_initial.borrow_mut();
            if let Some(obj) = idx.get(&msg.aircraft) {
                obj.record_message(msg);
            } else {
                let entry = AircraftEntry {
                    tail: msg.aircraft,
                    last_seen: msg.timestamp,
                    msg_count: 1,
                    last_label: msg.label,
                };
                let obj = AircraftEntryObject::new(entry);
                aircraft_store.append(&obj);
                idx.insert(msg.aircraft, obj);
            }
        }
    }
    let initial_count = store.n_items();
    let status_label = gtk4::Label::builder()
        .label(format!("{initial_count} / {initial_count} messages"))
        .build();

    header.pack_start(&pause_button);
    header.pack_start(&clear_button);
    header.pack_start(&collapse_button);
    header.set_title_widget(Some(&filter_entry));
    header.pack_end(&status_label);

    let filter = gtk4::CustomFilter::new(|_obj| true);
    let filter_model = gtk4::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    // SortListModel in the chain so column-header clicks reorder
    // the visible rows. Issue #585. Sorter starts as `None`; it's
    // bound to `column_view.sorter()` after the column-view exists
    // so the user's header-click state drives sort order.
    let sort_model =
        gtk4::SortListModel::new(Some(filter_model.clone()), Option::<gtk4::Sorter>::None);
    let selection = gtk4::NoSelection::new(Some(sort_model.clone()));
    let column_view = gtk4::ColumnView::builder()
        .model(&selection)
        .show_column_separators(true)
        .show_row_separators(true)
        .build();
    sort_model.set_sorter(column_view.sorter().as_ref());

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

    // Per-column sorters. Each is a `CustomSorter` reading the
    // inner `AcarsMessage` via `obj.imp().inner.borrow()` (no
    // clone on the comparator hot path) and falling back to
    // `Ordering::Equal` when the slot is empty (model churn).
    // Issue #585.
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

    let mut time_column: Option<gtk4::ColumnViewColumn> = None;

    for (idx, (title, render, expand)) in columns.into_iter().enumerate() {
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(move |_factory, item| {
            // Fail closed on unexpected item type rather than
            // panic — these callbacks fire during model churn /
            // teardown, and a panic here would crash the whole
            // UI process.
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
                .and_then(|o| o.downcast::<AcarsMessageObject>().ok())
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
        if idx == 0 {
            time_column = Some(column.clone());
        }
        column_view.append_column(&column);
    }
    // Default sort: Time descending so newest stays at top
    // (matches the append-at-bottom behaviour pre-#585; just
    // expressed as a sort instead of insertion order).
    if let Some(col) = time_column {
        column_view.sort_by_column(Some(&col), gtk4::SortType::Descending);
    }

    let scroll = gtk4::ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .hexpand(true)
        .build();

    // Aircraft filter + filter model — needed before
    // `build_aircraft_column_view` is called below.
    // `aircraft_store` was declared in the hydration block above.
    let aircraft_filter = gtk4::CustomFilter::new(|_obj| true);
    let aircraft_filter_model =
        gtk4::FilterListModel::new(Some(aircraft_store.clone()), Some(aircraft_filter.clone()));

    // Build the aircraft column view. Helper returns its own
    // `ScrolledWindow` so each tab retains its own `GtkAdjustment`
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

    let stack_switcher = gtk4::StackSwitcher::builder().stack(&stack).build();
    // Pack switcher between the action buttons (already
    // `pack_start`'d) and the filter entry (set as title widget).
    // `pack_start` preserves declaration order so the switcher
    // sits to the right of the collapse button.
    header.pack_start(&stack_switcher);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&stack);
    window.set_content(Some(&content));

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
        aircraft_index: aircraft_index_initial,
        stack,
    });
    *state.acars_viewer_handles.borrow_mut() = Some(Rc::clone(&handles));

    // Click-to-filter (issue #579): double-click / Enter / Space on
    // an aircraft row sets the filter entry to the tail and switches
    // to the Stream tab. `GtkColumnView::connect_activate` fires on
    // double-click / Enter / Space; single click stays as selection.
    {
        let handles = Rc::clone(&handles);
        aircraft_column_view.connect_activate(move |_view, position| {
            let Some(obj) = handles
                .aircraft_filter_model
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

    // ── Clear button ──────────────────────────────────────────────
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(&handles);
        clear_button.connect_clicked(move |_| {
            handles.store.remove_all();
            handles.aircraft_store.remove_all();
            handles.aircraft_index.borrow_mut().clear();
            state.acars_recent.borrow_mut().clear();
            // Don't reset acars_total_count — that's the
            // running total since toggle-on, distinct from the
            // visible count. Status label refresh in the
            // items_changed handler recomputes "filtered / total"
            // from the now-empty filter_model + total_count.
            handles.status_label.set_label("0 / 0 messages");
        });
    }

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
        let refresh = std::rc::Rc::new(move || {
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
        });

        // Wire the same refresh closure to all three signal
        // sources. Each clone is its own move-into-closure so
        // lifetimes work out.
        {
            let r = std::rc::Rc::clone(&refresh);
            handles
                .filter_model
                .connect_items_changed(move |_, _, _, _| r());
        }
        {
            let r = std::rc::Rc::clone(&refresh);
            handles
                .aircraft_filter_model
                .connect_items_changed(move |_, _, _, _| r());
        }
        {
            let r = std::rc::Rc::clone(&refresh);
            handles.stack.connect_visible_child_notify(move |_| r());
        }
    }

    // Wire close-request to clear the `AppState` weak-ref slot AND
    // the per-viewer handles slot (so the message-append site in
    // `window.rs` sees a clean disengage state on next open).
    {
        let state = Rc::clone(state);
        window.connect_close_request(move |_| {
            *state.acars_viewer_window.borrow_mut() = None;
            *state.acars_viewer_handles.borrow_mut() = None;
            glib::Propagation::Proceed
        });
    }

    window
}

/// Allocation-free, Unicode-aware case-insensitive comparison.
/// Used by the Aircraft + Text column sorters where comparator
/// fires once per row pair per sort. The naive
/// `a.to_lowercase().cmp(&b.to_lowercase())` allocates two
/// `String`s per call; this version threads `char::to_lowercase`
/// through the iterator pair and stops at the first non-equal
/// codepoint without ever materialising a folded string.
/// CR round 1 on PR #590.
fn cmp_case_insensitive(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
}

/// Build a `CustomSorter` over `AcarsMessageObject` rows. The
/// inner `AcarsMessage` is borrowed (no clone on the comparator
/// hot path) and `Ordering::Equal` is returned on any unexpected
/// row state (model churn / wrong wrapper type / empty slot).
/// Issue #585.
fn make_message_sorter<F>(cmp: F) -> gtk4::CustomSorter
where
    F: Fn(&sdr_acars::AcarsMessage, &sdr_acars::AcarsMessage) -> std::cmp::Ordering + 'static,
{
    gtk4::CustomSorter::new(move |a, b| {
        let Some(a_obj) = a.downcast_ref::<AcarsMessageObject>() else {
            return gtk4::Ordering::Equal;
        };
        let Some(b_obj) = b.downcast_ref::<AcarsMessageObject>() else {
            return gtk4::Ordering::Equal;
        };
        let a_inner = a_obj.imp().inner.borrow();
        let b_inner = b_obj.imp().inner.borrow();
        match (a_inner.as_ref(), b_inner.as_ref()) {
            (Some(a_msg), Some(b_msg)) => cmp(a_msg, b_msg).into(),
            _ => gtk4::Ordering::Equal,
        }
    })
}

/// Aircraft-tab column descriptor. Same shape as [`ColumnSpec`]
/// but the render closure operates on [`AircraftEntryObject`].
type AircraftColumnSpec = (&'static str, fn(&AircraftEntryObject) -> String, bool);

/// Build the aircraft-tab column view + scrolled window. Returns
/// the `ScrolledWindow` so the caller can pack it into the stack.
/// The column view emits `connect_activate` for click-to-filter;
/// the caller wires that handler so the activate closure can hold
/// `Rc`s of the relevant viewer state.
#[allow(clippy::too_many_lines)]
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

    // `SortListModel` in the chain so column-header clicks reorder
    // visible rows. Sorter starts as `None`; bound to the column
    // view's sorter once it exists.
    let sort_model =
        gtk4::SortListModel::new(Some(filter_model.clone()), Option::<gtk4::Sorter>::None);
    // SingleSelection (vs NoSelection on the Stream tab) so the
    // column view's `activate` signal fires on double-click / Enter.
    // Click-to-filter wiring depends on `connect_activate`, which
    // NoSelection does not propagate.
    let selection = gtk4::SingleSelection::new(Some(sort_model.clone()));
    selection.set_can_unselect(true);
    let column_view = gtk4::ColumnView::builder()
        .model(&selection)
        .show_column_separators(true)
        .show_row_separators(true)
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
    let Some(entry) = obj.entry() else {
        return String::new();
    };
    let dt: chrono::DateTime<chrono::Local> = entry.last_seen.into();
    dt.format("%H:%M:%S").to_string()
}

fn render_aircraft_count(obj: &AircraftEntryObject) -> String {
    obj.entry()
        .map(|e| e.msg_count.to_string())
        .unwrap_or_default()
}

fn render_aircraft_last_label(obj: &AircraftEntryObject) -> String {
    let Some(entry) = obj.entry() else {
        return String::new();
    };
    let raw = std::str::from_utf8(&entry.last_label)
        .unwrap_or("??")
        .to_string();
    match sdr_acars::label::lookup(entry.last_label) {
        Some(name) => format!("{raw} ({name})"),
        None => raw,
    }
}

/// Render the Time column. Uses the wrapper's `last_seen` (so
/// duplicate-collapsed rows show the most recent occurrence,
/// not the first), and prepends `(×N)` when `count > 1`.
fn render_time(obj: &AcarsMessageObject) -> String {
    let dt: chrono::DateTime<chrono::Local> = obj.last_seen().into();
    let formatted = dt.format("%H:%M:%S").to_string();
    let count = obj.count();
    if count > 1 {
        format!("(×{count}) {formatted}")
    } else {
        formatted
    }
}

/// Borrow the inner `AcarsMessage` and run `f`, falling back to
/// an empty string if the slot is unset (model churn / partial
/// teardown). All non-time columns flow through this helper so
/// the borrow + None-fallback pattern stays in one place.
fn render_inner<F: FnOnce(&sdr_acars::AcarsMessage) -> String>(
    obj: &AcarsMessageObject,
    f: F,
) -> String {
    obj.imp()
        .inner
        .borrow()
        .as_ref()
        .map_or_else(String::new, f)
}

fn render_freq(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| format!("{:.3}", m.freq_hz / 1_000_000.0))
}
fn render_aircraft(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| m.aircraft.to_string())
}
fn render_mode(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| char::from(m.mode).to_string())
}
fn render_label(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| {
        let raw = std::str::from_utf8(&m.label).unwrap_or("??").to_string();
        match sdr_acars::label::lookup(m.label) {
            Some(name) => format!("{raw} ({name})"),
            None => raw,
        }
    })
}
fn render_block(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| char::from(m.block_id).to_string())
}
fn render_ack(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| match m.ack {
        b'\x15' => "NAK".to_string(),
        b'!' => "!".to_string(),
        c if c.is_ascii_graphic() => char::from(c).to_string(),
        c => format!("0x{c:02X}"),
    })
}
fn render_text(obj: &AcarsMessageObject) -> String {
    render_inner(obj, |m| {
        // Multi-block reassembly indicator (#580). Surfaces both
        // happy-path "[N blocks]" and the ETB-without-ETX
        // "[N blocks, partial]" cases via `end_of_message`.
        if m.reassembled_block_count > 1 {
            let count = m.reassembled_block_count;
            if m.end_of_message {
                format!("[{count} blocks] {}", m.text)
            } else {
                format!("[{count} blocks, partial] {}", m.text)
            }
        } else {
            m.text.clone()
        }
    })
}

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
            channel_idx: 0,
            freq_hz: 131_550_000.0,
            level_db: 0.0,
            error_count: 0,
            mode: b'2',
            label,
            block_id: b'5',
            ack: b'!',
            aircraft,
            flight_id: None,
            message_no: None,
            text: String::new(),
            end_of_message: true,
            reassembled_block_count: 1,
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
        let later = fixture_message("X", *b"H1", t0 + Duration::from_mins(1));
        let earlier = fixture_message("X", *b"H1", t0 + Duration::from_secs(30));
        obj.record_message(&later);
        obj.record_message(&earlier);
        assert_eq!(obj.entry().unwrap().last_seen, t0 + Duration::from_mins(1));
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
