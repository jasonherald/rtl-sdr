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
    /// column-view factories pull the inner `AcarsMessage`
    /// back out via `obj.message().expect(...)` per render.
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
    let clear_button = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Clear all messages from the view (does not disable ACARS)")
        .build();
    let filter_entry = gtk4::SearchEntry::builder()
        .placeholder_text("Filter aircraft / label / text…")
        .build();
    let status_label = gtk4::Label::builder().label("0 / 0 messages").build();

    header.pack_start(&pause_button);
    header.pack_start(&clear_button);
    header.set_title_widget(Some(&filter_entry));
    header.pack_end(&status_label);

    // ─── Column view ───
    let store = gtk4::gio::ListStore::new::<AcarsMessageObject>();
    let filter = gtk4::CustomFilter::new(|_obj| true);
    let filter_model = gtk4::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    let selection = gtk4::NoSelection::new(Some(filter_model.clone()));
    let column_view = gtk4::ColumnView::builder()
        .model(&selection)
        .show_column_separators(true)
        .show_row_separators(true)
        .build();

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

    for (title, render, expand) in columns {
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(move |_factory, item| {
            let label = gtk4::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            item.downcast_ref::<gtk4::ListItem>()
                .expect("setup item is a ListItem")
                .set_child(Some(&label));
        });
        factory.connect_bind(move |_factory, item| {
            let item = item
                .downcast_ref::<gtk4::ListItem>()
                .expect("bind item is a ListItem");
            let label = item
                .child()
                .and_then(|w| w.downcast::<gtk4::Label>().ok())
                .expect("setup installed a Label child");
            let obj = item
                .item()
                .and_then(|o| o.downcast::<AcarsMessageObject>().ok())
                .expect("model row is an AcarsMessageObject");
            if let Some(msg) = obj.message() {
                label.set_text(&render(&msg));
            }
        });
        let column = gtk4::ColumnViewColumn::builder()
            .title(title)
            .factory(&factory)
            .resizable(true)
            .expand(expand)
            .build();
        column_view.append_column(&column);
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

    let handles = Rc::new(ViewerHandles {
        store,
        filter,
        filter_model,
        status_label: status_label.clone(),
        pause_button: pause_button.clone(),
        filter_entry: filter_entry.clone(),
    });
    *state.acars_viewer_handles.borrow_mut() = Some(handles);

    window
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
