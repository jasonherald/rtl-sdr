//! Non-modal WEFAX viewer window + the `app.wefax-open` action.
//!
//! Split out of the top-level `wefax_viewer.rs` module per the
//! 500-NLOC file-size gate (#832) when the live-viewer fix pass
//! (scroll-to-follow + auto-open on mode select) pushed the
//! monolithic file over budget. Everything here is GTK
//! window/widget wiring; the pure renderer and the `WefaxImageView`
//! widget itself stay in the parent module.

use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::messages::UiToDsp;
use crate::viewer::{plain_toast, show_toast_in};

use super::{
    VIEWER_WINDOW_HEIGHT, VIEWER_WINDOW_WIDTH, WEFAX_VIEWER_PLACEHOLDER_SUBTITLE, WefaxImageView,
};

// ─── Non-modal viewer window ─────────────────────────────────────────────────

/// Build the header bar's Pause/Resume toggle. Split out of
/// [`open_wefax_viewer_window`] per the 50-NLOC gate (#832).
fn build_pause_button(view: &WefaxImageView) -> gtk4::ToggleButton {
    let pause_btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume live chart update")
        .build();
    pause_btn.update_property(&[gtk4::accessible::Property::Label(
        "Pause or resume live WEFAX chart update",
    )]);
    let pause_view = view.clone();
    pause_btn.connect_toggled(move |btn| {
        pause_view.set_paused(btn.is_active());
    });
    pause_btn
}

/// Build the header bar's Clear button. Split out of
/// [`open_wefax_viewer_window`] per the 50-NLOC gate (#832).
fn build_clear_button(view: &WefaxImageView) -> gtk4::Button {
    let clear_btn = gtk4::Button::builder()
        .icon_name("edit-clear-all-symbolic")
        .tooltip_text("Clear the chart buffer and start fresh")
        .build();
    clear_btn.update_property(&[gtk4::accessible::Property::Label(
        "Clear WEFAX chart buffer",
    )]);
    let clear_view = view.clone();
    clear_btn.connect_clicked(move |_| {
        clear_view.clear();
    });
    clear_btn
}

/// Build the header bar's Export PNG button. Split out of
/// [`open_wefax_viewer_window`] per the 50-NLOC gate (#832).
fn build_export_button(view: &WefaxImageView, window: &adw::Window) -> gtk4::Button {
    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export the current WEFAX chart to PNG")
        .build();
    export_btn.update_property(&[gtk4::accessible::Property::Label(
        "Export WEFAX chart to PNG",
    )]);
    let export_view = view.clone();
    let window_for_export = window.downgrade();
    let export_btn_weak = export_btn.downgrade();
    export_btn.connect_clicked(move |_| {
        let Some(window_for_export) = window_for_export.upgrade() else {
            return;
        };
        let Some(btn) = export_btn_weak.upgrade() else {
            return;
        };
        if !btn.is_sensitive() {
            return;
        }
        btn.set_sensitive(false);
        let btn_for_complete = btn.downgrade();
        let path = default_export_path();
        let path_for_msg = path.clone();
        let window_weak = window_for_export.downgrade();
        export_view.export_png_async(path, move |result| {
            let toast = match result {
                Ok(()) => plain_toast(&format!("Saved {}", path_for_msg.display())),
                Err(e) => plain_toast(&format!("PNG export failed: {e}")),
            };
            if let Some(window) = window_weak.upgrade() {
                show_toast_in(&window, toast);
            }
            if let Some(btn) = btn_for_complete.upgrade() {
                btn.set_sensitive(true);
            }
        });
    });
    export_btn
}

/// Open the WEFAX viewer in a non-modal transient window. Returns the
/// inner [`WefaxImageView`] so the caller can pump snapshots into it.
///
/// Non-modal so the user can keep tuning while the chart builds.
///
/// The drawing area is wrapped in a `GtkScrolledWindow`: a WEFAX
/// chart has no fixed height and a full reception can run well past
/// the window's visible area, so the chart is painted near
/// 1:1 (via `set_content_width`/`set_content_height` tracking the
/// renderer's growing surface) and the view auto-follows the newest
/// line — see [`WefaxImageView::update_from_handle`].
pub fn open_wefax_viewer_window<W: gtk4::prelude::IsA<gtk4::Window>>(
    parent: &W,
    title: &str,
) -> (WefaxImageView, adw::Window) {
    let view = WefaxImageView::new();

    let window = adw::Window::builder()
        .title(title)
        .default_width(VIEWER_WINDOW_WIDTH)
        .default_height(VIEWER_WINDOW_HEIGHT)
        .transient_for(parent)
        .modal(false)
        .build();
    // Inherit the parent's GApplication so Wayland's
    // `xdg_toplevel_set_app_id` carries `com.sdr.rs` and the WM can
    // resolve our icon. See apt_viewer.rs for the full rationale.
    window.set_application(parent.application().as_ref());

    let header = adw::HeaderBar::new();
    let title_widget = adw::WindowTitle::new(title, WEFAX_VIEWER_PLACEHOLDER_SUBTITLE);
    header.set_title_widget(Some(&title_widget));
    view.set_title_widget(title_widget);

    header.pack_start(&build_pause_button(&view));
    header.pack_start(&build_clear_button(&view));
    header.pack_end(&build_export_button(&view, &window));

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .child(view.drawing_area())
        .build();
    view.set_scroll_adjustment(scrolled.vadjustment());

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar));

    window.set_content(Some(&toast_overlay));
    window.present();

    (view, window)
}

/// Default export path: `~/sdr-recordings/wefax-YYYY-MM-DD-HHMMSS.png`.
///
/// Mirrors [`crate::sstv_viewer`]'s `default_export_path` — probes
/// for existing files and appends `-1`, `-2`, … so two manual
/// exports inside the same wall-clock second don't collide.
fn default_export_path() -> PathBuf {
    let timestamp = glib::DateTime::now_local()
        .and_then(|dt| dt.format("%Y-%m-%d-%H%M%S"))
        .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
    let dir = glib::home_dir().join("sdr-recordings");
    let stem = format!("wefax-{timestamp}");
    let mut path = dir.join(format!("{stem}.png"));
    let mut suffix = 1_u32;
    while path.exists() {
        path = dir.join(format!("{stem}-{suffix}.png"));
        suffix += 1;
    }
    path
}

// ─── Live viewer action ──────────────────────────────────────────────────────

/// Wire the `app.wefax-open` action onto `app`. Activating it (via
/// the app menu or `Ctrl+Shift+F`) opens a non-modal WEFAX viewer
/// window. If a viewer is already open, activating it presents
/// (focuses) the existing window and re-sends the current
/// `WefaxImageHandle` to the DSP so the viewer reflects the latest
/// state — mirrors [`crate::sstv_viewer::connect_sstv_action`].
pub fn connect_wefax_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    let action = gio::SimpleAction::new("wefax-open", None);
    let parent_provider = Rc::clone(parent_provider);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_wefax_viewer_if_needed(&parent_provider, &state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.wefax-open", &["<Ctrl><Shift>f"]);
}

/// Open the WEFAX viewer window if it isn't already open, registering
/// the new view in `state.wefax_viewer` and sending `SetWefaxImage` to
/// the DSP so the decoder tap starts pushing lines into the handle.
/// No-op if a viewer is already open (re-presents it instead).
///
/// Called both from the `app.wefax-open` action (`Ctrl+Shift+F`) and
/// from `window/dsp_events.rs::on_demod_mode_changed` when the user
/// selects WEFAX demod mode — without the latter, selecting the mode
/// from the dropdown silently decoded nothing because
/// `UiToDsp::SetWefaxImage` (which the decode tap needs) was only
/// ever sent by the keyboard-shortcut path. Per whole-branch review.
pub fn open_wefax_viewer_if_needed(
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    if state.wefax_viewer.borrow().is_some() {
        // Re-send the image handle so the tap stays wired even if a
        // future code path ever clears it (idempotent), then raise
        // the existing window. Mirrors
        // `sstv_viewer::open_sstv_viewer_if_needed`.
        state.send_dsp(UiToDsp::SetWefaxImage(state.wefax_image.handle()));
        if let Some(window) = state
            .wefax_viewer_window
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
        {
            window.present();
        }
        return;
    }
    let Some(parent) = parent_provider() else {
        tracing::warn!("wefax-open invoked with no main window available");
        return;
    };
    let (view, window) = open_wefax_viewer_window(&parent, "WEFAX Chart");
    // Attach the shared handle to the view so the Clear button wipes
    // the source-side pixel buffer too — otherwise the next
    // `update_from_handle` replays the old rows.
    view.set_handle(state.wefax_image.handle());
    *state.wefax_viewer.borrow_mut() = Some(view);
    *state.wefax_viewer_window.borrow_mut() = Some(window.downgrade());

    // Hand the shared handle to the DSP so the decoder tap can push
    // lines into it. The handle is a clone of the long-lived
    // singleton in `AppState::wefax_image`.
    state.send_dsp(UiToDsp::SetWefaxImage(state.wefax_image.handle()));

    let state_for_close = Rc::clone(state);
    window.connect_close_request(move |_| {
        *state_for_close.wefax_viewer.borrow_mut() = None;
        *state_for_close.wefax_viewer_window.borrow_mut() = None;
        // Closing the viewer does NOT send `ClearWefaxImage` — the
        // decoder keeps running and the shared handle keeps
        // accumulating data, mirroring the SSTV/LRPT
        // close-without-clear semantics so a future auto-save flow
        // (Task 13) still sees completed charts.
        glib::Propagation::Proceed
    });
}
