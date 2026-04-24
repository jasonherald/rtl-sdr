//! Scanner control panel — master switch, live state readout,
//! default dwell/hang timing, and session lockout button.
//!
//! Lays out as an `AdwPreferencesPage` with three titled sections
//! (Scanner / Active / Timing) matching the activity-bar redesign's
//! Apple-style rhythm. UI wiring of user actions →
//! `UiToDsp::*` commands lives in `window.rs::connect_scanner_panel`.

use std::sync::Arc;

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
    /// The `AdwPreferencesPage` widget packed into the Scanner
    /// activity stack slot.
    pub widget: adw::PreferencesPage,
    /// Master on/off toggle. Still a bare `gtk4::Switch` (wrapped
    /// as the suffix of an `AdwActionRow` in the page) so
    /// `window.rs` + `shortcuts.rs` keep using the same
    /// `set_state` / `set_active` / `connect_active_notify` API.
    pub master_switch: gtk4::Switch,
    /// Action row displaying the active channel. Title is fixed
    /// ("Channel"); subtitle carries the dynamic value
    /// (`"{name} — {freq}"` while listening / hanging, `"—"`
    /// otherwise). Subtitle is multi-line — long bookmark names
    /// wrap in place instead of truncating with "…". Call
    /// `set_subtitle` to update.
    pub active_channel_row: adw::ActionRow,
    /// Action row displaying the scanner's live state. Title is
    /// fixed ("State"); subtitle carries the phase label ("Off",
    /// "Scanning…", "Listening…", "Listening", "Hang…"). Call
    /// `set_subtitle` to update.
    pub state_row: adw::ActionRow,
    /// Default dwell (settle after retune) in ms. Value is
    /// folded into `ScannerChannel::dwell_ms` at projection time
    /// when the per-bookmark override is `None`.
    pub default_dwell_row: adw::SpinRow,
    /// Default hang (linger after squelch closes) in ms. Same
    /// projection-time fallback model as `default_dwell_row`.
    pub default_hang_row: adw::SpinRow,
    /// Lockout action row — visible only while the scanner has
    /// an active channel latched. Hide / show via `lockout_row`
    /// (the button alone inside an always-visible row would
    /// leave an empty titled strip).
    pub lockout_row: adw::ActionRow,
    /// "Lockout" button, packed as a suffix on `lockout_row`.
    /// Exposed as a field so `window.rs` can wire its click
    /// handler without walking the row's child tree.
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

/// Placeholder subtitle shown on `active_channel_row` when the
/// scanner isn't latched on a channel. Kept as a constant so
/// every reset path in `window.rs` hits the same string.
pub const ACTIVE_CHANNEL_PLACEHOLDER: &str = "—";
/// Placeholder subtitle for `state_row` before the first
/// `ScannerStateChanged` event arrives.
pub const STATE_PLACEHOLDER: &str = "Off";

/// `AdwActionRow::set_subtitle_lines(0)` disables the one-line
/// cap and lets long text wrap to as many lines as needed. The
/// active channel's subtitle can be a long bookmark nickname +
/// formatted frequency ("KY State Police District 7 Dispatch
/// — 154.680 MHz") and single-line ellipsize made the row
/// unreadable in sidebar widths; wrap-in-place is the better
/// affordance.
const SUBTITLE_UNLIMITED_LINES: i32 = 0;

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
/// Lays out as an `AdwPreferencesPage` with three titled sections
/// matching the activity-bar redesign's Apple-style rhythm (design
/// doc §3.5). Flat groups, no `AdwExpanderRow` wrappers — same
/// call as the General / Radio / Audio / Display panels.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_scanner_panel() -> ScannerPanel {
    // --- Scanner master switch ---
    //
    // Kept as a bare `gtk4::Switch` so `window.rs` and
    // `shortcuts.rs` callers don't change signature. Wrapped in
    // an `AdwActionRow` as the row suffix so the preferences-page
    // chrome matches the rest of the activity panels.
    let master_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    let master_switch_row = adw::ActionRow::builder()
        .title("Scanner")
        .subtitle("Enable rotation through active channels")
        .build();
    master_switch_row.add_suffix(&master_switch);
    master_switch_row.set_activatable_widget(Some(&master_switch));

    // --- Active state readouts ---
    //
    // Two `AdwActionRow`s with static titles + dynamic subtitles.
    // `set_subtitle_lines(0)` disables the default one-line cap
    // so long bookmark names wrap in place instead of
    // truncating — makes long names like "KY State Police
    // District 7 Dispatch — 154.680 MHz" fully readable in the
    // sidebar without the user having to hover for a tooltip.
    let active_channel_row = adw::ActionRow::builder()
        .title("Channel")
        .subtitle(ACTIVE_CHANNEL_PLACEHOLDER)
        .subtitle_lines(SUBTITLE_UNLIMITED_LINES)
        .build();
    let state_row = adw::ActionRow::builder()
        .title("State")
        .subtitle(STATE_PLACEHOLDER)
        .subtitle_lines(SUBTITLE_UNLIMITED_LINES)
        .build();

    // --- Lockout action row ---
    //
    // Button packed as a suffix on an action row so the hide /
    // show control toggles the whole row (title + button
    // together). Hiding just the button would leave an empty
    // "Current channel" title strip when the scanner isn't
    // latched.
    let lockout_button = gtk4::Button::builder()
        .label("Lockout")
        .css_classes(["destructive-action", "flat"])
        .valign(gtk4::Align::Center)
        .build();
    let lockout_row = adw::ActionRow::builder()
        .title("Current channel")
        .subtitle("Skip for the rest of this session")
        .subtitle_lines(SUBTITLE_UNLIMITED_LINES)
        .visible(false)
        .build();
    lockout_row.add_suffix(&lockout_button);
    lockout_row.set_activatable_widget(Some(&lockout_button));

    // --- Timing spin rows ---
    let default_dwell_row = adw::SpinRow::builder()
        .title("Default dwell")
        .subtitle("ms — settle after retune")
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
        .title("Default hang")
        .subtitle("ms — linger after squelch closes")
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

    // --- Sectioned preferences page ---
    //
    // `title` + `description` pattern mirrors the other panels
    // (Audio / Radio / Display) so the header rhythm stays
    // consistent across activities.
    let scanner_group = adw::PreferencesGroup::builder()
        .title("Scanner")
        .description("Sweep through bookmarked frequencies")
        .build();
    scanner_group.add(&master_switch_row);

    let active_group = adw::PreferencesGroup::builder()
        .title("Active")
        .description("Current channel and detector state")
        .build();
    active_group.add(&active_channel_row);
    active_group.add(&state_row);
    active_group.add(&lockout_row);

    let timing_group = adw::PreferencesGroup::builder()
        .title("Timing")
        .description("How long to linger on each channel")
        .build();
    timing_group.add(&default_dwell_row);
    timing_group.add(&default_hang_row);

    let widget = adw::PreferencesPage::new();
    widget.add(&scanner_group);
    widget.add(&active_group);
    widget.add(&timing_group);

    ScannerPanel {
        widget,
        master_switch,
        active_channel_row,
        state_row,
        default_dwell_row,
        default_hang_row,
        lockout_row,
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
