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

use crate::state::AppState;

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

// (build_acars_viewer_window + per-feature wiring lands in
// Tasks 8-12; this task ships only the wrapper + open helper.)

fn build_acars_viewer_window(_state: &Rc<AppState>) -> adw::Window {
    // Placeholder — Task 8 fills this in.
    adw::Window::builder()
        .title("ACARS")
        .default_width(ACARS_VIEWER_WINDOW_WIDTH)
        .default_height(ACARS_VIEWER_WINDOW_HEIGHT)
        .modal(false)
        .build()
}
