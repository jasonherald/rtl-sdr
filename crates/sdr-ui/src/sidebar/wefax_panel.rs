//! WEFAX auto-catch activity panel. Turn Decode on; the catcher scans and
//! decodes a chart unattended. Standard `AdwPreferencesPage` layout (see
//! `scanner_panel.rs` / `radio_panel.rs` for the idiom this follows —
//! deliberately NOT Orbcomm's `ScrolledWindow` layout, which exists only
//! because that panel hosts a scrolling log).
//!
//! This module builds the widget only. Wiring the switch/combo/status
//! label to the catcher state machine (`sidebar::wefax_catcher`) and the
//! DSP presence events lands in a later task.

use std::cell::Cell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::prelude::*;

/// Per-panel runtime handles a future wiring pass drives. Stashed on
/// [`WefaxPanel::handles`] so the connect step can reach the widgets
/// without re-walking the tree.
pub struct WefaxPanelHandles {
    pub enable_switch: gtk4::Switch,
    pub station_row: adw::ComboRow,
    pub status_label: gtk4::Label,
    /// Re-entrancy guard around an ack-driven `set_active` call so the
    /// switch's own `active` notify handler doesn't re-dispatch a
    /// `SetWefaxEnabled`-style message for a state change the wiring
    /// layer just made itself. Mirrors
    /// `OrbcommPanelHandles::suppress_switch_notify`.
    pub suppress_switch_notify: Cell<bool>,
}

pub struct WefaxPanel {
    pub widget: adw::PreferencesPage,
    pub handles: Rc<WefaxPanelHandles>,
}

/// Build the "Decode" enable row (switch + accessible label) inside its
/// own `PreferencesGroup`. Mirrors `orbcomm_panel::build_enable_group`.
fn build_enable_group() -> (adw::PreferencesGroup, gtk4::Switch) {
    let enable_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    enable_switch.update_property(&[gtk4::accessible::Property::Label("Enable WEFAX auto-catch")]);
    let enable_row = adw::ActionRow::builder()
        .title("Auto-catch WEFAX")
        .subtitle("Scan receivable stations and decode a chart automatically")
        .build();
    enable_row.add_suffix(&enable_switch);
    // Clicking anywhere on the row toggles the switch, not just the
    // switch itself. Per Task 5 minor / Task 6 fold-in (epic #913).
    enable_row.set_activatable_widget(Some(&enable_switch));
    let enable_group = adw::PreferencesGroup::new();
    enable_group.add(&enable_row);
    (enable_group, enable_switch)
}

/// Build the "Station" group holding the station-selection combo row.
/// The combo's model is populated later (from `stations_by_distance`,
/// "Auto" plus named stations) once the wiring layer has access to the
/// ground-station catalog.
fn build_station_group() -> (adw::PreferencesGroup, adw::ComboRow) {
    let station_group = adw::PreferencesGroup::builder().title("Station").build();
    let station_row = adw::ComboRow::builder().title("Station").build();
    station_group.add(&station_row);
    (station_group, station_row)
}

/// Build the "Status" group holding the catcher state-machine label.
fn build_status_group() -> (adw::PreferencesGroup, gtk4::Label) {
    let status_group = adw::PreferencesGroup::builder().title("Status").build();
    let status_label = gtk4::Label::builder().label("Idle").xalign(0.0).build();
    let status_row = adw::ActionRow::builder().title("State").build();
    status_row.add_suffix(&status_label);
    status_group.add(&status_row);
    (status_group, status_label)
}

/// Build the WEFAX activity panel: Decode enable switch, station picker,
/// and a status row reflecting the auto-catch state machine.
#[must_use]
pub fn build_wefax_panel() -> WefaxPanel {
    let page = adw::PreferencesPage::new();

    let (enable_group, enable_switch) = build_enable_group();
    page.add(&enable_group);

    let (station_group, station_row) = build_station_group();
    page.add(&station_group);

    let (status_group, status_label) = build_status_group();
    page.add(&status_group);

    WefaxPanel {
        widget: page,
        handles: Rc::new(WefaxPanelHandles {
            enable_switch,
            station_row,
            status_label,
            suppress_switch_notify: Cell::new(false),
        }),
    }
}
