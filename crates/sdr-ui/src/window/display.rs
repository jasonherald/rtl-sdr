//! Display panel wiring (FFT window, colormap, averaging).

use libadwaita::prelude::*;

use super::{AppState, FFT_SIZES, FftWindow, Rc, SidebarPanels, UiToDsp, adw, sidebar, spectrum};

/// FFT window function options matching the display panel combo.
pub(super) const WINDOW_FUNCTIONS: [FftWindow; 3] = [
    FftWindow::Rectangular,
    FftWindow::Blackman,
    FftWindow::Nuttall,
];

/// Colormap options matching the display panel combo.
pub(super) const COLORMAP_STYLES: [spectrum::colormap::ColormapStyle; 4] = [
    spectrum::colormap::ColormapStyle::Turbo,
    spectrum::colormap::ColormapStyle::Viridis,
    spectrum::colormap::ColormapStyle::Plasma,
    spectrum::colormap::ColormapStyle::Inferno,
];

/// Averaging mode options matching the display panel combo.
pub(super) const AVERAGING_MODES: [spectrum::AveragingMode; 4] = [
    spectrum::AveragingMode::None,
    spectrum::AveragingMode::PeakHold,
    spectrum::AveragingMode::RunningAvg,
    spectrum::AveragingMode::MinHold,
];

/// Connect display panel controls to DSP commands.
#[allow(clippy::too_many_lines)]
pub(super) fn connect_display_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    wire_fft_controls(panels, state);

    wire_spectrum_colors(panels, state, spectrum_handle, config);

    wire_averaging(panels, spectrum_handle);
}

/// FFT size / window-function / frame-rate rows of the Display panel.
/// Split out per the 50-NLOC gate (#817).
fn wire_fft_controls(panels: &SidebarPanels, state: &Rc<AppState>) {
    // FFT size
    let state_fft = Rc::clone(state);
    panels
        .display
        .fft_size_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            if let Some(&size) = FFT_SIZES.get(idx) {
                state_fft.send_dsp(UiToDsp::SetFftSize(size));
                // Waterfall resize happens in push_fft_data when the first
                // new-size frame arrives — avoids race with queued old-size frames.
            }
        });

    // Window function
    let state_wf = Rc::clone(state);
    panels
        .display
        .window_fn_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            if let Some(&window) = WINDOW_FUNCTIONS.get(idx) {
                state_wf.send_dsp(UiToDsp::SetWindowFunction(window));
            }
        });

    // Frame rate (FFT rate control)
    let state_fps = Rc::clone(state);
    panels
        .display
        .frame_rate_row
        .connect_value_notify(move |row| {
            state_fps.send_dsp(UiToDsp::SetFftRate(row.value()));
        });
}

/// Colormap and dB-range rows of the Display panel.
/// Split out per the 50-NLOC gate (#817).
fn wire_spectrum_colors(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Colormap
    let spectrum_for_cmap = Rc::clone(spectrum_handle);
    panels
        .display
        .color_map_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            let style = COLORMAP_STYLES
                .get(idx)
                .copied()
                .unwrap_or(spectrum::colormap::ColormapStyle::Turbo);
            spectrum_for_cmap.set_colormap(style);
        });

    wire_db_range(panels, state, spectrum_handle, config);
}

/// Averaging mode/factor rows of the Display panel.
/// Split out per the 50-NLOC gate (#817).
fn wire_averaging(panels: &SidebarPanels, spectrum_handle: &Rc<spectrum::SpectrumHandle>) {
    // Averaging mode selector.
    let spectrum_avg = Rc::clone(spectrum_handle);
    panels
        .display
        .averaging_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            let mode = AVERAGING_MODES
                .get(idx)
                .copied()
                .unwrap_or(spectrum::AveragingMode::None);
            spectrum_avg.set_averaging_mode(mode);
        });

    // Theme selector (System / Dark / Light).
    panels
        .display
        .theme_row
        .connect_selected_notify(move |row| {
            let style_manager = adw::StyleManager::default();
            let scheme = match row.selected() {
                sidebar::display_panel::THEME_DARK => adw::ColorScheme::ForceDark,
                sidebar::display_panel::THEME_LIGHT => adw::ColorScheme::ForceLight,
                _ => adw::ColorScheme::Default,
            };
            style_manager.set_color_scheme(scheme);
        });
}

/// Min/max dB rows — spectrum dB range with cross-row validation.
/// Split out per the 50-NLOC gate (#817).
fn wire_db_range(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Min dB level — update the spectrum dB range (skip if min >= max).
    let spectrum_min = Rc::clone(spectrum_handle);
    let max_row_for_min = panels.display.max_db_row.clone();
    panels.display.min_db_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        let min_db = row.value() as f32;
        #[allow(clippy::cast_possible_truncation)]
        let max_db = max_row_for_min.value() as f32;
        if min_db >= max_db {
            return;
        }
        spectrum_min.set_db_range(min_db, max_db);
        tracing::debug!(min_db, max_db, "dB range changed");
    });

    wire_max_db_row(panels, state, spectrum_handle, config);
}

/// Max-dB row of the spectrum dB range (skip if max <= min).
/// Split out per the 50-NLOC gate (#817).
fn wire_max_db_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Max dB level — update the spectrum dB range (skip if max <= min).
    let spectrum_max = Rc::clone(spectrum_handle);
    let min_row_for_max = panels.display.min_db_row.clone();
    panels.display.max_db_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        let max_db = row.value() as f32;
        #[allow(clippy::cast_possible_truncation)]
        let min_db = min_row_for_max.value() as f32;
        if max_db <= min_db {
            return;
        }
        spectrum_max.set_db_range(min_db, max_db);
        tracing::debug!(min_db, max_db, "dB range changed");
    });

    // Spectrum fill mode toggle.
    let spectrum_fill = Rc::clone(spectrum_handle);
    panels
        .display
        .fill_mode_row
        .connect_active_notify(move |row| {
            spectrum_fill.set_fill_enabled(row.is_active());
            tracing::debug!(fill = row.is_active(), "fill mode changed");
        });

    wire_waterfall_toggle(panels, state, spectrum_handle, config);
}

/// Waterfall master toggle (#646) — seed-then-wire the persisted gate.
/// Split out per the 50-NLOC gate (#817).
fn wire_waterfall_toggle(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Waterfall master toggle (#646). Two inputs combine into the
    // DSP gate: this user-facing toggle and the auto-pause-on-
    // minimize handler in `wire_window_minimize_pause`. Both feed
    // `state.resolve_and_send_waterfall_gate()` which dispatches a
    // single `SetFftEnabled(bool)` to the engine.
    //
    // Seed-then-wire: `set_active` fires `connect_active_notify` on
    // some GTK4 builds, so we apply the persisted state first, send
    // the resolved gate to the DSP, and only then connect the
    // handler. The handler's first call after wiring is a real user
    // toggle, not the seed.
    let initial_waterfall_enabled = sidebar::display_panel::read_waterfall_enabled(config);
    state.waterfall_user_enabled.set(initial_waterfall_enabled);
    panels
        .display
        .waterfall_enabled_row
        .set_active(initial_waterfall_enabled);
    let initial_resolved = state.resolve_and_send_waterfall_gate();
    if !initial_resolved {
        // Persisted-off launch: ensure the displays start blank
        // instead of inheriting whatever the last frame painted
        // before the previous shutdown — matters when
        // `waterfall_state` was just initialized with non-zero
        // pixels (race-window unlikely, but the cost is one
        // memset).
        spectrum_handle.clear_displays();
    }
    let state_wf = Rc::clone(state);
    let config_wf = std::sync::Arc::clone(config);
    let spectrum_wf = Rc::clone(spectrum_handle);
    panels
        .display
        .waterfall_enabled_row
        .connect_active_notify(move |row| {
            let active = row.is_active();
            state_wf.waterfall_user_enabled.set(active);
            sidebar::display_panel::save_waterfall_enabled(&config_wf, active);
            let resolved = state_wf.resolve_and_send_waterfall_gate();
            // Clear the visible state on disable so the user doesn't
            // see a frozen pre-disable snapshot. Skipped on enable —
            // the next FFT frame will paint the start of fresh data
            // naturally without an explicit clear. Per #646.
            if !resolved {
                spectrum_wf.clear_displays();
            }
            tracing::info!(active, "waterfall master toggle changed (#646)");
        });
}
