//! Display settings panel — FFT size, window function, frame rate, color map.

use libadwaita as adw;
use libadwaita::prelude::*;

/// Default frame rate in FPS.
const DEFAULT_FPS: f64 = 20.0;
/// Minimum frame rate in FPS.
const MIN_FPS: f64 = 1.0;
/// Maximum frame rate in FPS.
const MAX_FPS: f64 = 60.0;

/// Default FFT size selector index (2048 = index 2).
const DEFAULT_FFT_SIZE_INDEX: u32 = 2;

/// Default window function selector index (Blackman = index 1).
const DEFAULT_WINDOW_FN_INDEX: u32 = 1;

/// Display settings panel with references to interactive rows.
pub struct DisplayPanel {
    /// The `AdwPreferencesGroup` widget to pack into the sidebar.
    pub widget: adw::PreferencesGroup,
    /// FFT size selector.
    pub fft_size_row: adw::ComboRow,
    /// Window function selector.
    pub window_fn_row: adw::ComboRow,
    /// Frame rate control.
    pub frame_rate_row: adw::SpinRow,
    /// Color map selector.
    pub color_map_row: adw::ComboRow,
}

/// Build the display settings panel.
pub fn build_display_panel() -> DisplayPanel {
    let group = adw::PreferencesGroup::builder()
        .title("Display")
        .description("Spectrum & waterfall settings")
        .build();

    // --- FFT Size ---
    let fft_size_model = gtk4::StringList::new(&["512", "1024", "2048", "4096", "8192"]);
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
    let fps_adj = gtk4::Adjustment::new(DEFAULT_FPS, MIN_FPS, MAX_FPS, 1.0, 5.0, 0.0);
    let frame_rate_row = adw::SpinRow::builder()
        .title("Frame Rate")
        .subtitle("FPS")
        .adjustment(&fps_adj)
        .digits(0)
        .build();

    // --- Color Map ---
    let colormap_model = gtk4::StringList::new(&["Turbo"]);
    let color_map_row = adw::ComboRow::builder()
        .title("Color Map")
        .model(&colormap_model)
        .build();

    group.add(&fft_size_row);
    group.add(&window_fn_row);
    group.add(&frame_rate_row);
    group.add(&color_map_row);

    // TODO: Connect all rows to spectrum display (PR #7)

    DisplayPanel {
        widget: group,
        fft_size_row,
        window_fn_row,
        frame_rate_row,
        color_map_row,
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
}
