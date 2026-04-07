//! Main window construction — header bar, split view, breakpoints, DSP bridge.

use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_pipeline::iq_frontend::FftWindow;
use sdr_radio::DeemphasisMode;
use sdr_source_rtlsdr::SAMPLE_RATES;

use crate::dsp_controller;
use crate::header;
use crate::header::demod_selector;
use crate::messages::{DspToUi, SourceType, UiToDsp};
use crate::shortcuts;
use crate::sidebar;
use crate::sidebar::SidebarPanels;
use crate::spectrum;
use crate::state::AppState;
use crate::status_bar::{self, StatusBar};

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 1200;
/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 800;
/// Sidebar collapse breakpoint width in pixels.
const SIDEBAR_BREAKPOINT_PX: f64 = 800.0;

/// FFT sizes available in the display panel dropdown (must match panel order).
const FFT_SIZES: &[usize] = &[512, 1024, 2048, 4096, 8192];

/// Decimation factors available in the source panel dropdown (must match panel order).
const DECIMATION_FACTORS: &[u32] = &[1, 2, 4, 8, 16];

/// Interval in milliseconds for polling the DSP→UI channel.
const DSP_POLL_INTERVAL_MS: u64 = 16;

/// Build and present the main application window.
#[allow(clippy::too_many_lines)]
pub fn build_window(app: &adw::Application) {
    // --- Channel setup ---
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let (ui_tx, ui_rx) = mpsc::channel::<UiToDsp>();

    // Shared application state with DSP sender.
    let state = AppState::new_shared(ui_tx);

    // --- Build UI ---
    let (split_view, panels, spectrum_handle_raw, status_bar) = build_split_view(&state);
    let spectrum_handle = Rc::new(spectrum_handle_raw);
    let sidebar_toggle = build_sidebar_toggle(&split_view);
    let (header, play_button, demod_dropdown, freq_selector) =
        build_header_bar(&sidebar_toggle, &state);
    let toolbar_view = build_toolbar_view(&header, &split_view);
    let breakpoint = build_breakpoint(&split_view);

    // Toast overlay wraps the toolbar view for error notifications.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar_view));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("SDR-RS")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&toast_overlay)
        .build();

    window.add_breakpoint(breakpoint);

    // Set initial status bar values and mode-specific control visibility.
    if let Some(mode) = demod_selector::index_to_demod_mode(demod_dropdown.selected()) {
        let label = header::demod_mode_label(mode);
        let bw = panels.radio.bandwidth_row.value();
        status_bar.update_demod(label, bw);

        panels.radio.apply_demod_visibility(mode);
    }
    #[allow(clippy::cast_precision_loss)]
    status_bar.update_frequency(freq_selector.frequency() as f64);

    setup_app_actions(app, &window);

    // --- Keyboard shortcuts ---
    shortcuts::setup_shortcuts(&window, &play_button, &sidebar_toggle, &demod_dropdown);

    // Ctrl+? shows keyboard shortcuts dialog.
    let window_for_shortcuts = window.downgrade();
    let shortcuts_action = gio::SimpleAction::new("show-help-overlay", None);
    shortcuts_action.connect_activate(move |_, _| {
        if let Some(w) = window_for_shortcuts.upgrade() {
            shortcuts::show_shortcuts_dialog(&w);
        }
    });
    window.add_action(&shortcuts_action);
    app.set_accels_for_action("win.show-help-overlay", &["<Ctrl>slash"]);

    // --- Wire sidebar panels and frequency/demod to DSP + status bar ---
    let status_bar_demod = Rc::new(status_bar);

    connect_sidebar_panels(
        &panels,
        &state,
        &spectrum_handle,
        &freq_selector,
        &demod_dropdown,
        &status_bar_demod,
    );

    // Wire cursor readout from spectrum to status bar.
    let status_bar_for_cursor = Rc::clone(&status_bar_demod);
    spectrum_handle.connect_cursor_moved(move |freq_hz, power_db| {
        status_bar_for_cursor.update_cursor(freq_hz, power_db);
    });

    let status_bar_for_freq = Rc::clone(&status_bar_demod);
    let state_freq = Rc::clone(&state);
    freq_selector.connect_frequency_changed(move |freq| {
        tracing::debug!(frequency_hz = freq, "frequency changed");
        #[allow(clippy::cast_precision_loss)]
        let freq_f64 = freq as f64;
        state_freq.center_frequency.set(freq_f64);
        state_freq.send_dsp(UiToDsp::Tune(freq_f64));
        status_bar_for_freq.update_frequency(freq_f64);
    });
    let status_bar_for_demod = Rc::clone(&status_bar_demod);
    let bw_row_for_demod = panels.radio.bandwidth_row.clone();
    let radio_for_demod = panels.radio.clone();
    demod_dropdown.connect_selected_notify(move |dd| {
        if let Some(mode) = demod_selector::index_to_demod_mode(dd.selected()) {
            let label = header::demod_mode_label(mode);
            let bw = bw_row_for_demod.value();
            status_bar_for_demod.update_demod(label, bw);
            radio_for_demod.apply_demod_visibility(mode);
        }
    });

    // --- Wire radio panel bandwidth changes to status bar ---
    let status_bar_for_bw = Rc::clone(&status_bar_demod);
    let state_for_bw = Rc::clone(&state);
    panels.radio.bandwidth_row.connect_value_notify(move |row| {
        let mode = state_for_bw.demod_mode.get();
        let label = header::demod_mode_label(mode);
        status_bar_for_bw.update_demod(label, row.value());
    });

    // --- Spawn DSP thread ---
    dsp_controller::spawn_dsp_thread(dsp_tx, ui_rx);

    // --- Poll DspToUi channel from the GTK main loop ---
    let play_button_weak = play_button.downgrade();
    let state_rx = Rc::clone(&state);
    let toast_overlay_weak = toast_overlay.downgrade();

    let gain_row_for_dsp = panels.source.gain_row.clone();
    glib::timeout_add_local(Duration::from_millis(DSP_POLL_INTERVAL_MS), move || {
        // Drain all pending DSP messages.
        loop {
            match dsp_rx.try_recv() {
                Ok(msg) => {
                    handle_dsp_message(
                        msg,
                        &spectrum_handle,
                        &play_button_weak,
                        &state_rx,
                        &toast_overlay_weak,
                        &status_bar_demod,
                        &gain_row_for_dsp,
                    );
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::warn!("DSP channel disconnected");
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });

    window.present();
}

/// Handle a single message from the DSP thread.
fn handle_dsp_message(
    msg: DspToUi,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    play_button_weak: &glib::WeakRef<gtk4::ToggleButton>,
    state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    status_bar: &Rc<StatusBar>,
    gain_row: &adw::SpinRow,
) {
    match msg {
        DspToUi::FftData(data) => {
            spectrum_handle.push_fft_data(&data);
        }
        DspToUi::SignalLevel(level) => {
            status_bar.update_signal_level(level);
            spectrum_handle.push_signal_level(level);
        }
        DspToUi::Error(err_msg) => {
            tracing::warn!(error = %err_msg, "DSP error");
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::new(&err_msg);
                overlay.add_toast(toast);
            }
        }
        DspToUi::SourceStopped => {
            tracing::info!("source stopped");
            state.is_running.set(false);
            if let Some(btn) = play_button_weak.upgrade() {
                btn.set_active(false);
                btn.set_icon_name("media-playback-start-symbolic");
            }
        }
        DspToUi::SampleRateChanged(rate) => {
            tracing::info!(effective_sample_rate = rate, "sample rate changed");
            status_bar.update_sample_rate(rate);
        }
        DspToUi::DeviceInfo(info) => {
            tracing::info!(device_info = %info, "device info received");
        }
        DspToUi::GainList(gains) => {
            if let (Some(&min), Some(&max)) = (gains.first(), gains.last()) {
                tracing::info!(
                    count = gains.len(),
                    min_db = min,
                    max_db = max,
                    "tuner gain list received"
                );
                // Update the gain slider range to match the device's actual capabilities
                gain_row.adjustment().set_lower(min);
                gain_row.adjustment().set_upper(max);
            }
        }
    }
}

/// Build the `AdwOverlaySplitView` with sidebar configuration panels, content,
/// and status bar.
///
/// Returns the split view, sidebar panels, spectrum display handle, and status bar.
fn build_split_view(
    state: &Rc<AppState>,
) -> (
    adw::OverlaySplitView,
    SidebarPanels,
    spectrum::SpectrumHandle,
    StatusBar,
) {
    // Sidebar — configuration panels.
    let (sidebar_scroll, panels) = sidebar::build_sidebar();

    // Main content area — spectrum display (FFT plot + waterfall) + status bar.
    let (spectrum_view, spectrum_handle) = spectrum::build_spectrum_view(state.ui_tx.clone());
    spectrum_view.add_css_class("spectrum-area");

    // Status bar at the bottom.
    let status_bar = status_bar::build_status_bar();

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    content_box.append(&spectrum_view);
    content_box.append(&status_bar.widget);

    let split_view = adw::OverlaySplitView::builder()
        .sidebar(&sidebar_scroll)
        .content(&content_box)
        .show_sidebar(true)
        .build();

    (split_view, panels, spectrum_handle, status_bar)
}

/// Build the sidebar toggle button bound to the split view.
fn build_sidebar_toggle(split_view: &adw::OverlaySplitView) -> gtk4::ToggleButton {
    let toggle = gtk4::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Toggle sidebar")
        .active(true)
        .build();

    toggle.connect_toggled(glib::clone!(
        #[weak]
        split_view,
        move |btn| {
            split_view.set_show_sidebar(btn.is_active());
        }
    ));

    toggle
}

/// Build the `AdwHeaderBar` with play/stop, frequency selector, demod selector,
/// and volume control.
///
/// Returns the header bar, play button, demod dropdown, and frequency selector
/// (for shortcuts, status bar wiring, and frequency change callbacks).
fn build_header_bar(
    sidebar_toggle: &gtk4::ToggleButton,
    state: &Rc<AppState>,
) -> (
    adw::HeaderBar,
    gtk4::ToggleButton,
    gtk4::DropDown,
    header::frequency_selector::FrequencySelector,
) {
    // Play/stop button
    let play_button = gtk4::ToggleButton::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Start / Stop")
        .css_classes(["play-button"])
        .build();

    // Connect play/stop button to DSP
    let state_play = Rc::clone(state);
    play_button.connect_toggled(move |btn| {
        if btn.is_active() {
            btn.set_icon_name("media-playback-stop-symbolic");
            state_play.is_running.set(true);
            state_play.send_dsp(UiToDsp::Start);
        } else {
            btn.set_icon_name("media-playback-start-symbolic");
            state_play.is_running.set(false);
            state_play.send_dsp(UiToDsp::Stop);
        }
    });

    // Frequency selector as the title widget.
    // NOTE: The frequency-changed callback is connected later in `build_window`
    // so it can also update the status bar.
    let freq_selector = header::build_frequency_selector();

    // Demod selector dropdown
    let (demod_dropdown, _demod_mode_cell) = header::build_demod_selector();
    let state_demod = Rc::clone(state);
    demod_dropdown.connect_selected_notify(move |dd| {
        if let Some(mode) = demod_selector::index_to_demod_mode(dd.selected()) {
            state_demod.demod_mode.set(mode);
            state_demod.send_dsp(UiToDsp::SetDemodMode(mode));
            tracing::debug!(?mode, "demod mode sent to DSP");
        }
    });

    // Volume button (ScaleButton with audio icons)
    let volume_button = gtk4::ScaleButton::new(
        0.0,
        1.0,
        0.05,
        &[
            "audio-volume-muted-symbolic",
            "audio-volume-low-symbolic",
            "audio-volume-medium-symbolic",
            "audio-volume-high-symbolic",
        ],
    );
    volume_button.set_value(1.0);
    volume_button.set_tooltip_text(Some("Volume"));
    let state_vol = Rc::clone(state);
    volume_button.connect_value_changed(move |_btn, value| {
        #[allow(clippy::cast_possible_truncation)]
        state_vol.send_dsp(UiToDsp::SetVolume(value as f32));
    });

    // App menu
    let menu_button = build_menu_button();

    let header = adw::HeaderBar::builder()
        .title_widget(&freq_selector.widget)
        .build();

    header.pack_start(sidebar_toggle);
    header.pack_start(&play_button);
    header.pack_start(&demod_dropdown);
    header.pack_end(&menu_button);
    header.pack_end(&volume_button);

    (header, play_button, demod_dropdown.clone(), freq_selector)
}

/// Build the app menu button with Keyboard Shortcuts / About / Quit actions.
fn build_menu_button() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("_Keyboard Shortcuts"), Some("win.show-help-overlay"));
    menu.append(Some("_About SDR-RS"), Some("app.about"));
    menu.append(Some("_Quit"), Some("app.quit"));

    gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .build()
}

/// Wrap header and content in an `AdwToolbarView`.
fn build_toolbar_view(
    header: &adw::HeaderBar,
    content: &adw::OverlaySplitView,
) -> adw::ToolbarView {
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(header);
    toolbar_view.set_content(Some(content));
    toolbar_view
}

/// Create a breakpoint that collapses the sidebar below `SIDEBAR_BREAKPOINT_PX`.
fn build_breakpoint(split_view: &adw::OverlaySplitView) -> adw::Breakpoint {
    let condition = adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        SIDEBAR_BREAKPOINT_PX,
        adw::LengthUnit::Px,
    );

    let breakpoint = adw::Breakpoint::new(condition);
    breakpoint.add_setter(split_view, "collapsed", Some(&true.into()));

    breakpoint
}

/// Connect all sidebar panel controls to dispatch `UiToDsp` commands.
fn connect_sidebar_panels(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    status_bar: &Rc<StatusBar>,
) {
    connect_source_panel(panels, state);
    connect_radio_panel(panels, state);
    connect_display_panel(panels, state, spectrum_handle);
    connect_audio_panel(panels, state);
    connect_navigation_panel(panels, state, freq_selector, demod_dropdown, status_bar);
}

/// Connect source panel controls to DSP commands.
#[allow(clippy::too_many_lines)]
fn connect_source_panel(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Sample rate selector
    let state_sr = Rc::clone(state);
    panels
        .source
        .sample_rate_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            if let Some(&rate) = SAMPLE_RATES.get(idx) {
                state_sr.send_dsp(UiToDsp::SetSampleRate(rate));
            }
        });

    // DC blocking toggle
    let state_dc_block = Rc::clone(state);
    panels
        .source
        .dc_blocking_row
        .connect_active_notify(move |row| {
            state_dc_block.send_dsp(UiToDsp::SetDcBlocking(row.is_active()));
        });

    // IQ inversion toggle
    let state_iq_inv = Rc::clone(state);
    panels
        .source
        .iq_inversion_row
        .connect_active_notify(move |row| {
            state_iq_inv.send_dsp(UiToDsp::SetIqInversion(row.is_active()));
        });

    // Decimation selector
    let state_decim = Rc::clone(state);
    panels
        .source
        .decimation_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            if let Some(&factor) = DECIMATION_FACTORS.get(idx) {
                state_decim.send_dsp(UiToDsp::SetDecimation(factor));
            }
        });

    // Gain control
    let state_gain = Rc::clone(state);
    panels.source.gain_row.connect_value_notify(move |row| {
        state_gain.send_dsp(UiToDsp::SetGain(row.value()));
    });

    // AGC toggle
    let state_agc = Rc::clone(state);
    panels.source.agc_row.connect_active_notify(move |row| {
        state_agc.send_dsp(UiToDsp::SetAgc(row.is_active()));
    });

    // IQ correction toggle
    let state_iq_corr = Rc::clone(state);
    panels
        .source
        .iq_correction_row
        .connect_active_notify(move |row| {
            state_iq_corr.send_dsp(UiToDsp::SetIqCorrection(row.is_active()));
        });

    // PPM correction
    let state_ppm = Rc::clone(state);
    panels.source.ppm_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        state_ppm.send_dsp(UiToDsp::SetPpmCorrection(row.value() as i32));
    });

    // Source type selector — guard against transient out-of-range indices
    let state_source = Rc::clone(state);
    panels
        .source
        .device_row
        .connect_selected_notify(move |row| {
            let source_type = match row.selected() {
                0 => SourceType::RtlSdr,
                1 => SourceType::Network,
                2 => SourceType::File,
                _ => return, // ignore transient indices
            };
            state_source.send_dsp(UiToDsp::SetSourceType(source_type));
        });

    // Network hostname — send on every edit so Play always has current value
    let state_host = Rc::clone(state);
    let port_for_host = panels.source.port_row.clone();
    let proto_for_host = panels.source.protocol_row.clone();
    panels.source.hostname_row.connect_changed(move |row| {
        let hostname = row.text().to_string();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = port_for_host.value() as u16;
        let protocol = if proto_for_host.selected() == 1 {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        state_host.send_dsp(UiToDsp::SetNetworkConfig {
            hostname,
            port,
            protocol,
        });
    });

    // Network port
    let state_port = Rc::clone(state);
    let host_for_port = panels.source.hostname_row.clone();
    let proto_for_port = panels.source.protocol_row.clone();
    panels.source.port_row.connect_value_notify(move |row| {
        let hostname = host_for_port.text().to_string();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = row.value() as u16;
        let protocol = if proto_for_port.selected() == 1 {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        state_port.send_dsp(UiToDsp::SetNetworkConfig {
            hostname,
            port,
            protocol,
        });
    });

    // Network protocol
    let state_proto = Rc::clone(state);
    let host_for_proto = panels.source.hostname_row.clone();
    let port_for_proto = panels.source.port_row.clone();
    panels
        .source
        .protocol_row
        .connect_selected_notify(move |row| {
            let hostname = host_for_proto.text().to_string();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let port = port_for_proto.value() as u16;
            let protocol = match row.selected() {
                0 => sdr_types::Protocol::TcpClient,
                1 => sdr_types::Protocol::Udp,
                _ => return, // ignore transient indices
            };
            state_proto.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        });

    // File path — send on every edit so Play always has current value
    let state_file = Rc::clone(state);
    panels.source.file_path_row.connect_changed(move |row| {
        let path = std::path::PathBuf::from(row.text().to_string());
        state_file.send_dsp(UiToDsp::SetFilePath(path));
    });
}

/// Connect radio panel controls to DSP commands.
fn connect_radio_panel(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Bandwidth
    let state_bw = Rc::clone(state);
    panels.radio.bandwidth_row.connect_value_notify(move |row| {
        state_bw.send_dsp(UiToDsp::SetBandwidth(row.value()));
    });

    // Squelch enable
    let state_squelch_en = Rc::clone(state);
    panels
        .radio
        .squelch_enabled_row
        .connect_active_notify(move |row| {
            state_squelch_en.send_dsp(UiToDsp::SetSquelchEnabled(row.is_active()));
        });

    // Squelch level
    let state_squelch_lvl = Rc::clone(state);
    panels
        .radio
        .squelch_level_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_squelch_lvl.send_dsp(UiToDsp::SetSquelch(row.value() as f32));
        });

    // Deemphasis
    let state_de = Rc::clone(state);
    panels
        .radio
        .deemphasis_row
        .connect_selected_notify(move |row| {
            let mode = match row.selected() {
                1 => DeemphasisMode::Eu50,
                2 => DeemphasisMode::Us75,
                _ => DeemphasisMode::None,
            };
            state_de.send_dsp(UiToDsp::SetDeemphasis(mode));
        });

    // Noise blanker
    let state_noise_blanker = Rc::clone(state);
    panels
        .radio
        .noise_blanker_row
        .connect_active_notify(move |row| {
            state_noise_blanker.send_dsp(UiToDsp::SetNbEnabled(row.is_active()));
        });

    // Noise blanker level
    let state_nb_level = Rc::clone(state);
    panels.radio.nb_level_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        state_nb_level.send_dsp(UiToDsp::SetNbLevel(row.value() as f32));
    });

    // FM IF NR
    let state_fm_nr = Rc::clone(state);
    panels.radio.fm_if_nr_row.connect_active_notify(move |row| {
        state_fm_nr.send_dsp(UiToDsp::SetFmIfNrEnabled(row.is_active()));
    });

    // WFM Stereo
    let state_stereo = Rc::clone(state);
    panels.radio.stereo_row.connect_active_notify(move |row| {
        state_stereo.send_dsp(UiToDsp::SetWfmStereo(row.is_active()));
    });
}

/// FFT window function options matching the display panel combo.
const WINDOW_FUNCTIONS: [FftWindow; 3] = [
    FftWindow::Rectangular,
    FftWindow::Blackman,
    FftWindow::Nuttall,
];

/// Colormap options matching the display panel combo.
const COLORMAP_STYLES: [spectrum::colormap::ColormapStyle; 4] = [
    spectrum::colormap::ColormapStyle::Turbo,
    spectrum::colormap::ColormapStyle::Viridis,
    spectrum::colormap::ColormapStyle::Plasma,
    spectrum::colormap::ColormapStyle::Inferno,
];

/// Averaging mode options matching the display panel combo.
const AVERAGING_MODES: [spectrum::AveragingMode; 4] = [
    spectrum::AveragingMode::None,
    spectrum::AveragingMode::PeakHold,
    spectrum::AveragingMode::RunningAvg,
    spectrum::AveragingMode::MinHold,
];

/// Connect display panel controls to DSP commands.
fn connect_display_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
) {
    // FFT size
    let state_fft = Rc::clone(state);
    panels
        .display
        .fft_size_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            if let Some(&size) = FFT_SIZES.get(idx) {
                state_fft.send_dsp(UiToDsp::SetFftSize(size));
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

/// Connect navigation panel (band presets + bookmarks) to DSP commands.
fn connect_navigation_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    status_bar: &Rc<StatusBar>,
) {
    // Navigation callback: tune + set mode + set bandwidth, update UI widgets.
    let state_nav = Rc::clone(state);
    let fs = freq_selector.clone();
    let dd_weak = demod_dropdown.downgrade();
    let sb = Rc::clone(status_bar);

    panels.navigation.connect_navigate(move |freq, mode, bw| {
        #[allow(clippy::cast_precision_loss)]
        let freq_f64 = freq as f64;
        state_nav.center_frequency.set(freq_f64);
        state_nav.demod_mode.set(mode);

        // Send Tune and Bandwidth directly. SetDemodMode is sent by the
        // demod dropdown callback when we update its selection below.
        state_nav.send_dsp(UiToDsp::Tune(freq_f64));
        state_nav.send_dsp(UiToDsp::SetBandwidth(bw));

        // Update frequency selector display (does NOT fire callback — no duplicate Tune).
        fs.set_frequency(freq);

        // Update demod dropdown — its callback sends SetDemodMode to DSP.
        if let Some(dd) = dd_weak.upgrade()
            && let Some(idx) = demod_selector::demod_mode_to_index(mode)
        {
            dd.set_selected(idx);
        }

        // Update status bar
        sb.update_frequency(freq_f64);
        let label = header::demod_mode_label(mode);
        sb.update_demod(label, bw);

        tracing::info!(
            frequency = freq,
            ?mode,
            bandwidth = bw,
            "navigated to frequency"
        );
    });

    // "Add Bookmark" button
    let state_bm = Rc::clone(state);
    let radio_bw = panels.radio.bandwidth_row.clone();
    let nav = &panels.navigation;
    let bm_rc = nav.bookmarks.clone();
    let bm_list = nav.bookmark_list.clone();
    let bm_scroll = nav.bookmark_scroll.clone();
    let on_nav = nav.on_navigate.clone();
    let active_bm = nav.active_bookmark.clone();
    let name_entry = nav.name_entry.clone();

    nav.add_button.connect_clicked(move |_| {
        let freq = state_bm.center_frequency.get();
        let mode = state_bm.demod_mode.get();
        let bw = radio_bw.value();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let freq_u64 = freq as u64;
        let entered = name_entry.text();
        let name = if entered.is_empty() {
            sidebar::navigation_panel::format_frequency(freq_u64)
        } else {
            entered.to_string()
        };
        let bookmark = sidebar::navigation_panel::Bookmark::new(&name, freq_u64, mode, bw);
        bm_rc.borrow_mut().push(bookmark);
        sidebar::navigation_panel::save_bookmarks(&bm_rc.borrow());
        sidebar::navigation_panel::rebuild_bookmark_list(
            &bm_list,
            &bm_scroll,
            &bm_rc,
            &on_nav,
            &active_bm,
            &name_entry,
        );
        name_entry.set_text("");
    });
}

/// Connect audio panel controls to DSP commands.
fn connect_audio_panel(panels: &SidebarPanels, state: &Rc<AppState>) {
    // Audio device selector — routes PipeWire output to the selected sink
    let state_dev = Rc::clone(state);
    let node_names = panels.audio.device_node_names.clone();
    panels.audio.device_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        if let Some(node_name) = node_names.get(idx) {
            state_dev.send_dsp(UiToDsp::SetAudioDevice(node_name.clone()));
        }
    });
}

/// Register application-level actions (About, Quit).
fn setup_app_actions(app: &adw::Application, window: &adw::ApplicationWindow) {
    // Quit action
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            window.close();
        }
    ));
    app.add_action(&quit_action);
    app.set_accels_for_action("app.quit", &["<Ctrl>q"]);

    // About action
    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            let about = adw::AboutDialog::builder()
                .application_name("SDR-RS")
                .developer_name("Jason Herald")
                .version(env!("CARGO_PKG_VERSION"))
                .application_icon("com.sdr.rs")
                .license_type(gtk4::License::MitX11)
                .website("https://github.com/jasonherald/rtl-sdr")
                .comments("Software-defined radio for Linux")
                .developers(["Jason Herald"])
                .copyright("\u{00a9} 2026 Jason Herald")
                .issue_url("https://github.com/jasonherald/rtl-sdr/issues")
                .debug_info(format!(
                    "GTK {}.{}.{}\nLibadwaita {}.{}.{}\nPlatform: {}",
                    gtk4::major_version(),
                    gtk4::minor_version(),
                    gtk4::micro_version(),
                    adw::major_version(),
                    adw::minor_version(),
                    adw::micro_version(),
                    std::env::consts::OS,
                ))
                .build();
            about.present(Some(&window));
        }
    ));
    app.add_action(&about_action);
    app.set_accels_for_action("app.about", &["F1"]);
}
