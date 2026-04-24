//! Display settings panel — FFT size, window function, frame rate, color map,
//! dB range, fill mode, averaging mode, and theme.

use libadwaita as adw;
use libadwaita::prelude::*;

/// Default frame rate in FPS.
const DEFAULT_FPS: f64 = 20.0;
/// Minimum frame rate in FPS.
const MIN_FPS: f64 = 1.0;
/// Maximum frame rate in FPS.
const MAX_FPS: f64 = 60.0;

/// Available FFT sizes — single source of truth for dropdown and DSP mapping.
pub const FFT_SIZES: &[usize] = &[512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

/// Default FFT size selector index (2048 = index 2).
const DEFAULT_FFT_SIZE_INDEX: u32 = 2;

/// Theme selector indices (must match `StringList` order in `build_display_panel`).
pub const THEME_SYSTEM: u32 = 0;
pub const THEME_DARK: u32 = 1;
pub const THEME_LIGHT: u32 = 2;

/// Default window function selector index (Blackman = index 1).
const DEFAULT_WINDOW_FN_INDEX: u32 = 1;

/// Default minimum dB level for the display range.
const DEFAULT_MIN_DB: f64 = -70.0;
/// Default maximum dB level for the display range.
const DEFAULT_MAX_DB: f64 = 0.0;

/// Minimum dB level the `Min Level` spin row will accept.
const MIN_DB_FLOOR: f64 = -200.0;
/// Maximum value the `Min Level` spin row will accept.
const MIN_DB_CEILING: f64 = 0.0;
/// Minimum value the `Max Level` spin row will accept.
const MAX_DB_FLOOR: f64 = -120.0;
/// Maximum value the `Max Level` spin row will accept.
const MAX_DB_CEILING: f64 = 20.0;
/// Step for the dB spin rows (keyboard / scroll).
const DB_STEP: f64 = 1.0;
/// Page step for the dB spin rows.
const DB_PAGE: f64 = 10.0;
/// Step for the frame-rate spin row.
const FPS_STEP: f64 = 1.0;
/// Page step for the frame-rate spin row.
const FPS_PAGE: f64 = 5.0;

/// Display settings panel with references to interactive rows.
pub struct DisplayPanel {
    /// The `AdwPreferencesPage` widget packed into the Display
    /// activity stack slot. Hosts four titled
    /// `AdwPreferencesGroup`s (FFT / Waterfall / Levels /
    /// Appearance) — see [`build_display_panel`].
    pub widget: adw::PreferencesPage,
    /// FFT size selector.
    pub fft_size_row: adw::ComboRow,
    /// Window function selector.
    pub window_fn_row: adw::ComboRow,
    /// Frame rate control.
    pub frame_rate_row: adw::SpinRow,
    /// Color map selector.
    pub color_map_row: adw::ComboRow,
    /// Minimum dB level for the display range.
    pub min_db_row: adw::SpinRow,
    /// Maximum dB level for the display range.
    pub max_db_row: adw::SpinRow,
    /// Toggle for spectrum fill area under the trace.
    pub fill_mode_row: adw::SwitchRow,
    /// Spectrum averaging mode selector.
    pub averaging_row: adw::ComboRow,
    /// Theme selector (System / Dark / Light).
    pub theme_row: adw::ComboRow,
}

/// Build the display settings panel.
///
/// Lays out as an `AdwPreferencesPage` with four titled sections
/// matching the activity-bar redesign's Apple-style rhythm (design
/// doc §3.4). Flat groups, no `AdwExpanderRow` wrappers — same call
/// as the General / Radio / Audio panels.
#[allow(clippy::too_many_lines)]
pub fn build_display_panel() -> DisplayPanel {
    // --- FFT Size ---
    let fft_labels: Vec<String> = FFT_SIZES.iter().map(usize::to_string).collect();
    let fft_label_refs: Vec<&str> = fft_labels.iter().map(String::as_str).collect();
    let fft_size_model = gtk4::StringList::new(&fft_label_refs);
    let fft_size_row = adw::ComboRow::builder()
        .title("FFT Size")
        .model(&fft_size_model)
        .selected(DEFAULT_FFT_SIZE_INDEX)
        .build();

    // --- Window Function ---
    let window_fn_model = gtk4::StringList::new(&["Rectangular", "Blackman", "Nuttall"]);
    let window_fn_row = adw::ComboRow::builder()
        .title("Window Function")
        .model(&window_fn_model)
        .selected(DEFAULT_WINDOW_FN_INDEX)
        .build();

    // --- Frame Rate ---
    let fps_adj = gtk4::Adjustment::new(DEFAULT_FPS, MIN_FPS, MAX_FPS, FPS_STEP, FPS_PAGE, 0.0);
    let frame_rate_row = adw::SpinRow::builder()
        .title("Frame Rate")
        .subtitle("FPS")
        .adjustment(&fps_adj)
        .digits(0)
        .build();

    // --- Color Map ---
    let colormap_model = gtk4::StringList::new(&["Turbo", "Viridis", "Plasma", "Inferno"]);
    let color_map_row = adw::ComboRow::builder()
        .title("Color Map")
        .model(&colormap_model)
        .build();

    // --- Min dB ---
    let min_db_adj = gtk4::Adjustment::new(
        DEFAULT_MIN_DB,
        MIN_DB_FLOOR,
        MIN_DB_CEILING,
        DB_STEP,
        DB_PAGE,
        0.0,
    );
    let min_db_row = adw::SpinRow::builder()
        .title("Min Level")
        .subtitle("dB")
        .adjustment(&min_db_adj)
        .digits(0)
        .build();

    // --- Max dB ---
    let max_db_adj = gtk4::Adjustment::new(
        DEFAULT_MAX_DB,
        MAX_DB_FLOOR,
        MAX_DB_CEILING,
        DB_STEP,
        DB_PAGE,
        0.0,
    );
    let max_db_row = adw::SpinRow::builder()
        .title("Max Level")
        .subtitle("dB")
        .adjustment(&max_db_adj)
        .digits(0)
        .build();

    // Cross-couple the min/max adjustments so the UI can't produce
    // an inverted range (min_db >= max_db). When min moves, we
    // raise max's lower bound to `new_min + DB_STEP`; if max is
    // below that, GTK auto-clamps max up — a single-dB "range drag"
    // that keeps the pair valid. Symmetric for max → min's upper.
    // Separate from the DSP-dispatch handlers in `window.rs`, which
    // kept a defensive `min >= max` early-return; that branch is
    // now unreachable via UI but cheap to leave as belt-and-braces.
    let max_adj_weak = max_db_adj.downgrade();
    min_db_row.connect_value_notify(move |row| {
        if let Some(adj) = max_adj_weak.upgrade() {
            adj.set_lower(row.value() + DB_STEP);
        }
    });
    let min_adj_weak = min_db_adj.downgrade();
    max_db_row.connect_value_notify(move |row| {
        if let Some(adj) = min_adj_weak.upgrade() {
            adj.set_upper(row.value() - DB_STEP);
        }
    });

    // --- Fill Mode ---
    let fill_mode_row = adw::SwitchRow::builder()
        .title("Spectrum Fill")
        .active(true)
        .build();

    // --- Averaging Mode ---
    let averaging_model = gtk4::StringList::new(&["None", "Peak Hold", "Average", "Min Hold"]);
    let averaging_row = adw::ComboRow::builder()
        .title("Averaging")
        .model(&averaging_model)
        .build();

    // --- Theme ---
    let theme_model = gtk4::StringList::new(&["System", "Dark", "Light"]);
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .model(&theme_model)
        .build();

    // --- Sectioned preferences page ---
    // Section `title` + `description` pattern mirrors the other
    // panels (Audio / Radio / Source) so the header rhythm is
    // consistent across activities. Descriptions are plain English
    // — hints for users new to FFT / dB terminology.
    let fft_group = adw::PreferencesGroup::builder()
        .title("FFT")
        .description("Frequency transform resolution and update rate")
        .build();
    fft_group.add(&fft_size_row);
    fft_group.add(&window_fn_row);
    fft_group.add(&frame_rate_row);

    let waterfall_group = adw::PreferencesGroup::builder()
        .title("Waterfall")
        .description("Color mapping for the scrolling history")
        .build();
    waterfall_group.add(&color_map_row);

    let levels_group = adw::PreferencesGroup::builder()
        .title("Levels")
        .description("Signal range and averaging on the spectrum trace")
        .build();
    levels_group.add(&min_db_row);
    levels_group.add(&max_db_row);
    levels_group.add(&averaging_row);
    levels_group.add(&fill_mode_row);

    let appearance_group = adw::PreferencesGroup::builder()
        .title("Appearance")
        .description("Application theme")
        .build();
    appearance_group.add(&theme_row);

    let page = adw::PreferencesPage::new();
    page.add(&fft_group);
    page.add(&waterfall_group);
    page.add(&levels_group);
    page.add(&appearance_group);

    // FFT size and window function connected via window.rs

    DisplayPanel {
        widget: page,
        fft_size_row,
        window_fn_row,
        frame_rate_row,
        color_map_row,
        min_db_row,
        max_db_row,
        fill_mode_row,
        averaging_row,
        theme_row,
    }
}

#[cfg(test)]
mod tests {
    /// Compile-time validation that frame rate constants are consistent.
    const _: () = {
        assert!(super::MIN_FPS <= super::MAX_FPS);
        assert!(super::DEFAULT_FPS >= super::MIN_FPS);
        assert!(super::DEFAULT_FPS <= super::MAX_FPS);
    };

    /// Compile-time validation that dB range constants are consistent.
    const _: () = {
        assert!(super::MIN_DB_FLOOR <= super::MIN_DB_CEILING);
        assert!(super::MAX_DB_FLOOR <= super::MAX_DB_CEILING);
        assert!(super::DEFAULT_MIN_DB >= super::MIN_DB_FLOOR);
        assert!(super::DEFAULT_MIN_DB <= super::MIN_DB_CEILING);
        assert!(super::DEFAULT_MAX_DB >= super::MAX_DB_FLOOR);
        assert!(super::DEFAULT_MAX_DB <= super::MAX_DB_CEILING);
        // Default pair ordering — a future constant tweak that
        // crosses the defaults would produce a spectrum view with
        // min above max, which the trace renderer treats as an
        // empty range (no pixels lit). Assert directly so the
        // regression fails the build, not the first frame.
        assert!(super::DEFAULT_MIN_DB <= super::DEFAULT_MAX_DB);
    };
}
