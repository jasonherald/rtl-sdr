//! Main window construction — header bar, split view, breakpoints, DSP bridge.

use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_radio::DeemphasisMode;
use sdr_source_rtlsdr::SAMPLE_RATES;

use crate::dsp_controller;
use crate::header;
use crate::header::demod_selector;
use crate::messages::{DspToUi, UiToDsp};
use crate::sidebar;
use crate::sidebar::SidebarPanels;
use crate::spectrum;
use crate::state::AppState;

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
pub fn build_window(app: &adw::Application) {
    // --- Channel setup ---
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let (ui_tx, ui_rx) = mpsc::channel::<UiToDsp>();

    // Shared application state with DSP sender.
    let state = AppState::new_shared(ui_tx);

    // --- Build UI ---
    let (split_view, panels, spectrum_handle) = build_split_view();
    let sidebar_toggle = build_sidebar_toggle(&split_view);
    let (header, play_button) = build_header_bar(&sidebar_toggle, &state);
    let toolbar_view = build_toolbar_view(&header, &split_view);
    let breakpoint = build_breakpoint(&split_view);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("SDR-RS")
        .default_width(DEFAULT_WIDTH)
        .default_height(DEFAULT_HEIGHT)
        .content(&toolbar_view)
        .build();

    window.add_breakpoint(breakpoint);

    // Toast overlay for error messages.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar_view));
    window.set_content(Some(&toast_overlay));

    setup_app_actions(app, &window);

    // --- Connect sidebar panels to DSP ---
    connect_sidebar_panels(&panels, &state);

    // --- Spawn DSP thread ---
    dsp_controller::spawn_dsp_thread(dsp_tx, ui_rx);

    // --- Poll DspToUi channel from the GTK main loop ---
    let spectrum_handle = Rc::new(spectrum_handle);
    let play_button_weak = play_button.downgrade();
    let state_rx = Rc::clone(&state);
    let toast_overlay_weak = toast_overlay.downgrade();

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
) {
    match msg {
        DspToUi::FftData(data) => {
            spectrum_handle.push_fft_data(&data);
        }
        DspToUi::SnrUpdate(_snr) => {
            // Store for future status bar.
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
        }
        DspToUi::DeviceInfo(info) => {
            tracing::info!(device_info = %info, "device info received");
        }
    }
}

/// Build the `AdwOverlaySplitView` with sidebar configuration panels and content.
///
/// Returns the split view, sidebar panels, and spectrum display handle.
fn build_split_view() -> (
    adw::OverlaySplitView,
    SidebarPanels,
    spectrum::SpectrumHandle,
) {
    // Sidebar — configuration panels.
    let (sidebar_scroll, panels) = sidebar::build_sidebar();

    // Main content area — spectrum display (FFT plot + waterfall).
    let (spectrum_view, spectrum_handle) = spectrum::build_spectrum_view();
    spectrum_view.add_css_class("spectrum-area");

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    content_box.append(&spectrum_view);

    let split_view = adw::OverlaySplitView::builder()
        .sidebar(&sidebar_scroll)
        .content(&content_box)
        .show_sidebar(true)
        .build();

    (split_view, panels, spectrum_handle)
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
/// Returns the header bar and the play button (for updating state from `DspToUi`).
fn build_header_bar(
    sidebar_toggle: &gtk4::ToggleButton,
    state: &Rc<AppState>,
) -> (adw::HeaderBar, gtk4::ToggleButton) {
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

    // Frequency selector as the title widget
    let freq_selector = header::build_frequency_selector();
    let state_freq = Rc::clone(state);
    freq_selector.connect_frequency_changed(move |freq| {
        tracing::debug!(frequency_hz = freq, "frequency changed");
        #[allow(clippy::cast_precision_loss)]
        let freq_f64 = freq as f64;
        state_freq.center_frequency.set(freq_f64);
        state_freq.send_dsp(UiToDsp::Tune(freq_f64));
    });

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

    (header, play_button)
}

/// Build the app menu button with About / Quit actions.
fn build_menu_button() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
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
fn connect_sidebar_panels(panels: &SidebarPanels, state: &Rc<AppState>) {
    connect_source_panel(panels, state);
    connect_radio_panel(panels, state);
    connect_display_panel(panels, state);
}

/// Connect source panel controls to DSP commands.
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

    // FM IF NR
    let state_fm_nr = Rc::clone(state);
    panels.radio.fm_if_nr_row.connect_active_notify(move |row| {
        state_fm_nr.send_dsp(UiToDsp::SetFmIfNrEnabled(row.is_active()));
    });
}

/// Connect display panel controls to DSP commands.
fn connect_display_panel(panels: &SidebarPanels, state: &Rc<AppState>) {
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
                .application_icon("audio-radio-symbolic")
                .license_type(gtk4::License::MitX11)
                .website("https://github.com/jasonherald/rtl-sdr")
                .build();
            about.present(Some(&window));
        }
    ));
    app.add_action(&about_action);
}
