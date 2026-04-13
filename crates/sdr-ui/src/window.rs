//! Main window construction — header bar, split view, breakpoints, DSP bridge.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_core::Engine;
use sdr_pipeline::iq_frontend::FftWindow;
use sdr_radio::DeemphasisMode;
use sdr_source_rtlsdr::SAMPLE_RATES;

use crate::header;
use crate::header::demod_selector;
use crate::messages::{DspToUi, SourceType, UiToDsp};
use crate::shortcuts;
use crate::sidebar;
use crate::sidebar::SidebarPanels;
use crate::spectrum;
use crate::state::AppState;
use crate::status_bar::{self, StatusBar};

/// Default recording directory under the user's home.
const RECORDING_DIR_NAME: &str = "sdr-recordings";

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 1200;
/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 800;
/// Sidebar collapse breakpoint width in pixels.
const SIDEBAR_BREAKPOINT_PX: f64 = 800.0;

/// FFT sizes — re-exported from display panel (single source of truth).
use crate::sidebar::display_panel::FFT_SIZES;
#[cfg(feature = "sherpa")]
use crate::sidebar::transcript_panel::DISPLAY_MODE_FINAL_IDX;

/// Decimation factors available in the source panel dropdown (must match panel order).
const DECIMATION_FACTORS: &[u32] = &[1, 2, 4, 8, 16];

/// Interval in milliseconds for polling the DSP→UI channel.
const DSP_POLL_INTERVAL_MS: u64 = 16;

/// Build and present the main application window.
#[allow(clippy::too_many_lines)]
pub fn build_window(app: &adw::Application, config: &std::sync::Arc<sdr_config::ConfigManager>) {
    // --- Engine bootstrap ---
    //
    // The headless engine (sdr-core) owns the DSP controller thread, the
    // command/event channels, and the shared FFT buffer. The GTK side
    // consumes those pieces through the Engine facade — `command_sender`
    // and `fft_buffer` are migration helpers that hand back the same raw
    // channel-and-Arc plumbing the previous `dsp_controller::spawn_dsp_thread`
    // call assembled inline. The Engine itself is wrapped in `Rc` and
    // captured by the DSP-poll closure below so it lives for the lifetime
    // of this window. When the window closes, the closure (and therefore
    // the Engine) is dropped, the command channel disconnects, and the
    // detached DSP thread exits naturally.
    //
    // We `expect` on `Engine::new` because the only failure mode is the
    // OS rejecting `std::thread::Builder::spawn`, which previously caused
    // the process to exit anyway via `std::process::exit(1)` inside the
    // old `spawn_dsp_thread`. Keeping the same panic-on-spawn-failure
    // semantics for now; the FFI side will surface this as an error code.
    let engine = Rc::new(Engine::new().expect("DSP engine should spawn"));
    let ui_tx = engine.command_sender();
    let dsp_rx = engine
        .subscribe()
        .expect("Engine::subscribe must return Some on its first call");
    let fft_shared = engine.fft_buffer();

    // Shared application state with DSP sender.
    let state = AppState::new_shared(ui_tx);

    // --- Build UI ---
    let (
        split_view,
        panels,
        spectrum_handle_raw,
        status_bar,
        transcript_panel,
        transcript_revealer,
    ) = build_split_view(&state, config);
    let spectrum_handle = Rc::new(spectrum_handle_raw);
    let sidebar_toggle = build_sidebar_toggle(&split_view);
    let (header, play_button, demod_dropdown, freq_selector, screenshot_button, rr_button) =
        build_header_bar(&sidebar_toggle, &state);

    // Transcript toggle button in header bar.
    let transcript_button = gtk4::ToggleButton::builder()
        .icon_name("document-page-setup-symbolic")
        .tooltip_text("Toggle transcript panel")
        .build();
    header.pack_end(&transcript_button);

    let revealer_clone = transcript_revealer.clone();
    transcript_button.connect_toggled(move |btn| {
        revealer_clone.set_reveal_child(btn.is_active());
    });

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

    setup_app_actions(app, &window, config, &rr_button);

    // Wire transcript panel (separate from sidebar panels).
    let transcription_engine = connect_transcript_panel(&transcript_panel, &state, config);

    // On window close, signal the worker to stop without blocking.
    window.connect_close_request(move |_| {
        transcription_engine.borrow_mut().shutdown_nonblocking();
        glib::Propagation::Proceed
    });

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

    // Wire waterfall screenshot button.
    let spectrum_screenshot = Rc::clone(&spectrum_handle);
    screenshot_button.connect_clicked(move |_| {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let dir = glib::user_special_dir(glib::UserDirectory::Pictures)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let path = dir.join(format!("sdr-rs-waterfall-{timestamp}.png"));
        match spectrum_screenshot.export_waterfall_png(&path) {
            Ok(()) => {
                tracing::info!(?path, "waterfall exported");
                crate::notify::send(
                    "Waterfall Exported",
                    &format!("Saved to {}", path.display()),
                    Some(&path),
                );
            }
            Err(e) => {
                tracing::warn!("waterfall export failed: {e}");
                crate::notify::send("Export Failed", &e, None);
            }
        }
    });

    // Wire RadioReference browse button.
    {
        let bm_list = panels.navigation.bookmark_list.clone();
        let bm_scroll = panels.navigation.bookmark_scroll.clone();
        let bm_rc = panels.navigation.bookmarks.clone();
        let on_nav = panels.navigation.on_navigate.clone();
        let active_bm = panels.navigation.active_bookmark.clone();
        let name_entry = panels.navigation.name_entry.clone();
        let on_save = panels.navigation.on_save.clone();

        rr_button.connect_clicked(move |btn| {
            let bm_list = bm_list.clone();
            let bm_scroll = bm_scroll.clone();
            let bm_rc = bm_rc.clone();
            let on_nav = on_nav.clone();
            let active_bm = active_bm.clone();
            let name_entry = name_entry.clone();
            let on_save = on_save.clone();

            crate::radioreference::show_browse_dialog(btn, move || {
                // Reload bookmarks from disk and rebuild the sidebar list.
                *bm_rc.borrow_mut() = sidebar::navigation_panel::load_bookmarks();
                sidebar::navigation_panel::rebuild_bookmark_list(
                    &bm_list,
                    &bm_scroll,
                    &bm_rc,
                    &on_nav,
                    &active_bm,
                    &name_entry,
                    &on_save,
                );
            });
        });
    }

    // Wire cursor readout from spectrum to status bar.
    let status_bar_for_cursor = Rc::clone(&status_bar_demod);
    spectrum_handle.connect_cursor_moved(move |freq_hz, power_db| {
        status_bar_for_cursor.update_cursor(freq_hz, power_db);
    });

    // Wire VFO offset changes (click-to-tune / drag) to the frequency display
    // and status bar so the header shows the actual tuned frequency.
    let status_bar_for_vfo = Rc::clone(&status_bar_demod);
    let state_for_vfo = Rc::clone(&state);
    let fs_for_vfo = freq_selector.clone();
    spectrum_handle.connect_vfo_offset_changed(move |offset_hz| {
        let center = state_for_vfo.center_frequency.get();
        let tuned = center + offset_hz;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let tuned_u64 = tuned.max(0.0) as u64;
        fs_for_vfo.set_frequency(tuned_u64);
        status_bar_for_vfo.update_frequency(tuned);
    });

    let status_bar_for_freq = Rc::clone(&status_bar_demod);
    let state_freq = Rc::clone(&state);
    let spectrum_for_freq = Rc::clone(&spectrum_handle);
    freq_selector.connect_frequency_changed(move |freq| {
        tracing::debug!(frequency_hz = freq, "frequency changed");
        #[allow(clippy::cast_precision_loss)]
        let freq_f64 = freq as f64;
        state_freq.center_frequency.set(freq_f64);
        state_freq.send_dsp(UiToDsp::Tune(freq_f64));
        status_bar_for_freq.update_frequency(freq_f64);
        spectrum_for_freq.set_center_frequency(freq_f64);
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

    // --- Poll DspToUi channel and shared FFT buffer from the GTK main loop ---
    //
    // The DSP thread itself was already spawned by `Engine::new` above;
    // we just hook the GTK main loop into the channels and FFT buffer it
    // exposed. The closure captures an `Rc<Engine>` clone, which is what
    // keeps the engine alive while the timeout is registered. To make
    // the lifetime self-cleaning, the closure also captures a `Weak`
    // reference to the window: when the window drops (i.e., on close),
    // the next timeout tick fails to upgrade the weak ref, calls
    // `engine.shutdown()` to send a final `Stop`, and returns
    // `ControlFlow::Break`. Returning Break removes this source from the
    // GLib main context, which drops the closure and the captured
    // `Rc<Engine>` clone — at which point the engine itself drops (its
    // last Rc), closing the command channel and letting the detached
    // controller thread exit naturally on its next `recv_timeout` tick.
    //
    // Without this Weak check the closure would outlive the window
    // (`glib::timeout_add_local` attaches to the *global* main context,
    // not to the window) and the engine would persist as a headless
    // background DSP process for as long as the application stayed
    // alive. CodeRabbit caught that one in PR #251.
    let play_button_weak = play_button.downgrade();
    let state_rx = Rc::clone(&state);
    let toast_overlay_weak = toast_overlay.downgrade();
    let window_weak = window.downgrade();

    let gain_row_for_dsp = panels.source.gain_row.clone();
    let record_audio_for_dsp = panels.audio.record_audio_row.clone();
    let record_iq_for_dsp = panels.source.record_iq_row.clone();
    let transcription_enable_for_dsp = transcript_panel.enable_row.clone();
    let engine_for_dsp = Rc::clone(&engine);
    // We deliberately discard the SourceId returned by `timeout_add_local`:
    // the window-lifecycle gate at the top of the closure returns
    // `ControlFlow::Break` when the window is dropped, which is GLib's
    // idiomatic "remove this source" signal. There's no other code path
    // that needs to remove the source explicitly.
    let _ = glib::timeout_add_local(Duration::from_millis(DSP_POLL_INTERVAL_MS), move || {
        // Window-lifecycle gate. If the window is gone, send the engine
        // an explicit Stop and ask GLib to drop this source. The
        // shutdown call is best-effort: if the engine has already torn
        // itself down (e.g., the controller panicked) the channel is
        // closed and we just log-and-continue.
        if window_weak.upgrade().is_none() {
            if let Err(err) = engine_for_dsp.shutdown() {
                tracing::debug!(
                    ?err,
                    "engine.shutdown() during window close (channel may already be closed)"
                );
            }
            return glib::ControlFlow::Break;
        }

        // Check for new FFT data from the shared buffer (zero-alloc path).
        fft_shared.take_if_ready(|data| {
            spectrum_handle.push_fft_data(data);
        });

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
                        &record_audio_for_dsp,
                        &record_iq_for_dsp,
                        &transcription_enable_for_dsp,
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
#[allow(clippy::too_many_arguments)]
fn handle_dsp_message(
    msg: DspToUi,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    play_button_weak: &glib::WeakRef<gtk4::ToggleButton>,
    state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    status_bar: &Rc<StatusBar>,
    gain_row: &adw::SpinRow,
    record_audio_row: &adw::SwitchRow,
    record_iq_row: &adw::SwitchRow,
    transcription_enable_row: &adw::SwitchRow,
) {
    match msg {
        DspToUi::FftData(_) => {
            // FFT data now comes via SharedFftBuffer, not the channel.
            // This variant is kept for backward compatibility but shouldn't
            // be sent in normal operation.
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
            // Reset recording and transcription toggles when the source stops.
            record_audio_row.set_active(false);
            record_iq_row.set_active(false);
            transcription_enable_row.set_active(false);
        }
        DspToUi::SampleRateChanged(rate) => {
            tracing::info!(effective_sample_rate = rate, "sample rate changed");
            status_bar.update_sample_rate(rate);
        }
        DspToUi::DisplayBandwidth(raw_rate) => {
            tracing::info!(raw_sample_rate = raw_rate, "display bandwidth updated");
            spectrum_handle.set_display_bandwidth(raw_rate);
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
        DspToUi::AudioRecordingStarted(path) => {
            tracing::info!(?path, "audio recording started");
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let name = path
                    .file_name()
                    .map_or("file".to_string(), |n| n.to_string_lossy().to_string());
                let toast = adw::Toast::new(&format!("Recording audio: {name}"));
                overlay.add_toast(toast);
            }
        }
        DspToUi::AudioRecordingStopped => {
            tracing::info!("audio recording stopped");
            record_audio_row.set_active(false);
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::new("Audio recording saved");
                overlay.add_toast(toast);
            }
        }
        DspToUi::IqRecordingStarted(path) => {
            tracing::info!(?path, "IQ recording started");
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let name = path
                    .file_name()
                    .map_or("file".to_string(), |n| n.to_string_lossy().to_string());
                let toast = adw::Toast::new(&format!("Recording IQ: {name}"));
                overlay.add_toast(toast);
            }
        }
        DspToUi::IqRecordingStopped => {
            tracing::info!("IQ recording stopped");
            record_iq_row.set_active(false);
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::new("IQ recording saved");
                overlay.add_toast(toast);
            }
        }
        // Task 16 will wire up the full UI response (stop transcription session,
        // re-run Auto Break row visibility rules). For now just log the event.
        DspToUi::DemodModeChanged(mode) => {
            tracing::debug!(?mode, "demod mode changed (Task 16 handler pending)");
        }
    }
}

/// Build the `AdwOverlaySplitView` with sidebar configuration panels, content,
/// and status bar.
///
/// Returns the split view, sidebar panels, spectrum display handle, and status bar.
fn build_split_view(
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> (
    adw::OverlaySplitView,
    SidebarPanels,
    spectrum::SpectrumHandle,
    StatusBar,
    sidebar::transcript_panel::TranscriptPanel,
    gtk4::Revealer,
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

    // Transcript panel — slides out from the right.
    let transcript_panel = sidebar::transcript_panel::build_transcript_panel(config);
    let transcript_scroll = gtk4::ScrolledWindow::builder()
        .child(&transcript_panel.widget)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .width_request(320)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let transcript_revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideLeft)
        .transition_duration(200)
        .reveal_child(false)
        .child(&transcript_scroll)
        .hexpand(false)
        .build();

    // Wrap content + transcript revealer in an HBox.
    let content_with_transcript = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .build();
    content_with_transcript.append(&content_box);
    content_with_transcript.append(&transcript_revealer);

    let split_view = adw::OverlaySplitView::builder()
        .sidebar(&sidebar_scroll)
        .content(&content_with_transcript)
        .show_sidebar(true)
        .build();

    (
        split_view,
        panels,
        spectrum_handle,
        status_bar,
        transcript_panel,
        transcript_revealer,
    )
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
    gtk4::Button,
    gtk4::Button,
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
    // Waterfall screenshot button
    let screenshot_button = gtk4::Button::builder()
        .icon_name("camera-photo-symbolic")
        .tooltip_text("Export waterfall to PNG")
        .build();

    // RadioReference frequency browser button
    let rr_button = gtk4::Button::builder()
        .icon_name("network-wireless-symbolic")
        .tooltip_text("RadioReference Frequency Browser")
        .visible(crate::preferences::accounts_page::has_rr_credentials())
        .build();

    header.pack_end(&menu_button);
    header.pack_end(&volume_button);
    header.pack_end(&rr_button);
    header.pack_end(&screenshot_button);

    (
        header,
        play_button,
        demod_dropdown.clone(),
        freq_selector,
        screenshot_button,
        rr_button,
    )
}

/// Build the app menu button with Preferences / Keyboard Shortcuts / About / Quit actions.
fn build_menu_button() -> gtk4::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("_Preferences"), Some("app.preferences"));
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
    // Transcript panel is wired separately (not in SidebarPanels).
    connect_navigation_panel(
        panels,
        state,
        freq_selector,
        demod_dropdown,
        status_bar,
        spectrum_handle,
    );
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

    // IQ recording toggle
    let state_iq_rec = Rc::clone(state);
    panels
        .source
        .record_iq_row
        .connect_active_notify(move |row| {
            if row.is_active() {
                let path = recording_path("iq");
                tracing::info!(?path, "starting IQ recording");
                state_iq_rec.send_dsp(UiToDsp::StartIqRecording(path));
            } else {
                tracing::info!("stopping IQ recording");
                state_iq_rec.send_dsp(UiToDsp::StopIqRecording);
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

    // Auto-squelch
    let state_auto_sq = Rc::clone(state);
    panels
        .radio
        .auto_squelch_row
        .connect_active_notify(move |row| {
            state_auto_sq.send_dsp(UiToDsp::SetAutoSquelch(row.is_active()));
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

    // Notch filter enable
    let state_notch_en = Rc::clone(state);
    panels
        .radio
        .notch_enabled_row
        .connect_active_notify(move |row| {
            state_notch_en.send_dsp(UiToDsp::SetNotchEnabled(row.is_active()));
        });

    // Notch filter frequency
    let state_notch_freq = Rc::clone(state);
    panels
        .radio
        .notch_freq_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_notch_freq.send_dsp(UiToDsp::SetNotchFrequency(row.value() as f32));
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
#[allow(clippy::too_many_lines)]
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

/// Restore optional tuning-profile settings from a bookmark to DSP and UI.
fn restore_bookmark_profile(
    bookmark: &sidebar::navigation_panel::Bookmark,
    state: &AppState,
    radio: &sidebar::RadioPanel,
    gain_row: &adw::SpinRow,
    agc_row: &adw::SwitchRow,
) {
    if let Some(sq_en) = bookmark.squelch_enabled {
        state.send_dsp(UiToDsp::SetSquelchEnabled(sq_en));
        radio.squelch_enabled_row.set_active(sq_en);
    }
    if let Some(sq_lvl) = bookmark.squelch_level {
        state.send_dsp(UiToDsp::SetSquelch(sq_lvl));
        #[allow(clippy::cast_lossless)]
        radio.squelch_level_row.set_value(sq_lvl as f64);
    }
    // AGC must be set before gain — switching to manual mode first
    // ensures the saved gain value actually takes effect.
    if let Some(agc) = bookmark.agc {
        state.send_dsp(UiToDsp::SetAgc(agc));
        agc_row.set_active(agc);
    }
    if let Some(gain) = bookmark.gain {
        if bookmark.agc != Some(true) {
            state.send_dsp(UiToDsp::SetGain(gain));
        }
        gain_row.set_value(gain);
    }
    if let Some(vol) = bookmark.volume {
        state.send_dsp(UiToDsp::SetVolume(vol));
    }
    if let Some(de_idx) = bookmark.deemphasis {
        let deemp = match de_idx {
            1 => DeemphasisMode::Eu50,
            2 => DeemphasisMode::Us75,
            _ => DeemphasisMode::None,
        };
        state.send_dsp(UiToDsp::SetDeemphasis(deemp));
        radio.deemphasis_row.set_selected(de_idx);
    }
    if let Some(nb_en) = bookmark.nb_enabled {
        state.send_dsp(UiToDsp::SetNbEnabled(nb_en));
        radio.noise_blanker_row.set_active(nb_en);
    }
    if let Some(nb_lvl) = bookmark.nb_level {
        state.send_dsp(UiToDsp::SetNbLevel(nb_lvl));
        #[allow(clippy::cast_lossless)]
        radio.nb_level_row.set_value(nb_lvl as f64);
    }
    if let Some(fm_nr) = bookmark.fm_if_nr {
        state.send_dsp(UiToDsp::SetFmIfNrEnabled(fm_nr));
        radio.fm_if_nr_row.set_active(fm_nr);
    }
    if let Some(stereo) = bookmark.wfm_stereo {
        state.send_dsp(UiToDsp::SetWfmStereo(stereo));
        radio.stereo_row.set_active(stereo);
    }
    if let Some(hp) = bookmark.high_pass {
        state.send_dsp(UiToDsp::SetHighPass(hp));
    }
}

/// Connect navigation panel (band presets + bookmarks) to DSP commands.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn connect_navigation_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    status_bar: &Rc<StatusBar>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
) {
    // Navigation callback: restore full tuning profile from bookmark.
    let state_nav = Rc::clone(state);
    let fs = freq_selector.clone();
    let dd_weak = demod_dropdown.downgrade();
    let sb = Rc::clone(status_bar);
    let spectrum_nav = Rc::clone(spectrum_handle);
    let radio_nav = panels.radio.clone();
    let source_nav_gain = panels.source.gain_row.clone();
    let source_nav_agc = panels.source.agc_row.clone();

    panels.navigation.connect_navigate(move |bookmark| {
        let freq = bookmark.frequency;
        let mode = sidebar::navigation_panel::parse_demod_mode(&bookmark.demod_mode);
        let bw = bookmark.bandwidth;

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
        spectrum_nav.set_center_frequency(freq_f64);

        // Update demod dropdown — its callback sends SetDemodMode to DSP.
        if let Some(dd) = dd_weak.upgrade()
            && let Some(idx) = demod_selector::demod_mode_to_index(mode)
        {
            dd.set_selected(idx);
        }

        // Update bandwidth widget (fires its own DSP callback).
        radio_nav.bandwidth_row.set_value(bw);

        // Restore optional tuning-profile settings (squelch, gain, etc.).
        restore_bookmark_profile(
            bookmark,
            &state_nav,
            &radio_nav,
            &source_nav_gain,
            &source_nav_agc,
        );

        // Update mode-specific control visibility for the restored mode.
        radio_nav.apply_demod_visibility(mode);

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

    // "Add Bookmark" button — capture full tuning profile from current UI state.
    let state_bm = Rc::clone(state);
    let radio_bm = panels.radio.clone();
    let source_gain_bm = panels.source.gain_row.clone();
    let source_agc_bm = panels.source.agc_row.clone();
    let nav = &panels.navigation;
    let bm_rc = nav.bookmarks.clone();
    let bm_list = nav.bookmark_list.clone();
    let bm_scroll = nav.bookmark_scroll.clone();
    let on_nav = nav.on_navigate.clone();
    let active_bm = nav.active_bookmark.clone();
    let on_save_bm = nav.on_save.clone();
    let name_entry = nav.name_entry.clone();

    nav.add_button.connect_clicked(move |_| {
        let freq = state_bm.center_frequency.get();
        let mode = state_bm.demod_mode.get();
        let bw = radio_bm.bandwidth_row.value();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let freq_u64 = freq as u64;
        let entered = name_entry.text();
        let name = if entered.is_empty() {
            sidebar::navigation_panel::format_frequency(freq_u64)
        } else {
            entered.to_string()
        };

        // Capture full tuning profile from current UI widget state.
        #[allow(clippy::cast_possible_truncation)]
        let profile = sidebar::navigation_panel::TuningProfile {
            squelch_enabled: radio_bm.squelch_enabled_row.is_active(),
            squelch_level: radio_bm.squelch_level_row.value() as f32,
            gain: source_gain_bm.value(),
            agc: source_agc_bm.is_active(),
            volume: None, // Volume ScaleButton not in sidebar — don't persist.
            deemphasis: radio_bm.deemphasis_row.selected(),
            nb_enabled: radio_bm.noise_blanker_row.is_active(),
            nb_level: radio_bm.nb_level_row.value() as f32,
            fm_if_nr: radio_bm.fm_if_nr_row.is_active(),
            wfm_stereo: radio_bm.stereo_row.is_active(),
            high_pass: None, // No UI widget yet — don't persist.
        };
        let bookmark =
            sidebar::navigation_panel::Bookmark::with_profile(&name, freq_u64, mode, bw, &profile);
        bm_rc.borrow_mut().push(bookmark);
        sidebar::navigation_panel::save_bookmarks(&bm_rc.borrow());
        sidebar::navigation_panel::rebuild_bookmark_list(
            &bm_list,
            &bm_scroll,
            &bm_rc,
            &on_nav,
            &active_bm,
            &name_entry,
            &on_save_bm,
        );
        name_entry.set_text("");
    });

    // Save button — update the active bookmark with current settings.
    let save_bm_rc = nav.bookmarks.clone();
    let save_active = nav.active_bookmark.clone();
    let save_bm_list = nav.bookmark_list.clone();
    let save_bm_scroll = nav.bookmark_scroll.clone();
    let save_on_nav = nav.on_navigate.clone();
    let save_on_save = nav.on_save.clone();
    let save_name_entry = nav.name_entry.clone();
    let save_state = Rc::clone(state);
    let save_radio_bw = panels.radio.bandwidth_row.clone();
    let save_radio_sq_en = panels.radio.squelch_enabled_row.clone();
    let save_radio_sq_lvl = panels.radio.squelch_level_row.clone();
    let save_radio_deemp = panels.radio.deemphasis_row.clone();
    let save_radio_nben = panels.radio.noise_blanker_row.clone();
    let save_radio_nben_lvl = panels.radio.nb_level_row.clone();
    let save_radio_nr = panels.radio.fm_if_nr_row.clone();
    let save_radio_stereo = panels.radio.stereo_row.clone();
    let save_source_gain = panels.source.gain_row.clone();
    let save_source_agc = panels.source.agc_row.clone();
    nav.connect_save(move || {
        let active = save_active.borrow().clone();
        if active.name.is_empty() && active.frequency == 0 {
            return; // No active bookmark to save.
        }
        let freq = save_state.center_frequency.get();
        let mode = save_state.demod_mode.get();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let freq_u64 = freq as u64;
        let bw = save_radio_bw.value();
        let profile = sidebar::navigation_panel::TuningProfile {
            squelch_enabled: save_radio_sq_en.is_active(),
            #[allow(clippy::cast_possible_truncation)]
            squelch_level: save_radio_sq_lvl.value() as f32,
            gain: save_source_gain.value(),
            agc: save_source_agc.is_active(),
            volume: None,
            deemphasis: save_radio_deemp.selected(),
            nb_enabled: save_radio_nben.is_active(),
            #[allow(clippy::cast_possible_truncation)]
            nb_level: save_radio_nben_lvl.value() as f32,
            fm_if_nr: save_radio_nr.is_active(),
            wfm_stereo: save_radio_stereo.is_active(),
            high_pass: None,
        };
        // Find and update the active bookmark in the list.
        let mut bms = save_bm_rc.borrow_mut();
        if let Some(bm) = bms
            .iter_mut()
            .find(|b| b.name == active.name && b.frequency == active.frequency)
        {
            bm.frequency = freq_u64;
            bm.demod_mode = sidebar::navigation_panel::demod_mode_to_string(mode);
            bm.bandwidth = bw;
            bm.squelch_enabled = Some(profile.squelch_enabled);
            bm.squelch_level = Some(profile.squelch_level);
            bm.gain = Some(profile.gain);
            bm.agc = Some(profile.agc);
            bm.volume = profile.volume;
            bm.deemphasis = Some(profile.deemphasis);
            bm.nb_enabled = Some(profile.nb_enabled);
            bm.nb_level = Some(profile.nb_level);
            bm.fm_if_nr = Some(profile.fm_if_nr);
            bm.wfm_stereo = Some(profile.wfm_stereo);
            bm.high_pass = profile.high_pass;
            // Keep ActiveBookmark in sync with the updated frequency.
            *save_active.borrow_mut() = sidebar::navigation_panel::ActiveBookmark {
                name: active.name.clone(),
                frequency: freq_u64,
            };
        }
        sidebar::navigation_panel::save_bookmarks(&bms);
        drop(bms);
        // Rebuild to update subtitle.
        sidebar::navigation_panel::rebuild_bookmark_list(
            &save_bm_list,
            &save_bm_scroll,
            &save_bm_rc,
            &save_on_nav,
            &save_active,
            &save_name_entry,
            &save_on_save,
        );
        tracing::info!("bookmark saved: {}", active.name);
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

    // Audio recording toggle
    let state_rec = Rc::clone(state);
    panels
        .audio
        .record_audio_row
        .connect_active_notify(move |row| {
            if row.is_active() {
                let path = recording_path("audio");
                tracing::info!(?path, "starting audio recording");
                state_rec.send_dsp(UiToDsp::StartAudioRecording(path));
            } else {
                tracing::info!("stopping audio recording");
                state_rec.send_dsp(UiToDsp::StopAudioRecording);
            }
        });
}

/// Re-enable every transcription settings row that gets locked during
/// an active session.
///
/// Single source of truth for the row-unlock side of the four
/// session-end paths in [`connect_transcript_panel`]:
///
/// 1. `TranscriptionEvent::Error` arm in the timeout closure
/// 2. `TryRecvError::Disconnected` arm in the timeout closure
/// 3. Synchronous `engine.start()` failure in `connect_active_notify`
/// 4. Normal stop (off branch of `connect_active_notify`)
///
/// Takes weak refs so paths 1 and 2 (which hold weak refs to avoid
/// keeping widgets alive past their UI lifetime) can call it directly.
/// Paths 3 and 4 hold strong refs and pass `&strong.downgrade()` —
/// the temporary lives through the function call.
///
/// Tolerant of any individual weak ref failing to upgrade (window close
/// race) — each row is checked independently so a partially-dropped UI
/// still recovers what it can.
fn unlock_transcription_session_rows(
    model_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "whisper")] silence_row: &glib::WeakRef<adw::SpinRow>,
    noise_gate_row: &glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")] display_mode_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")] vad_threshold_row: &glib::WeakRef<adw::SpinRow>,
) {
    if let Some(row) = model_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "whisper")]
    if let Some(row) = silence_row.upgrade() {
        row.set_sensitive(true);
    }
    if let Some(row) = noise_gate_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = display_mode_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = vad_threshold_row.upgrade() {
        row.set_sensitive(true);
    }
}

/// Connect transcript panel controls to DSP commands.
///
/// Returns the engine handle so it can be stopped on window close.
#[allow(clippy::too_many_lines)]
fn connect_transcript_panel(
    transcript: &sidebar::transcript_panel::TranscriptPanel,
    state: &Rc<AppState>,
    #[cfg_attr(not(feature = "sherpa"), allow(unused_variables))] config: &std::sync::Arc<
        sdr_config::ConfigManager,
    >,
) -> Rc<RefCell<sdr_transcription::TranscriptionEngine>> {
    use sdr_transcription::{TranscriptionEngine, TranscriptionEvent};

    let engine: Rc<RefCell<TranscriptionEngine>> =
        Rc::new(RefCell::new(TranscriptionEngine::new()));

    let state_clone = Rc::clone(state);
    let engine_clone = Rc::clone(&engine);
    let status_label = transcript.status_label.clone();
    let progress_bar = transcript.progress_bar.clone();
    let text_view = transcript.text_view.clone();
    let model_row = transcript.model_row.clone();
    #[cfg(feature = "whisper")]
    let silence_row = transcript.silence_row.clone();
    let noise_gate_row = transcript.noise_gate_row.clone();
    // Weak refs used by the async event-loop closure to drive the same
    // teardown the synchronous error path does (see below) when the
    // backend fires TranscriptionEvent::Error mid-session. Weak so the
    // timeout closure doesn't keep widgets alive past their UI lifetime.
    let enable_row_weak = transcript.enable_row.downgrade();
    let model_row_weak = model_row.downgrade();
    #[cfg(feature = "whisper")]
    let silence_row_weak = silence_row.downgrade();
    let noise_gate_row_weak = noise_gate_row.downgrade();
    #[cfg(feature = "sherpa")]
    let display_mode_row = transcript.display_mode_row.clone();
    #[cfg(feature = "sherpa")]
    let vad_threshold_row = transcript.vad_threshold_row.clone();
    #[cfg(feature = "sherpa")]
    let live_line_label = transcript.live_line_label.clone();
    #[cfg(feature = "sherpa")]
    let display_mode_row_weak = display_mode_row.downgrade();
    #[cfg(feature = "sherpa")]
    let vad_threshold_row_weak = vad_threshold_row.downgrade();
    #[cfg(feature = "sherpa")]
    let live_line_weak = live_line_label.downgrade();

    #[cfg(feature = "sherpa")]
    {
        let status_label_reload = status_label.clone();
        let progress_bar_reload = progress_bar.clone();
        let enable_row_reload = transcript.enable_row.clone();
        // Config handle for the deferred-persistence path. We write
        // KEY_SHERPA_MODEL only after InitEvent::Ready fires so a
        // failed recognizer swap can't leave a broken model idx in
        // config that would wedge next startup's init_sherpa_host.
        let config_for_reload_persist = std::sync::Arc::clone(config);
        transcript.model_row.connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            let Some(new_model) = sdr_transcription::SherpaModel::ALL.get(idx).copied() else {
                return;
            };

            tracing::info!(?new_model, "user changed model — triggering runtime reload");

            // Disable BOTH rows while the reload is in flight:
            // - model_row so the user can't queue up multiple reloads
            //   via rapid switching
            // - enable_row so the user can't start/stop transcription
            //   on top of an in-flight recognizer swap. Without this,
            //   the stop-path teardown would re-enable model_row before
            //   the reload finishes, reopening the queued-reload window
            //   this block is closing.
            // Both are re-enabled from the timeout closure on Ready /
            // Failed / channel disconnect.
            row.set_sensitive(false);
            enable_row_reload.set_sensitive(false);
            let model_row_reload_weak = row.downgrade();
            let enable_row_reload_weak = enable_row_reload.downgrade();

            // Show the status area.
            status_label_reload.set_text(&format!("Reloading {}...", new_model.label()));
            status_label_reload.set_css_classes(&["dim-label"]);
            status_label_reload.set_visible(true);
            progress_bar_reload.set_fraction(0.0);
            progress_bar_reload.set_visible(true);

            let event_rx = sdr_transcription::reload_sherpa_host(new_model);

            // Drain progress events on the main thread via a periodic timeout.
            let status_weak = status_label_reload.downgrade();
            let progress_weak = progress_bar_reload.downgrade();
            let mut current_component: String = new_model.label().to_owned();
            // Capture an Arc clone + the new idx for the deferred
            // persistence path — written to config on Ready, dropped
            // silently on Failed/Disconnected.
            let config_for_this_reload = std::sync::Arc::clone(&config_for_reload_persist);
            let persist_idx = idx;
            glib::timeout_add_local(Duration::from_millis(100), move || {
                let Some(status) = status_weak.upgrade() else {
                    // Widgets are gone (window closing); model row is too,
                    // so no need to re-enable it.
                    return glib::ControlFlow::Break;
                };
                let Some(progress) = progress_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };

                loop {
                    match event_rx.try_recv() {
                        Ok(sdr_transcription::InitEvent::DownloadStart { component }) => {
                            component.clone_into(&mut current_component);
                            status.set_text(&format!("Downloading {component}..."));
                            progress.set_fraction(0.0);
                        }
                        Ok(sdr_transcription::InitEvent::DownloadProgress { pct }) => {
                            status.set_text(&format!("Downloading {current_component}... {pct}%"));
                            progress.set_fraction(f64::from(pct) / 100.0);
                        }
                        Ok(sdr_transcription::InitEvent::Extracting { component }) => {
                            component.clone_into(&mut current_component);
                            status.set_text(&format!("Extracting {component}..."));
                        }
                        Ok(sdr_transcription::InitEvent::CreatingRecognizer) => {
                            status.set_text("Creating recognizer...");
                            progress.set_visible(false);
                        }
                        Ok(sdr_transcription::InitEvent::Ready) => {
                            tracing::info!("sherpa host reload complete");
                            status.set_text("");
                            status.set_visible(false);
                            progress.set_visible(false);
                            if let Some(model_row) = model_row_reload_weak.upgrade() {
                                model_row.set_sensitive(true);
                            }
                            if let Some(enable_row) = enable_row_reload_weak.upgrade() {
                                enable_row.set_sensitive(true);
                            }
                            // Deferred persistence: the recognizer swap
                            // succeeded, so it's now safe to save the
                            // new selection to config. If this Ready
                            // arm never fires (reload failed), config
                            // keeps the previous model idx and next
                            // startup gets a known-working recognizer.
                            config_for_this_reload.write(|v| {
                                v[crate::sidebar::transcript_panel::KEY_SHERPA_MODEL] =
                                    serde_json::json!(persist_idx);
                            });
                            return glib::ControlFlow::Break;
                        }
                        Ok(sdr_transcription::InitEvent::Failed { message }) => {
                            tracing::warn!(%message, "sherpa host reload failed");
                            status.set_text(&format!("Reload failed: {message}"));
                            status.set_css_classes(&["error"]);
                            status.set_visible(true);
                            progress.set_visible(false);
                            if let Some(model_row) = model_row_reload_weak.upgrade() {
                                model_row.set_sensitive(true);
                            }
                            if let Some(enable_row) = enable_row_reload_weak.upgrade() {
                                enable_row.set_sensitive(true);
                            }
                            return glib::ControlFlow::Break;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            // Worker dropped its sender without sending Ready
                            // or Failed — unusual but don't strand the UI in
                            // a "Reloading..." state. Surface the disconnect
                            // as an error and re-enable the controls so the
                            // user can try a different model.
                            tracing::warn!(
                                "sherpa host reload event channel disconnected unexpectedly"
                            );
                            status.set_text("Reload failed: recognizer worker disconnected");
                            status.set_css_classes(&["error"]);
                            status.set_visible(true);
                            progress.set_visible(false);
                            if let Some(model_row) = model_row_reload_weak.upgrade() {
                                model_row.set_sensitive(true);
                            }
                            if let Some(enable_row) = enable_row_reload_weak.upgrade() {
                                enable_row.set_sensitive(true);
                            }
                            return glib::ControlFlow::Break;
                        }
                    }
                }
                glib::ControlFlow::Continue
            });
        });
    }

    transcript.enable_row.connect_active_notify(move |row| {
        if row.is_active() {
            // Read selected model from dropdown.
            // Lock model and tuning controls while transcription is active.
            model_row.set_sensitive(false);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
            // All settings lock during a session for mid-session fault
            // tolerance — walks back PR 4's earlier display_mode_row
            // exception. User stops, changes, starts.
            #[cfg(feature = "sherpa")]
            display_mode_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            vad_threshold_row.set_sensitive(false);

            let model_idx = model_row.selected() as usize;

            // Read tuning slider values.
            #[cfg(feature = "whisper")]
            #[allow(clippy::cast_possible_truncation)]
            let silence_threshold = silence_row.value() as f32;
            // Sherpa builds: silence_threshold is unused by SherpaBackend
            // (see build_recognizer_config doc comment). Pass a sentinel.
            #[cfg(feature = "sherpa")]
            let silence_threshold: f32 = 0.0;
            #[allow(clippy::cast_possible_truncation)]
            let noise_gate_ratio = noise_gate_row.value() as f32;

            // Build BackendConfig — Whisper and Sherpa are mutually exclusive
            // cargo features, so exactly one variant is compiled in.
            #[cfg(feature = "whisper")]
            let model = {
                let whisper_model = sdr_transcription::WhisperModel::ALL
                    .get(model_idx)
                    .copied()
                    .unwrap_or(sdr_transcription::WhisperModel::TinyEn);
                sdr_transcription::ModelChoice::Whisper(whisper_model)
            };
            #[cfg(feature = "sherpa")]
            let model = {
                let sherpa_model = sdr_transcription::SherpaModel::ALL
                    .get(model_idx)
                    .copied()
                    .unwrap_or(sdr_transcription::SherpaModel::StreamingZipformerEn);
                sdr_transcription::ModelChoice::Sherpa(sherpa_model)
            };

            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation)]
            let vad_threshold = vad_threshold_row.value() as f32;
            // Whisper builds compile the field but ignore it (no Silero VAD).
            #[cfg(feature = "whisper")]
            let vad_threshold: f32 = sdr_transcription::VAD_THRESHOLD_DEFAULT;

            let config = sdr_transcription::BackendConfig {
                model,
                silence_threshold,
                noise_gate_ratio,
                vad_threshold,
                // Task 14 replaces this hardcoded default with a read
                // from the auto_break_row toggle. For now, default to
                // Vad so Tasks 7-13 can build the workspace.
                segmentation_mode: sdr_transcription::SegmentationMode::Vad,
            };

            // Scope the borrow so it's dropped before any potential re-entry
            // from row.set_active(false) on error.
            let start_result = engine_clone.borrow_mut().start(config);
            match start_result {
                Ok(event_rx) => {
                    if let Some(audio_tx) = engine_clone.borrow().audio_sender() {
                        state_clone
                            .send_dsp(crate::messages::UiToDsp::EnableTranscription(audio_tx));
                    }

                    status_label.set_text("Starting...");
                    status_label.set_visible(true);

                    // Weak refs for the entire timeout source — see the
                    // weak-ref decl block at the top of connect_transcript_panel
                    // for the rationale (don't keep widgets alive past their
                    // UI lifetime through the glib timeout source).
                    let status_weak = status_label.downgrade();
                    let progress_weak = progress_bar.downgrade();
                    let tv_weak = text_view.downgrade();
                    let enable_row_weak = enable_row_weak.clone();
                    let model_row_weak = model_row_weak.clone();
                    #[cfg(feature = "whisper")]
                    let silence_row_weak = silence_row_weak.clone();
                    let noise_gate_row_weak = noise_gate_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let display_mode_row_weak = display_mode_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let vad_threshold_row_weak = vad_threshold_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let live_line_weak = live_line_weak.clone();

                    glib::timeout_add_local(Duration::from_millis(100), move || {
                        // Upgrade once per tick. If any widget has been
                        // dropped (e.g. window closed), stop the timeout
                        // immediately so we don't resurrect dead UI.
                        let Some(status) = status_weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };
                        let Some(progress) = progress_weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };
                        let Some(tv) = tv_weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };

                        loop {
                            match event_rx.try_recv() {
                                Ok(event) => match event {
                                    TranscriptionEvent::Downloading { progress_pct } => {
                                        status.set_text(&format!(
                                            "Downloading model ({progress_pct}%)..."
                                        ));
                                        status.set_visible(true);
                                        progress.set_fraction(f64::from(progress_pct) / 100.0);
                                        progress.set_visible(true);
                                    }
                                    TranscriptionEvent::Ready => {
                                        status.set_text("Listening...");
                                        status.set_css_classes(&["success"]);
                                        progress.set_visible(false);
                                    }
                                    TranscriptionEvent::Partial { text } => {
                                        #[cfg(feature = "sherpa")]
                                        {
                                            // Belt-and-suspenders: only paint
                                            // the live line if (a) the current
                                            // model actually supports partials
                                            // and (b) display mode is Live.
                                            //
                                            // (a) defends against a future bug
                                            // where an offline model accidentally
                                            // emits a Partial event — today the
                                            // offline session loop never does,
                                            // but the UI shouldn't trust that.
                                            // Without this check, italics would
                                            // appear on Moonshine/Parakeet on
                                            // any spurious Partial.
                                            //
                                            // (b) honors the user's display-mode
                                            // preference for partial-emitting
                                            // models. Re-read on every event so
                                            // mid-session toggle takes effect.
                                            let model_supports_partials = model_row_weak
                                                .upgrade()
                                                .is_some_and(|row| {
                                                    let idx = row.selected() as usize;
                                                    sdr_transcription::SherpaModel::ALL
                                                        .get(idx)
                                                        .copied()
                                                        .is_some_and(
                                                            sdr_transcription::SherpaModel::supports_partials,
                                                        )
                                                });
                                            let show_live = model_supports_partials
                                                && display_mode_row_weak.upgrade().is_some_and(
                                                    |row| row.selected() != DISPLAY_MODE_FINAL_IDX,
                                                );
                                            if show_live
                                                && let Some(label) = live_line_weak.upgrade()
                                            {
                                                label.set_text(&text);
                                                label.set_visible(true);
                                            }
                                            // Privacy: never log the raw text.
                                            tracing::debug!(
                                                target: "transcription",
                                                partial_chars = text.chars().count(),
                                                "sherpa partial received"
                                            );
                                        }
                                        #[cfg(not(feature = "sherpa"))]
                                        {
                                            // Whisper never emits Partial, but
                                            // the enum variant is compiled in.
                                            // Defensive no-op.
                                            let _ = text;
                                        }
                                    }
                                    TranscriptionEvent::Text { timestamp, text } => {
                                        let buf = tv.buffer();
                                        let mut end = buf.end_iter();
                                        buf.insert(&mut end, &format!("[{timestamp}] {text}\n"));
                                        let mark = buf.create_mark(None, &buf.end_iter(), false);
                                        tv.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
                                        buf.delete_mark(&mark);

                                        // An utterance committed — the live
                                        // line is now stale. Clear and hide
                                        // it so the next Partial starts fresh.
                                        #[cfg(feature = "sherpa")]
                                        if let Some(label) = live_line_weak.upgrade() {
                                            label.set_text("");
                                            label.set_visible(false);
                                        }
                                    }
                                    TranscriptionEvent::Error(msg) => {
                                        // Fatal — backend has exited.
                                        // Mirror the synchronous start()
                                        // failure teardown so the UI
                                        // isn't left locked.
                                        unlock_transcription_session_rows(
                                            &model_row_weak,
                                            #[cfg(feature = "whisper")]
                                            &silence_row_weak,
                                            &noise_gate_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &display_mode_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &vad_threshold_row_weak,
                                        );
                                        if let Some(enable) = enable_row_weak.upgrade() {
                                            enable.set_active(false);
                                        }
                                        status.set_text(&msg);
                                        status.set_css_classes(&["error"]);
                                        status.set_visible(true);
                                        progress.set_visible(false);
                                        // Clear any stale partial so it
                                        // doesn't linger into the next session.
                                        #[cfg(feature = "sherpa")]
                                        if let Some(label) = live_line_weak.upgrade() {
                                            label.set_text("");
                                            label.set_visible(false);
                                        }
                                        return glib::ControlFlow::Break;
                                    }
                                },
                                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    // Distinguish a normal user-initiated stop
                                    // from a spontaneous backend death:
                                    //
                                    // - User stop: the off branch of
                                    //   enable_row.connect_active_notify already
                                    //   ran (it dropped audio_tx, which is what
                                    //   caused the worker to exit and drop
                                    //   event_tx, which we're now seeing as
                                    //   Disconnected). The toggle is already
                                    //   inactive and all the rows have been
                                    //   re-enabled. Nothing to do here — the
                                    //   off branch did the cleanup. Without
                                    //   this check the disconnect arm overwrote
                                    //   the off branch's clean state with a
                                    //   spurious "Transcription stopped
                                    //   unexpectedly" error message on every
                                    //   normal stop.
                                    //
                                    // - Spontaneous death: the worker dropped
                                    //   event_tx without the user clicking
                                    //   anything. The toggle is still active.
                                    //   Mirror the Error arm's teardown so the
                                    //   UI doesn't strand the user with locked
                                    //   controls and a stale "Listening..."
                                    //   status.
                                    let was_user_stop =
                                        enable_row_weak.upgrade().is_none_or(|e| !e.is_active());

                                    if was_user_stop {
                                        tracing::debug!(
                                            "transcription event channel closed (user stop)"
                                        );
                                        return glib::ControlFlow::Break;
                                    }

                                    tracing::warn!(
                                        "transcription event channel disconnected unexpectedly"
                                    );
                                    unlock_transcription_session_rows(
                                        &model_row_weak,
                                        #[cfg(feature = "whisper")]
                                        &silence_row_weak,
                                        &noise_gate_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &display_mode_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &vad_threshold_row_weak,
                                    );
                                    if let Some(enable) = enable_row_weak.upgrade() {
                                        enable.set_active(false);
                                    }
                                    status.set_text("Transcription stopped unexpectedly");
                                    status.set_css_classes(&["error"]);
                                    status.set_visible(true);
                                    progress.set_visible(false);
                                    #[cfg(feature = "sherpa")]
                                    if let Some(label) = live_line_weak.upgrade() {
                                        label.set_text("");
                                        label.set_visible(false);
                                    }
                                    return glib::ControlFlow::Break;
                                }
                            }
                        }
                        glib::ControlFlow::Continue
                    });
                }
                Err(e) => {
                    tracing::warn!("failed to start transcription: {e}");
                    unlock_transcription_session_rows(
                        &model_row.downgrade(),
                        #[cfg(feature = "whisper")]
                        &silence_row.downgrade(),
                        &noise_gate_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &display_mode_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &vad_threshold_row.downgrade(),
                    );
                    // Reset the toggle FIRST (the else branch clears
                    // status_label as part of its normal teardown), then
                    // set the error text so the user actually sees it.
                    // Otherwise the failure is silent — only in stderr.
                    row.set_active(false);
                    status_label.set_text(&e.to_string());
                    status_label.set_css_classes(&["error"]);
                    status_label.set_visible(true);
                    progress_bar.set_visible(false);
                }
            }
        } else {
            unlock_transcription_session_rows(
                &model_row.downgrade(),
                #[cfg(feature = "whisper")]
                &silence_row.downgrade(),
                &noise_gate_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &display_mode_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &vad_threshold_row.downgrade(),
            );
            state_clone.send_dsp(crate::messages::UiToDsp::DisableTranscription);
            engine_clone.borrow_mut().shutdown_nonblocking();
            status_label.set_text("");
            status_label.set_visible(false);
            progress_bar.set_visible(false);
            // Clear any stale partial on stop so the previous session's
            // last in-progress text doesn't linger on screen.
            #[cfg(feature = "sherpa")]
            {
                live_line_label.set_text("");
                live_line_label.set_visible(false);
            }
        }
    });

    engine
}

/// Register application-level actions (Preferences, About, Quit).
fn setup_app_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    rr_button: &gtk4::Button,
) {
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

    // Preferences action
    let prefs_action = gio::SimpleAction::new("preferences", None);
    let config_for_prefs = std::sync::Arc::clone(config);
    let rr_button_prefs = rr_button.clone();
    prefs_action.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            let prefs_window =
                crate::preferences::build_preferences_window(&window, &config_for_prefs);
            // Update RR button visibility when preferences window closes
            let rr_btn = rr_button_prefs.clone();
            prefs_window.connect_close_request(move |_| {
                rr_btn.set_visible(crate::preferences::accounts_page::has_rr_credentials());
                glib::Propagation::Proceed
            });
            prefs_window.present();
        }
    ));
    app.add_action(&prefs_action);
    app.set_accels_for_action("app.preferences", &["<Ctrl>comma"]);

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

/// Generate a timestamped recording file path.
///
/// Creates the recording directory if it doesn't exist.
/// Returns a path like `~/sdr-recordings/audio-2026-04-08-173001.wav`.
fn recording_path(prefix: &str) -> std::path::PathBuf {
    let base = glib::home_dir().join(RECORDING_DIR_NAME);
    if let Err(e) = std::fs::create_dir_all(&base) {
        tracing::warn!("failed to create recording directory: {e}");
    }
    let now = glib::DateTime::now_local();
    let timestamp = now
        .and_then(|dt| dt.format("%Y-%m-%d-%H%M%S"))
        .map_or_else(|_| "unknown".to_string(), |s| s.to_string());
    base.join(format!("{prefix}-{timestamp}.wav"))
}
