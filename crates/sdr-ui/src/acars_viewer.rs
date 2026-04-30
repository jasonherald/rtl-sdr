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
}

/// Default window dimensions (per spec `acars_viewer.rs` budget).
const ACARS_VIEWER_WINDOW_WIDTH: i32 = 1100;
const ACARS_VIEWER_WINDOW_HEIGHT: i32 = 600;

// ── glib::Object wrapper around an AcarsMessage ────────────────

mod imp {
    use std::cell::RefCell;

    use gtk4::glib;
    use gtk4::glib::subclass::prelude::{ObjectImpl, ObjectSubclass};

    #[derive(Default)]
    pub struct AcarsMessageObject {
        pub inner: RefCell<Option<sdr_acars::AcarsMessage>>,
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
    #[must_use]
    pub fn new(msg: sdr_acars::AcarsMessage) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().inner.borrow_mut() = Some(msg);
        obj
    }

    /// Borrow the wrapped message. Returns `None` only if a
    /// caller called `take()` (we don't); callers may
    /// `expect()` in factory closures.
    #[must_use]
    pub fn message(&self) -> Option<sdr_acars::AcarsMessage> {
        self.imp().inner.borrow().clone()
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
type ColumnSpec = (&'static str, fn(&sdr_acars::AcarsMessage) -> String, bool);

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
    {
        let recent = state.acars_recent.borrow();
        for msg in recent.iter().cloned() {
            store.append(&AcarsMessageObject::new(msg));
        }
    }
    let initial_count = store.n_items();
    let status_label = gtk4::Label::builder()
        .label(format!("{initial_count} / {initial_count} messages"))
        .build();

    header.pack_start(&pause_button);
    header.pack_start(&clear_button);
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

    // Seven columns per spec section "Content":
    //   Time | Freq | Aircraft | Mode | Label | Block | Text
    let columns: [ColumnSpec; 7] = [
        ("Time", render_time, false),
        ("Freq", render_freq, false),
        ("Aircraft", render_aircraft, false),
        ("Mode", render_mode, false),
        ("Label", render_label, false),
        ("Block", render_block, false),
        ("Text", render_text, true),
    ];

    // Per-column sorters. Each is a `CustomSorter` reading the
    // inner `AcarsMessage` via `obj.imp().inner.borrow()` (no
    // clone on the comparator hot path) and falling back to
    // `Ordering::Equal` when the slot is empty (model churn).
    // Issue #585.
    let sorters: [gtk4::CustomSorter; 7] = [
        make_message_sorter(|a, b| a.timestamp.cmp(&b.timestamp)),
        make_message_sorter(|a, b| {
            a.freq_hz
                .partial_cmp(&b.freq_hz)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        make_message_sorter(|a, b| a.aircraft.to_lowercase().cmp(&b.aircraft.to_lowercase())),
        make_message_sorter(|a, b| a.mode.cmp(&b.mode)),
        make_message_sorter(|a, b| a.label.cmp(&b.label)),
        make_message_sorter(|a, b| a.block_id.cmp(&b.block_id)),
        make_message_sorter(|a, b| a.text.to_lowercase().cmp(&b.text.to_lowercase())),
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
            // Borrow rather than clone the inner message —
            // factory.connect_bind fires on every visible row
            // every store change; cloning the full
            // `AcarsMessage` (with String + ArrayString fields)
            // here would be wasted work when the render fns
            // only need a borrow.
            let inner = obj.imp().inner.borrow();
            if let Some(msg) = inner.as_ref() {
                label.set_text(&render(msg));
            }
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

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&scroll);
    window.set_content(Some(&content));

    // Hoist handles so all signal handlers below can clone from it.
    let handles = Rc::new(ViewerHandles {
        store,
        filter,
        filter_model,
        status_label: status_label.clone(),
        pause_button: pause_button.clone(),
        filter_entry: filter_entry.clone(),
    });
    *state.acars_viewer_handles.borrow_mut() = Some(Rc::clone(&handles));

    // ── Clear button ──────────────────────────────────────────────
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(&handles);
        clear_button.connect_clicked(move |_| {
            handles.store.remove_all();
            state.acars_recent.borrow_mut().clear();
            // Don't reset acars_total_count — that's the
            // running total since toggle-on, distinct from the
            // visible count. Status label refresh in the
            // items_changed handler recomputes "filtered / total"
            // from the now-empty filter_model + total_count.
            handles.status_label.set_label("0 / 0 messages");
        });
    }

    // ── Filter: live substring match on aircraft + label + text ──
    {
        let filter = handles.filter.clone();
        let entry = handles.filter_entry.clone();
        entry.connect_search_changed(move |entry| {
            let needle_str: String = entry.text().as_str().to_lowercase();
            filter.set_filter_func(move |obj| {
                let Some(obj) = obj.downcast_ref::<AcarsMessageObject>() else {
                    return false;
                };
                // Borrow the inner message — the filter predicate
                // fires for every row on every keystroke + every
                // append, so cloning the full message here would
                // be a measurable hot-path cost on long-running
                // sessions.
                let inner = obj.imp().inner.borrow();
                let Some(msg) = inner.as_ref() else {
                    return false;
                };
                if needle_str.is_empty() {
                    return true;
                }
                let needle = &needle_str;
                msg.aircraft.to_lowercase().contains(needle)
                    || std::str::from_utf8(&msg.label)
                        .is_ok_and(|s| s.to_lowercase().contains(needle))
                    || msg.text.to_lowercase().contains(needle)
            });
        });
    }

    // ── Status label: <filtered> / <total> ───────────────────────
    // Re-evaluated on every store change. `items-changed` fires on
    // append AND on filter re-evaluation, so this catches both.
    //
    // Read the filter-model count off the signal's `model`
    // argument rather than capturing a strong clone — the latter
    // creates a self-reference (model owns the handler, handler
    // owns the model) that would keep the viewer model + store
    // alive past window close.
    {
        let status = handles.status_label.clone();
        let store = handles.store.clone();
        handles
            .filter_model
            .connect_items_changed(move |model, _, _, _| {
                let filtered = model.n_items();
                let total = store.n_items();
                status.set_label(&format!("{filtered} / {total} messages"));
            });
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

fn render_time(msg: &sdr_acars::AcarsMessage) -> String {
    let dt: chrono::DateTime<chrono::Local> = msg.timestamp.into();
    dt.format("%H:%M:%S").to_string()
}
fn render_freq(msg: &sdr_acars::AcarsMessage) -> String {
    format!("{:.3}", msg.freq_hz / 1_000_000.0)
}
fn render_aircraft(msg: &sdr_acars::AcarsMessage) -> String {
    msg.aircraft.to_string()
}
fn render_mode(msg: &sdr_acars::AcarsMessage) -> String {
    char::from(msg.mode).to_string()
}
fn render_label(msg: &sdr_acars::AcarsMessage) -> String {
    let raw = std::str::from_utf8(&msg.label).unwrap_or("??").to_string();
    match sdr_acars::label::lookup(msg.label) {
        Some(name) => format!("{raw} ({name})"),
        None => raw,
    }
}
fn render_block(msg: &sdr_acars::AcarsMessage) -> String {
    char::from(msg.block_id).to_string()
}
fn render_text(msg: &sdr_acars::AcarsMessage) -> String {
    msg.text.clone()
}
