//! Orbcomm activity panel (epic #867, Orbcomm slice).
//!
//! Docked left-activity surface that replaces the former floating
//! `orbcomm_viewer` window: enable toggle, a 3×3 channel-activity grid,
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

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::orbcomm_render::{
    METERS_PER_KM, channel_label_text, format_lat, format_lon, format_utc_hms,
};
use crate::sidebar::satellites_heard::HeardRow;
use crate::state::AppState;

/// Heard-list aging tick (seconds) — matches the old heard-group tick.
const HEARD_TICK_SECS: u32 = 5;

/// Cap on retained log entries; the oldest is trimmed once this is
/// exceeded. Named `_ENTRIES` rather than `_LINES`: one entry is one
/// `format_packet_row` result, which may itself span several rendered
/// text lines (a `MessageComplete` hexdump block).
const MAX_LOG_ENTRIES: usize = 500;

/// Pixel tolerance for the "scrolled to bottom" auto-follow check.
/// `GtkAdjustment` values are fractional, so an exact compare against
/// `upper() - page_size()` would miss sub-pixel rests.
const SCROLL_BOTTOM_TOLERANCE_PX: f64 = 1.0;

/// Per-panel runtime handles the `DspToUi::Orbcomm*` dispatch sites
/// in `window/dsp_events/orbcomm_events.rs` drive. Stashed on
/// `AppState::orbcomm_panel_handles` so those handlers can reach the
/// widgets without re-walking the tree. The panel lives for the app
/// lifetime, so — unlike the retired floating viewer — these are set
/// once and never cleared.
pub struct OrbcommPanelHandles {
    pub enable_switch: gtk4::Switch,
    /// Re-entrancy guard around the ack-driven `set_active` call so
    /// the switch's own `active` notify handler doesn't re-dispatch
    /// `SetOrbcommEnabled` for a state change WE just made.
    pub suppress_switch_notify: Cell<bool>,
    /// One label per `ORBCOMM_CHANNELS_HZ` entry, same order, laid out
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
    let grid = gtk4::Grid::builder()
        .row_spacing(6)
        .column_spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    let mut channel_cells = Vec::with_capacity(sdr_orbcomm::ORBCOMM_CHANNELS_HZ.len());
    for (i, &hz) in sdr_orbcomm::ORBCOMM_CHANNELS_HZ.iter().enumerate() {
        let label = gtk4::Label::builder()
            .label(channel_label_text(hz, None))
            .justify(gtk4::Justification::Center)
            .build();
        let (col, row) = (
            i32::try_from(i % 3).unwrap_or(0),
            i32::try_from(i / 3).unwrap_or(0),
        );
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
    // GtkLabel has no `monospace` property (that lives on GtkTextView);
    // the built-in `.monospace` CSS style class gives the same effect.
    let breakdown_label = gtk4::Label::builder()
        .xalign(0.0)
        .css_classes(["monospace"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    let breakdown_group = adw::PreferencesGroup::builder()
        .title("Packet types")
        .build();
    breakdown_group.add(&breakdown_label);

    // ── Packet / message log ──
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

    OrbcommPanel {
        widget: root,
        handles,
    }
}

impl OrbcommPanelHandles {
    /// Append one rendered log entry (a `format_packet_row` result) to
    /// the panel's log, trimming from the front once [`MAX_LOG_ENTRIES`]
    /// is exceeded, and auto-scrolling to the bottom if the user was
    /// already there. Ported verbatim in behavior from the retired
    /// `orbcomm_viewer::append_log_entry`.
    pub fn append_log_entry(&self, entry: &str) {
        let adj = self.scrolled_window.vadjustment();
        let was_at_bottom = (adj.value() + adj.page_size() - adj.upper()).abs()
            < SCROLL_BOTTOM_TOLERANCE_PX
            || adj.upper() <= adj.page_size();

        let joined = {
            let mut entries = self.log_entries.borrow_mut();
            entries.push_back(entry.to_string());
            while entries.len() > MAX_LOG_ENTRIES {
                entries.pop_front();
            }
            // Reference each entry (`String::as_str`) rather than
            // cloning it — the ring already owns every entry's bytes
            // once, so this join is the only content copy on the
            // append path (into the new joined `String`), not two.
            entries
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.log_view.buffer().set_text(&joined);

        if was_at_bottom {
            // GTK4 recomputes `GtkTextView`'s adjustment bounds on the
            // next size-allocate pass, not synchronously inside
            // `set_text` — reading `adj.upper()` right here can still
            // observe the PRE-append bound and leave the view one
            // entry short on every auto-follow. Defer the scroll to
            // the next main-loop idle so it reads the bound only after
            // GTK has recomputed it. Weak ref: if the panel is torn
            // down before the idle fires, just drop the scroll.
            let adj_weak = adj.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(adj) = adj_weak.upgrade() {
                    adj.set_value(adj.upper());
                }
            });
        }
    }

    /// Refresh every channel-grid cell from a fresh `ChannelStats`
    /// slice. `DspToUi::OrbcommChannelStats` order matches
    /// `ORBCOMM_CHANNELS_HZ` order, so this zips by index — but once a
    /// stats entry exists, `s.freq_hz` is the authoritative frequency,
    /// not the constant (used only as the pre-first-tick placeholder).
    /// Channels outside the source span get the `dim-label` CSS class +
    /// `set_sensitive(false)`, same convention as the retired viewer.
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

    /// Set the enable switch to match an `OrbcommEnabledChanged` ack,
    /// guarding the programmatic `set_active` so its own `active`
    /// notify handler doesn't bounce a redundant `SetOrbcommEnabled`
    /// back to the DSP. No-op if the switch already shows the acked
    /// state.
    pub fn apply_enabled_ack(&self, enabled: bool) {
        if self.enable_switch.is_active() == enabled {
            return;
        }
        self.suppress_switch_notify.set(true);
        self.enable_switch.set_active(enabled);
        self.suppress_switch_notify.set(false);
    }

    /// Rebuild the By-Spacecraft rows from a `HeardRow` snapshot.
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
    parts.join(" · ")
}

/// Wire the Orbcomm panel: stash its handles on `AppState` for the
/// `DspToUi::Orbcomm*` dispatch sites, dispatch `SetOrbcommEnabled` on
/// the Decode switch, and arm the heard-list aging tick.
pub fn connect_orbcomm_panel(panels: &crate::sidebar::SidebarPanels, state: &Rc<AppState>) {
    let handles = Rc::clone(&panels.orbcomm.handles);
    *state.orbcomm_panel_handles.borrow_mut() = Some(Rc::clone(&handles));

    // Enable switch → SetOrbcommEnabled (ack-driven state; guard the
    // programmatic set_active in apply_enabled_ack).
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(&handles);
        handles
            .enable_switch
            .clone()
            .connect_active_notify(move |sw| {
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
