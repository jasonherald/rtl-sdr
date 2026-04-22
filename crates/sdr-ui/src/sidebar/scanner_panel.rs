//! Scanner control panel at the bottom of the left sidebar.
//!
//! Master switch, active-channel / state display, default
//! dwell/hang sliders (collapsed expander), and session lockout
//! button (visible only when scanner is on an active channel).
//! UI wiring of user actions → `UiToDsp::*` commands lives in
//! `window.rs::connect_scanner_panel`.

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;

/// Widgets for the scanner sidebar panel. The controller layer
/// in `window.rs` connects signal handlers that dispatch
/// `UiToDsp::SetScannerEnabled` / `UpdateScannerChannels` /
/// `LockoutScannerChannel` based on these widgets.
///
/// `Clone` is derived so the panel can be cloned into the DSP
/// timeout callback alongside `RadioPanel` et al — each field
/// is a `GObject` wrapper, so clone is a cheap refcount bump.
#[derive(Clone)]
pub struct ScannerPanel {
    /// The outer container — append to the sidebar `Box`.
    pub widget: gtk4::Box,
    /// Master on/off toggle.
    pub master_switch: gtk4::Switch,
    /// "Active: {name} — {freq}" while scanner is Listening /
    /// Hanging; shows "Active: —" otherwise.
    pub active_channel_label: gtk4::Label,
    /// Human-readable phase label — "Off", "Scanning…",
    /// "Listening…", "Listening", "Hang…".
    pub state_label: gtk4::Label,
    /// Default dwell (settle after retune) in ms. Value is
    /// folded into `ScannerChannel::dwell_ms` at projection time
    /// when the per-bookmark override is `None`.
    pub default_dwell_row: adw::SpinRow,
    /// Default hang (linger after squelch closes) in ms. Same
    /// projection-time fallback model as `default_dwell_row`.
    pub default_hang_row: adw::SpinRow,
    /// "Lockout current channel" — only visible while the
    /// scanner has an active channel latched.
    pub lockout_button: gtk4::Button,
}

/// Minimum default-dwell value exposed in the UI (ms).
pub const DWELL_MIN_MS: f64 = 50.0;
/// Maximum default-dwell value exposed in the UI (ms).
pub const DWELL_MAX_MS: f64 = 500.0;
/// Minimum default-hang value exposed in the UI (ms).
pub const HANG_MIN_MS: f64 = 500.0;
/// Maximum default-hang value exposed in the UI (ms).
pub const HANG_MAX_MS: f64 = 5000.0;

/// `ConfigManager` key for the default-dwell slider value.
pub const CONFIG_KEY_DEFAULT_DWELL_MS: &str = "scanner_default_dwell_ms";
/// `ConfigManager` key for the default-hang slider value.
pub const CONFIG_KEY_DEFAULT_HANG_MS: &str = "scanner_default_hang_ms";

/// Default-dwell initial value (ms) when no persisted value exists.
pub const DEFAULT_DWELL_MS: u32 = 100;
/// Default-hang initial value (ms) when no persisted value exists.
pub const DEFAULT_HANG_MS: u32 = 2_000;

/// Load the persisted default-dwell value, or return
/// [`DEFAULT_DWELL_MS`] if the key is missing or malformed.
#[must_use]
pub fn load_default_dwell_ms(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(CONFIG_KEY_DEFAULT_DWELL_MS)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEFAULT_DWELL_MS)
    })
}

/// Persist the default-dwell value.
pub fn save_default_dwell_ms(config: &Arc<ConfigManager>, ms: u32) {
    config.write(|v| {
        v[CONFIG_KEY_DEFAULT_DWELL_MS] = serde_json::json!(ms);
    });
}

/// Load the persisted default-hang value, or return
/// [`DEFAULT_HANG_MS`] if the key is missing or malformed.
#[must_use]
pub fn load_default_hang_ms(config: &Arc<ConfigManager>) -> u32 {
    config.read(|v| {
        v.get(CONFIG_KEY_DEFAULT_HANG_MS)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEFAULT_HANG_MS)
    })
}

/// Persist the default-hang value.
pub fn save_default_hang_ms(config: &Arc<ConfigManager>, ms: u32) {
    config.write(|v| {
        v[CONFIG_KEY_DEFAULT_HANG_MS] = serde_json::json!(ms);
    });
}

/// Build the scanner panel and return its owned widgets.
///
/// The outer `widget` is appended to the sidebar `Box`; the rest
/// are captured by the controller layer for signal wiring.
#[must_use]
pub fn build_scanner_panel() -> ScannerPanel {
    let widget = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(8)
        .build();

    let heading = gtk4::Label::builder()
        .label("Scanner")
        .css_classes(["heading"])
        .halign(gtk4::Align::Start)
        .build();
    widget.append(&heading);

    let switch_row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .build();
    let switch_label = gtk4::Label::builder()
        .label("Scanner")
        .hexpand(true)
        .halign(gtk4::Align::Start)
        .build();
    let master_switch = gtk4::Switch::builder().halign(gtk4::Align::End).build();
    switch_row.append(&switch_label);
    switch_row.append(&master_switch);
    widget.append(&switch_row);

    // Long bookmark names ("KY State Police District 7 Dispatch —
    // 154.680 MHz") would otherwise grow the label's natural
    // width past the sidebar width and shove the whole sidebar
    // wider on every retune. `ellipsize(End)` + `xalign(0.0)` +
    // `hexpand(true)` + `max_width_chars(1)` tells GTK: "grow
    // as much as the parent allows, but stop requesting extra
    // width past that point — truncate with `…` instead." The
    // `max_width_chars(1)` is the idiomatic GTK incantation for
    // "don't let the label's preferred width drive the layout";
    // actual visible width follows `hexpand`.
    let active_channel_label = gtk4::Label::builder()
        .label("Active: —")
        .halign(gtk4::Align::Start)
        .xalign(0.0)
        .hexpand(true)
        .max_width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["caption"])
        .build();
    widget.append(&active_channel_label);

    let state_label = gtk4::Label::builder()
        .label("State: Off")
        .halign(gtk4::Align::Start)
        .xalign(0.0)
        .hexpand(true)
        .max_width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    widget.append(&state_label);

    let lockout_button = gtk4::Button::builder()
        .label("Lockout current channel")
        .css_classes(["destructive-action", "flat"])
        .visible(false)
        .build();
    widget.append(&lockout_button);

    let expander = adw::ExpanderRow::builder().title("Settings").build();
    let default_dwell_row = adw::SpinRow::builder()
        .title("Default dwell (ms)")
        .adjustment(&gtk4::Adjustment::new(
            f64::from(DEFAULT_DWELL_MS),
            DWELL_MIN_MS,
            DWELL_MAX_MS,
            10.0,
            50.0,
            0.0,
        ))
        .digits(0)
        .build();
    let default_hang_row = adw::SpinRow::builder()
        .title("Default hang (ms)")
        .adjustment(&gtk4::Adjustment::new(
            f64::from(DEFAULT_HANG_MS),
            HANG_MIN_MS,
            HANG_MAX_MS,
            100.0,
            500.0,
            0.0,
        ))
        .digits(0)
        .build();
    expander.add_row(&default_dwell_row);
    expander.add_row(&default_hang_row);

    let settings_group = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    settings_group.append(&expander);
    widget.append(&settings_group);

    ScannerPanel {
        widget,
        master_switch,
        active_channel_label,
        state_label,
        default_dwell_row,
        default_hang_row,
        lockout_button,
    }
}
