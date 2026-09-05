//! Orbcomm viewer window (issue #865, Task 11).
//!
//! Floating top-level `adw::Window` showing decoded Orbcomm packets
//! and reassembled subscriber messages as a scrolling monospace log,
//! plus a per-channel activity strip. Same lifecycle pattern as
//! `acars_viewer` / `apt_viewer`: opened via the `app.orbcomm-open`
//! action (`Ctrl+Shift+O`), weakly held in
//! `AppState::orbcomm_viewer_window` so a second activation presents
//! the existing window rather than spawning a duplicate.
//!
//! Unlike ACARS, Orbcomm has no airband-lock geometry to unwind on
//! enable/disable — the decode tap just mixes down a fixed set of
//! [`sdr_orbcomm::ORBCOMM_CHANNELS_HZ`] channels in parallel with
//! whatever the user is otherwise tuned to. So the enable switch
//! tracks the DSP's `OrbcommEnabledChanged` ack rather than setting
//! itself optimistically: cleanup (source stop) force-disables the
//! tap with its own ack, and an optimistic switch would then show
//! "on" for a tap that's actually dead.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::AdwWindowExt;

use sdr_orbcomm::channelizer::ChannelStats;

use crate::orbcomm_render::channel_label_text;
use crate::state::AppState;

/// Default viewer window size. Wide enough for a full ephemeris log
/// line at the monospace font size without wrapping.
const ORBCOMM_VIEWER_WINDOW_WIDTH: i32 = 900;
const ORBCOMM_VIEWER_WINDOW_HEIGHT: i32 = 600;

/// Cap on retained log entries; the oldest is trimmed once this is
/// exceeded. Mirrors the ACARS viewer's bounded store — a multi-hour
/// unattended session shouldn't grow UI memory without bound. Named
/// `_ENTRIES` rather than `_LINES`: one entry is one `format_packet_row`
/// result, which may itself span several rendered text lines (a
/// `MessageComplete` hexdump block).
const MAX_LOG_ENTRIES: usize = 500;

/// Pixel tolerance for the "scrolled to bottom" auto-follow check.
/// `GtkAdjustment` values are fractional, so an exact compare against
/// `upper() - page_size()` would miss sub-pixel rests.
const SCROLL_BOTTOM_TOLERANCE_PX: f64 = 1.0;

// ─── Window ─────────────────────────────────────────────────────

/// Per-viewer handles the `DspToUi::Orbcomm*` append sites in
/// `window/dsp_events/orbcomm_events.rs` need. Stored on `AppState`
/// (`orbcomm_viewer_handles`, a sibling field of
/// `orbcomm_viewer_window`) so the dispatch sites can fetch them
/// without re-walking the widget tree. Cleared on the window's
/// close-request. Mirrors `acars_viewer::ViewerHandles`.
pub struct ViewerHandles {
    /// Sends `UiToDsp::SetOrbcommEnabled` on user toggle. State is
    /// driven by `OrbcommEnabledChanged` acks (see the module doc
    /// comment for why), never set optimistically.
    pub enable_switch: gtk4::Switch,
    /// Re-entrancy guard around the ack-driven `set_active` call so
    /// the switch's own `active` notify handler doesn't re-dispatch
    /// `SetOrbcommEnabled` for a state change WE just made.
    pub suppress_switch_notify: Cell<bool>,
    /// Monospace log view. Fully re-rendered from `log_entries` on
    /// every append (see that field's doc) rather than edited
    /// in-place.
    pub log_view: gtk4::TextView,
    /// `ScrolledWindow` wrapping `log_view`, used for the
    /// auto-scroll-to-bottom behavior on new arrivals — same
    /// direct-`GtkAdjustment` approach as `acars_viewer` (its
    /// `ColumnView::scroll_to` needs gtk4 `v4_12`; the workspace
    /// pins `v4_10`).
    pub scrolled_window: gtk4::ScrolledWindow,
    /// One label per `sdr_orbcomm::ORBCOMM_CHANNELS_HZ` entry, same
    /// order, updated from `DspToUi::OrbcommChannelStats`.
    pub channel_labels: Vec<gtk4::Label>,
    /// Bounded ring of rendered log entries — one
    /// [`format_packet_row`] result per entry (a single line for a
    /// `Packet` event, a multi-line hexdump block for a
    /// `MessageComplete` event). Capped at [`MAX_LOG_ENTRIES`], oldest
    /// dropped first. The `log_view` buffer is fully re-rendered from
    /// this ring on every append — simpler to get right than
    /// incremental `GtkTextIter` surgery. The join borrows each
    /// entry's bytes (`String::as_str`, no clone) rather than
    /// duplicating the ring, so the cost of a re-render is one
    /// allocation the size of the rendered log — bounded by
    /// `MAX_LOG_ENTRIES`, not by decode rate.
    pub log_entries: std::cell::RefCell<VecDeque<String>>,
}

/// Open the Orbcomm viewer window if not already open. If a viewer
/// is already open (held weakly in `state.orbcomm_viewer_window`),
/// present it instead of opening a second one. Exact signature mirror
/// of `acars_viewer::open_acars_viewer_if_needed` — Orbcomm has no
/// parent-window dependency either (the window pulls the app via the
/// gio default-application registry, same as ACARS/APT).
pub fn open_orbcomm_viewer_if_needed(state: &Rc<AppState>) {
    if let Some(weak) = state.orbcomm_viewer_window.borrow().as_ref()
        && let Some(window) = weak.upgrade()
    {
        window.present();
        return;
    }
    let window = build_orbcomm_viewer_window(state);
    *state.orbcomm_viewer_window.borrow_mut() = Some(window.downgrade());
    window.present();
}

/// Wire the `app.orbcomm-open` action (`Ctrl+Shift+O`) and hand it to
/// [`open_orbcomm_viewer_if_needed`]. Called from `window.rs` beside
/// `connect_apt_action` / `connect_lrpt_action` / `connect_sstv_action`.
/// No `parent_provider` parameter (unlike those three) — Orbcomm's
/// open function doesn't need one, matching `acars_viewer`.
pub fn connect_orbcomm_action(app: &adw::Application, state: &Rc<AppState>) {
    let action = gtk4::gio::SimpleAction::new("orbcomm-open", None);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_orbcomm_viewer_if_needed(&state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.orbcomm-open", &["<Ctrl><Shift>o"]);
}

/// Header bar carrying the "Decode" enable switch. Returns both so the
/// caller can keep the switch in [`ViewerHandles`] — the ack path
/// (`apply_enabled_ack`) drives it, and it never sets itself
/// optimistically (module docs).
fn build_header_bar(state: &Rc<AppState>) -> (adw::HeaderBar, gtk4::Switch) {
    let header = adw::HeaderBar::new();
    let enable_switch = gtk4::Switch::builder()
        .valign(gtk4::Align::Center)
        .active(state.orbcomm_enabled.get())
        .tooltip_text("Enable Orbcomm decoding (9 fixed 137 MHz downlink channels)")
        .build();
    enable_switch.update_property(&[gtk4::accessible::Property::Label("Enable Orbcomm decoding")]);
    let enable_label = gtk4::Label::builder()
        .label("Decode")
        .valign(gtk4::Align::Center)
        .build();
    let enable_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    enable_box.append(&enable_label);
    enable_box.append(&enable_switch);
    header.pack_start(&enable_box);
    (header, enable_switch)
}

/// Per-channel activity strip: one label per
/// [`sdr_orbcomm::ORBCOMM_CHANNELS_HZ`] entry, in that order — the
/// order [`refresh_channel_strip`] indexes by.
fn build_channel_strip() -> (gtk4::Box, Vec<gtk4::Label>) {
    let strip = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    strip.set_margin_start(12);
    strip.set_margin_end(12);
    strip.set_margin_top(6);
    strip.set_margin_bottom(6);
    let channel_labels: Vec<gtk4::Label> = sdr_orbcomm::ORBCOMM_CHANNELS_HZ
        .iter()
        .map(|&hz| {
            let label = gtk4::Label::builder()
                .label(channel_label_text(hz, None))
                .justify(gtk4::Justification::Center)
                .tooltip_text(
                    "err = checksum failures on locked strides \
                     (including single-bit-repair rejects); \
                     dimmed when the channel falls outside the source span",
                )
                .build();
            strip.append(&label);
            label
        })
        .collect();
    (strip, channel_labels)
}

/// Monospace, non-wrapping log view inside its own scroller. The
/// scroller is returned alongside because [`append_log_entry`]'s
/// auto-follow drives its `GtkAdjustment` directly.
fn build_log_view() -> (gtk4::TextView, gtk4::ScrolledWindow) {
    let log_view = gtk4::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk4::WrapMode::None)
        .top_margin(4)
        .bottom_margin(4)
        .left_margin(6)
        .right_margin(6)
        .build();
    let scrolled_window = gtk4::ScrolledWindow::builder()
        .child(&log_view)
        .vexpand(true)
        .hexpand(true)
        .build();
    (log_view, scrolled_window)
}

/// Enable-switch dispatch + close-request teardown for a freshly built
/// viewer window.
fn wire_viewer_signals(
    window: &adw::Window,
    enable_switch: &gtk4::Switch,
    handles: &Rc<ViewerHandles>,
    state: &Rc<AppState>,
) {
    // ─── Enable switch → SetOrbcommEnabled ───
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(handles);
        enable_switch.connect_active_notify(move |sw| {
            if handles.suppress_switch_notify.get() {
                return;
            }
            state.send_dsp(crate::messages::UiToDsp::SetOrbcommEnabled(sw.is_active()));
        });
    }

    // Wire close-request to clear both `AppState` slots, mirroring
    // `acars_viewer`.
    {
        let state = Rc::clone(state);
        window.connect_close_request(move |_| {
            *state.orbcomm_viewer_window.borrow_mut() = None;
            *state.orbcomm_viewer_handles.borrow_mut() = None;
            glib::Propagation::Proceed
        });
    }
}

fn build_orbcomm_viewer_window(state: &Rc<AppState>) -> adw::Window {
    let window = adw::Window::builder()
        .title("Orbcomm")
        .default_width(ORBCOMM_VIEWER_WINDOW_WIDTH)
        .default_height(ORBCOMM_VIEWER_WINDOW_HEIGHT)
        .modal(false)
        .build();
    // Link to the GApplication so Wayland's app-id + icon resolve
    // correctly. Same rationale as `acars_viewer`/`apt_viewer`: this
    // window has no parent to inherit application-ness from.
    if let Some(app) =
        gtk4::gio::Application::default().and_then(|a| a.downcast::<gtk4::Application>().ok())
    {
        window.set_application(Some(&app));
    }

    let (header, enable_switch) = build_header_bar(state);
    let (strip, channel_labels) = build_channel_strip();
    let (log_view, scrolled_window) = build_log_view();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&strip);
    content.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    content.append(&scrolled_window);
    window.set_content(Some(&content));

    let handles = Rc::new(ViewerHandles {
        enable_switch: enable_switch.clone(),
        suppress_switch_notify: Cell::new(false),
        log_view,
        scrolled_window,
        channel_labels,
        log_entries: std::cell::RefCell::new(VecDeque::new()),
    });
    *state.orbcomm_viewer_handles.borrow_mut() = Some(Rc::clone(&handles));

    // Seed the strip from whatever stats already exist — reopening
    // the viewer mid-session shows the live counts immediately
    // rather than blanking until the next `OrbcommChannelStats` tick.
    refresh_channel_strip(&handles, &state.orbcomm_channel_stats.borrow());

    wire_viewer_signals(&window, &enable_switch, &handles, state);

    window
}

/// Refresh every channel-strip label from a fresh `ChannelStats`
/// slice (`DspToUi::OrbcommChannelStats` order matches
/// `ORBCOMM_CHANNELS_HZ` order, so this zips by index rather than
/// searching by frequency — but once a stats entry exists for that
/// index, `s.freq_hz` is the authoritative frequency, not the
/// constant: an order/length mismatch between the emitted slice and
/// `ORBCOMM_CHANNELS_HZ` would otherwise silently pair the wrong
/// counts with the wrong label. `ORBCOMM_CHANNELS_HZ[i]` is used only
/// as the pre-first-tick placeholder, when no stats entry exists yet
/// for that index at all). Channels outside the source span get the
/// `dim-label` CSS class + `set_sensitive(false)`, same convention as
/// the rest of the sidebar (see `status_bar.rs`'s role badge).
pub(crate) fn refresh_channel_strip(handles: &ViewerHandles, stats: &[ChannelStats]) {
    for (i, label) in handles.channel_labels.iter().enumerate() {
        let Some(s) = stats.get(i) else {
            let freq_hz = sdr_orbcomm::ORBCOMM_CHANNELS_HZ[i];
            label.set_label(&channel_label_text(freq_hz, None));
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

/// Set the enable switch to match an `OrbcommEnabledChanged` ack,
/// guarding the programmatic `set_active` so its own `active` notify
/// handler doesn't bounce a redundant `SetOrbcommEnabled` back to the
/// DSP. No-op if the switch already shows the acked state (cleanup's
/// force-disable ack, for instance, commonly repeats the state the
/// switch already shows).
pub(crate) fn apply_enabled_ack(handles: &ViewerHandles, enabled: bool) {
    if handles.enable_switch.is_active() == enabled {
        return;
    }
    handles.suppress_switch_notify.set(true);
    handles.enable_switch.set_active(enabled);
    handles.suppress_switch_notify.set(false);
}

/// Append one rendered log entry (a [`format_packet_row`] result) to
/// the viewer's log, trimming from the front once [`MAX_LOG_ENTRIES`]
/// is exceeded, and auto-scrolling to the bottom if the user was
/// already there (mirrors `acars_viewer`'s auto-scroll-to-top for its
/// newest-first sort — this log is oldest-first, so the anchor is the
/// bottom instead).
pub(crate) fn append_log_entry(handles: &ViewerHandles, entry: &str) {
    let adj = handles.scrolled_window.vadjustment();
    let was_at_bottom = (adj.value() + adj.page_size() - adj.upper()).abs()
        < SCROLL_BOTTOM_TOLERANCE_PX
        || adj.upper() <= adj.page_size();

    let joined = {
        let mut entries = handles.log_entries.borrow_mut();
        entries.push_back(entry.to_string());
        while entries.len() > MAX_LOG_ENTRIES {
            entries.pop_front();
        }
        // Reference each entry (`String::as_str`) rather than
        // cloning it — the ring already owns every entry's bytes
        // once, so this join is the only content copy on the append
        // path (into the new joined `String`), not two.
        entries
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    };
    handles.log_view.buffer().set_text(&joined);

    if was_at_bottom {
        // GTK4 recomputes `GtkTextView`'s adjustment bounds on the
        // next size-allocate pass, not synchronously inside
        // `set_text` — reading `adj.upper()` right here can still
        // observe the PRE-append bound and leave the view one entry
        // short on every auto-follow. Defer the scroll to the next
        // main-loop idle (same `glib::idle_add_local_once` idiom
        // `dsp_events/acars_events.rs::drain_deferred_aos_actions`
        // uses to run after the current dispatch) so it reads the
        // bound only after GTK has recomputed it. Weak ref: if the
        // window closes before the idle fires, just drop the scroll.
        let adj_weak = adj.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(adj) = adj_weak.upgrade() {
                adj.set_value(adj.upper());
            }
        });
    }
}
