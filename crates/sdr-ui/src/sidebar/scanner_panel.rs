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

/// Dwell `SpinRow` step increment (ms). Drives arrow-key nudges
/// and the ± buttons on the spin row.
pub const DWELL_STEP_MS: f64 = 10.0;
/// Dwell `SpinRow` page increment (ms). Drives Page Up/Down and
/// the scroll-wheel bump.
pub const DWELL_PAGE_MS: f64 = 50.0;
/// Hang `SpinRow` step increment (ms) — larger than dwell since
/// hang is in whole seconds range.
pub const HANG_STEP_MS: f64 = 100.0;
/// Hang `SpinRow` page increment (ms).
pub const HANG_PAGE_MS: f64 = 500.0;

/// Shared parse-fallback-clamp pipeline used by the
/// default-dwell and default-hang loaders. Pulls a `u64` out of
/// the config at `key`, narrows to `u32` (silently falling back
/// to `default` on overflow or missing / non-numeric values),
/// then clamps into `[min, max]` so an out-of-range persisted
/// value can't hand the consumer a nonsense number.
fn load_clamped_u32(
    config: &Arc<ConfigManager>,
    key: &str,
    default: u32,
    min: u32,
    max: u32,
) -> u32 {
    config.read(|v| {
        v.get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(default)
            .clamp(min, max)
    })
}

/// Load the persisted default-dwell value, or return
/// [`DEFAULT_DWELL_MS`] if the key is missing or malformed.
/// Clamps to `[DWELL_MIN_MS, DWELL_MAX_MS]` so a hand-edited
/// config with an out-of-range value doesn't hand the `SpinRow`
/// a value it can't display.
#[must_use]
pub fn load_default_dwell_ms(config: &Arc<ConfigManager>) -> u32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let min = DWELL_MIN_MS as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max = DWELL_MAX_MS as u32;
    load_clamped_u32(
        config,
        CONFIG_KEY_DEFAULT_DWELL_MS,
        DEFAULT_DWELL_MS,
        min,
        max,
    )
}

/// Persist the default-dwell value.
pub fn save_default_dwell_ms(config: &Arc<ConfigManager>, ms: u32) {
    config.write(|v| {
        v[CONFIG_KEY_DEFAULT_DWELL_MS] = serde_json::json!(ms);
    });
}

/// Load the persisted default-hang value, or return
/// [`DEFAULT_HANG_MS`] if the key is missing or malformed.
/// Clamps to `[HANG_MIN_MS, HANG_MAX_MS]` — same rationale as
/// `load_default_dwell_ms`.
#[must_use]
pub fn load_default_hang_ms(config: &Arc<ConfigManager>) -> u32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let min = HANG_MIN_MS as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max = HANG_MAX_MS as u32;
    load_clamped_u32(
        config,
        CONFIG_KEY_DEFAULT_HANG_MS,
        DEFAULT_HANG_MS,
        min,
        max,
    )
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
            DWELL_STEP_MS,
            DWELL_PAGE_MS,
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
            HANG_STEP_MS,
            HANG_PAGE_MS,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> Arc<ConfigManager> {
        Arc::new(ConfigManager::in_memory(&serde_json::json!({})))
    }

    #[test]
    fn dwell_missing_key_returns_default() {
        let config = make_config();
        assert_eq!(load_default_dwell_ms(&config), DEFAULT_DWELL_MS);
    }

    #[test]
    fn hang_missing_key_returns_default() {
        let config = make_config();
        assert_eq!(load_default_hang_ms(&config), DEFAULT_HANG_MS);
    }

    #[test]
    fn dwell_malformed_value_falls_back_to_default() {
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            CONFIG_KEY_DEFAULT_DWELL_MS: "not-a-number",
        })));
        assert_eq!(load_default_dwell_ms(&config), DEFAULT_DWELL_MS);
    }

    #[test]
    fn hang_malformed_value_falls_back_to_default() {
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            CONFIG_KEY_DEFAULT_HANG_MS: [1, 2, 3],
        })));
        assert_eq!(load_default_hang_ms(&config), DEFAULT_HANG_MS);
    }

    #[test]
    fn dwell_below_min_clamps_up() {
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            CONFIG_KEY_DEFAULT_DWELL_MS: 1_u64,
        })));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let min = DWELL_MIN_MS as u32;
        assert_eq!(load_default_dwell_ms(&config), min);
    }

    #[test]
    fn dwell_above_max_clamps_down() {
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            CONFIG_KEY_DEFAULT_DWELL_MS: 999_999_u64,
        })));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let max = DWELL_MAX_MS as u32;
        assert_eq!(load_default_dwell_ms(&config), max);
    }

    #[test]
    fn hang_below_min_clamps_up() {
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            CONFIG_KEY_DEFAULT_HANG_MS: 0_u64,
        })));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let min = HANG_MIN_MS as u32;
        assert_eq!(load_default_hang_ms(&config), min);
    }

    #[test]
    fn hang_above_max_clamps_down() {
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            CONFIG_KEY_DEFAULT_HANG_MS: 9_999_999_u64,
        })));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let max = HANG_MAX_MS as u32;
        assert_eq!(load_default_hang_ms(&config), max);
    }

    #[test]
    fn dwell_save_then_load_round_trips_in_range_value() {
        let config = make_config();
        // 150 is inside [DWELL_MIN_MS, DWELL_MAX_MS] = [50, 500].
        save_default_dwell_ms(&config, 150);
        assert_eq!(load_default_dwell_ms(&config), 150);
    }

    #[test]
    fn hang_save_then_load_round_trips_in_range_value() {
        let config = make_config();
        // 3000 is inside [HANG_MIN_MS, HANG_MAX_MS] = [500, 5000].
        save_default_hang_ms(&config, 3_000);
        assert_eq!(load_default_hang_ms(&config), 3_000);
    }

    #[test]
    fn u32_overflow_from_u64_falls_back_to_default() {
        // u32::MAX + 1 survives as u64 but can't narrow to u32,
        // so the `try_from` guard returns `None` and we fall
        // back to DEFAULT_DWELL_MS (which then clamps into range).
        let config = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            CONFIG_KEY_DEFAULT_DWELL_MS: u64::from(u32::MAX) + 1,
        })));
        assert_eq!(load_default_dwell_ms(&config), DEFAULT_DWELL_MS);
    }
}
