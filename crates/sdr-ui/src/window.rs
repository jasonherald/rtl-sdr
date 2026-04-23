//! Main window construction — header bar, split view, breakpoints, DSP bridge.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_core::Engine;
use sdr_pipeline::iq_frontend::FftWindow;
use sdr_radio::DeemphasisMode;
use sdr_rtltcp_discovery::{
    AdvertiseOptions, Advertiser, Browser, DiscoveredServer, DiscoveryEvent, TxtRecord,
    local_hostname,
};
use sdr_server_rtltcp::{InitialDeviceState, Server, ServerConfig};
use sdr_source_rtlsdr::SAMPLE_RATES;

use crate::header;
use crate::header::demod_selector;
use crate::messages::{DspToUi, SourceType, UiToDsp};
use crate::shortcuts;
use crate::sidebar;
use crate::sidebar::SidebarPanels;
use crate::sidebar::source_panel::{
    DEVICE_FILE, DEVICE_NETWORK, DEVICE_RTLSDR, DEVICE_RTLTCP, NETWORK_PROTOCOL_TCPCLIENT_IDX,
    NETWORK_PROTOCOL_UDP_IDX,
};
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

/// Slide-in/out duration for right-side flyouts (transcript,
/// bookmarks) in milliseconds. Centralized so the two
/// revealers stay in lockstep — drifting values would make
/// one panel feel snappier than the other when the user
/// toggles between them.
const RIGHT_FLYOUT_TRANSITION_MS: u32 = 200;

/// FFT sizes — re-exported from display panel (single source of truth).
use crate::sidebar::display_panel::FFT_SIZES;
#[cfg(feature = "sherpa")]
use crate::sidebar::transcript_panel::DISPLAY_MODE_FINAL_IDX;

/// Decimation factors available in the source panel dropdown (must match panel order).
const DECIMATION_FACTORS: &[u32] = &[1, 2, 4, 8, 16];

/// Interval in milliseconds for polling the DSP→UI channel.
const DSP_POLL_INTERVAL_MS: u64 = 16;

/// Toast display time (seconds) for scanner "force-disable" notices.
const SCANNER_TOAST_TIMEOUT_SECS: u32 = 3;

/// Shared "kill the scanner on a manual tune" hook. Built once in
/// `build_window` and cloned into every manual-change handler
/// (frequency selector, demod dropdown, bandwidth row, bookmark
/// recall / preset selection). Calling [`Self::trigger`] is a
/// no-op when the scanner is already off, so wiring it into a
/// handler that fires during programmatic widget updates is
/// cheap and idempotent.
///
/// Holds `glib::WeakRef`s rather than owned widget clones —
/// each clone of this helper is captured by a signal handler
/// that lives on a widget in the window, so a strong ref chain
/// (handler → this helper → widget → handler) would keep the
/// window alive after teardown. Upgrade-or-early-return in
/// `trigger` handles the post-teardown case.
struct ScannerForceDisable {
    master_switch: glib::WeakRef<gtk4::Switch>,
    toast_overlay: glib::WeakRef<adw::ToastOverlay>,
}

impl ScannerForceDisable {
    /// Force the scanner off and toast the user about why. No-op
    /// when the master switch has been dropped (post-teardown)
    /// or when the scanner is already off. Calls `set_active(false)`
    /// on the master switch — the switch's `connect_active_notify`
    /// handler dispatches `SetScannerEnabled(false)` to the
    /// engine, so no explicit DSP send is needed here.
    fn trigger(&self, reason: &str) {
        let Some(master_switch) = self.master_switch.upgrade() else {
            return;
        };
        if !master_switch.is_active() {
            return;
        }
        master_switch.set_active(false);
        if let Some(overlay) = self.toast_overlay.upgrade() {
            let toast = adw::Toast::builder()
                .title(format!("Scanner stopped — {reason}"))
                .timeout(SCANNER_TOAST_TIMEOUT_SECS)
                .build();
            overlay.add_toast(toast);
        }
    }
}

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
    // `Engine::new` can fail if the OS rejects `std::thread::Builder::spawn`
    // (rare, but possible under resource pressure). Earlier drafts of this
    // function used `.expect()` and panicked, which CodeRabbit correctly
    // flagged — panicking from inside a GTK activation handler produces
    // an unclean shutdown and no user-visible error. We now log the error
    // and call `app.quit()` so the process shuts down cleanly; subsequent
    // activations can retry. The window is never presented in this
    // failure path, so the user sees the app briefly register on the
    // taskbar and then exit — not ideal UX, but the root cause is a
    // host-OS resource issue the user will see in the tracing logs.
    let engine = match Engine::new(config.path().to_path_buf()) {
        Ok(e) => Rc::new(e),
        Err(err) => {
            tracing::error!(error = %err, "failed to spawn DSP engine — aborting window build");
            app.quit();
            return;
        }
    };
    let ui_tx = engine.command_sender();
    let Some(dsp_rx) = engine.subscribe() else {
        // `Engine::subscribe` is a one-shot; a second caller would
        // get `None`. We're the first (and only) subscriber, so this
        // arm only fires if someone threads the engine through a
        // pre-subscribe hook in the future. Log, quit, return.
        tracing::error!(
            "Engine::subscribe returned None — another subscriber \
             already took the event receiver"
        );
        app.quit();
        return;
    };
    let fft_shared = engine.fft_buffer();

    // Shared application state with DSP sender.
    let state = AppState::new_shared(ui_tx);

    // --- Build UI ---
    let LayoutHandles {
        root: layout_root,
        left_split_view,
        right_split_view,
        left_activity_bar,
        right_activity_bar,
        left_stack,
        right_stack: _right_stack,
        panels,
        spectrum_handle: spectrum_handle_raw,
        status_bar,
        transcript_panel,
        bookmarks_revealer,
    } = build_layout(&state, config);
    let spectrum_handle = Rc::new(spectrum_handle_raw);
    let sidebar_toggle = build_sidebar_toggle(&left_split_view);
    let (
        header,
        play_button,
        demod_dropdown,
        freq_selector,
        screenshot_button,
        rr_button,
        favorites_handle,
    ) = build_header_bar(&sidebar_toggle, &state);

    // Bookmarks flyout toggle — packed `pack_end` first so it
    // ends up LEFT of the transcript toggle in the header's
    // right cluster. `pack_end` stacks right-to-left, so the
    // last `pack_end` call sits furthest from the right edge.
    // Keeping bookmarks near the far edge matches the visual
    // mapping "click the icon → panel slides in from directly
    // under it on the right."
    let bookmarks_toggle = gtk4::ToggleButton::builder()
        .icon_name("user-bookmarks-symbolic")
        .tooltip_text("Toggle bookmarks panel (Ctrl+B)")
        .build();
    // `tooltip_text` alone isn't reliably announced by screen
    // readers — set the accessible label explicitly, matching
    // the pattern used for the other icon-only controls in this
    // file (pinned servers menu, copy server address, etc.).
    bookmarks_toggle
        .update_property(&[gtk4::accessible::Property::Label("Toggle bookmarks panel")]);
    header.pack_end(&bookmarks_toggle);

    // Restore the flyout open/closed state saved at last shutdown.
    // Set the toggle's `active` property before wiring the handler
    // so the initial `set_active` doesn't feed a no-op write back
    // through `connect_toggled` — `connect_toggled` fires only on
    // changes, but explicitly wiring the handler after the initial
    // state is set keeps the "saved state → initial reveal" path
    // free of config round-trips.
    let bookmarks_initial_open = config.read(|v| {
        v.get(sidebar::bookmarks_panel::CONFIG_KEY_FLYOUT_OPEN)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    });
    bookmarks_toggle.set_active(bookmarks_initial_open);
    bookmarks_revealer.set_reveal_child(bookmarks_initial_open);

    let bookmarks_revealer_clone = bookmarks_revealer.clone();
    let bookmarks_config = std::sync::Arc::clone(config);
    bookmarks_toggle.connect_toggled(move |btn| {
        let open = btn.is_active();
        bookmarks_revealer_clone.set_reveal_child(open);
        bookmarks_config.write(|v| {
            v[sidebar::bookmarks_panel::CONFIG_KEY_FLYOUT_OPEN] = serde_json::json!(open);
        });
    });

    // Transcript toggle button in header bar — now drives the right
    // activity bar's transcript button (which in turn toggles the
    // right split view's show-sidebar). Keeping the header toggle
    // preserves muscle memory from the pre-activity-bar layout;
    // future sub-tickets may remove it as activity-bar-first UX
    // beds in.
    let transcript_button = gtk4::ToggleButton::builder()
        .icon_name("document-page-setup-symbolic")
        .tooltip_text("Toggle transcript panel (Ctrl+Shift+1)")
        .build();
    transcript_button
        .update_property(&[gtk4::accessible::Property::Label("Toggle transcript panel")]);
    header.pack_end(&transcript_button);

    // The right-side transcript is no longer an opaque revealer
    // that could stack over the bookmarks flyout — it lives in the
    // right split view's sidebar. Bookmarks still hang off the
    // content HBox in that split view, so the two right-side panels
    // no longer compete for the same space and the old mutual-
    // exclusion handler can be dropped.

    // --- Activity-bar wiring ---
    //
    // Left (multi-button): click on a NEW icon → deselect siblings,
    // switch stack, open panel. Click on the CURRENTLY-selected
    // icon → icon stays selected (design doc §4.2), panel toggles.
    // `:checked` CSS renders the accent tint automatically.
    if let Some(general_btn) = left_activity_bar.buttons.get("general") {
        general_btn.set_active(true);
    }
    wire_activity_bar_clicks(&left_activity_bar, &left_stack, &left_split_view, "general");

    // Header sidebar toggle ↔ left split view `show-sidebar` sync.
    // Without this, clicking the currently-selected activity icon to
    // collapse the panel leaves the header toggle stuck in `active`;
    // the user's next header click then sets `show-sidebar=false`
    // again (no-op) instead of reopening the panel. Mirrors the same
    // `connect_show_sidebar_notify` pattern used on the right side.
    let sidebar_toggle_weak = sidebar_toggle.downgrade();
    left_split_view.connect_show_sidebar_notify(move |sv| {
        if let Some(toggle) = sidebar_toggle_weak.upgrade()
            && toggle.is_active() != sv.shows_sidebar()
        {
            toggle.set_active(sv.shows_sidebar());
        }
    });

    // Right (single-button): the transcript toggle, the header
    // transcript button, and `right_split_view.show-sidebar` form a
    // tri-state that must stay in sync. Approach: each toggle button
    // writes its `active` state through to `show-sidebar`; the split
    // view's `notify::show-sidebar` reflects back into both buttons.
    // `set_active(x)` on an already-in-state-`x` button is a GTK
    // no-op (no `toggled` signal), so the two-way propagation is
    // naturally idempotent.
    if let Some(right_transcript_btn) = right_activity_bar.buttons.get("transcript") {
        let right_split_weak = right_split_view.downgrade();
        right_transcript_btn.connect_toggled(move |btn| {
            if let Some(sv) = right_split_weak.upgrade() {
                sv.set_show_sidebar(btn.is_active());
            }
        });

        let right_split_weak = right_split_view.downgrade();
        transcript_button.connect_toggled(move |btn| {
            if let Some(sv) = right_split_weak.upgrade() {
                sv.set_show_sidebar(btn.is_active());
            }
        });

        let right_btn_weak = right_transcript_btn.downgrade();
        let header_btn_weak = transcript_button.downgrade();
        right_split_view.connect_show_sidebar_notify(move |sv| {
            let visible = sv.shows_sidebar();
            if let Some(rb) = right_btn_weak.upgrade()
                && rb.is_active() != visible
            {
                rb.set_active(visible);
            }
            if let Some(hdr) = header_btn_weak.upgrade()
                && hdr.is_active() != visible
            {
                hdr.set_active(visible);
            }
        });

        // Initial alignment — panel is closed at launch, so both
        // buttons start inactive. Config-driven restoration comes
        // in sub-ticket #428.
        right_transcript_btn.set_active(false);
        transcript_button.set_active(false);
    }

    let toolbar_view = build_toolbar_view(&header, &layout_root);
    let breakpoint = build_breakpoint(&left_split_view, &right_split_view);

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
    let transcription_engine = connect_transcript_panel(
        &transcript_panel,
        &state,
        config,
        &panels.radio.squelch_enabled_row,
        &toast_overlay,
    );

    // On window close, signal the worker to stop without blocking.
    window.connect_close_request(move |_| {
        transcription_engine.borrow_mut().shutdown_nonblocking();
        glib::Propagation::Proceed
    });

    // --- Keyboard shortcuts ---
    shortcuts::setup_shortcuts(
        &window,
        &play_button,
        &sidebar_toggle,
        &bookmarks_toggle,
        &demod_dropdown,
        &panels.scanner.master_switch,
        &left_activity_bar,
        &right_activity_bar,
    );

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

    // Shared force-disable hook — cloned into every manual-change
    // handler so a user tune / demod switch / bandwidth tweak /
    // bookmark recall drops the scanner out of rotation. Rc so
    // each handler can hold an independent clone without fighting
    // over ownership; internals are cheap GObject refcount bumps.
    let scanner_force_disable = Rc::new(ScannerForceDisable {
        master_switch: panels.scanner.master_switch.downgrade(),
        toast_overlay: toast_overlay.downgrade(),
    });

    connect_sidebar_panels(
        &panels,
        &state,
        &spectrum_handle,
        &freq_selector,
        &demod_dropdown,
        &status_bar_demod,
        &toast_overlay,
        config,
        &favorites_handle,
        &scanner_force_disable,
    );

    // Seed the scanner with the persisted bookmark list on
    // startup. Scanner starts Idle so no retune happens, but
    // the channels are in place if the user flips F8 or the
    // master switch. Defaults come from config via the shared
    // projection helper — matches the on-mutation re-projection
    // path so initial-load and post-edit semantics are identical.
    sidebar::navigation_panel::project_and_push_scanner_channels(
        &panels.bookmarks.bookmarks.borrow(),
        &state,
        config,
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
        let bookmarks_for_rr = Rc::clone(&panels.bookmarks);
        let name_entry_for_rr = panels.navigation.name_entry.clone();

        rr_button.connect_clicked(move |btn| {
            let bookmarks_for_rr = Rc::clone(&bookmarks_for_rr);
            let name_entry_for_rr = name_entry_for_rr.clone();

            crate::radioreference::show_browse_dialog(btn, move || {
                // Reload bookmarks from disk and rebuild the flyout.
                // `BookmarksPanel::rebuild` keeps this call site on
                // the panel boundary rather than reaching through
                // the panel's individual `Rc` fields.
                *bookmarks_for_rr.bookmarks.borrow_mut() =
                    sidebar::navigation_panel::load_bookmarks();
                bookmarks_for_rr.rebuild_after_mutation(&name_entry_for_rr);
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
    let force_disable_freq = Rc::clone(&scanner_force_disable);
    freq_selector.connect_frequency_changed(move |freq| {
        tracing::debug!(frequency_hz = freq, "frequency changed");
        // Flip scanner off before dispatching the tune so the
        // engine receives the `SetScannerEnabled(false)` first —
        // the subsequent `Tune` then lands on an Idle scanner and
        // doesn't race a retune command.
        force_disable_freq.trigger("manual tune");
        #[allow(clippy::cast_precision_loss)]
        let freq_f64 = freq as f64;
        state_freq.center_frequency.set(freq_f64);
        state_freq.send_dsp(UiToDsp::Tune(freq_f64));
        status_bar_for_freq.update_frequency(freq_f64);
        spectrum_for_freq.set_center_frequency(freq_f64);
    });
    // Single demod-change handler: gate → force-disable → dispatch
    // → cosmetic UI updates. Order matters: force-disable must
    // reach the engine BEFORE SetDemodMode so the scanner isn't
    // still rotating when the new demod lands. Previously the
    // dispatch lived in build_header_bar and force-disable here,
    // which left a race because GTK fires handlers in
    // registration order.
    let status_bar_for_demod = Rc::clone(&status_bar_demod);
    let bw_row_for_demod = panels.radio.bandwidth_row.clone();
    let radio_for_demod = panels.radio.clone();
    let force_disable_demod = Rc::clone(&scanner_force_disable);
    let state_demod = Rc::clone(&state);
    demod_dropdown.connect_selected_notify(move |dd| {
        // DSP-origin guard — when the scanner's
        // ScannerActiveChannelChanged fan-out programmatically
        // changes the dropdown, skip EVERYTHING (dispatch and
        // force-disable and cosmetic updates are all paid for
        // by the scanner's own widget-sync code).
        if state_demod.suppress_demod_notify.get() {
            return;
        }
        let Some(mode) = demod_selector::index_to_demod_mode(dd.selected()) else {
            return;
        };
        // Stop scanner BEFORE queuing SetDemodMode so the engine
        // receives the commands in the right order.
        force_disable_demod.trigger("manual demod change");
        state_demod.demod_mode.set(mode);
        state_demod.send_dsp(UiToDsp::SetDemodMode(mode));
        tracing::debug!(?mode, "demod mode sent to DSP");
        // Cosmetic UI sync last.
        let label = header::demod_mode_label(mode);
        let bw = bw_row_for_demod.value();
        status_bar_for_demod.update_demod(label, bw);
        radio_for_demod.apply_demod_visibility(mode);
    });

    // --- Wire radio panel bandwidth changes to status bar ---
    let status_bar_for_bw = Rc::clone(&status_bar_demod);
    let state_for_bw = Rc::clone(&state);
    let radio_for_bw_reset = panels.radio.clone();
    let spectrum_for_bw_reset = Rc::clone(&spectrum_handle);
    panels.radio.bandwidth_row.connect_value_notify(move |row| {
        let mode = state_for_bw.demod_mode.get();
        let label = header::demod_mode_label(mode);
        status_bar_for_bw.update_demod(label, row.value());
        // Reset affordances track the spin-row value on EVERY
        // change — user-initiated edits AND DSP echoes. Lives
        // in this handler (not the `connect_radio_panel` one)
        // because that one short-circuits on the
        // `suppress_bandwidth_notify` flag and would miss VFO
        // drag echoes. Per issue #341.
        update_bandwidth_reset_sensitivity(&radio_for_bw_reset, &state_for_bw);
        update_vfo_reset_button_visibility(
            &radio_for_bw_reset,
            &spectrum_for_bw_reset,
            &state_for_bw,
        );
    });

    // Floating "Reset VFO" button on the spectrum — routes
    // through the DSP for both dispatches so the echoes
    // (`BandwidthChanged`, `VfoOffsetChanged`) drive the UI
    // reflection. No direct widget manipulation that would
    // skip the DSP / scanner-mutex / force-disable machinery.
    let state_for_vfo_reset = Rc::clone(&state);
    let force_disable_vfo_reset = Rc::clone(&scanner_force_disable);
    spectrum_handle.vfo_reset_button.connect_clicked(move |_| {
        // Reset is a manual change — stop the scanner first so a
        // retune on the user's cleaned-up channel doesn't race
        // with the reset dispatch (same contract every other
        // manual-change site in `build_window` obeys).
        force_disable_vfo_reset.trigger("manual VFO reset");
        let mode = state_for_vfo_reset.demod_mode.get();
        // If the mode default is unresolvable (unreachable for
        // any current variant), skip the bandwidth reset rather
        // than dispatching `SetBandwidth(0.0)`; the offset reset
        // still lands. Error already logged by the helper.
        match sdr_radio::demod::default_bandwidth_for_mode(mode) {
            Ok(default_bw) => {
                state_for_vfo_reset.send_dsp(UiToDsp::SetBandwidth(default_bw));
            }
            Err(e) => {
                tracing::warn!(
                    ?mode,
                    error = %e,
                    "default_bandwidth_for_mode failed on VFO reset — skipping bandwidth dispatch"
                );
            }
        }
        state_for_vfo_reset.send_dsp(UiToDsp::SetVfoOffset(0.0));
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
    let radio_panel_for_dsp = panels.radio.clone();
    let scanner_panel_for_dsp = panels.scanner.clone();
    let freq_selector_for_dsp = freq_selector.clone();
    let demod_dropdown_for_dsp = demod_dropdown.clone();
    // Just the three widgets the rtl_tcp status renderer touches —
    // cloning the whole SourcePanel would be a lot of refcount
    // traffic for one signal handler. Weak refs, upgraded per
    // message, keep the closure from keeping widgets alive past
    // window close (same pattern as `ServerStatusWidgetsWeak`).
    let rtl_tcp_status_row_weak = panels.source.rtl_tcp_status_row.downgrade();
    let rtl_tcp_disconnect_button_weak = panels.source.rtl_tcp_disconnect_button.downgrade();
    let rtl_tcp_retry_button_weak = panels.source.rtl_tcp_retry_button.downgrade();
    let rtl_tcp_role_row_weak = panels.source.rtl_tcp_role_row.downgrade();
    let rtl_tcp_auth_key_row_weak = panels.source.rtl_tcp_auth_key_row.downgrade();
    let rtl_tcp_hostname_row_weak = panels.source.hostname_row.downgrade();
    let rtl_tcp_port_row_weak = panels.source.port_row.downgrade();
    // Weak refs to the two persistent ControllerBusy toasts, so
    // clicking either action dismisses BOTH (pre-`CodeRabbit`
    // round 11 on PR #408 only the clicked toast dismissed and
    // the sibling stale-action could later rebuild the source
    // against a healthy session), and so a transition away from
    // ControllerBusy (e.g. the controller slot freed up and we
    // reached `Connected` directly) sweeps the live pair. `Rc<
    // RefCell<Vec<..>>>` lives at the DSP-poll closure scope so
    // it persists across ticks but drops with the timeout
    // source. `glib::WeakRef` inside the Vec so a dropped toast
    // doesn't keep a strong reference — the vec is just a
    // "remember to dismiss these on state change" ledger.
    let pending_controller_busy_toasts: Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>> =
        Rc::new(RefCell::new(Vec::new()));
    // Network audio sink status row — same weak-ref pattern as
    // the rtl_tcp status row above so a window close can't keep
    // the row alive past its useful life. Per issue #247.
    let network_sink_status_row_weak = panels.audio.network_status_row.downgrade();
    let transcription_enable_for_dsp = transcript_panel.enable_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_row_for_dsp = transcript_panel.auto_break_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_min_open_row_for_dsp = transcript_panel.auto_break_min_open_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_tail_row_for_dsp = transcript_panel.auto_break_tail_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_min_segment_row_for_dsp = transcript_panel.auto_break_min_segment_row.clone();
    #[cfg(feature = "sherpa")]
    let model_row_for_dsp = transcript_panel.model_row.clone();
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
                        &radio_panel_for_dsp,
                        &scanner_panel_for_dsp,
                        &freq_selector_for_dsp,
                        &demod_dropdown_for_dsp,
                        &rtl_tcp_status_row_weak,
                        &rtl_tcp_disconnect_button_weak,
                        &rtl_tcp_retry_button_weak,
                        &rtl_tcp_role_row_weak,
                        &rtl_tcp_auth_key_row_weak,
                        &rtl_tcp_hostname_row_weak,
                        &rtl_tcp_port_row_weak,
                        &pending_controller_busy_toasts,
                        &network_sink_status_row_weak,
                        &transcription_enable_for_dsp,
                        #[cfg(feature = "sherpa")]
                        &auto_break_row_for_dsp,
                        #[cfg(feature = "sherpa")]
                        &auto_break_min_open_row_for_dsp,
                        #[cfg(feature = "sherpa")]
                        &auto_break_tail_row_for_dsp,
                        #[cfg(feature = "sherpa")]
                        &auto_break_min_segment_row_for_dsp,
                        #[cfg(feature = "sherpa")]
                        &model_row_for_dsp,
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

/// Clear the scanner's active-channel UI surfaces back to the
/// idle look: empty cache, placeholder label, hidden lockout
/// button. Shared between the four events that mean "scanner
/// isn't parked on a channel anymore":
///   - `ScannerActiveChannelChanged { key: None }` (explicit
///     idle edge)
///   - `ScannerEmptyRotation` (rotation exhausted)
///   - `ScannerMutexStopped::ScannerStoppedFor{Recording,Transcription}`
///     (mutex fired)
///
/// Without the helper, those stop paths would depend on the
/// engine sending a separate `ActiveChannelChanged { key: None }`
/// event in the same tick — which it does today, but relying on
/// that ordering across four sites was brittle.
fn clear_scanner_active_channel_ui(
    scanner_panel: &sidebar::scanner_panel::ScannerPanel,
    state: &AppState,
) {
    *state.scanner_active_key.borrow_mut() = None;
    scanner_panel.active_channel_label.set_text("Active: —");
    scanner_panel.lockout_button.set_visible(false);
}

/// Handle a single message from the DSP thread.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    radio_panel: &sidebar::radio_panel::RadioPanel,
    scanner_panel: &sidebar::scanner_panel::ScannerPanel,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    rtl_tcp_status_row_weak: &glib::WeakRef<adw::ActionRow>,
    rtl_tcp_disconnect_button_weak: &glib::WeakRef<gtk4::Button>,
    rtl_tcp_retry_button_weak: &glib::WeakRef<gtk4::Button>,
    rtl_tcp_role_row_weak: &glib::WeakRef<adw::ComboRow>,
    rtl_tcp_auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    rtl_tcp_hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    rtl_tcp_port_row_weak: &glib::WeakRef<adw::SpinRow>,
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
    network_sink_status_row_weak: &glib::WeakRef<adw::ActionRow>,
    transcription_enable_row: &adw::SwitchRow,
    #[cfg(feature = "sherpa")] auto_break_row: &adw::SwitchRow,
    #[cfg(feature = "sherpa")] auto_break_min_open_row: &adw::SpinRow,
    #[cfg(feature = "sherpa")] auto_break_tail_row: &adw::SpinRow,
    #[cfg(feature = "sherpa")] auto_break_min_segment_row: &adw::SpinRow,
    #[cfg(feature = "sherpa")] model_row: &adw::ComboRow,
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
        DspToUi::DemodModeChanged(new_mode) => {
            tracing::info!(?new_mode, "demod mode changed");

            // Re-run Auto Break row visibility rules with the new mode.
            // The row is only visible when the current mode is NFM AND an
            // offline sherpa model is selected. Task 13 installed the
            // "offline model" check as a signal-chain reaction to model_row
            // changes; this layer adds the NFM gate on top, fired by the
            // demod-mode-change event.
            #[cfg(feature = "sherpa")]
            {
                let is_nfm = new_mode == sdr_types::DemodMode::Nfm;
                let model_idx = model_row.selected() as usize;
                let selected_is_offline = sdr_transcription::SherpaModel::ALL
                    .get(model_idx)
                    .copied()
                    .is_some_and(|m| !m.supports_partials());
                let toggle_visible = is_nfm && selected_is_offline;
                auto_break_row.set_visible(toggle_visible);
                // Timing sliders follow the toggle's visibility AND
                // the "Auto Break is actually ON" mutex. If the toggle
                // itself just got hidden (switched out of NFM), the
                // sliders must hide too.
                let sliders_visible = toggle_visible && auto_break_row.is_active();
                auto_break_min_open_row.set_visible(sliders_visible);
                auto_break_tail_row.set_visible(sliders_visible);
                auto_break_min_segment_row.set_visible(sliders_visible);
            }

            // If a transcription session is currently active, stop it and
            // surface a toast. The band has conceptually changed, so the
            // session must restart from scratch — session config (model,
            // VAD threshold, Auto Break toggle) is preserved; the user
            // clicks Start to resume on the new band.
            if transcription_enable_row.is_active() {
                tracing::info!("stopping active transcription due to demod mode change");
                // Toggling enable_row off triggers the existing stop path
                // (connect_active_notify handler wired elsewhere in window.rs).
                transcription_enable_row.set_active(false);

                if let Some(overlay) = toast_overlay_weak.upgrade() {
                    let toast = adw::Toast::new(
                        "Transcription stopped — demod mode changed. Press Start to resume.",
                    );
                    overlay.add_toast(toast);
                }
            }

            // Mode change shifts the default bandwidth — refresh
            // both the per-field sensitivity AND the floating
            // button's visibility so they track the new mode's
            // default. Per issue #341.
            update_bandwidth_reset_sensitivity(radio_panel, state);
            update_vfo_reset_button_visibility(radio_panel, spectrum_handle, state);
        }
        DspToUi::BandwidthChanged(bw) => {
            // DSP-originated bandwidth change (typically a VFO drag
            // on the spectrum). Reflect it in the Radio panel's
            // spin row so the numeric readout stays in lockstep
            // with the active filter width.
            //
            // Set the suppress flag around the `set_value` call so
            // the spin's `connect_value_notify` handler knows this
            // update is DSP-originated and doesn't dispatch a
            // redundant `UiToDsp::SetBandwidth` back to the
            // controller. Restored after the set_value returns so
            // user-originated edits from the next event loop tick
            // are dispatched normally.
            state.suppress_bandwidth_notify.set(true);
            radio_panel.bandwidth_row.set_value(bw);
            state.suppress_bandwidth_notify.set(false);
        }
        DspToUi::VfoOffsetChanged(offset) => {
            // DSP-originated VFO offset change — typically a
            // "reset VFO offset" button that dispatched
            // `SetVfoOffset(0)`. Update the overlay + frequency
            // display so the UI reflects the new offset without
            // the caller having to optimistically guess locally.
            // Per issue #341.
            spectrum_handle.set_vfo_offset(offset);
            let tuned = state.center_frequency.get() + offset;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let tuned_u64 = tuned.max(0.0) as u64;
            freq_selector.set_frequency(tuned_u64);
            status_bar.update_frequency(tuned);
            // Offset change is one of the two inputs to the
            // floating reset button's visibility — refresh it so
            // clicking reset hides the button and a subsequent
            // user drag re-shows it. Per issue #341.
            update_vfo_reset_button_visibility(radio_panel, spectrum_handle, state);
        }
        DspToUi::CtcssSustainedChanged(sustained) => {
            tracing::debug!(sustained, "CTCSS sustained-gate edge");
            radio_panel.set_ctcss_sustained(sustained);
        }
        DspToUi::VoiceSquelchOpenChanged(open) => {
            tracing::debug!(open, "voice squelch gate edge");
            radio_panel.set_voice_squelch_open(open);
        }
        DspToUi::RtlTcpConnectionState(conn_state) => {
            tracing::debug!(?conn_state, "rtl_tcp connection state");
            // Upgrade all three weak refs atomically; any missing
            // widget means the window's gone, so we drop the event
            // rather than render a ghost status row.
            if let (Some(status_row), Some(disconnect), Some(retry)) = (
                rtl_tcp_status_row_weak.upgrade(),
                rtl_tcp_disconnect_button_weak.upgrade(),
                rtl_tcp_retry_button_weak.upgrade(),
            ) {
                apply_rtl_tcp_connection_state(&status_row, &disconnect, &retry, &conn_state);
            }
            // #396 toast surface: fire toast + manipulate widgets
            // on the EDGE of every transition into a role-denial
            // terminal state (or into Connected from one of those
            // states, for the keyring save path). Edge detection
            // uses a u8-discriminant cell on AppState so we don't
            // re-fire the toast on every same-state republish.
            let prev_disc = state.last_rtl_tcp_state_disc.get();
            let now_disc = crate::state::rtl_tcp_state_discriminant(&conn_state);
            if prev_disc != now_disc {
                state.last_rtl_tcp_state_disc.set(now_disc);
                handle_rtl_tcp_state_toast(
                    &conn_state,
                    prev_disc,
                    state,
                    toast_overlay_weak,
                    rtl_tcp_role_row_weak,
                    rtl_tcp_auth_key_row_weak,
                    rtl_tcp_hostname_row_weak,
                    rtl_tcp_port_row_weak,
                    pending_controller_busy_toasts,
                );
            }
            // Status-bar role badge (#396) — show the role the
            // SERVER admitted us into, never the role the user
            // requested. Pre-CodeRabbit round 1 on PR #408 the
            // badge was derived from the role-picker selection,
            // which could silently mis-label sessions where the
            // server admitted a different role (e.g. a pre-#392
            // RTLX server that hands every client a Control-
            // equivalent slot without honoring role requests,
            // or a hypothetical future server with
            // role-downgrade semantics). `granted_role` is
            // populated by the extended handshake: `Some(true)`
            // → Controller, `Some(false)` → Listener, `None` →
            // unknown (legacy server, or pre-#392 RTLX build
            // that doesn't write the field). Hide the badge
            // when unknown AND in every non-Connected state.
            let role_badge = match &conn_state {
                sdr_types::RtlTcpConnectionState::Connected {
                    granted_role: Some(true),
                    ..
                } => Some(crate::status_bar::RtlTcpRoleBadge::Controller),
                sdr_types::RtlTcpConnectionState::Connected {
                    granted_role: Some(false),
                    ..
                } => Some(crate::status_bar::RtlTcpRoleBadge::Listener),
                _ => None,
            };
            status_bar.update_role(role_badge);
        }
        DspToUi::NetworkSinkStatus(status) => {
            tracing::debug!(?status, "network sink status");
            if let Some(row) = network_sink_status_row_weak.upgrade() {
                apply_network_sink_status(&row, &status);
            }
        }
        // --- Scanner (#317) ---
        DspToUi::ScannerActiveChannelChanged {
            key,
            freq_hz,
            demod_mode,
            bandwidth,
            name,
            ctcss,
            voice_squelch,
        } => {
            // Cache the active channel key for the lockout button
            // click handler in `connect_scanner_panel`. Written
            // before the widget sync below so a racing user click
            // during this frame sees the latest key.
            state.scanner_active_key.borrow_mut().clone_from(&key);
            if key.is_some() {
                // Update the cached tuning state so downstream
                // reads (bandwidth notify's status-bar rewrite,
                // Add / Save Bookmark, anything else that reads
                // `state.center_frequency` / `state.demod_mode`)
                // see the scanner's current channel, not the
                // channel the user last tuned manually.
                #[allow(clippy::cast_precision_loss)]
                let freq_f64 = freq_hz as f64;
                state.center_frequency.set(freq_f64);
                state.demod_mode.set(demod_mode);

                scanner_panel.active_channel_label.set_text(&format!(
                    "Active: {} — {}",
                    name,
                    sidebar::navigation_panel::format_frequency(freq_hz),
                ));
                // Sync every widget that mirrors the current tune.
                // The selector's `set_frequency` does NOT fire its
                // own callback, so no SetFrequency bounces back.
                freq_selector.set_frequency(freq_hz);
                spectrum_handle.set_center_frequency(freq_f64);
                status_bar.update_frequency(freq_f64);
                let label = header::demod_selector::demod_mode_label(demod_mode);
                status_bar.update_demod(label, bandwidth);
                // Programmatic updates of the demod dropdown +
                // bandwidth row — suppress the notify handlers so
                // the scanner's retune doesn't ricochet back into
                // `SetDemodMode` / `SetBandwidth` commands.
                state.suppress_demod_notify.set(true);
                if let Some(idx) = header::demod_selector::demod_mode_to_index(demod_mode) {
                    demod_dropdown.set_selected(idx);
                }
                state.suppress_demod_notify.set(false);
                // Mode-specific row visibility (WFM stereo,
                // FM-IF-NR, etc.) is normally driven by the
                // dropdown's `connect_selected_notify` handler,
                // which we just suppressed. Call it directly so
                // the radio panel reflects the scanner's channel
                // instead of the previous mode's row set.
                radio_panel.apply_demod_visibility(demod_mode);
                state.suppress_bandwidth_notify.set(true);
                radio_panel.bandwidth_row.set_value(bandwidth);
                state.suppress_bandwidth_notify.set(false);

                // CTCSS + voice-squelch widget sync — keeps
                // Add/Save Bookmark honest when the user stashes
                // a channel the scanner landed on. The set calls
                // bounce back through the widgets'
                // connect_selected_notify handlers as redundant
                // `SetCtcssMode` / `SetVoiceSquelchMode`
                // dispatches, which are idempotent at the
                // engine (the scanner retune has already applied
                // the same values). Same trade-off the master-
                // switch `connect_active_notify` migration made
                // in round 1.
                //
                // `None` on the channel:
                // - CTCSS: scanner forces engine to Off, so the
                //   row tracks that and goes to Off.
                // - voice-squelch: scanner leaves engine alone,
                //   so we leave the widget alone too (what's on
                //   the widget matches what's on the engine).
                let ctcss_for_widget = ctcss.unwrap_or(sdr_radio::af_chain::CtcssMode::Off);
                let ctcss_idx =
                    sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(ctcss_for_widget);
                radio_panel.ctcss_row.set_selected(ctcss_idx);
                if let Some(vs_mode) = voice_squelch {
                    radio_panel.apply_voice_squelch_mode_ui(vs_mode);
                    // Reset the open/closed badge too — mode
                    // change rebuilds the voice-squelch detector,
                    // so a stale "open" from the previous channel
                    // must not carry over. The next
                    // `VoiceSquelchOpenChanged` edge from DSP
                    // repaints it accurately. Mirrors the manual
                    // selector path at `voice_squelch_row.connect_selected_notify`.
                    radio_panel.set_voice_squelch_open(false);
                }

                scanner_panel.lockout_button.set_visible(true);
            } else {
                clear_scanner_active_channel_ui(scanner_panel, state);
            }
        }
        DspToUi::ScannerStateChanged(scanner_state) => {
            let label = match scanner_state {
                sdr_scanner::ScannerState::Idle => "Off",
                sdr_scanner::ScannerState::Retuning => "Scanning…",
                sdr_scanner::ScannerState::Dwelling => "Dwelling…",
                sdr_scanner::ScannerState::Listening => "Listening",
                sdr_scanner::ScannerState::Hanging => "Hang…",
            };
            scanner_panel
                .state_label
                .set_text(&format!("State: {label}"));
        }
        DspToUi::ScannerEmptyRotation => {
            tracing::info!("scanner rotation empty");
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(adw::Toast::new(
                    "Scanner has no active channels (all locked or disabled)",
                ));
            }
            // Engine is already back to Idle — drop the master
            // switch to match. `set_state` propagates to `active`
            // and fires `notify::active`, which the master switch's
            // `connect_active_notify` handler dispatches as a
            // redundant `SetScannerEnabled(false)` — idempotent on
            // the engine side (scanner's already Idle), so no harm.
            scanner_panel.master_switch.set_state(false);
            // Clear the active-channel surfaces locally rather
            // than waiting for a separate `ActiveChannelChanged
            // { key: None }` event — the engine sends it today,
            // but relying on that ordering across four stop
            // sites was brittle.
            clear_scanner_active_channel_ui(scanner_panel, state);
        }
        DspToUi::ScannerMutexStopped(reason) => {
            tracing::info!(?reason, "scanner mutex stopped");
            // Widget-state sync for recording comes for free via
            // the paired `AudioRecordingStopped` / `IqRecordingStopped`
            // events that `stop_any_recording` emits in the
            // controller. Transcription has no matching stopped
            // event; deactivate the switch here. Scanner sync for
            // the `ScannerStoppedFor*` variants flips the master
            // switch so the sidebar reflects the engine state.
            let message = match reason {
                sdr_core::messages::ScannerMutexReason::RecordingStoppedForScanner => {
                    "Recording stopped — Scanner activated"
                }
                sdr_core::messages::ScannerMutexReason::TranscriptionStoppedForScanner => {
                    transcription_enable_row.set_active(false);
                    "Transcription stopped — Scanner activated"
                }
                sdr_core::messages::ScannerMutexReason::ScannerStoppedForRecording => {
                    scanner_panel.master_switch.set_state(false);
                    clear_scanner_active_channel_ui(scanner_panel, state);
                    "Scanner stopped — recording started"
                }
                sdr_core::messages::ScannerMutexReason::ScannerStoppedForTranscription => {
                    scanner_panel.master_switch.set_state(false);
                    clear_scanner_active_channel_ui(scanner_panel, state);
                    "Scanner stopped — transcription started"
                }
            };
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(adw::Toast::new(message));
            }
        }
    }
}

/// Render a `NetworkSinkStatus` into the audio panel's status row.
/// Three states map to three subtitles + colors:
///   - `Active` → "Streaming to host:port (TCP/UDP)"
///   - `Inactive` → "Inactive" (e.g. just switched back to local)
///   - `Error { message }` → "Error: <message>"
///
/// Per issue #247.
fn apply_network_sink_status(row: &adw::ActionRow, status: &sdr_core::NetworkSinkStatus) {
    use sdr_core::NetworkSinkStatus;
    let subtitle = match status {
        NetworkSinkStatus::Active { endpoint, protocol } => {
            let proto_label = match protocol {
                sdr_types::Protocol::TcpClient => "TCP",
                sdr_types::Protocol::Udp => "UDP",
            };
            format!("Streaming to {endpoint} ({proto_label})")
        }
        NetworkSinkStatus::Inactive => "Inactive".to_string(),
        NetworkSinkStatus::Error { message } => format!("Error: {message}"),
    };
    row.set_subtitle(&subtitle);
}

/// Render a `RtlTcpConnectionState` into the status row + button
/// sensitivities. Pulled out of the renderer so the message
/// handler can call it with individual weak-upgraded widgets
/// instead of holding a whole `SourcePanel` clone across the
/// signal-handler boundary.
/// Fire a toast + manipulate widgets on each **edge transition**
/// into a terminal role-denial state (`ControllerBusy`,
/// `AuthRequired`, `AuthFailed`), or on a successful `Connected`
/// immediately following an auth-required transition (to save
/// the user-entered key to the per-server keyring).
///
/// `adw::Toast::set_timeout(0)` keeps a toast on screen until
/// the user dismisses it or an explicit `dismiss()` fires. Used
/// for the two `ControllerBusy` action toasts — the stakes are
/// high enough (the user has to actively choose between Take-
/// control, Listener, or abandoning the connect) that a
/// time-limited toast would feel like silent retry behavior.
/// Per `CodeRabbit` round 12 on PR #408.
const TOAST_TIMEOUT_PERSISTENT: u32 = 0;

/// Short toast timeout in seconds for transient-acknowledgement
/// notices — the `AuthRequired` / `AuthFailed` copy that
/// complements a revealed key-entry row. Long enough to read, short
/// enough to clear without user interaction once the user has
/// moved on to typing. Per `CodeRabbit` round 12 on PR #408.
const TOAST_TIMEOUT_SHORT_SECS: u32 = 5;

/// Called only from the edge-detection path in
/// `handle_dsp_message`; the caller already verified
/// `prev_disc != now_disc` and stored the new discriminant.
/// Per issue #396.
#[allow(
    clippy::too_many_arguments,
    reason = "toast composition needs read access to multiple panel widgets \
              + a dispatch handle; collapsing into a single context struct \
              would move the same argument count one layer up"
)]
#[allow(
    clippy::doc_markdown,
    reason = "doc references to Connected / ControllerBusy / AuthRequired / \
              AuthFailed are type variants — enum paths would make the prose \
              unreadable; backticks on each would overwhelm the paragraph"
)]
#[allow(
    clippy::too_many_lines,
    reason = "linear arm-by-arm toast + row + state handling for all 8 rtl_tcp connection-state variants; splitting would scatter the shared setup (pending-toasts sweep, edge-log) and obscure the 1:1 mapping from variant to UX gesture"
)]
fn handle_rtl_tcp_state_toast(
    state_val: &sdr_types::RtlTcpConnectionState,
    prev_disc: u8,
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    role_row_weak: &glib::WeakRef<adw::ComboRow>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
) {
    use sdr_types::RtlTcpConnectionState;

    use crate::state::{
        RTL_TCP_STATE_DISC_AUTH_FAILED, RTL_TCP_STATE_DISC_AUTH_REQUIRED,
        RTL_TCP_STATE_DISC_CONNECTING, RTL_TCP_STATE_DISC_CONTROLLER_BUSY,
    };

    // Sweep any still-live ControllerBusy toasts on any
    // transition that isn't re-entering ControllerBusy. Pre-
    // `CodeRabbit` round 11 on PR #408 each ControllerBusy
    // toast's button handler only dismissed itself, so a stale
    // "Take control" / "Connect as Listener" action sat visible
    // after the server went away (Connected directly, Disconnect,
    // Failed, etc.) and could later rebuild the source
    // unexpectedly against a healthy session. The
    // `timeout(0)` persistence is intentional — we WANT these to
    // stick around until the user interacts OR the state
    // resolves itself — but "the state resolved itself" needs
    // its own cleanup pass.
    if !matches!(state_val, RtlTcpConnectionState::ControllerBusy) {
        let mut pending = pending_controller_busy_toasts.borrow_mut();
        for weak in pending.drain(..) {
            if let Some(toast) = weak.upgrade() {
                toast.dismiss();
            }
        }
    }

    match state_val {
        RtlTcpConnectionState::ControllerBusy => {
            // Toast with two action buttons: "Connect as
            // Listener" flips the role combo (its change handler
            // re-dispatches SetRtlTcpClientConfig) and fires a
            // normal retry; "Take control" dispatches the one-shot
            // `RetryRtlTcpWithTakeover` message which rebuilds
            // the source with `request_takeover = true` on the
            // hello.
            let Some(overlay) = toast_overlay_weak.upgrade() else {
                return;
            };
            // Before creating the new pair, sweep any still-
            // live toasts from a prior `ControllerBusy` entry
            // (e.g. the user hit `Retry` without clicking either
            // action, and the server is still busy on the
            // rebound). Otherwise the overlay would stack two
            // pairs, and dismissing one pair via the cross-
            // dismiss helpers below would leave the other pair
            // orphaned. Per `CodeRabbit` round 11 on PR #408.
            {
                let mut pending = pending_controller_busy_toasts.borrow_mut();
                for weak in pending.drain(..) {
                    if let Some(toast) = weak.upgrade() {
                        toast.dismiss();
                    }
                }
            }

            let toast = adw::Toast::builder()
                .title("Controller slot is occupied on this server.")
                .timeout(TOAST_TIMEOUT_PERSISTENT)
                .build();
            let listen_toast = adw::Toast::builder()
                .title("Or connect as Listener (read-only).")
                .timeout(TOAST_TIMEOUT_PERSISTENT)
                .build();
            // Cross-dismiss: clicking either action dismisses
            // BOTH toasts, so a stale sibling action can't fire
            // later against a session that's already resolved.
            // `WeakRef` rather than strong clones — the toasts
            // hand out their own strong refs to the overlay
            // internally, and we only need to reach the sibling
            // when it's still live.
            let toast_weak = toast.downgrade();
            let listen_toast_weak = listen_toast.downgrade();

            // Track the two action buttons as separate signals.
            // AdwToast supports a single primary action via
            // `set_button_label` + `connect_button_clicked`; the
            // "Take control" action lands there, and the
            // "Connect as Listener" option lives in the
            // sibling toast below so users still see both
            // choices.
            toast.set_button_label(Some("Take control"));
            let state_for_takeover = Rc::clone(app_state);
            let listen_weak_for_takeover = listen_toast_weak.clone();
            toast.connect_button_clicked(move |t| {
                state_for_takeover.send_dsp(UiToDsp::RetryRtlTcpWithTakeover);
                t.dismiss();
                if let Some(sibling) = listen_weak_for_takeover.upgrade() {
                    sibling.dismiss();
                }
            });
            overlay.add_toast(toast);

            // Second toast offering the Listen fallback. Two
            // separate toasts beats a single one because AdwToast
            // exposes only one action button — splitting the two
            // paths keeps both discoverable.
            listen_toast.set_button_label(Some("Connect as Listener"));
            let state_for_listen = Rc::clone(app_state);
            let role_row_for_listen = role_row_weak.clone();
            let toast_weak_for_listen = toast_weak.clone();
            listen_toast.connect_button_clicked(move |t| {
                if let Some(role_row) = role_row_for_listen.upgrade() {
                    // Flipping the combo to Listen fires its
                    // `selected-notify` handler which dispatches
                    // `SetRtlTcpClientConfig` with the new role.
                    // Follow with RetryRtlTcpNow so the user
                    // doesn't have to click Retry themselves.
                    role_row.set_selected(crate::sidebar::source_panel::RTL_TCP_ROLE_LISTEN_IDX);
                }
                state_for_listen.send_dsp(UiToDsp::RetryRtlTcpNow);
                t.dismiss();
                if let Some(sibling) = toast_weak_for_listen.upgrade() {
                    sibling.dismiss();
                }
            });
            overlay.add_toast(listen_toast);

            // Record the pair so the non-ControllerBusy state
            // transition at the top of this function can sweep
            // them if the server resolves itself without user
            // interaction.
            {
                let mut pending = pending_controller_busy_toasts.borrow_mut();
                pending.push(toast_weak);
                pending.push(listen_toast_weak);
            }
        }

        RtlTcpConnectionState::AuthRequired => {
            // Remember the active server so a subsequent
            // successful Connected can save the user-entered
            // key to the right keyring entry.
            record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
            // Reveal + focus the Server key field so the user
            // can enter the key.
            if let Some(row) = auth_key_row_weak.upgrade() {
                row.set_visible(true);
                row.grab_focus();
            }
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::builder()
                    .title("Server requires an authentication key.")
                    .timeout(TOAST_TIMEOUT_SHORT_SECS)
                    .build();
                overlay.add_toast(toast);
            }
        }

        RtlTcpConnectionState::AuthFailed => {
            record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
            // Clear the saved per-server key from the keyring
            // too — not just the widget. Pre-CodeRabbit round 2
            // on PR #408 only `row.set_text("")` was called, so
            // the keyring entry survived the rejection and the
            // next discovery / favorites / Play-restart path
            // would auto-load the same rejected bytes into the
            // row via `apply_rtl_tcp_connect` / the startup
            // restore, silently bouncing the user straight back
            // into `AuthFailed`. Now we delete the saved key
            // whenever the server explicitly rejects it; the
            // user has to re-enter (or paste the new) key on
            // the next attempt, which is the only recovery path
            // from a rotated server key anyway. Per issue #396.
            let active = app_state.rtl_tcp_active_server.borrow().clone();
            if let Some((host, port_str)) = active.rsplit_once(':')
                && let Ok(port) = port_str.parse::<u16>()
                && let Err(e) = clear_client_auth_key_from_keyring(host, port)
            {
                tracing::warn!(
                    server = %active,
                    %e,
                    "rtl_tcp: client auth key keyring clear on AuthFailed failed (non-fatal)"
                );
            }
            if let Some(row) = auth_key_row_weak.upgrade() {
                row.set_visible(true);
                row.grab_focus();
                // Clear the entered value so the user doesn't
                // re-submit the same wrong key by reflex on the
                // next Retry.
                row.set_text("");
            }
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::builder()
                    .title("Key rejected. Check with the server owner.")
                    .timeout(TOAST_TIMEOUT_SHORT_SECS)
                    .build();
                overlay.add_toast(toast);
            }
        }

        RtlTcpConnectionState::Connected { .. } => {
            // Save the user-entered key to the per-server
            // keyring so subsequent reconnects auto-use it.
            // Fires on the edge from any of:
            //
            // - `AuthRequired` / `AuthFailed` — user typed a
            //   key in response to a denial toast;
            // - `Connecting` — user had auth configured up
            //   front (server advertised `auth_required` via
            //   mDNS, key was entered before the first
            //   connect, and the handshake succeeded in a
            //   single `Connecting → Connected` hop);
            // - `ControllerBusy` — user entered a key before
            //   the first connect, server denied with
            //   `ControllerBusy`, and the user's subsequent
            //   Take-control / Listener retry (via
            //   `RetryRtlTcpWithTakeover` or `RetryRtlTcpNow`)
            //   succeeded. Added per `CodeRabbit` round 12 on
            //   PR #408 — without this branch an auth-required
            //   server that's also busy on the first attempt
            //   would accept the key on the takeover reconnect
            //   but never persist it to the keyring.
            //
            // Pre-round-1 on PR #408 only the auth-denial arms
            // triggered the save, so up-front keys never hit the
            // keyring and the user had to re-type them on every
            // reconnect. `save_current_auth_key_for_active_
            // server` is a no-op when the key row is empty, so
            // this is safe to trigger on every qualifying edge
            // even if the server doesn't require auth. Call
            // `record_active_rtl_tcp_server` first so the save-
            // path sees the right `host:port` even when the
            // user never hit an auth-denial arm (which is what
            // previously set the cache).
            if prev_disc == RTL_TCP_STATE_DISC_CONNECTING
                || prev_disc == RTL_TCP_STATE_DISC_CONTROLLER_BUSY
                || prev_disc == RTL_TCP_STATE_DISC_AUTH_REQUIRED
                || prev_disc == RTL_TCP_STATE_DISC_AUTH_FAILED
            {
                record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
                save_current_auth_key_for_active_server(app_state, auth_key_row_weak);
            }
        }

        // Non-toast states (Disconnected / Connecting / Retrying
        // / Failed) just update the status row subtitle via the
        // sibling call in `handle_dsp_message`. No additional
        // UX gesture needed here.
        RtlTcpConnectionState::Disconnected
        | RtlTcpConnectionState::Connecting
        | RtlTcpConnectionState::Retrying { .. }
        | RtlTcpConnectionState::Failed { .. } => {}
    }
}

/// Record the currently-displayed `rtl_tcp` server's `host:port`
/// on `AppState` so a subsequent successful `Connected` can save
/// the just-entered key to the right per-server keyring entry.
/// Empty on upgrade failure — the save path skips when the
/// cached identity is empty. Per #396.
///
/// **Cache-preserving fallback** (per `CodeRabbit` round 2 on
/// PR #408): if `app_state.rtl_tcp_active_server` is already
/// non-empty, this is a no-op. `apply_rtl_tcp_connect` writes
/// the stable advertised `hostname:port` (same form as
/// `favorite_key(server)`) directly into the cache at
/// connect-setup time, so every downstream per-server lookup
/// (keyring load/save/clear, favorite match) keys off the same
/// identity. Reading `hostname_row.text()` here would overwrite
/// the stable id with whatever the DSP is dialing — for
/// discovery connects that can be a resolved IPv4/IPv6 literal,
/// splitting "shack-pi.local.:1234" (favorites) from
/// "192.168.1.17:1234" (keyring) and breaking round-trip. The
/// widget-read fallback only runs in the manually-typed Play
/// path where `apply_rtl_tcp_connect` never ran.
fn record_active_rtl_tcp_server(
    app_state: &Rc<AppState>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    if !app_state.rtl_tcp_active_server.borrow().is_empty() {
        return;
    }
    let Some(host_row) = hostname_row_weak.upgrade() else {
        return;
    };
    let Some(port_row) = port_row_weak.upgrade() else {
        return;
    };
    let host = host_row.text().to_string();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let port = port_row.value() as u16;
    if !host.is_empty() && port != 0 {
        *app_state.rtl_tcp_active_server.borrow_mut() = format!("{host}:{port}");
    }
}

/// Invalidate the cached active `rtl_tcp` server identity when
/// the hostname / port widgets no longer match it. Called from
/// the `hostname_row.connect_changed` + `port_row.connect_value_
/// notify` handlers so a manual edit retargets per-server state
/// (keyring lookups, favorite matches, `rtl_tcp_active_server`)
/// to the newly-typed endpoint.
///
/// Without this, after the startup `LastConnectedServer` restore
/// or an `apply_rtl_tcp_connect` seeded the cache, typing a
/// different host or port in the source row would leave the
/// cache pointing at the old server — the first subsequent
/// `AuthFailed` / `Connected` arm would then
/// clear/save the key under the WRONG server. Per
/// `CodeRabbit` round 4 on PR #408.
///
/// **Comparison guard:** the cache is cleared only when its
/// current value differs from the widget-derived key. That
/// keeps `apply_rtl_tcp_connect`'s own `hostname_row.set_text` /
/// `port_row.set_value` writes (which fire these same handlers)
/// from spuriously clobbering the stable id the caller just
/// wrote. During a caller-driven server switch the cache IS
/// stale at the widget-write moment (old server id, new widget
/// text), so this invalidation fires correctly there too —
/// `apply_rtl_tcp_connect` overwrites the empty cache right
/// afterwards with the new stable id.
///
/// Also clears the auth-key row (visibility + text) so the
/// old server's key bytes can't leak onto a different endpoint.
/// The row's `connect_changed` handler re-dispatches
/// `SetRtlTcpClientConfig { auth_key: None, .. }` so DSP state
/// tracks the invalidation in lockstep with the UI.
fn invalidate_rtl_tcp_active_server_on_edit(
    app_state: &Rc<AppState>,
    hostname_row: &adw::EntryRow,
    port_row: &adw::SpinRow,
    auth_key_row: &adw::PasswordEntryRow,
) {
    let hostname = hostname_row.text().to_string();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let port = port_row.value() as u16;
    let current_key = format!("{hostname}:{port}");
    let should_clear = {
        let cached = app_state.rtl_tcp_active_server.borrow();
        !cached.is_empty() && *cached != current_key
    };
    if should_clear {
        app_state.rtl_tcp_active_server.borrow_mut().clear();
        auth_key_row.set_visible(false);
        auth_key_row.set_text("");
    }
}

/// Save the current Server-key-row text to the keyring under
/// the active `rtl_tcp` server's `host:port`. Called on a
/// successful Connected following AuthRequired / AuthFailed.
/// Empty text → clear the saved entry instead of writing empty
/// bytes; invalid hex → log + skip (the live connection
/// obviously accepted the text, but our keyring round-trip
/// demands valid hex). Per #396.
#[allow(
    clippy::doc_markdown,
    reason = "Connected / AuthRequired / AuthFailed are enum variants"
)]
fn save_current_auth_key_for_active_server(
    app_state: &Rc<AppState>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
) {
    let active = app_state.rtl_tcp_active_server.borrow().clone();
    if active.is_empty() {
        return;
    }
    let Some((host, port_str)) = active.rsplit_once(':') else {
        return;
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return;
    };
    let Some(row) = auth_key_row_weak.upgrade() else {
        return;
    };
    let text = row.text().to_string();
    if text.is_empty() {
        // User explicitly cleared the field BEFORE this connect
        // succeeded — mirror that intent in the keyring by
        // deleting the saved entry. Pre-CodeRabbit round 3 on
        // PR #408 this branch returned early with a stale
        // "nothing to save" comment, so clearing the row and
        // reconnecting left the old bytes in the keyring and
        // `apply_rtl_tcp_connect` would preload them on the
        // next discovery / favorites / last-connected path,
        // silently undoing the user's clear.
        if let Err(e) = clear_client_auth_key_from_keyring(host, port) {
            tracing::warn!(
                server = %active,
                %e,
                "rtl_tcp: client auth key keyring clear failed (empty row)"
            );
        }
        return;
    }
    let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&text) else {
        tracing::warn!(
            server = %active,
            "rtl_tcp: client auth key hex is invalid — skipping keyring save"
        );
        return;
    };
    if let Err(e) = save_client_auth_key_to_keyring(host, port, &bytes) {
        tracing::warn!(
            server = %active,
            %e,
            "rtl_tcp: client auth key keyring save failed"
        );
    } else {
        tracing::info!(
            server = %active,
            "rtl_tcp: client auth key saved to keyring for next reconnect"
        );
    }
}

fn apply_rtl_tcp_connection_state(
    status_row: &adw::ActionRow,
    disconnect_button: &gtk4::Button,
    retry_button: &gtk4::Button,
    state: &sdr_types::RtlTcpConnectionState,
) {
    use sdr_types::RtlTcpConnectionState;
    status_row.set_subtitle(&sidebar::source_panel::format_rtl_tcp_state(state));
    let is_active = matches!(
        state,
        RtlTcpConnectionState::Connecting
            | RtlTcpConnectionState::Connected { .. }
            | RtlTcpConnectionState::Retrying { .. }
    );
    // "Retry now" is only meaningful when there's an active source
    // to short-circuit out of its backoff wait — Retrying (most
    // common) or any of the four terminal states (Failed +
    // role-denials added in #396). After an explicit Disconnect
    // the controller drops `state.source`, and
    // `UiToDsp::RetryRtlTcpNow` is a no-op (it checks
    // `state.source.as_mut()` → None → early return). Leaving the
    // button visibly enabled in that state misleads the user into
    // thinking they can reconnect in one click; the correct
    // post-Disconnect path is to press Play.
    let can_retry_now = matches!(
        state,
        RtlTcpConnectionState::Retrying { .. } | RtlTcpConnectionState::Failed { .. }
    ) || state.needs_user_action();
    disconnect_button.set_sensitive(is_active);
    retry_button.set_sensitive(can_retry_now);
}

/// Build the `AdwOverlaySplitView` with sidebar configuration panels, content,
/// and status bar.
///
/// Returns the split view, sidebar panels, spectrum display handle, and status bar.
#[allow(
    clippy::type_complexity,
    reason = "splitting into a struct would trade one named return for one named struct whose fields are used exactly once by the caller — net neutral for readability, net negative for locality of widget construction"
)]
/// Minimum left-panel width in pixels — narrower than this makes
/// `AdwPreferencesGroup` content wrap awkwardly (design doc §4.4).
const LEFT_SIDEBAR_MIN_WIDTH: f64 = 220.0;
/// Minimum right-panel width. The transcript panel's controls
/// (model combo, VAD slider, auto-break sliders) need more breathing
/// room than a preferences row — below this they stack awkwardly
/// and the transcript text view loses usable line width.
const RIGHT_SIDEBAR_MIN_WIDTH: f64 = 360.0;
/// Default left-panel width — matches today's sidebar width.
const LEFT_SIDEBAR_DEFAULT_WIDTH: f64 = 320.0;
/// Default right-panel width — gives the transcript panel room for
/// its wider controls without the user having to resize on every
/// launch.
const RIGHT_SIDEBAR_DEFAULT_WIDTH: f64 = 420.0;

/// Handles returned by [`build_layout`] for downstream wiring. Bundled
/// into a struct rather than a tuple because the return list grew past
/// the clippy threshold during the activity-bar scaffolding migration.
struct LayoutHandles {
    /// Root horizontal container for the whole window content area.
    root: gtk4::Box,
    /// Outer split view — sidebar hosts the left activity stack,
    /// content hosts the nested right split view.
    left_split_view: adw::OverlaySplitView,
    /// Inner split view — sidebar hosts the right activity stack
    /// (`sidebar_position=End`), content hosts spectrum + status
    /// + the legacy bookmarks revealer.
    right_split_view: adw::OverlaySplitView,
    /// Left activity bar widget + per-entry toggle buttons.
    left_activity_bar: sidebar::ActivityBar,
    /// Right activity bar widget + per-entry toggle buttons.
    right_activity_bar: sidebar::ActivityBar,
    /// Left panel content switcher — 5 children keyed by entry name.
    left_stack: gtk4::Stack,
    /// Right panel content switcher — 1 child keyed `"transcript"`.
    right_stack: gtk4::Stack,
    panels: SidebarPanels,
    spectrum_handle: spectrum::SpectrumHandle,
    status_bar: StatusBar,
    transcript_panel: sidebar::transcript_panel::TranscriptPanel,
    /// Legacy bookmarks flyout — hangs off the right-split-view
    /// content box. Retained for this scaffolding PR so the header
    /// `Ctrl+B` path keeps working; sub-ticket #422 will migrate the
    /// bookmarks list into the General activity panel and this
    /// revealer can then be dropped.
    bookmarks_revealer: gtk4::Revealer,
}

#[allow(clippy::too_many_lines)]
fn build_layout(
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> LayoutHandles {
    // Legacy sidebar — packed into the General activity slot so that
    // Source / Radio / Audio / Display / Scanner / Navigation
    // controls stay reachable during the scaffolding-to-real-panel
    // migration window. Sub-tickets #422-#426 gradually pull each
    // sub-section out into its own dedicated activity child; the last
    // migration removes this bridge entirely and the General slot
    // becomes a band-presets / bookmarks / source preferences page
    // per the design doc §3.1.
    let (legacy_sidebar_scroll, panels) = sidebar::build_sidebar();
    sidebar::server_panel::connect_server_panel_persistence(&panels.server, config);

    // Spectrum display (FFT + waterfall) + status bar.
    let (spectrum_view, spectrum_handle) = spectrum::build_spectrum_view(state.ui_tx.clone());
    spectrum_view.add_css_class("spectrum-area");
    let status_bar = status_bar::build_status_bar();

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    content_box.append(&spectrum_view);
    content_box.append(&status_bar.widget);

    // Transcript panel — becomes the only child of the right stack
    // (the real widget, not a placeholder) because this sub-ticket's
    // design is that transcription keeps working through the
    // scaffolding. See PR description for the one-real-panel
    // rationale (design doc §3.6 end-state).
    let transcript_panel = sidebar::transcript_panel::build_transcript_panel(config);
    let transcript_scroll = gtk4::ScrolledWindow::builder()
        .child(&transcript_panel.widget)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Legacy bookmarks flyout — still hosted as a right-side revealer
    // in the inner split view's content area. Header `bookmarks_toggle`
    // + `Ctrl+B` still drive this revealer unchanged. Sub-ticket #422
    // migrates the list into the General activity panel and this
    // revealer is deleted at that time.
    let bookmarks_revealer = gtk4::Revealer::builder()
        .transition_type(gtk4::RevealerTransitionType::SlideLeft)
        .transition_duration(RIGHT_FLYOUT_TRANSITION_MS)
        .reveal_child(false)
        .child(&panels.bookmarks.widget)
        .hexpand(false)
        .build();

    let content_inner = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .build();
    content_inner.append(&content_box);
    content_inner.append(&bookmarks_revealer);

    // Left panel stack — General hosts the full legacy sidebar
    // (preserves all Source / Radio / Audio / Display / Scanner
    // control reachability during the migration), the other four
    // activities are placeholder `Label`s until sub-tickets
    // #422-#426 swap each one for a real panel. The `name` strings
    // MUST remain stable because they're the config-persistence
    // keys (§5 of the design doc).
    let left_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::None)
        .hexpand(true)
        .vexpand(true)
        .build();
    for entry in sidebar::LEFT_ACTIVITIES {
        if entry.name == "general" {
            left_stack.add_named(&legacy_sidebar_scroll, Some(entry.name));
            continue;
        }
        let placeholder = gtk4::Label::builder()
            .label(format!(
                "{} — coming in a follow-up sub-ticket",
                entry.display_name
            ))
            .wrap(true)
            .justify(gtk4::Justification::Center)
            .hexpand(true)
            .vexpand(true)
            .build();
        left_stack.add_named(&placeholder, Some(entry.name));
    }

    // Right panel stack — single child today, hosts the real
    // transcript widget (not a placeholder) so transcription keeps
    // working during the migration window.
    let right_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::None)
        .hexpand(true)
        .vexpand(true)
        .build();
    right_stack.add_named(&transcript_scroll, Some("transcript"));

    // Inner (right) split view — sidebar sits on the trailing edge
    // so the right activity bar is the rightmost element on-screen.
    let right_split_view = adw::OverlaySplitView::builder()
        .sidebar_position(gtk4::PackType::End)
        .sidebar(&right_stack)
        .content(&content_inner)
        .show_sidebar(false)
        .min_sidebar_width(RIGHT_SIDEBAR_MIN_WIDTH)
        .max_sidebar_width(RIGHT_SIDEBAR_DEFAULT_WIDTH * 2.0)
        .sidebar_width_fraction(RIGHT_SIDEBAR_DEFAULT_WIDTH / f64::from(DEFAULT_WIDTH))
        .build();

    // Outer (left) split view — sidebar hosts the left activity
    // stack. Starts open with "general" visible so a fresh launch
    // lands on the General placeholder instead of an empty frame.
    let left_split_view = adw::OverlaySplitView::builder()
        .sidebar(&left_stack)
        .content(&right_split_view)
        .show_sidebar(true)
        .min_sidebar_width(LEFT_SIDEBAR_MIN_WIDTH)
        .max_sidebar_width(LEFT_SIDEBAR_DEFAULT_WIDTH * 2.0)
        .sidebar_width_fraction(LEFT_SIDEBAR_DEFAULT_WIDTH / f64::from(DEFAULT_WIDTH))
        .build();
    left_stack.set_visible_child_name("general");

    let left_activity_bar =
        sidebar::build_activity_bar(sidebar::LEFT_ACTIVITIES, sidebar::ActivityBarSide::Left);
    let right_activity_bar =
        sidebar::build_activity_bar(sidebar::RIGHT_ACTIVITIES, sidebar::ActivityBarSide::Right);

    let root = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .build();
    root.append(&left_activity_bar.widget);
    root.append(&left_split_view);
    root.append(&right_activity_bar.widget);

    LayoutHandles {
        root,
        left_split_view,
        right_split_view,
        left_activity_bar,
        right_activity_bar,
        left_stack,
        right_stack,
        panels,
        spectrum_handle,
        status_bar,
        transcript_panel,
        bookmarks_revealer,
    }
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

/// Handles handed back from `build_header_bar` for the `rtl_tcp`
/// favorites slide-out. The `button` is packed into the header bar
/// and drops its popover on click; the `list` is the scrollable
/// `ListBox` inside that popover — `connect_rtl_tcp_discovery`
/// clears + re-populates it when the favorites map changes. The
/// `empty_label` is shown when the list is empty so the user sees
/// "No pinned servers yet" instead of a blank popover.
struct FavoritesHeaderHandle {
    button: gtk4::MenuButton,
    popover: gtk4::Popover,
    list: gtk4::ListBox,
    empty_label: gtk4::Label,
}

/// Build the `AdwHeaderBar` with play/stop, frequency selector, demod selector,
/// and volume control.
///
/// Returns the header bar, play button, demod dropdown, and frequency selector
/// (for shortcuts, status bar wiring, and frequency change callbacks).
#[allow(
    clippy::too_many_lines,
    reason = "widget-assembly — splitting scatters one-time wire-up across helpers without readability win"
)]
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
    FavoritesHeaderHandle,
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

    // Demod selector dropdown. The DSP-dispatch handler used to
    // live here, but it would race the scanner force-disable
    // that runs from build_window's handler — scanner would hear
    // SetDemodMode first, then the stop command. Dispatch wiring
    // moved to build_window so force-disable + send_dsp can run
    // in a single handler in the right order.
    let (demod_dropdown, _demod_mode_cell) = header::build_demod_selector();

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

    // Favorites slide-out button — opens a popover listing the
    // user's pinned `rtl_tcp` servers. Entries populated
    // dynamically by `connect_rtl_tcp_discovery`. MenuButton
    // auto-toggles and handles click-outside dismissal.
    let favorites_handle = build_favorites_header();

    header.pack_end(&menu_button);
    header.pack_end(&volume_button);
    header.pack_end(&rr_button);
    header.pack_end(&screenshot_button);
    header.pack_end(&favorites_handle.button);

    (
        header,
        play_button,
        demod_dropdown.clone(),
        freq_selector,
        screenshot_button,
        rr_button,
        favorites_handle,
    )
}

/// Width of the favorites popover's scrollable list. Wide enough
/// for a `rtl_tcp://hostname.local.:12345 — R820T (29 gains)`
/// subtitle without wrapping.
const FAVORITES_POPOVER_WIDTH_PX: i32 = 420;
/// Max height of the favorites popover's scrollable list. Caps the
/// popover so a large favorites set doesn't paint past the bottom
/// of the window; the internal `ScrolledWindow` handles overflow.
const FAVORITES_POPOVER_HEIGHT_PX: i32 = 360;

/// Build the header-bar favorites button + its popover contents.
/// The popover hosts a `ListBox` (populated by
/// `connect_rtl_tcp_discovery` whenever the favorites map mutates)
/// wrapped in a capped `ScrolledWindow`. The empty-state label is
/// shown when the list is empty and hidden when it's populated —
/// callers are responsible for that toggle alongside row rebuilds.
fn build_favorites_header() -> FavoritesHeaderHandle {
    let popover = gtk4::Popover::builder()
        .autohide(true)
        .has_arrow(true)
        .width_request(FAVORITES_POPOVER_WIDTH_PX)
        .build();
    popover.add_css_class("menu");

    let title = gtk4::Label::builder()
        .label("Pinned servers")
        .halign(gtk4::Align::Start)
        .margin_start(12)
        .margin_top(12)
        .margin_bottom(6)
        .css_classes(["heading"])
        .build();

    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(6)
        .margin_end(6)
        .margin_bottom(6)
        .build();

    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .max_content_height(FAVORITES_POPOVER_HEIGHT_PX)
        .propagate_natural_height(true)
        .child(&list)
        .build();

    let empty_label = gtk4::Label::builder()
        .label("No pinned servers yet.\n\nStar a discovered server to pin it here.")
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .css_classes(["dim-label"])
        .build();

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(0)
        .build();
    content.append(&title);
    content.append(&empty_label);
    content.append(&scroll);
    popover.set_child(Some(&content));

    let button = gtk4::MenuButton::builder()
        .icon_name("starred-symbolic")
        .tooltip_text("Pinned rtl_tcp servers")
        .popover(&popover)
        .build();
    // Screen-reader name. Tooltips aren't announced by most
    // ATs — icon-only controls need an explicit accessible
    // label via the GtkAccessible `Label` property.
    button.update_property(&[gtk4::accessible::Property::Label("Pinned servers menu")]);

    FavoritesHeaderHandle {
        button,
        popover,
        list,
        empty_label,
    }
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
fn build_toolbar_view(header: &adw::HeaderBar, content: &gtk4::Box) -> adw::ToolbarView {
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(header);
    toolbar_view.set_content(Some(content));
    toolbar_view
}

/// Wire click handlers on every button of a multi-activity bar so:
///
/// - Clicking a *different* button swaps the stack's visible child
///   and forces the split view's sidebar open.
/// - Clicking the *currently-selected* button keeps that button
///   visually selected (design doc §4.2 — the user's mental model is
///   "I'm still in Radio, I just closed the panel for a second") and
///   toggles the split view's sidebar show/hide.
///
/// `initial_selected` must match the stack's initial visible child
/// and the button the caller pre-activated via `set_active(true)`.
///
/// The `:checked` CSS pseudo-class (driven by `ToggleButton::active`)
/// renders the accent tint — no manual CSS class juggling needed.
///
/// Mutual exclusion is enforced manually rather than via
/// `ToggleButton::set_group`; see `sidebar::activity_bar` module docs.
///
/// Only suitable for bars with more than one entry. Single-button
/// bars (like the right transcript bar today) wire `active` directly
/// to `show_sidebar` — there's no "select vs. toggle panel"
/// distinction to preserve.
fn wire_activity_bar_clicks(
    bar: &sidebar::ActivityBar,
    stack: &gtk4::Stack,
    split_view: &adw::OverlaySplitView,
    initial_selected: &'static str,
) {
    let selected: Rc<RefCell<&'static str>> = Rc::new(RefCell::new(initial_selected));

    for (&name, btn) in &bar.buttons {
        let selected = Rc::clone(&selected);
        let bar_buttons: Vec<(&'static str, glib::WeakRef<gtk4::ToggleButton>)> = bar
            .buttons
            .iter()
            .map(|(n, b)| (*n, b.downgrade()))
            .collect();
        let stack_weak = stack.downgrade();
        let split_view_weak = split_view.downgrade();
        btn.connect_clicked(move |clicked_btn| {
            let prev = *selected.borrow();
            if prev == name {
                // Clicking the already-selected icon keeps it
                // selected visually (force-restore the `active`
                // property after GTK's default click-flip) and
                // toggles the panel open/closed.
                clicked_btn.set_active(true);
                if let Some(sv) = split_view_weak.upgrade() {
                    sv.set_show_sidebar(!sv.shows_sidebar());
                }
            } else {
                // Click on a different activity — deselect siblings,
                // swap stack child, open panel.
                for (other_name, weak) in &bar_buttons {
                    if let Some(other) = weak.upgrade()
                        && *other_name != name
                        && other.is_active()
                    {
                        other.set_active(false);
                    }
                }
                clicked_btn.set_active(true);
                if let Some(stk) = stack_weak.upgrade() {
                    stk.set_visible_child_name(name);
                }
                if let Some(sv) = split_view_weak.upgrade() {
                    sv.set_show_sidebar(true);
                }
                *selected.borrow_mut() = name;
            }
        });
    }
}

/// Create a breakpoint that collapses both sidebars below
/// `SIDEBAR_BREAKPOINT_PX`. Both split views flip to overlay mode at
/// narrow widths so the spectrum keeps its minimum real estate.
fn build_breakpoint(
    left_split_view: &adw::OverlaySplitView,
    right_split_view: &adw::OverlaySplitView,
) -> adw::Breakpoint {
    let condition = adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        SIDEBAR_BREAKPOINT_PX,
        adw::LengthUnit::Px,
    );

    let breakpoint = adw::Breakpoint::new(condition);
    breakpoint.add_setter(left_split_view, "collapsed", Some(&true.into()));
    breakpoint.add_setter(right_split_view, "collapsed", Some(&true.into()));

    breakpoint
}

/// Connect all sidebar panel controls to dispatch `UiToDsp` commands.
#[allow(clippy::too_many_arguments)]
fn connect_sidebar_panels(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    status_bar: &Rc<StatusBar>,
    toast_overlay: &adw::ToastOverlay,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites_header: &FavoritesHeaderHandle,
    scanner_force_disable: &Rc<ScannerForceDisable>,
) {
    // Shared "is the rtl_tcp server currently live?" flag. Written by
    // the server panel's start/stop handler, read by the source
    // panel's device-type guard so the two panels can enforce the
    // "local RTL-SDR source and server-sharing-the-dongle are
    // mutually exclusive" rule without either side owning state the
    // other has to synthesize. `Rc<Cell<bool>>` is ideal: GTK single-
    // threaded, no interior locking needed, cheap to clone into
    // closures.
    let server_running: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));

    // Shared favorites map — key (stable hostname:port) → rich
    // `FavoriteEntry` record. Loaded once here and handed to
    // both `connect_source_panel` (role picker mutates
    // `requested_role` per-server) and `connect_rtl_tcp_discovery`
    // (re-announce path refreshes metadata). Pre-`CodeRabbit`
    // round 8 on PR #408 each function built its own view: the
    // role picker read + wrote the on-disk JSON via
    // `load_favorites`/`save_favorites` while discovery held a
    // separate in-memory HashMap. A subsequent `ServerAnnounced`
    // would preserve the stale in-memory role from the map and
    // clobber the user's just-saved selection on next re-
    // announce. Hoisting the map here makes both paths mutate
    // the SAME `Rc<RefCell<..>>` so persistence stays
    // consistent. `Rc<RefCell<HashMap>>` mirrors the
    // `displayed_rows` pattern — single-threaded GTK main loop,
    // no lock contention.
    let favorites: Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    > = Rc::new(RefCell::new(
        crate::sidebar::source_panel::load_favorites(config)
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect(),
    ));

    connect_source_panel(
        panels,
        state,
        toast_overlay,
        Rc::clone(&server_running),
        config,
        &favorites,
    );
    connect_source_rtlsdr_probe(panels);
    connect_rtl_tcp_discovery(panels, state, config, favorites_header, &favorites);
    connect_server_panel(panels, toast_overlay, server_running);
    connect_radio_panel(panels, state, scanner_force_disable);
    connect_display_panel(panels, state, spectrum_handle);
    connect_audio_panel(panels, state);
    connect_scanner_panel(panels, state, config);
    // Transcript panel is wired separately (not in SidebarPanels).
    connect_navigation_panel(
        panels,
        state,
        freq_selector,
        demod_dropdown,
        status_bar,
        spectrum_handle,
        scanner_force_disable,
    );

    // Mutation-triggered scanner re-projection. Fires on scan
    // checkbox, priority star, and delete — every per-bookmark
    // change that affects the projected channel list. Install
    // this *after* `connect_sidebar_panels` finishes the other
    // panel wiring so early construction-time rebuilds (which
    // pre-date the callback) don't dispatch a spurious empty
    // `UpdateScannerChannels`.
    //
    // The callback lives inside `BookmarksPanel.on_mutated`, so
    // capturing a strong `Rc<BookmarksPanel>` would close a
    // retain cycle (panel → on_mutated → closure → panel) and
    // leak on teardown. Downgrade to `Weak` and upgrade-or-return
    // inside the closure — reads `.bookmarks` via the upgraded
    // handle so the projection still lands against the live
    // backing store. Same pattern the Save closure uses in
    // `sidebar::build_sidebar`.
    let bookmarks_weak = Rc::downgrade(&panels.bookmarks);
    let state_for_mutated = Rc::clone(state);
    let config_for_mutated = std::sync::Arc::clone(config);
    panels.bookmarks.connect_mutated(move || {
        let Some(bookmarks) = bookmarks_weak.upgrade() else {
            return;
        };
        sidebar::navigation_panel::project_and_push_scanner_channels(
            &bookmarks.bookmarks.borrow(),
            &state_for_mutated,
            &config_for_mutated,
        );
    });
}

/// Connect source panel controls to DSP commands.
#[allow(clippy::too_many_lines)]
/// Spawn an mDNS browser for `_rtl_tcp._tcp.local.` services and wire
/// its events into the `rtl_tcp_discovered_row` expander. Each
/// discovered server gets an `AdwActionRow` with a Connect button that
/// populates hostname/port and switches the source type.
///
/// The `Browser` handle is moved into the `timeout_add_local` closure
/// so it lives for the lifetime of the main context (= the app), and
/// mDNS discovery runs continuously whether or not the RTL-TCP source
/// is currently selected. That's fine — discovery is cheap and having
/// the list pre-populated when the user switches to RTL-TCP makes the
/// UX immediate instead of "wait 5 s for the first advertisement."
fn connect_rtl_tcp_discovery(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites_header: &FavoritesHeaderHandle,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
) {
    use std::collections::HashMap;
    use std::time::Instant;

    /// Grace window after which a server that has stopped
    /// re-announcing gets pruned from the UI list. A healthy mDNS
    /// responder re-announces well before its TTL (default 120 s on
    /// most daemons) expires; 3 minutes without a refresh means the
    /// responder is either dead or network-partitioned.
    ///
    /// Defense-in-depth: mdns-sd's daemon SHOULD fire
    /// `ServiceRemoved` on TTL expiry, but a crashed server that
    /// vanishes without a goodbye may leave the cache entry around
    /// longer than the client wants. Expiring client-side keeps the
    /// Connect button from offering a dead endpoint.
    const STALE_ROW_GRACE: std::time::Duration = std::time::Duration::from_mins(3);

    /// Poll cadence for the mDNS discovery event channel. 200 ms is
    /// fast enough that newly-announced servers appear "instantly" to
    /// the user and cheap enough to be always-on even when RTL-TCP is
    /// not the selected source type.
    const DISCOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

    /// Subtitle shown on the discovered-servers expander when mDNS
    /// discovery is non-functional (either `Browser::start` failed or
    /// the browser thread exited at runtime). Distinguishes "nothing
    /// to see yet" from "we gave up listening" — without this the UI
    /// would lie by showing the idle "No servers discovered…" state.
    const DISCOVERY_UNAVAILABLE_SUBTITLE: &str = "Discovery unavailable on this system.";

    // "Manage favorites…" button inside the discovered-servers
    // expander — a second entry point into the same popover as
    // the header-bar star button. Wired here because the
    // `MenuButton` whose `popup()` we trigger lives in the
    // header. Weak ref on the button keeps the closure drop-safe
    // if the header is torn down before the source panel (though
    // in practice the window owns both and they drop together).
    let favorites_menu_weak = favorites_header.button.downgrade();
    panels
        .source
        .manage_favorites_button
        .connect_clicked(move |_| {
            if let Some(btn) = favorites_menu_weak.upgrade() {
                // `MenuButton::popup` activates the attached
                // popover anchored to the menu button itself, so
                // the slide-out appears from the header regardless
                // of which entry point the user clicked.
                btn.popup();
            }
        });

    let (disc_tx, disc_rx) = mpsc::channel::<DiscoveryEvent>();
    // `Option<Browser>` — `None` on mDNS startup failure. We still
    // need the rest of this function to run so the *manually*-
    // persisted `last_connected` / favorites restore can repopulate
    // the client UI. Only the discovery poller is skipped in the
    // `None` branch (there'd be nothing to poll, and `disc_tx` is
    // already dropped so `disc_rx` would immediately return
    // `TryRecvError::Disconnected` and spin forever).
    let browser = match Browser::start(move |event| {
        // Ignore send errors — means the UI thread dropped the rx,
        // which only happens on shutdown.
        let _ = disc_tx.send(event);
    }) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(%e, "mDNS browser failed to start — discovery disabled");
            panels
                .source
                .rtl_tcp_discovered_row
                .set_subtitle(DISCOVERY_UNAVAILABLE_SUBTITLE);
            None
        }
    };

    // Tracks the `AdwActionRow` per-server so we can remove it on
    // `ServerWithdrawn` OR when the row goes stale past
    // `STALE_ROW_GRACE`. Keyed by full DNS-SD instance name (stable
    // across nickname changes). Value carries the row widget + the
    // last `DiscoveredServer` payload seen for that instance —
    // `server.last_seen` drives both staleness pruning and the
    // per-tick freshness indicator rendered in the row subtitle.
    let displayed_rows: Rc<RefCell<HashMap<String, (adw::ActionRow, DiscoveredServer)>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Auxiliary map: favorite_key (hostname:port) → weak ref on
    // the currently-rendered discovery-row star `ToggleButton`.
    // Let the favorites-popover Unstar handler find and flip the
    // matching discovery toggle immediately rather than waiting
    // for the next mDNS re-announce — without this, the filled
    // star would stay rendered while the map says otherwise, and
    // the first user click on the stale star would fire
    // `toggled` with `active=false` (wasted click from the
    // user's perspective: they wanted to re-pin).
    //
    // Weak refs only — the `ToggleButton`s are strongly owned by
    // their parent `AdwActionRow`s (as prefix widgets) which are
    // strongly owned by `displayed_rows`. Stale entries
    // (rows that have since been removed from `displayed_rows`)
    // fail to upgrade and self-clean at lookup time; no explicit
    // prune necessary at the <50-server scale this map is sized
    // for.
    let discovered_star_buttons: Rc<RefCell<HashMap<String, glib::WeakRef<gtk4::ToggleButton>>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Weak ref on the expander so the timeout closure doesn't keep
    // the window alive after close — upgrade() returns None on a
    // destroyed widget and the poller breaks out.
    let expander_weak = panels.source.rtl_tcp_discovered_row.downgrade();
    let hostname_row = panels.source.hostname_row.clone();
    let port_row = panels.source.port_row.clone();
    let protocol_row = panels.source.protocol_row.clone();
    let device_row = panels.source.device_row.clone();
    let role_row = panels.source.rtl_tcp_role_row.clone();
    let auth_key_row = panels.source.rtl_tcp_auth_key_row.clone();
    let state = Rc::clone(state);
    // Shared config handle — the Connect button on each discovered
    // row clones it once more inside the closure so it can persist
    // a `LastConnectedServer` snapshot on click.
    let config_for_discovery = std::sync::Arc::clone(config);

    // Favorites map — key (stable hostname:port) → rich
    // `FavoriteEntry` record. Created by the parent
    // `connect_sidebar_panels` so the role-picker handler in
    // `connect_source_panel` can mutate the SAME map this
    // function's re-announce path reads. Per CodeRabbit round 8
    // on PR #408: pre-fix the role-picker reloaded favorites
    // from disk, mutated a local `Vec`, and saved — a
    // later `ServerAnnounced` would preserve the stale
    // in-memory role from this map and clobber the just-saved
    // selection on next disk flush. Sharing keeps both paths
    // honest. The clone we hold here is a cheap `Rc::clone`; the
    // parent retains the original so the Arc-count stays > 0
    // for the lifetime of both handlers.
    let favorites = Rc::clone(favorites);

    // Weak refs to the favorites popover's contents. The star-
    // toggle closure (attached to each row's `ToggleButton`) and
    // the discovery poll timer both need to refresh the popover
    // when the favorites map mutates. Strong captures would create
    // the same closure-cycle pattern the #329 / #335 lessons
    // taught us to avoid — per-callback atomic upgrade + drop
    // keeps the popover widgets releasable on window close.
    let favorites_popover_weak = FavoritesPopoverWeak::from_header(favorites_header);
    // Bundle of per-row action dependencies. Built once, cloned
    // into the three rebuild call sites (startup seed, star
    // toggle, re-announce refresh). `rebuild_favorites_popover`
    // hands a clone to each row's Connect / Copy / Unstar
    // closure, so each button ends up with a single `Rc` clone
    // instead of nine weak-ref captures.
    let favorite_row_ctx: Rc<FavoriteRowContext> = Rc::new(FavoriteRowContext {
        popover: favorites_popover_weak.clone(),
        favorites: Rc::clone(&favorites),
        config: std::sync::Arc::clone(&config_for_discovery),
        state: Rc::clone(&state),
        hostname_row: hostname_row.downgrade(),
        port_row: port_row.downgrade(),
        protocol_row: protocol_row.downgrade(),
        device_row: device_row.downgrade(),
        role_row: role_row.downgrade(),
        auth_key_row: auth_key_row.downgrade(),
        expander_weak: expander_weak.clone(),
        // Weak refs — see `FavoriteRowContext.displayed_rows`
        // docstring for the retain-cycle reasoning.
        displayed_rows: Rc::downgrade(&displayed_rows),
        discovered_star_buttons: Rc::downgrade(&discovered_star_buttons),
    });
    // Seed the popover's content from the restored favorites so
    // the list is ready when the user first clicks the header
    // star, without waiting for a mutation to trigger a rebuild.
    rebuild_favorites_popover(&favorite_row_ctx, &favorites.borrow());

    // Rebuild on every popover show so the "seen Xm ago" subtitles
    // reflect current wall-clock time. Without this, the ages
    // captured by `format_favorite_subtitle` at startup / star
    // toggle / re-announce freeze between popover openings — a
    // user who closes the popover and reopens it 10 minutes later
    // would still see "seen just now" for servers that actually
    // went offline during that gap.
    //
    // `favorite_row_ctx.popover.popover` is the same weak ref the
    // per-row Connect closure uses to dismiss the popover, so no
    // new capture shape is introduced. The closure holds
    // `Rc<FavoriteRowContext>`; no retain cycle because
    // `FavoriteRowContext.popover` is weak.
    {
        let ctx_for_show = Rc::clone(&favorite_row_ctx);
        favorites_header.popover.connect_show(move |_| {
            rebuild_favorites_popover(&ctx_for_show, &ctx_for_show.favorites.borrow());
        });
    }

    // Populate the hostname / port fields on startup from the last
    // connected server, if any. Runs once before the poller starts
    // so the user sees "the server they were last on" immediately
    // instead of having to wait for a fresh mDNS beacon. No-op on
    // first launch / after a config reset.
    //
    // Protocol row is forced to TCP *before* the hostname / port
    // writes. Those writes fire `connect_changed` / `connect_value_
    // notify` handlers that re-read `protocol_row.selected()` and
    // dispatch `SetNetworkConfig { protocol: ... }`. If the shared
    // protocol row was restored to UDP from a prior raw-Network
    // session, the restore path would otherwise push a UDP
    // `SetNetworkConfig` against the RTL-TCP endpoint on the very
    // first tick. Pinning TCP first keeps the restore both silent
    // to the user and correct end-to-end.
    if let Some(last) = crate::sidebar::source_panel::load_last_connected(&config_for_discovery) {
        protocol_row.set_selected(NETWORK_PROTOCOL_TCPCLIENT_IDX);
        hostname_row.set_text(&last.host);
        port_row.set_value(f64::from(last.port));
    }

    // Poll the discovery channel from the main thread. Cheap enough
    // to be always-on; discovery events are bursty at start and then
    // idle.
    //
    // Gated on `Some(browser)` so we don't spawn a poller against a
    // dead `disc_rx` when mDNS startup failed. The
    // `DISCOVERY_UNAVAILABLE_SUBTITLE` set in the `Err` branch
    // stays on the expander as the long-term idle state; the
    // restore / favorites paths above already ran unconditionally.
    let Some(browser) = browser else {
        return;
    };
    let _ = glib::timeout_add_local(DISCOVERY_POLL_INTERVAL, move || {
        // Keep the Browser alive as long as the timeout closure is
        // attached.
        let _keep_browser = &browser;
        // If the window / expander has been destroyed, stop polling
        // and let the browser + closure captures drop. Prevents leaked
        // pollers after a hypothetical close-and-reopen of the main
        // window.
        let Some(expander) = expander_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // Prune stale rows before processing incoming events. A
        // responder that crashed or network-partitioned won't send
        // ServerWithdrawn, so without this pass the Connect button
        // for a dead server keeps showing until mDNS cache TTL fires
        // (if it fires at all). 3-minute grace is long enough that
        // a healthy responder's re-announce keeps its row alive.
        {
            let mut rows = displayed_rows.borrow_mut();
            let now = Instant::now();
            let stale_names: Vec<String> = rows
                .iter()
                .filter(|(_, (_, server))| {
                    now.saturating_duration_since(server.last_seen) > STALE_ROW_GRACE
                })
                .map(|(name, _)| name.clone())
                .collect();
            for name in stale_names {
                if let Some((row, _)) = rows.remove(&name) {
                    tracing::debug!(instance = %name, "pruning stale rtl_tcp discovery row");
                    expander.remove(&row);
                }
            }
            // Refresh each surviving row's subtitle with a fresh
            // "seen N ago" stamp. Without this per-tick refresh the
            // age text would freeze at whatever it said when the row
            // was built (or last re-announced) and silently mislead
            // the user about how recent a server is. GTK short-
            // circuits the set_subtitle call when the string is
            // unchanged, so this is nearly free on quiescent rows.
            for (row, server) in rows.values() {
                let elapsed = now.saturating_duration_since(server.last_seen);
                row.set_subtitle(&format_discovery_subtitle(server, elapsed));
            }
            if rows.is_empty() {
                expander.set_subtitle("No servers discovered on the local network yet.");
            } else {
                expander.set_subtitle(&format!("{} server(s) visible", rows.len()));
            }
        }

        loop {
            let event = match disc_rx.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Browser thread exited — `disc_tx` dropped. Stop
                    // polling and surface the degraded state; without
                    // the Break this timeout would spin forever and
                    // the UI would keep claiming "No servers
                    // discovered yet" when we've in fact given up.
                    tracing::warn!(
                        "mDNS discovery channel disconnected — stopping discovery poller"
                    );
                    // Drain any previously announced rows before we
                    // break out. Without this, they'd linger in the
                    // expander indefinitely — no more
                    // `ServerWithdrawn` events will arrive, and the
                    // stale-age pruner at the top of the tick is
                    // also about to stop firing. Users would see
                    // rows that look Connect-able for endpoints
                    // the UI has already declared unavailable.
                    let mut rows = displayed_rows.borrow_mut();
                    for (_, (row, _)) in rows.drain() {
                        expander.remove(&row);
                    }
                    drop(rows);
                    expander.set_subtitle(DISCOVERY_UNAVAILABLE_SUBTITLE);
                    return glib::ControlFlow::Break;
                }
            };
            match event {
                DiscoveryEvent::ServerAnnounced(server) => {
                    let mut rows = displayed_rows.borrow_mut();
                    let title = if server.txt.nickname.is_empty() {
                        server.instance_name.clone()
                    } else {
                        server.txt.nickname.clone()
                    };
                    // Identity host — the advertised mDNS
                    // hostname, matching `favorite_key(&server)`.
                    // `apply_rtl_tcp_connect` uses its `host`
                    // argument as the stable id for
                    // `rtl_tcp_active_server`, keyring lookups,
                    // favorite matches, and
                    // `LastConnectedServer`. Pre-`CodeRabbit`
                    // round 6 on PR #408 this preferred
                    // `server.addresses.first()` (a resolved
                    // IPv4/IPv6 literal when mDNS had resolved
                    // one), which split per-server state
                    // between `shack-pi.local.:1234` (what
                    // favorites store) and `192.168.1.17:1234`
                    // (what the discovery connect path
                    // persisted) — role / auth round-tripping
                    // through discovery + favorites + startup
                    // restore broke silently. The DSP's actual
                    // dial path (`RtlTcpSource::with_config` →
                    // `(host, port).to_socket_addrs()`) resolves
                    // the hostname at connect time, so keeping
                    // identity on the advertised name is
                    // strictly better: stable across IP
                    // changes AND correct by the
                    // favorite-key contract.
                    let host = server.hostname.clone();
                    // Age is effectively 0 here — `server.last_seen` was
                    // stamped by the browser thread a few ms ago —
                    // `format_age` will render "just now". Subsequent
                    // poll ticks refresh this with the actual age.
                    let elapsed = Instant::now().saturating_duration_since(server.last_seen);
                    let subtitle = format_discovery_subtitle(&server, elapsed);

                    // Re-announce for a known instance_name: remove the
                    // old row and fall through to build a fresh one.
                    // Rebuilding captures the current (host, port) in
                    // the new Connect closure; otherwise the stale
                    // values from first-announce would stick. See the
                    // displayed_rows docstring above.
                    if let Some((existing_row, _)) = rows.remove(&server.instance_name) {
                        expander.remove(&existing_row);
                    }

                    let row = adw::ActionRow::builder()
                        .title(&title)
                        .subtitle(&subtitle)
                        .build();

                    // Star toggle — prefix icon, pinning this
                    // server to the top of the discovered list and
                    // persisting the choice across app launches.
                    // Using the outlined / filled star icon pair
                    // so the toggle state reads clearly without
                    // extra CSS.
                    let star_btn = gtk4::ToggleButton::builder()
                        .icon_name(FAVORITE_ICON_OUTLINE)
                        .valign(gtk4::Align::Center)
                        .css_classes(["flat"])
                        .tooltip_text("Pin as favorite")
                        .build();
                    // Use the stable hostname+port key, not
                    // `instance_name`. `instance_name` comes from
                    // the server's TXT nickname, which the operator
                    // can edit — keying favorites off it would
                    // silently drop the star on any rename.
                    let star_key = favorite_key(&server);
                    let starred_initially = favorites.borrow().contains_key(&star_key);
                    star_btn.set_active(starred_initially);
                    if starred_initially {
                        star_btn.set_icon_name(FAVORITE_ICON_FILLED);
                    }
                    // Initial accessible name — state-dependent so
                    // screen readers announce the action the click
                    // will take, not the icon's current appearance.
                    // Updated again inside the toggle closure when
                    // the user flips the state.
                    set_favorite_toggle_accessible_name(&star_btn, starred_initially);
                    // Register the star_btn against its
                    // favorite_key so the favorites-popover
                    // Unstar handler can find and flip this
                    // exact toggle when the user unstars from
                    // the popover. `insert` overwrites any
                    // prior (stale) weak ref under the same key
                    // — e.g. from a re-announce rebuild of the
                    // row, where the old button was dropped.
                    let star_key_for_map = favorite_key(&server);
                    discovered_star_buttons
                        .borrow_mut()
                        .insert(star_key_for_map, star_btn.downgrade());
                    // Capture the display metadata into move-able
                    // values so the toggle closure can build a
                    // `FavoriteEntry` without holding onto
                    // `server` (which is consumed by the HashMap
                    // insert further down).
                    let star_nickname = if server.txt.nickname.is_empty() {
                        server.instance_name.clone()
                    } else {
                        server.txt.nickname.clone()
                    };
                    let star_tuner_name = Some(server.txt.tuner.clone());
                    let star_gain_count = Some(server.txt.gains);
                    // Capture the announce-derived auth flag so
                    // a fresh star persists it alongside the
                    // rest of the metadata. Pre-`CodeRabbit`
                    // round 6 on PR #408 this was hard-set to
                    // `None` at star time, which meant a newly-
                    // starred auth-required server looked
                    // "unknown" until the next mDNS refresh —
                    // `apply_rtl_tcp_connect` + the startup
                    // restore wouldn't reveal the key row
                    // ahead of the first `AuthRequired` bounce.
                    // The discovery-refresh path below already
                    // writes `server.txt.auth_required` on re-
                    // announce; this keeps the two entry points
                    // consistent so freshly-starred favorites
                    // carry the same hint as refreshed ones.
                    let star_auth_required = server.txt.auth_required;
                    let star_favorites = Rc::clone(&favorites);
                    let star_config = std::sync::Arc::clone(&config_for_discovery);
                    let star_expander_weak = expander_weak.clone();
                    // Closure captures `star_row_ctx` only — reaches
                    // `displayed_rows` via its `Weak` field inside.
                    // A separate `Rc::clone(&displayed_rows)` capture
                    // here would reintroduce the retain cycle the
                    // `FavoriteRowContext.displayed_rows` docstring
                    // describes (map → row → signal → ctx → map).
                    let star_row_ctx = Rc::clone(&favorite_row_ctx);
                    star_btn.connect_toggled(move |btn| {
                        let active = btn.is_active();
                        btn.set_icon_name(if active {
                            FAVORITE_ICON_FILLED
                        } else {
                            FAVORITE_ICON_OUTLINE
                        });
                        // Keep the accessible name in sync with
                        // the new state so AT announces the next
                        // action ("Unpin from favorites" after the
                        // user just pinned it, and vice versa).
                        set_favorite_toggle_accessible_name(btn, active);
                        {
                            let mut favs = star_favorites.borrow_mut();
                            if active {
                                // Build a fresh entry with the
                                // current metadata. Replaces any
                                // older entry with the same key
                                // (= metadata refresh on re-star).
                                favs.insert(
                                    star_key.clone(),
                                    sidebar::source_panel::FavoriteEntry {
                                        key: star_key.clone(),
                                        nickname: star_nickname.clone(),
                                        tuner_name: star_tuner_name.clone(),
                                        gain_count: star_gain_count,
                                        last_seen_unix: Some(
                                            sidebar::source_panel::now_unix_seconds(),
                                        ),
                                        // Fresh star — no role preference
                                        // yet; `auth_required` is captured
                                        // from the current mDNS announce's
                                        // TXT record above so
                                        // `apply_rtl_tcp_connect` + the
                                        // startup restore can pre-reveal
                                        // the key row immediately, without
                                        // waiting on a mDNS re-announce.
                                        // Per `CodeRabbit` round 6 on
                                        // PR #408 and issue #396.
                                        requested_role: None,
                                        auth_required: star_auth_required,
                                    },
                                );
                            } else {
                                favs.remove(&star_key);
                            }
                            // Persist immediately. Order within
                            // the persisted list is unspecified —
                            // the slide-out sorts on read.
                            let snapshot: Vec<sidebar::source_panel::FavoriteEntry> =
                                favs.values().cloned().collect();
                            crate::sidebar::source_panel::save_favorites(&star_config, &snapshot);
                        }
                        // Rebuild the expander so the row moves
                        // to/from the top per the new favorite
                        // state. Reuses the `displayed_rows` map
                        // (strong refs on the AdwActionRow
                        // widgets) — ordering is the only thing
                        // that changes. The map is held Weak via
                        // `FavoriteRowContext`; upgrade fails
                        // silently if the discovery timer has
                        // already torn down, which means there's
                        // nothing to reorder anyway.
                        if let (Some(expander), Some(rows)) = (
                            star_expander_weak.upgrade(),
                            star_row_ctx.displayed_rows.upgrade(),
                        ) {
                            reorder_discovered_rows(
                                &expander,
                                &rows.borrow(),
                                &star_favorites.borrow(),
                            );
                        }
                        // Refresh the header-bar favorites popover
                        // so the star-toggle reflects there too.
                        // Upgrade-and-drop inside the rebuild keeps
                        // the closure leak-free per the #329
                        // weak-ref pattern.
                        rebuild_favorites_popover(&star_row_ctx, &star_favorites.borrow());
                    });
                    row.add_prefix(&star_btn);

                    let connect_btn = gtk4::Button::with_label("Connect");
                    connect_btn.add_css_class("suggested-action");
                    connect_btn.set_valign(gtk4::Align::Center);

                    let click_host = host.clone();
                    let click_port = server.port;
                    let hr = hostname_row.clone();
                    let pr = port_row.clone();
                    let protor = protocol_row.clone();
                    let dr = device_row.clone();
                    let rr = role_row.clone();
                    let akr = auth_key_row.clone();
                    let st = Rc::clone(&state);
                    let cfg = std::sync::Arc::clone(&config_for_discovery);
                    // Friendly nickname for the persisted snapshot.
                    // Prefer the TXT nickname if the responder set
                    // one, fall back to the DNS-SD instance name.
                    let click_nickname = if server.txt.nickname.is_empty() {
                        server.instance_name.clone()
                    } else {
                        server.txt.nickname.clone()
                    };
                    connect_btn.connect_clicked(move |_| {
                        // Shared ordering-sensitive flow lives in
                        // `apply_rtl_tcp_connect` — see its doc for
                        // why `protocol_row` gets set to TCP before
                        // the host/port writes and why
                        // `SetSourceType` only fires conditionally.
                        apply_rtl_tcp_connect(
                            &click_host,
                            click_port,
                            &click_nickname,
                            &hr,
                            &pr,
                            &protor,
                            &dr,
                            &rr,
                            &akr,
                            &st,
                            &cfg,
                        );
                    });
                    row.add_suffix(&connect_btn);
                    expander.add_row(&row);
                    // If this server is already favorited, refresh
                    // the persisted metadata (tuner name, gain
                    // count, nickname, last-seen) off the fresh
                    // announce. Keeps the favorites slide-out's
                    // display honest when the user revisits it
                    // after the server has been renamed /
                    // re-announced with updated TXT records.
                    let fav_key = favorite_key(&server);
                    {
                        let mut favs = favorites.borrow_mut();
                        if favs.contains_key(&fav_key) {
                            let refreshed_nickname = if server.txt.nickname.is_empty() {
                                server.instance_name.clone()
                            } else {
                                server.txt.nickname.clone()
                            };
                            // Preserve any saved `requested_role`
                            // from the previous favorites entry (the
                            // user's last pick sticks across
                            // re-announces); refresh the
                            // `auth_required` hint from the incoming
                            // TXT so the UI reveals the key field
                            // BEFORE the user clicks Connect. Per #396.
                            let preserved_role = favs.get(&fav_key).and_then(|f| f.requested_role);
                            favs.insert(
                                fav_key.clone(),
                                sidebar::source_panel::FavoriteEntry {
                                    key: fav_key.clone(),
                                    nickname: refreshed_nickname,
                                    tuner_name: Some(server.txt.tuner.clone()),
                                    gain_count: Some(server.txt.gains),
                                    last_seen_unix: Some(sidebar::source_panel::now_unix_seconds()),
                                    requested_role: preserved_role,
                                    auth_required: server.txt.auth_required,
                                },
                            );
                            let snapshot: Vec<sidebar::source_panel::FavoriteEntry> =
                                favs.values().cloned().collect();
                            crate::sidebar::source_panel::save_favorites(
                                &config_for_discovery,
                                &snapshot,
                            );
                            // Refresh the header-bar popover's
                            // rendering of this entry (age + tuner
                            // metadata). Cheap — it rebuilds the
                            // whole list but at favorites scale
                            // that's trivial.
                            rebuild_favorites_popover(&favorite_row_ctx, &favs);
                        }
                    }
                    rows.insert(server.instance_name.clone(), (row, server));
                    // Reorder after insert so favorites float to
                    // the top of the new view.
                    reorder_discovered_rows(&expander, &rows, &favorites.borrow());

                    expander.set_subtitle(&format!("{} server(s) visible", rows.len()));
                }
                DiscoveryEvent::ServerWithdrawn { instance_name } => {
                    let mut rows = displayed_rows.borrow_mut();
                    if let Some((row, _)) = rows.remove(&instance_name) {
                        expander.remove(&row);
                    }
                    if rows.is_empty() {
                        expander.set_subtitle("No servers discovered on the local network yet.");
                    } else {
                        expander.set_subtitle(&format!("{} server(s) visible", rows.len()));
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Cadence for the USB hotplug poll that drives server-panel
/// visibility. 3 s is the sweet spot: fast enough that a user
/// plugging in a dongle sees the panel within the time it takes them
/// to reach the sidebar with the mouse, slow enough that the
/// per-tick `rusb::devices()` USB-bus enumerate is invisible in
/// profile traces.
const SERVER_PANEL_HOTPLUG_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Icon name for the un-filled ("not pinned") star on discovery
/// rows. GNOME Symbolic icon set — `non-starred-symbolic` renders
/// the outline glyph, which is visually distinct from the filled
/// pinned state so the affordance reads clearly without relying
/// on the `ToggleButton::is_active` styling alone.
const FAVORITE_ICON_OUTLINE: &str = "non-starred-symbolic";
/// Icon name for the filled ("pinned") star. Paired with
/// `FAVORITE_ICON_OUTLINE` so toggling swaps the glyph, not just
/// the button chrome.
const FAVORITE_ICON_FILLED: &str = "starred-symbolic";

/// Stable persistence key for a discovered server's favorite
/// state. We key by **advertised hostname + port**, not by the
/// DNS-SD `instance_name`, because `instance_name` is derived
/// from the user-editable TXT nickname — renaming the server
/// would silently drop the saved favorite on the next announce.
/// Hostname is the machine's mDNS identity (e.g. `shack-pi.local.`)
/// which stays put across nickname changes; paired with port it's
/// unique enough that two servers on the same host (different
/// ports) remain distinct favorites. A full machine rename breaks
/// the favorite — acceptable, since a rename semantically IS a
/// different host.
fn favorite_key(server: &DiscoveredServer) -> String {
    format!("{}:{}", server.hostname, server.port)
}

/// Order favorites for popover display: primary key lowercased
/// nickname (alphabetical, case-insensitive), secondary key the
/// stable `FavoriteEntry.key` (hostname:port).
///
/// The secondary key is load-bearing — `HashMap::values()`
/// iteration order is non-deterministic, and two favorites with
/// the same nickname would otherwise reshuffle across inserts /
/// removals / app restarts (tie-broken by whatever the hash
/// state happened to be that tick). Tying to `key` pins the
/// order across all three.
fn sort_favorites_for_display(entries: &mut [&sidebar::source_panel::FavoriteEntry]) {
    entries.sort_by(|a, b| {
        a.nickname
            .to_lowercase()
            .cmp(&b.nickname.to_lowercase())
            .then_with(|| a.key.cmp(&b.key))
    });
}

/// Update the `GtkAccessible` `Label` on the discovery-row star
/// toggle. The label describes the action the next click will
/// take (NOT the icon's current appearance), so a screen reader
/// announces "Unpin from favorites" when the row is currently
/// pinned and "Pin as favorite" when it isn't. Called once at
/// row-build time and again inside the toggled closure so the
/// name stays in sync with state.
fn set_favorite_toggle_accessible_name(btn: &gtk4::ToggleButton, is_favorite: bool) {
    let label = if is_favorite {
        "Unpin from favorites"
    } else {
        "Pin as favorite"
    };
    btn.update_property(&[gtk4::accessible::Property::Label(label)]);
}

/// Execute the shared RTL-TCP connect sequence — used by both the
/// discovery-row Connect button and the favorites-popover Connect
/// button. Centralizes the ordering-sensitive steps so a future
/// fix can't land on one caller and miss the other:
///
/// 1. **Snapshot** `already_rtl_tcp` before touching `device_row`.
///    If the selector was ALREADY on RTL-TCP, `set_selected` is a
///    no-op and the device-row notify handler won't fire — we
///    need to dispatch `SetSourceType` ourselves to force the
///    controller to reopen the source against the new endpoint.
///    If it was on a different source type, the notify handler
///    fires and dispatches `SetSourceType` for us; an explicit
///    send here would double-open.
///
/// 2. **Pin TCP** on `protocol_row` BEFORE writing host / port.
///    `hostname_row.set_text` and `port_row.set_value` fire
///    change handlers that re-read `protocol_row.selected()` to
///    build their `SetNetworkConfig`. If the shared protocol row
///    is still on UDP from a prior raw-Network session, those
///    handlers would dispatch a stale-UDP config against the
///    clicked endpoint before the RTL-TCP switch lands — a
///    transient retarget of any live raw-Network source. `rtl_tcp`
///    is always TCP, so we force TCP unconditionally.
///
/// 3. **Write host / port**, flip `device_row` to RTL-TCP, dispatch
///    the fresh `SetNetworkConfig`, persist a `LastConnectedServer`
///    snapshot so next launch pre-populates the fields without
///    waiting for mDNS.
///
/// 4. **Conditionally** dispatch `SetSourceType(RtlTcp)` — only when
///    `already_rtl_tcp` was true (step 1's rationale).
///
/// Caller-owned follow-ups (popover `popdown`, etc.) happen after
/// this helper returns.
#[allow(
    clippy::too_many_arguments,
    reason = "each arg is a distinct widget / state handle the caller owns in its own shape (strong Rc clone vs weak-upgraded strong). Bundling into a struct would duplicate FavoriteRowContext for the favorites caller and invent a mirror struct for the discovery caller, trading argument count for two near-identical shim types."
)]
fn apply_rtl_tcp_connect(
    host: &str,
    port: u16,
    nickname: &str,
    hostname_row: &adw::EntryRow,
    port_row: &adw::SpinRow,
    protocol_row: &adw::ComboRow,
    device_row: &adw::ComboRow,
    role_row: &adw::ComboRow,
    auth_key_row: &adw::PasswordEntryRow,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use crate::sidebar::source_panel::{
        FavoriteRole, KEY_RTL_TCP_CLIENT_LAST_ROLE, RTL_TCP_ROLE_CONTROL_IDX,
        RTL_TCP_ROLE_LISTEN_IDX, load_favorites,
    };

    let already_rtl_tcp = device_row.selected() == DEVICE_RTLTCP;
    protocol_row.set_selected(NETWORK_PROTOCOL_TCPCLIENT_IDX);
    hostname_row.set_text(host);
    port_row.set_value(f64::from(port));
    // Restore saved per-server state (#396) BEFORE the
    // `SetNetworkConfig` / `SetSourceType` dispatch so the DSP
    // thread's first use of the new endpoint already carries the
    // right `requested_role` + `auth_key`. Pre-CodeRabbit round 1
    // on PR #408 this helper only pushed host / port / source,
    // which meant the new favorite metadata (`requested_role`,
    // `auth_required`) and per-server client-key keyring helpers
    // were inert from the discovery + favorites entry points —
    // role always reverted to the global default and keys never
    // auto-filled.
    //
    // Resolution order for role:
    // - If the server is a favorite and that favorite carries a
    //   `requested_role`, use it.
    // - Otherwise fall back to the global
    //   `KEY_RTL_TCP_CLIENT_LAST_ROLE` default (if any).
    // - Otherwise leave the picker alone (Control is the
    //   picker's built-in default for fresh servers).
    //
    // For the auth-key row:
    // - Reveal the row if the favorite's `auth_required` is
    //   `Some(true)` — user doesn't have to hit an
    //   `AuthRequired` denial before seeing the field.
    // - Load any saved keyring hex for this `host:port` and
    //   pre-fill the row so the subsequent connect succeeds in
    //   a single `Connecting → Connected` hop.
    //
    // Both operations are no-ops for servers we've never
    // favorited AND never connected to; the picker stays on
    // Control and the row stays hidden, matching pre-#408
    // behavior.
    // Stable-id rule (per CodeRabbit round 2 on PR #408): all
    // per-server state — keyring entries, favorite matches,
    // `app_state.rtl_tcp_active_server` — keys off the
    // *advertised* `hostname:port`, the same form
    // `favorite_key(server)` produces on mDNS announce. The
    // `host` param threaded into this helper already is that
    // stable value (discovery + favorites both pass the
    // advertised hostname, not a resolved IP), so we build the
    // key from it directly rather than reading it back from
    // `hostname_row.text()` — the row carries the dial target
    // the DSP actually connects to, which could be a resolved
    // IP or an IPv6 literal and would split identity between
    // "favorite shack-pi.local.:1234" and "resolved
    // 192.168.1.17:1234". Cache it on `AppState` so the
    // subsequent auth-flow helpers (`save_current_auth_key_for_
    // active_server`, the keyring-clear on `AuthFailed`, the
    // role-picker's per-favorite update) use this same stable
    // id without re-reading the widget.
    let server_key = format!("{host}:{port}");
    state
        .rtl_tcp_active_server
        .borrow_mut()
        .clone_from(&server_key);
    let favorite_entry = load_favorites(config)
        .into_iter()
        .find(|f| f.key == server_key);
    let favorite_role = favorite_entry
        .as_ref()
        .and_then(|f| f.requested_role)
        .or_else(|| {
            config.read(|v| {
                v.get(KEY_RTL_TCP_CLIENT_LAST_ROLE)
                    .and_then(|rv| serde_json::from_value::<FavoriteRole>(rv.clone()).ok())
            })
        });
    // Always set the role explicitly — never leave the combo
    // showing whatever a prior favorite-restore put there. Pre-
    // `CodeRabbit` round 9 on PR #408 this was `if let Some(
    // fav_role) = favorite_role { ... }`, so a fresh server
    // with no per-favorite role and no global
    // `KEY_RTL_TCP_CLIENT_LAST_ROLE` would silently inherit
    // whatever `Listen` a previous favorite had set — meaning
    // the first connect against a never-seen server could
    // accidentally request Listener instead of the legacy-safe
    // Control default. `unwrap_or(Control)` forces the picker
    // to the right default every time `apply_rtl_tcp_connect`
    // runs.
    let resolved_role = favorite_role.unwrap_or(FavoriteRole::Control);
    let idx = match resolved_role {
        FavoriteRole::Control => RTL_TCP_ROLE_CONTROL_IDX,
        FavoriteRole::Listen => RTL_TCP_ROLE_LISTEN_IDX,
    };
    role_row.set_selected(idx);
    // Auth-row state is driven by two inputs:
    // - `auth_required = Some(true)` on the favorite → the
    //   server advertises a required key, so reveal the row so
    //   the user can enter one (or see a saved one below) BEFORE
    //   the first connect lands — saves the
    //   `AuthRequired` bounce.
    // - A saved key in the per-server keyring → pre-fill the
    //   hex representation so a pre-configured auth connect
    //   succeeds in a single `Connecting → Connected` hop.
    //
    // Pre-CodeRabbit round 2 on PR #408 each of these was a
    // positive-only mutation: on the "no auth / no saved key"
    // path the row kept whatever visibility and text the
    // previous server left behind, so switching from
    // auth-required server A to no-auth server B would leak
    // A's revealed row + pre-filled key bytes into B — the
    // next connect would dispatch `SetRtlTcpClientConfig` with
    // A's key bound to B's endpoint. Now we rewrite both fields
    // deterministically: `set_visible(should_reveal)` and
    // `set_text(saved_hex_or_empty)` fire on every call.
    let has_auth_required = matches!(
        favorite_entry.as_ref().and_then(|f| f.auth_required),
        Some(true)
    );
    let saved_key_bytes = load_client_auth_key_from_keyring(host, port);
    let should_reveal = has_auth_required || saved_key_bytes.is_some();
    auth_key_row.set_visible(should_reveal);
    if let Some(bytes) = saved_key_bytes {
        auth_key_row.set_text(&crate::sidebar::server_panel::auth_key_to_hex(&bytes));
    } else {
        auth_key_row.set_text("");
    }
    // Dispatch a fresh `SetRtlTcpClientConfig` so the DSP
    // thread has the restored role + key in place before the
    // `SetNetworkConfig` + `SetSourceType` below trigger the
    // actual handshake. Without this the DSP would use its
    // last-known values (possibly stale from a prior server)
    // and the first connect could land with the wrong role or
    // a dead auth key from another session.
    // Transient out-of-range ComboRow indices fall back to
    // Control — the legacy-safe default. Collapsed with the
    // explicit Control arm since both produce the same
    // `FavoriteRole::Control`.
    let requested_role = match role_row.selected() {
        RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
        _ => FavoriteRole::Control,
    }
    .as_wire_role();
    let key_text = auth_key_row.text().to_string();
    let auth_key: Option<Vec<u8>> = if key_text.is_empty() {
        None
    } else {
        crate::sidebar::server_panel::auth_key_from_hex(&key_text)
    };
    state.send_dsp(UiToDsp::SetRtlTcpClientConfig {
        requested_role,
        auth_key,
    });
    device_row.set_selected(DEVICE_RTLTCP);
    state.send_dsp(UiToDsp::SetNetworkConfig {
        hostname: host.to_string(),
        port,
        protocol: sdr_types::Protocol::TcpClient,
    });
    crate::sidebar::source_panel::save_last_connected(
        config,
        &crate::sidebar::source_panel::LastConnectedServer {
            host: host.to_string(),
            port,
            nickname: nickname.to_string(),
        },
    );
    if already_rtl_tcp {
        state.send_dsp(UiToDsp::SetSourceType(SourceType::RtlTcp));
    }
}

/// Re-add rows to an `AdwExpanderRow` in a deterministic order:
/// favorites (alphabetical by instance name) first, then
/// non-favorites (same alpha order). Called after any mutation
/// that could change the sort — new announce, favorite toggle —
/// so the user's pinned entries stay glued to the top. GTK4 gives
/// us no in-place reorder API for expander children, so we
/// remove-and-re-add. At the expected scale (<50 servers on any
/// realistic LAN) the reparenting is invisible.
fn reorder_discovered_rows(
    expander: &adw::ExpanderRow,
    rows: &std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>,
    favorites: &std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>,
) {
    // Remove every row from the expander — widgets live in the
    // HashMap, so no drop happens.
    for (row, _) in rows.values() {
        expander.remove(row);
    }
    // Sort keys: favorites first, then alpha. Favorite check goes
    // through `favorite_key(server)` (hostname+port) so it matches
    // what the star-toggle persists. Alpha tiebreak uses the
    // `instance_name` (HashMap key) so rendering order stays
    // predictable across re-announces.
    let mut keys: Vec<&String> = rows.keys().collect();
    keys.sort_by(|a, b| {
        let a_fav = rows
            .get(a.as_str())
            .is_some_and(|(_, srv)| favorites.contains_key(&favorite_key(srv)));
        let b_fav = rows
            .get(b.as_str())
            .is_some_and(|(_, srv)| favorites.contains_key(&favorite_key(srv)));
        match (a_fav, b_fav) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });
    for key in keys {
        if let Some((row, _)) = rows.get(key) {
            expander.add_row(row);
        }
    }
}

/// Weak references to the widgets inside the header-bar favorites
/// popover. The discovery-flow closures (star toggles, re-announce
/// refresh) refresh popover contents whenever the favorites map
/// mutates; strong captures here would hold the list / label / popover
/// alive for the closure's lifetime, defeating window-close
/// cleanup. Same per-tick-upgrade pattern established in
/// `ServerStatusWidgetsWeak` on #329.
///
/// `Clone` so we can hand a copy to each per-row action closure;
/// `glib::WeakRef` is Rc-like internally, so cloning is cheap.
#[derive(Clone)]
struct FavoritesPopoverWeak {
    list: glib::WeakRef<gtk4::ListBox>,
    empty_label: glib::WeakRef<gtk4::Label>,
    popover: glib::WeakRef<gtk4::Popover>,
}

impl FavoritesPopoverWeak {
    fn from_header(handle: &FavoritesHeaderHandle) -> Self {
        Self {
            list: handle.list.downgrade(),
            empty_label: handle.empty_label.downgrade(),
            popover: handle.popover.downgrade(),
        }
    }
}

/// Bundle of dependencies that per-row action closures (Connect /
/// Copy / Unstar) need to capture. Passed by `Rc<FavoriteRowContext>`
/// through `rebuild_favorites_popover` and `attach_favorite_row_actions`
/// so each row-button closure only clones the `Rc` instead of
/// re-capturing nine individual weak refs. All widget handles are
/// `glib::WeakRef` to keep the closures leak-free per the
/// `ServerStatusWidgetsWeak` pattern on #329.
///
/// `displayed_rows` is stored as `std::rc::Weak` specifically to
/// break a retain cycle: the `AdwActionRow` values inside the map
/// own their `connect_toggled` / `connect_clicked` closures, and
/// those closures capture this `FavoriteRowContext`. A strong
/// `Rc<RefCell<HashMap<...>>>` here would close the loop (map →
/// row → signal closure → context → map) and keep the widgets
/// alive past window close. The primary owner of the map — the
/// discovery-polling `glib::timeout_add_local` timer — retains
/// the strong `Rc`, so the upgrade at use-time is reliable while
/// the timer is running and correctly fails when it isn't.
struct FavoriteRowContext {
    popover: FavoritesPopoverWeak,
    favorites: Rc<RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>>,
    config: std::sync::Arc<sdr_config::ConfigManager>,
    state: Rc<AppState>,
    hostname_row: glib::WeakRef<adw::EntryRow>,
    port_row: glib::WeakRef<adw::SpinRow>,
    protocol_row: glib::WeakRef<adw::ComboRow>,
    device_row: glib::WeakRef<adw::ComboRow>,
    /// Role picker — `apply_rtl_tcp_connect` needs it so the
    /// per-server `requested_role` can be restored before
    /// the new endpoint's first connect dispatch. Per
    /// `CodeRabbit` round 1 on PR #408.
    role_row: glib::WeakRef<adw::ComboRow>,
    /// Auth-key row — `apply_rtl_tcp_connect` reveals it
    /// when the favorite advertises `auth_required` and
    /// pre-fills any saved key from the keyring so a
    /// pre-configured auth connect lands in a single
    /// `Connecting → Connected` hop. Per `CodeRabbit` round 1
    /// on PR #408.
    auth_key_row: glib::WeakRef<adw::PasswordEntryRow>,
    expander_weak: glib::WeakRef<adw::ExpanderRow>,
    displayed_rows: std::rc::Weak<
        RefCell<std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>>,
    >,
    /// Keyed by `favorite_key(server)` (hostname:port), maps to
    /// a weak ref on the star `ToggleButton` in the currently-
    /// rendered discovery row for that server (if any). Weak
    /// here for the same retain-cycle reason as `displayed_rows`:
    /// the per-row Unstar closure captures this context, and a
    /// strong `Rc` field would close the loop back through the
    /// inner `WeakRef`s to the rows themselves.
    discovered_star_buttons: std::rc::Weak<
        RefCell<std::collections::HashMap<String, glib::WeakRef<gtk4::ToggleButton>>>,
    >,
}

/// Clear the `ListBox` and rebuild one row per `FavoriteEntry`,
/// sorted alphabetically by nickname. Toggles the empty-state
/// label visibility so the popover reads cleanly in both the
/// no-favorites and has-favorites states.
///
/// Silent no-op when either popover widget is gone (window torn
/// down). Each row gets Connect / Copy / Unstar suffix buttons via
/// `attach_favorite_row_actions`.
fn rebuild_favorites_popover(
    ctx: &Rc<FavoriteRowContext>,
    favorites: &std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>,
) {
    let (Some(list), Some(empty)) = (
        ctx.popover.list.upgrade(),
        ctx.popover.empty_label.upgrade(),
    ) else {
        return;
    };
    // Clear existing rows. `ListBox::remove` detaches without
    // dropping the widgets past us — the HashMap has already
    // gone through its mutation above this call.
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let has_any = !favorites.is_empty();
    empty.set_visible(!has_any);
    list.set_visible(has_any);
    if !has_any {
        return;
    }
    let now = sidebar::source_panel::now_unix_seconds();
    let mut entries: Vec<&sidebar::source_panel::FavoriteEntry> = favorites.values().collect();
    sort_favorites_for_display(&mut entries);
    for entry in entries {
        let row = adw::ActionRow::builder()
            .title(&entry.nickname)
            .subtitle(format_favorite_subtitle(entry, now))
            .activatable(false)
            .build();
        attach_favorite_row_actions(&row, entry, ctx);
        list.append(&row);
    }
}

/// Build the three suffix buttons on a favorites-popover row:
/// Connect (suggested-action, pins TCP + dispatches to DSP), Copy
/// (writes `host:port` to the clipboard), and Unstar (removes from
/// favorites, persists, reorders discovery, rebuilds the popover).
///
/// Dependencies flow through `FavoriteRowContext` so each closure
/// only clones the `Rc` — not nine individual weak refs. The
/// Connect-button ordering (`protocol_row.set_selected(TCP)`
/// BEFORE `hostname_row.set_text` / `port_row.set_value`) mirrors
/// the discovery-row Connect handler established in PR #335: the
/// hostname / port writes fire change handlers that read the
/// protocol row, so the row must already be on TCP or those
/// handlers will dispatch a stale-UDP `SetNetworkConfig`.
fn attach_favorite_row_actions(
    row: &adw::ActionRow,
    entry: &sidebar::source_panel::FavoriteEntry,
    ctx: &Rc<FavoriteRowContext>,
) {
    // Connect button — pins TCP, loads host/port, switches to RTL-TCP.
    let connect_btn = gtk4::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
    connect_btn.set_valign(gtk4::Align::Center);
    let connect_ctx = Rc::clone(ctx);
    let connect_key = entry.key.clone();
    let connect_nickname = entry.nickname.clone();
    connect_btn.connect_clicked(move |_| {
        let Some((host, port)) = parse_host_port(&connect_key) else {
            // Corrupt key shouldn't happen in practice —
            // `favorite_key(server)` always produces
            // `hostname:port`. Log rather than silently dropping
            // the click, so a future schema drift is discoverable.
            tracing::warn!(
                key = %connect_key,
                "favorites popover: Connect clicked on un-parseable key, ignoring",
            );
            return;
        };
        let (
            Some(hostname_row),
            Some(port_row),
            Some(protocol_row),
            Some(device_row),
            Some(role_row),
            Some(auth_key_row),
        ) = (
            connect_ctx.hostname_row.upgrade(),
            connect_ctx.port_row.upgrade(),
            connect_ctx.protocol_row.upgrade(),
            connect_ctx.device_row.upgrade(),
            connect_ctx.role_row.upgrade(),
            connect_ctx.auth_key_row.upgrade(),
        )
        else {
            return;
        };
        // Shared ordering-sensitive flow lives in
        // `apply_rtl_tcp_connect`. The popover-specific follow-up
        // (popdown) happens after this returns.
        apply_rtl_tcp_connect(
            &host,
            port,
            &connect_nickname,
            &hostname_row,
            &port_row,
            &protocol_row,
            &device_row,
            &role_row,
            &auth_key_row,
            &connect_ctx.state,
            &connect_ctx.config,
        );
        // Dismiss the popover once the connection is dispatched
        // so the user sees the source row update underneath.
        if let Some(popover) = connect_ctx.popover.popover.upgrade() {
            popover.popdown();
        }
    });
    row.add_suffix(&connect_btn);

    // Copy button — writes `host:port` to the clipboard. Lets
    // the user grab the endpoint for pasting into another tool
    // without having to hand-transcribe the subtitle.
    let copy_btn = gtk4::Button::from_icon_name("edit-copy-symbolic");
    copy_btn.set_tooltip_text(Some("Copy host:port"));
    copy_btn.add_css_class("flat");
    copy_btn.set_valign(gtk4::Align::Center);
    // Icon-only button — give it an explicit accessible name so
    // screen readers don't fall back to the icon filename.
    copy_btn.update_property(&[gtk4::accessible::Property::Label("Copy server address")]);
    let copy_key = entry.key.clone();
    copy_btn.connect_clicked(move |btn| {
        // `WidgetExt::clipboard` reaches the display clipboard
        // via the button's realized display. If the popover has
        // been torn down the button isn't reachable anyway, so
        // we just use the button itself as the anchor widget.
        btn.clipboard().set_text(&copy_key);
    });
    row.add_suffix(&copy_btn);

    // Unstar button — removes from the favorites map, persists,
    // and rebuilds both the discovery expander (so the row moves
    // out of the pinned section) and the popover list (so the
    // row disappears from here).
    let unstar_btn = gtk4::Button::from_icon_name("starred-symbolic");
    unstar_btn.set_tooltip_text(Some("Remove from favorites"));
    unstar_btn.add_css_class("flat");
    unstar_btn.set_valign(gtk4::Align::Center);
    // Icon-only button — matches the tooltip here but stays as
    // a distinct property so screen readers announce it even
    // when tooltips are disabled / long-hover wouldn't fire.
    unstar_btn.update_property(&[gtk4::accessible::Property::Label("Remove from favorites")]);
    let unstar_key = entry.key.clone();
    let unstar_ctx = Rc::clone(ctx);
    unstar_btn.connect_clicked(move |_| {
        {
            let mut favs = unstar_ctx.favorites.borrow_mut();
            if favs.remove(&unstar_key).is_none() {
                // Already gone (e.g., double-click race). Nothing
                // to persist and nothing to rebuild.
                return;
            }
            let snapshot: Vec<sidebar::source_panel::FavoriteEntry> =
                favs.values().cloned().collect();
            crate::sidebar::source_panel::save_favorites(&unstar_ctx.config, &snapshot);
        }

        // If the discovery row for this key is currently rendered,
        // flip its star toggle to the unpinned state. The
        // toggle's own `connect_toggled` handler then does the
        // map cleanup (no-op — we already removed), the persist
        // (redundant but idempotent), the discovery reorder, and
        // the popover rebuild — so we early-return and skip OUR
        // reorder / rebuild below.
        //
        // Without this, the filled star would keep rendering
        // until the next mDNS beacon, which isn't just
        // cosmetic: the first user click on the stale filled
        // star fires `toggled` with `active=false` (the intent
        // was "re-pin"), silently wasting a click.
        if let Some(star_map) = unstar_ctx.discovered_star_buttons.upgrade() {
            let maybe_btn = star_map
                .borrow()
                .get(&unstar_key)
                .and_then(glib::WeakRef::upgrade);
            if let Some(btn) = maybe_btn
                && btn.is_active()
            {
                btn.set_active(false);
                return;
            }
        }

        // No discovery row visible for this key — do the reorder
        // and popover rebuild ourselves.
        //
        // `displayed_rows` is Weak on the context — upgrade fails
        // if the discovery timer has been torn down, which also
        // means there's nothing left to reorder.
        if let (Some(expander), Some(rows)) = (
            unstar_ctx.expander_weak.upgrade(),
            unstar_ctx.displayed_rows.upgrade(),
        ) {
            reorder_discovered_rows(&expander, &rows.borrow(), &unstar_ctx.favorites.borrow());
        }
        // Rebuild the popover so the unstarred row disappears.
        // GTK signal-lifetime guarantees we can `ListBox::remove`
        // our own row from inside this button-clicked handler:
        // GTK retains the signal's source widget for the
        // callback's duration, so the button won't drop under us.
        rebuild_favorites_popover(&unstar_ctx, &unstar_ctx.favorites.borrow());
    });
    row.add_suffix(&unstar_btn);
}

/// Parse a `hostname:port` favorite key back into its two fields.
/// Uses `rsplit_once(':')` so IPv6 literals with multiple colons
/// round-trip if we ever start producing them (today's
/// `favorite_key` only emits the DNS hostname, but the parser
/// should be the conservative half of that contract).
///
/// Returns `None` when the key lacks a colon or the port field
/// doesn't parse as `u16` — callers log and swallow.
fn parse_host_port(key: &str) -> Option<(String, u16)> {
    let (host, port_str) = key.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Render a `FavoriteEntry` into the one-line subtitle shown on
/// its row. Joined with ` • ` separators — matches the discovery-
/// row subtitle format so the two lists read consistently.
fn format_favorite_subtitle(entry: &sidebar::source_panel::FavoriteEntry, now_unix: u64) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(entry.key.clone());
    if let (Some(tuner), Some(gains)) = (entry.tuner_name.as_deref(), entry.gain_count) {
        parts.push(format!("{tuner} · {gains} gains"));
    }
    let seen = match entry.last_seen_unix {
        Some(ts) if ts > 0 => format!("seen {}", format_seen_age(now_unix, ts)),
        _ => "offline".to_string(),
    };
    parts.push(seen);
    parts.join(" • ")
}

/// Bucket boundaries for [`format_seen_age`]. Raw Unix-seconds
/// arithmetic (not `std::time::Duration`) because `last_seen_unix`
/// is stored as `u64` seconds in the favorites JSON and stays in
/// that domain end-to-end.
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;

/// Bucket a `now - last_seen` difference into a short human
/// string. Coarser buckets than the discovery-row's `format_age`
/// because favorites ages are typically much larger (minutes to
/// days) and the row subtitle has limited horizontal real estate.
fn format_seen_age(now_unix: u64, last_seen_unix: u64) -> String {
    if last_seen_unix >= now_unix {
        // Clock skew or freshly-stamped — render as the latest
        // bucket rather than a garbage negative value.
        return "just now".to_string();
    }
    let secs = now_unix - last_seen_unix;
    if secs < SECONDS_PER_MINUTE {
        "just now".to_string()
    } else if secs < SECONDS_PER_HOUR {
        format!("{}m ago", secs / SECONDS_PER_MINUTE)
    } else if secs < SECONDS_PER_DAY {
        format!("{}h ago", secs / SECONDS_PER_HOUR)
    } else {
        format!("{}d ago", secs / SECONDS_PER_DAY)
    }
}

/// Owned handle for a running `rtl_tcp` server + optional mDNS
/// advertisement. Drops in reverse order: advertiser first (so
/// peers see the goodbye packet before the server stops), then the
/// server itself (which consumes its accept thread + USB device).
///
/// `Advertiser` is an `Option` because the user can run the server
/// without LAN advertising via the "Announce via mDNS" switch.
struct RunningServer {
    server: Server,
    advertiser: Option<Advertiser>,
}

/// Read the `rtl_tcp` server auth key from the OS keyring, if
/// present. Returns `Some(bytes)` for a well-formed hex-encoded
/// entry, `None` for a missing key, keyring unavailable, empty
/// entry, or corrupt hex. Corrupt entries are logged at `warn`
/// so operators can diagnose without the UI silently regenerating
/// over their paste. Per issue #395.
fn load_server_auth_key_from_keyring() -> Option<Vec<u8>> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_KEY_AUTH_KEY, KEYRING_SERVICE, auth_key_from_hex};

    let store = KeyringStore::new(KEYRING_SERVICE);
    match store.get(KEYRING_KEY_AUTH_KEY) {
        Ok(Some(hex)) => {
            let Some(bytes) = auth_key_from_hex(&hex) else {
                tracing::warn!(
                    "rtl_tcp server auth key in keyring is malformed hex; regenerating on next toggle-on"
                );
                return None;
            };
            Some(bytes)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%e, "rtl_tcp server auth key keyring read failed");
            None
        }
    }
}

/// Write the `rtl_tcp` server auth key to the OS keyring as
/// lowercase hex. Returns the underlying keyring error so
/// callers can surface it via toast — the caller is responsible
/// for deciding UX fallback (e.g. revert the toggle, show a
/// banner). Per issue #395.
fn save_server_auth_key_to_keyring(
    bytes: &[u8],
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_KEY_AUTH_KEY, KEYRING_SERVICE, auth_key_to_hex};

    let store = KeyringStore::new(KEYRING_SERVICE);
    store.set(KEYRING_KEY_AUTH_KEY, &auth_key_to_hex(bytes))
}

/// Load the persisted server auth key, generating + saving a
/// fresh one when the keyring is either empty or corrupt. The
/// caller gets the fresh bytes regardless — a write failure
/// leaves the key in memory so the current session works, and
/// the next session's toggle-on retries the save path. Per
/// issue #395.
fn ensure_server_auth_key() -> Vec<u8> {
    if let Some(existing) = load_server_auth_key_from_keyring() {
        return existing;
    }
    let fresh = sdr_server_rtltcp::auth::generate_random_auth_key();
    if let Err(e) = save_server_auth_key_to_keyring(&fresh) {
        tracing::warn!(%e, "rtl_tcp server auth key keyring write failed — in-memory only");
    }
    fresh
}

/// Keyring-entry prefix for per-server **client** auth keys. The
/// full entry name is `{prefix}-{host}:{port}` — per-server
/// so the user can save distinct keys for distinct servers on
/// the LAN (different owners, different rotation schedules).
/// Kept distinct from `KEYRING_KEY_AUTH_KEY` (which stores the
/// local server's own key, single entry) so neither surface
/// ever reads the other's bytes by accident. Per issue #396.
const KEYRING_KEY_CLIENT_AUTH_KEY_PREFIX: &str = "rtl_tcp-client-auth-key-";

/// Build the keyring entry name for a client-side saved key
/// keyed by the server's `host:port` identity. Matches the
/// identity `FavoriteEntry.key` uses, so the keyring entry
/// survives server rename / nickname change. Per issue #396.
fn client_auth_key_entry_name(host: &str, port: u16) -> String {
    format!("{KEYRING_KEY_CLIENT_AUTH_KEY_PREFIX}{host}:{port}")
}

/// Load the saved auth key for the given `rtl_tcp` server, if
/// the user previously connected successfully with a key
/// against this `host:port`. Returns `None` for missing /
/// corrupt / keyring-unavailable cases — callers treat that
/// as "ask the user for a key" rather than silently connecting
/// without one. Per issue #396.
#[allow(
    dead_code,
    reason = "wired up in the #396 commit that adds the Server key entry row"
)]
fn load_client_auth_key_from_keyring(host: &str, port: u16) -> Option<Vec<u8>> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_SERVICE, auth_key_from_hex};

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    match store.get(&entry) {
        Ok(Some(hex)) => {
            let Some(bytes) = auth_key_from_hex(&hex) else {
                tracing::warn!(
                    entry = %entry,
                    "rtl_tcp client auth key in keyring is malformed hex; treating as missing"
                );
                return None;
            };
            Some(bytes)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%e, entry = %entry, "rtl_tcp client auth key keyring read failed");
            None
        }
    }
}

/// Save a successfully-used client auth key for the given
/// server to the OS keyring. Called AFTER a successful
/// auth-required connect so the user doesn't have to re-enter
/// the key on subsequent reconnects to the same server. A
/// keyring write failure is non-fatal — the current session
/// still works; the next launch will just prompt for the key
/// again. Per issue #396.
#[allow(
    dead_code,
    reason = "wired up in the #396 commit that adds the Server key entry row"
)]
fn save_client_auth_key_to_keyring(
    host: &str,
    port: u16,
    bytes: &[u8],
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_SERVICE, auth_key_to_hex};

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    store.set(&entry, &auth_key_to_hex(bytes))
}

/// Delete a saved client auth key for the given server. Called
/// from the UI when the user explicitly clears the key (e.g.
/// the server regenerated on the other end and the old key no
/// longer works; clearing avoids auto-sending the dead key on
/// every reconnect attempt). Missing-entry is treated as
/// success — the goal is "there is no saved key after this
/// call," which a missing entry already satisfies. Per #396.
fn clear_client_auth_key_from_keyring(
    host: &str,
    port: u16,
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::KEYRING_SERVICE;

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    store.delete(&entry)
}

/// Wire the server panel end-to-end: visibility gating, the master
/// share-over-network switch, and its downstream start/stop effects.
/// Errors surface via the `toast_overlay`, and the switch auto-
/// reverts to its off state so the UI never lies about whether a
/// server is actually running.
///
/// Visibility rule:
/// 1. at least one RTL-SDR dongle is visible on the local USB bus
///    (`sdr_rtlsdr::get_device_count() > 0`), and
/// 2. the active source type is **not** RTL-SDR — re-exposing the
///    same dongle over `rtl_tcp` while a local `RtlSdrSource` is
///    holding it would cause a USB-device double-open, and
/// 3. OR the server is already running (keep the panel visible so
///    the user can reach the Stop switch no matter what).
///
/// Visibility is recomputed on three triggers so the panel feels
/// responsive without polling the world: a low-frequency timer that
/// handles the USB side (hotplug has no GTK signal we can subscribe
/// to), a `device_row.connect_selected_notify` handler that fires
/// on every source-type change, and the share-row start/stop path
/// itself. A `Cell<u32>` tracks the last-seen device count so we
/// only pay the widget-state-update cost on an actual edge.
#[allow(
    clippy::too_many_lines,
    reason = "GTK signal-wiring: visibility + start-stop + control-locking all share state via nested closures — splitting scatters the captures"
)]
fn connect_server_panel(
    panels: &SidebarPanels,
    toast_overlay: &adw::ToastOverlay,
    server_running: Rc<std::cell::Cell<bool>>,
) {
    use std::cell::Cell;

    let server_widget_weak = panels.server.widget.downgrade();
    let device_row = panels.source.device_row.clone();
    let last_seen_count = Rc::new(Cell::new(u32::MAX));
    let running: Rc<RefCell<Option<RunningServer>>> = Rc::new(RefCell::new(None));

    // Pure function: does the combined rule say "show"? A running
    // server always wins — the user must be able to reach the Stop
    // switch regardless of hotplug / source-type state.
    let should_be_visible = |dongle_count: u32, selected: u32, is_running: bool| -> bool {
        is_running || (dongle_count > 0 && selected != DEVICE_RTLSDR)
    };

    // Apply visibility, using the cached dongle count. Shared
    // between the poll tick, the device-row notify handler, and the
    // start/stop path so all three callers stay in lockstep.
    let apply_visibility = {
        let server_widget_weak = server_widget_weak.clone();
        let device_row = device_row.clone();
        let last_seen_count = Rc::clone(&last_seen_count);
        let running = Rc::clone(&running);
        move || {
            let Some(widget) = server_widget_weak.upgrade() else {
                return;
            };
            let count = last_seen_count.get();
            // First invocation before any poll: count is u32::MAX,
            // which would evaluate true for `> 0`. Treat the
            // pre-first-tick state as "no dongle yet" so the panel
            // stays hidden until we actually know — prevents a
            // flash-of-unwanted-panel during startup.
            let effective_count = if count == u32::MAX { 0 } else { count };
            let is_running = running.borrow().is_some();
            widget.set_visible(should_be_visible(
                effective_count,
                device_row.selected(),
                is_running,
            ));
        }
    };

    // Reapply on source-type change. Cloned because
    // `connect_selected_notify` takes an `Fn(&ComboRow)` and we want
    // the same logic as the poll tick.
    let apply_on_device_change = apply_visibility.clone();
    panels
        .source
        .device_row
        .connect_selected_notify(move |_| apply_on_device_change());

    // Seed `last_seen_count` on the first tick. Using a glib timer
    // (rather than running the USB probe synchronously during
    // wiring) keeps the window-build path fast and avoids a libusb
    // session init on a thread that may not have one ready.
    let apply_on_tick = apply_visibility.clone();
    let poll_widget_weak = server_widget_weak.clone();
    let poll_last_seen_count = Rc::clone(&last_seen_count);
    let _ = glib::timeout_add_local(SERVER_PANEL_HOTPLUG_POLL_INTERVAL, move || {
        // If the widget is gone, tear the poller down — nothing to
        // show, and we don't want to leak `rusb::devices()` calls
        // past window close.
        if poll_widget_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        // `sdr_rtlsdr::get_device_count()` is a libusb enumerate
        // (vendor/product-ID filter over `rusb::devices()`). Fast
        // enough for the UI thread at a 3 s cadence; no syscall
        // churn worth moving to a worker.
        let count = sdr_rtlsdr::get_device_count();
        // First tick ALWAYS flips the cache off `u32::MAX`, so this
        // branch fires at least once even if the real count is 0 —
        // that's the "resolve the panel out of its pre-first-tick
        // hidden state" moment. Subsequent ticks only apply on a
        // real edge so we don't churn widget state every 3 s.
        if count != poll_last_seen_count.get() {
            tracing::debug!(
                previous = poll_last_seen_count.get(),
                current = count,
                "rtl_tcp server panel: local dongle count changed"
            );
            poll_last_seen_count.set(count);
            apply_on_tick();
        }
        glib::ControlFlow::Continue
    });

    // Wire the master share-over-network switch. The handler is the
    // authority on server lifecycle — on toggle we either start a
    // new `Server` (+ optional `Advertiser`) and store the handle,
    // or drop the handle so the accept thread tears down.
    connect_share_switch(
        panels,
        toast_overlay,
        Rc::clone(&running),
        apply_visibility.clone(),
        server_running,
    );

    // Poll `Server::stats()` on a timer, render the status rows,
    // and auto-stop the server if `has_stopped()` becomes true
    // (e.g. USB unplug or accept-thread failure).
    connect_server_status_polling(panels, Rc::clone(&running), apply_visibility);

    // Bandwidth advisory — toggled on the device-default sample
    // rate. Unlike the source panel's advisory (which also gates
    // on source type), the server is inherently a network path so
    // only the rate matters.
    let advisory_row_weak = panels.server.bandwidth_advisory_row.downgrade();
    let apply_server_bandwidth_advisory = move |row: &adw::ComboRow| {
        let Some(advisory) = advisory_row_weak.upgrade() else {
            return;
        };
        // Bounds-check the selected index before threshold compare.
        // `ComboRow::selected()` can emit transient out-of-range
        // values during widget-model churn (GTK model repopulate,
        // drag-mid-scroll, etc.) — a bare `>=` would treat those
        // as high-bandwidth and flash the advisory visible against
        // no legal selection. Mirrors the `SAMPLE_RATES.get()`
        // safety pattern used elsewhere in this file.
        let selected = row.selected();
        let is_legal = (selected as usize) < SAMPLE_RATES.len();
        advisory.set_visible(
            is_legal && selected >= crate::sidebar::source_panel::HIGH_BANDWIDTH_SAMPLE_RATE_IDX,
        );
    };
    // Seed initial visibility + subscribe for future changes.
    apply_server_bandwidth_advisory(&panels.server.sample_rate_row);
    panels
        .server
        .sample_rate_row
        .connect_selected_notify(apply_server_bandwidth_advisory);
}

/// Extracted out of `connect_server_panel` so the parent function
/// stays under clippy's `too_many_lines` limit. Handles exactly one
/// thing: the `share_row.connect_active_notify` wiring, with its
/// downstream start/stop effects (build `ServerConfig`, call
/// `Server::start`, optionally attach an `Advertiser`, lock or
/// unlock the panel controls, reapply visibility, and surface any
/// error via a toast while flipping the switch back to off).
/// Weak refs to every widget the share-switch handler reads or
/// mutates. Mirrors the `ServerStatusWidgetsWeak` pattern: the
/// closure attached to `share_row.connect_active_notify` would
/// otherwise create a self-cycle (`share_row` → closure →
/// `server_panel.share_row` → …) via the previous
/// `clone_server_panel` capture. With this struct we capture weak
/// refs only; strong refs live for the duration of one callback
/// via `upgrade()` and drop at function return, so the widgets can
/// be released on window close.
///
/// `source_device_row` is a sidebar neighbour (not in `ServerPanel`)
/// and comes along for the exclusivity guard read.
#[derive(Clone)]
struct ServerSwitchWidgetsWeak {
    nickname_row: glib::WeakRef<adw::EntryRow>,
    port_row: glib::WeakRef<adw::SpinRow>,
    bind_row: glib::WeakRef<adw::ComboRow>,
    advertise_row: glib::WeakRef<adw::SwitchRow>,
    compression_row: glib::WeakRef<adw::ComboRow>,
    listener_cap_row: glib::WeakRef<adw::SpinRow>,
    auth_require_row: glib::WeakRef<adw::SwitchRow>,
    device_defaults_row: glib::WeakRef<adw::ExpanderRow>,
    center_freq_row: glib::WeakRef<adw::SpinRow>,
    sample_rate_row: glib::WeakRef<adw::ComboRow>,
    gain_row: glib::WeakRef<adw::SpinRow>,
    ppm_row: glib::WeakRef<adw::SpinRow>,
    bias_tee_row: glib::WeakRef<adw::SwitchRow>,
    direct_sampling_row: glib::WeakRef<adw::SwitchRow>,
    status_row: glib::WeakRef<adw::ExpanderRow>,
    status_client_row: glib::WeakRef<adw::ActionRow>,
    status_uptime_row: glib::WeakRef<adw::ActionRow>,
    status_data_rate_row: glib::WeakRef<adw::ActionRow>,
    status_commanded_row: glib::WeakRef<adw::ActionRow>,
    activity_log_row: glib::WeakRef<adw::ExpanderRow>,
    activity_log_list: glib::WeakRef<gtk4::ListBox>,
    clients_row: glib::WeakRef<adw::ExpanderRow>,
    clients_list: glib::WeakRef<gtk4::ListBox>,
    source_device_row: glib::WeakRef<adw::ComboRow>,
}

/// Upgraded strong refs held for the duration of a single handler
/// invocation. Field names match `ServerPanel` so the existing
/// helpers (`build_server_config_from_panel`, `set_controls_locked`,
/// etc.) keep working after a simple type rename on their `panel`
/// parameter.
struct ServerSwitchWidgets {
    nickname_row: adw::EntryRow,
    port_row: adw::SpinRow,
    bind_row: adw::ComboRow,
    advertise_row: adw::SwitchRow,
    compression_row: adw::ComboRow,
    listener_cap_row: adw::SpinRow,
    auth_require_row: adw::SwitchRow,
    device_defaults_row: adw::ExpanderRow,
    center_freq_row: adw::SpinRow,
    sample_rate_row: adw::ComboRow,
    gain_row: adw::SpinRow,
    ppm_row: adw::SpinRow,
    bias_tee_row: adw::SwitchRow,
    direct_sampling_row: adw::SwitchRow,
    status_row: adw::ExpanderRow,
    status_client_row: adw::ActionRow,
    status_uptime_row: adw::ActionRow,
    status_data_rate_row: adw::ActionRow,
    status_commanded_row: adw::ActionRow,
    activity_log_row: adw::ExpanderRow,
    activity_log_list: gtk4::ListBox,
    clients_row: adw::ExpanderRow,
    clients_list: gtk4::ListBox,
    source_device_row: adw::ComboRow,
}

impl ServerSwitchWidgetsWeak {
    fn from_panels(panels: &SidebarPanels) -> Self {
        let s = &panels.server;
        Self {
            nickname_row: s.nickname_row.downgrade(),
            port_row: s.port_row.downgrade(),
            bind_row: s.bind_row.downgrade(),
            advertise_row: s.advertise_row.downgrade(),
            compression_row: s.compression_row.downgrade(),
            listener_cap_row: s.listener_cap_row.downgrade(),
            auth_require_row: s.auth_require_row.downgrade(),
            device_defaults_row: s.device_defaults_row.downgrade(),
            center_freq_row: s.center_freq_row.downgrade(),
            sample_rate_row: s.sample_rate_row.downgrade(),
            gain_row: s.gain_row.downgrade(),
            ppm_row: s.ppm_row.downgrade(),
            bias_tee_row: s.bias_tee_row.downgrade(),
            direct_sampling_row: s.direct_sampling_row.downgrade(),
            status_row: s.status_row.downgrade(),
            status_client_row: s.status_client_row.downgrade(),
            status_uptime_row: s.status_uptime_row.downgrade(),
            status_data_rate_row: s.status_data_rate_row.downgrade(),
            status_commanded_row: s.status_commanded_row.downgrade(),
            activity_log_row: s.activity_log_row.downgrade(),
            activity_log_list: s.activity_log_list.downgrade(),
            clients_row: s.clients_row.downgrade(),
            clients_list: s.clients_list.downgrade(),
            source_device_row: panels.source.device_row.downgrade(),
        }
    }

    /// Lift every weak ref atomically — any missing widget means
    /// the window's torn down and we skip the callback entirely.
    fn upgrade(&self) -> Option<ServerSwitchWidgets> {
        Some(ServerSwitchWidgets {
            nickname_row: self.nickname_row.upgrade()?,
            port_row: self.port_row.upgrade()?,
            bind_row: self.bind_row.upgrade()?,
            advertise_row: self.advertise_row.upgrade()?,
            compression_row: self.compression_row.upgrade()?,
            listener_cap_row: self.listener_cap_row.upgrade()?,
            auth_require_row: self.auth_require_row.upgrade()?,
            device_defaults_row: self.device_defaults_row.upgrade()?,
            center_freq_row: self.center_freq_row.upgrade()?,
            sample_rate_row: self.sample_rate_row.upgrade()?,
            gain_row: self.gain_row.upgrade()?,
            ppm_row: self.ppm_row.upgrade()?,
            bias_tee_row: self.bias_tee_row.upgrade()?,
            direct_sampling_row: self.direct_sampling_row.upgrade()?,
            status_row: self.status_row.upgrade()?,
            status_client_row: self.status_client_row.upgrade()?,
            status_uptime_row: self.status_uptime_row.upgrade()?,
            status_data_rate_row: self.status_data_rate_row.upgrade()?,
            status_commanded_row: self.status_commanded_row.upgrade()?,
            activity_log_row: self.activity_log_row.upgrade()?,
            activity_log_list: self.activity_log_list.upgrade()?,
            clients_row: self.clients_row.upgrade()?,
            clients_list: self.clients_list.upgrade()?,
            source_device_row: self.source_device_row.upgrade()?,
        })
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "share switch orchestrates server start/stop plus listener-cap + \
              auth-key live-update signals; splitting it would scatter the \
              `running` and `toast_overlay` Rc clones across multiple helpers \
              without improving clarity"
)]
fn connect_share_switch(
    panels: &SidebarPanels,
    toast_overlay: &adw::ToastOverlay,
    running: Rc<RefCell<Option<RunningServer>>>,
    apply_visibility: impl Fn() + Clone + 'static,
    server_running: Rc<std::cell::Cell<bool>>,
) {
    use std::cell::Cell;

    // Guards against our own `set_active(false)` (called when the
    // user-initiated start path errors out) re-entering the handler
    // and triggering a spurious stop dispatch on a server that
    // never started.
    let reentry_guard = Rc::new(Cell::new(false));
    let toast_overlay_weak = toast_overlay.downgrade();

    let share_row_weak = panels.server.share_row.downgrade();
    // Weak refs to every row/widget the handler reads or mutates.
    // Replaces the previous `clone_server_panel` strong capture,
    // which bumped share_row's GObject refcount and created a
    // self-cycle with the `connect_active_notify` subscription.
    // Upgraded per-callback so strong refs live for one tick only.
    let widgets_weak = ServerSwitchWidgetsWeak::from_panels(panels);

    // Clone the `running` handle for the listener-cap live-apply
    // closure BEFORE the `share_row` active-notify handler
    // below consumes the outer `running` by move. Both closures
    // share the same `RefCell`; neither holds a borrow past its
    // own tick. Per #395.
    let running_for_cap = Rc::clone(&running);
    // Additional `running` clones for the auth-related closures
    // (toggle, reveal, copy, regenerate). Same rationale — clone
    // before the share_row handler consumes the outer `running`.
    let running_for_auth_toggle = Rc::clone(&running);
    let running_for_auth_regen = Rc::clone(&running);

    // Clone the toast-overlay weak ref for every auth-side
    // closure that surfaces errors (toggle-on/off, copy,
    // regenerate). Same move-before-share_row problem: the
    // share_row closure below consumes the outer
    // `toast_overlay_weak`.
    let toast_overlay_for_auth_toggle = toast_overlay_weak.clone();
    let toast_overlay_for_copy = toast_overlay_weak.clone();
    let toast_overlay_for_regen = toast_overlay_weak.clone();

    // Shared state for the auth-key display row. `current_key`
    // holds the active key bytes while the server is running
    // with auth enabled; `None` when auth is off. `key_revealed`
    // tracks whether the subtitle currently shows the full hex
    // or the masked placeholder — the user toggles this via the
    // reveal button. Both are `Rc<...>` so the four closures
    // (toggle, reveal, copy, regenerate) share the same state
    // without borrow conflicts. Per issue #395.
    let current_auth_key: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
    let auth_key_revealed: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));

    // If auth was restored as ON from config, eagerly load the
    // key from the keyring so the key row reflects real state
    // before the user interacts with anything. The server isn't
    // running yet (that requires the share_row flip), so no
    // `set_auth_key` call here — just UI state.
    if panels.server.auth_require_row.is_active() {
        let key = ensure_server_auth_key();
        *current_auth_key.borrow_mut() = Some(key);
        panels.server.auth_key_row.set_visible(true);
        // Leave subtitle as the masked placeholder (widget
        // default) — user clicks Reveal to see the real value.
    }

    // Clone `current_auth_key` for the share_row closure before
    // it consumes local state. The closure reads the cell at
    // server-start time to thread the key into
    // `build_server_config_from_panel` without a second
    // `ensure_server_auth_key()` call. Per `CodeRabbit` round 1
    // on PR #406.
    let current_key_for_share = Rc::clone(&current_auth_key);

    // Widget-weak clones threaded into the auth toggle + regenerate
    // closures so they can rebuild the mDNS advertiser when auth
    // state changes. Without this, discovery clients keep seeing
    // stale `auth_required` TXT until the next server restart.
    // Per `CodeRabbit` round 1 on PR #406.
    let widgets_weak_for_auth_toggle = widgets_weak.clone();

    // Reentry guard for the auth-toggle handler. When the server
    // reports a failed `set_auth_key`, the handler reverts the
    // switch — but `set_active()` fires `connect_active_notify`
    // again, which would re-run the handler and double-toast.
    // Mirrors `reentry_guard` on the share_row.
    let auth_toggle_reentry_guard: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));

    panels.server.share_row.connect_active_notify(move |row| {
        if reentry_guard.get() {
            return;
        }
        let Some(widgets) = widgets_weak.upgrade() else {
            // Window is gone — the signal should stop firing soon.
            // Belt-and-suspenders early return.
            return;
        };
        let active = row.is_active();
        if active {
            // Exclusivity guard: can't claim the dongle for the
            // server while the UI still has RTL-SDR picked as the
            // local source type. Toast + revert the switch without
            // touching `running` or widget lock state.
            if widgets.source_device_row.selected() == DEVICE_RTLSDR {
                if let Some(overlay) = toast_overlay_weak.upgrade() {
                    overlay.add_toast(adw::Toast::new(
                        "Switch the source away from local RTL-SDR before sharing over network.",
                    ));
                }
                reentry_guard.set(true);
                row.set_active(false);
                reentry_guard.set(false);
                return;
            }
            // Build a ServerConfig from current panel state. Widget
            // readers run on the main thread — safe to block-read
            // the rows synchronously. The pending auth key is
            // read from `current_key_for_share` so a Reveal-and-Copy
            // operation before Play uses the same bytes
            // `Server::start` receives. Per `CodeRabbit` round 1
            // on PR #406.
            let pending_auth_key = current_key_for_share.borrow().clone();
            let config = build_server_config_from_panel(&widgets, pending_auth_key);
            match Server::start(config) {
                Ok(server) => {
                    // If advertising is on, build the TXT record
                    // from the tuner metadata the Server exposes.
                    // An Advertiser failure is non-fatal for the
                    // server itself (the accept loop keeps running
                    // without mDNS), but the user explicitly asked
                    // for LAN announcement so they need to KNOW the
                    // intent failed — surface a toast and leave
                    // `advertiser = None` so the stop path doesn't
                    // try to unregister something that never
                    // registered.
                    let advertiser = if widgets.advertise_row.is_active() {
                        match build_advertiser(&server, &widgets.nickname_row.text()) {
                            Ok(adv) => Some(adv),
                            Err(e) => {
                                tracing::warn!(error = %e, "mDNS advertiser failed; server running without LAN advertisement");
                                if let Some(overlay) = toast_overlay_weak.upgrade() {
                                    overlay.add_toast(adw::Toast::new(&format!(
                                        "Server running, but mDNS advertising failed: {e}"
                                    )));
                                }
                                None
                            }
                        }
                    } else {
                        None
                    };
                    set_controls_locked(&widgets, true);
                    widgets.status_row.set_visible(true);
                    widgets.activity_log_row.set_visible(true);
                    widgets.clients_row.set_visible(true);
                    *running.borrow_mut() = Some(RunningServer { server, advertiser });
                    // Flip the shared "server is live" flag AFTER
                    // the handle is stored so the source-panel
                    // guard can't race against a mid-construction
                    // state.
                    server_running.set(true);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to start rtl_tcp server");
                    if let Some(overlay) = toast_overlay_weak.upgrade() {
                        overlay.add_toast(adw::Toast::new(&format!(
                            "Couldn't share over network: {e}"
                        )));
                    }
                    // Revert the switch without re-entering this
                    // same handler — the reentry_guard covers the
                    // set_active call below.
                    reentry_guard.set(true);
                    if let Some(share) = share_row_weak.upgrade() {
                        share.set_active(false);
                    }
                    reentry_guard.set(false);
                }
            }
        } else {
            // Drop the handle → Server::drop signals shutdown and
            // joins the accept thread; Advertiser::drop unregisters
            // the mDNS record. Sequence matters (advertiser first
            // so peers see the goodbye packet before the server
            // stops) — field declaration order in `RunningServer`
            // would drop `server` first, so take the advertiser
            // explicitly first to reverse.
            if let Some(mut handle) = running.borrow_mut().take() {
                drop(handle.advertiser.take());
                drop(handle.server);
            }
            // Clear the shared "server is live" flag ahead of the
            // widget-visibility changes so an immediate source-type
            // re-selection triggered by the user's next action sees
            // the coherent post-stop state.
            server_running.set(false);
            set_controls_locked(&widgets, false);
            widgets.status_row.set_visible(false);
            widgets.activity_log_row.set_visible(false);
            widgets.clients_row.set_visible(false);
            reset_status_rows(&widgets);
            reset_activity_log(&widgets);
            reset_clients_list(&widgets);
        }
        apply_visibility();
    });

    // ====================================================
    // Auth controls (#394/#395) — toggle + reveal + copy +
    // regenerate. All four closures share `current_auth_key`
    // and `auth_key_revealed` via `Rc` + the running-server
    // handle via `running_for_auth_{toggle,regen}`.
    // ====================================================

    // Master "Require key" toggle.
    //
    // Order of operations (per `CodeRabbit` round 1 on PR #406):
    // 1. Apply the change to the running server FIRST.
    // 2. Refresh the mDNS advertiser so discovery TXT reflects
    //    the new `auth_required` flag.
    // 3. Only mutate UI state (current_auth_key, row visibility,
    //    subtitle, reveal button) after steps 1 and 2 succeeded.
    //
    // On any failure: revert the switch to its pre-toggle state
    // via `auth_toggle_reentry_guard` so UI ↔ server parity is
    // preserved. Discovery clients never see "auth advertised"
    // while the server is unauthed, or vice versa.
    //
    // When the server isn't running, steps 1+2 are no-ops and UI
    // mutation always proceeds — toggling auth with the switch
    // off is a config-only change and the next Start path
    // honors it via the pending-key plumbing.
    let key_row_for_toggle = panels.server.auth_key_row.downgrade();
    let reveal_button_for_toggle = panels.server.auth_key_reveal_button.downgrade();
    let current_key_for_toggle = Rc::clone(&current_auth_key);
    let revealed_for_toggle = Rc::clone(&auth_key_revealed);
    let auth_toggle_guard_for_handler = Rc::clone(&auth_toggle_reentry_guard);
    panels
        .server
        .auth_require_row
        .connect_active_notify(move |row| {
            if auth_toggle_guard_for_handler.get() {
                // Re-entered from our own `set_active` revert
                // path — let the signal settle without running
                // the handler again.
                return;
            }
            let Some(key_row) = key_row_for_toggle.upgrade() else {
                return;
            };
            let widgets = widgets_weak_for_auth_toggle.upgrade();

            if row.is_active() {
                // Pending key is the single source of truth for
                // both the server and any subsequent Reveal /
                // Copy. Generate / load once, reuse everywhere.
                let key = ensure_server_auth_key();

                // Step 1+2: apply to live server + refresh mDNS.
                let server_result = apply_live_auth_change(
                    &running_for_auth_toggle,
                    Some(key.clone()),
                    widgets.as_ref(),
                    &toast_overlay_for_auth_toggle,
                );

                if !server_result {
                    // Revert the switch. UI stays on the pre-
                    // toggle state; the user can click again
                    // after resolving the server issue.
                    auth_toggle_guard_for_handler.set(true);
                    row.set_active(false);
                    auth_toggle_guard_for_handler.set(false);
                    return;
                }

                // Step 3: UI mutation AFTER successful server
                // change.
                *current_key_for_toggle.borrow_mut() = Some(key);
                key_row.set_visible(true);
                // Reset to masked state on every toggle-on so the
                // key row doesn't surface a previously-revealed
                // value across sessions.
                revealed_for_toggle.set(false);
                key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
                if let Some(rb) = reveal_button_for_toggle.upgrade() {
                    rb.set_icon_name("view-reveal-symbolic");
                    rb.set_tooltip_text(Some("Reveal key"));
                    rb.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
                }
            } else {
                // Same structure for toggle-off. Server call
                // first; on failure revert the switch so the UI
                // stays honest about the running auth state.
                let server_result = apply_live_auth_change(
                    &running_for_auth_toggle,
                    None,
                    widgets.as_ref(),
                    &toast_overlay_for_auth_toggle,
                );

                if !server_result {
                    auth_toggle_guard_for_handler.set(true);
                    row.set_active(true);
                    auth_toggle_guard_for_handler.set(false);
                    return;
                }

                *current_key_for_toggle.borrow_mut() = None;
                key_row.set_visible(false);
                // Zero the revealed flag too so a next toggle-on
                // starts masked regardless of the prior reveal
                // state.
                revealed_for_toggle.set(false);
            }
        });

    // Reveal / conceal button — flips the subtitle between the
    // masked placeholder and the full hex-encoded key. Pure UI
    // state; doesn't touch keyring or server.
    let key_row_for_reveal = panels.server.auth_key_row.downgrade();
    let current_key_for_reveal = Rc::clone(&current_auth_key);
    let revealed_for_reveal = Rc::clone(&auth_key_revealed);
    panels
        .server
        .auth_key_reveal_button
        .connect_clicked(move |btn| {
            let Some(key_row) = key_row_for_reveal.upgrade() else {
                return;
            };
            let Ok(key_opt) = current_key_for_reveal.try_borrow() else {
                return;
            };
            let Some(bytes) = key_opt.as_ref() else {
                return;
            };
            let now_revealed = !revealed_for_reveal.get();
            revealed_for_reveal.set(now_revealed);
            if now_revealed {
                key_row.set_subtitle(&crate::sidebar::server_panel::auth_key_to_hex(bytes));
                btn.set_icon_name("view-conceal-symbolic");
                btn.set_tooltip_text(Some("Hide key"));
                // Flip the accessible label alongside the icon /
                // tooltip so screen readers announce the current
                // action rather than the stale build-time label.
                // Per `CodeRabbit` round 1 on PR #406.
                btn.update_property(&[gtk4::accessible::Property::Label("Hide key")]);
            } else {
                key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
                btn.set_icon_name("view-reveal-symbolic");
                btn.set_tooltip_text(Some("Reveal key"));
                btn.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
            }
        });

    // Copy button — always copies the FULL hex key regardless of
    // reveal state. Users typically click Copy without clicking
    // Reveal first.
    let current_key_for_copy = Rc::clone(&current_auth_key);
    panels
        .server
        .auth_key_copy_button
        .connect_clicked(move |btn| {
            let Ok(key_opt) = current_key_for_copy.try_borrow() else {
                return;
            };
            let Some(bytes) = key_opt.as_ref() else {
                return;
            };
            let hex = crate::sidebar::server_panel::auth_key_to_hex(bytes);
            // Grab the display's clipboard via the button's widget
            // ancestry. `clipboard()` on a widget returns the
            // primary clipboard for the display it's attached to.
            let clipboard = btn.clipboard();
            clipboard.set_text(&hex);
            if let Some(overlay) = toast_overlay_for_copy.upgrade() {
                overlay.add_toast(adw::Toast::new("Key copied to clipboard"));
            }
        });

    // Regenerate button — generates a fresh 32-byte key,
    // applies it to the live server, persists to keyring, and
    // updates the display row subtitle (preserving the current
    // revealed state so the user can verify the new value
    // immediately).
    //
    // Order of operations (per `CodeRabbit` round 2 on PR #406):
    // 1. Apply to the running server via
    //    `apply_live_auth_change` — shared with the toggle path.
    //    On failure (mutex poisoned, borrow race), toast + return
    //    BEFORE touching keyring or UI.
    // 2. Persist to keyring. Failure here is non-fatal (the
    //    in-memory key still works this session; next launch
    //    would read the OLD keyring value, which now forces the
    //    user to click Regenerate again — better than the old
    //    order where a keyring success + server failure would
    //    leave next-launch using a key the server never
    //    accepted).
    // 3. UI mutation (`current_auth_key`, subtitle, toast).
    //
    // Regenerate keeps `auth_required = true`, so the mDNS TXT
    // doesn't change — `apply_live_auth_change` skips the
    // advertiser rebuild when passed `widgets = None`.
    let key_row_for_regen = panels.server.auth_key_row.downgrade();
    let current_key_for_regen = Rc::clone(&current_auth_key);
    let revealed_for_regen = Rc::clone(&auth_key_revealed);
    panels
        .server
        .auth_key_regenerate_button
        .connect_clicked(move |_btn| {
            let Some(key_row) = key_row_for_regen.upgrade() else {
                return;
            };
            let fresh = sdr_server_rtltcp::auth::generate_random_auth_key();

            // Step 1: live server apply. `widgets = None` because
            // regenerate doesn't flip `auth_required`, so no
            // advertiser rebuild is needed.
            if !apply_live_auth_change(
                &running_for_auth_regen,
                Some(fresh.clone()),
                None,
                &toast_overlay_for_regen,
            ) {
                return;
            }

            // Step 2: persist to keyring. Failure is tolerable —
            // current in-memory key still works this session; the
            // user can click Regenerate again later when the
            // keyring recovers. Toast so they know, but don't
            // roll back the server (it already accepted the key).
            if let Err(e) = save_server_auth_key_to_keyring(&fresh) {
                tracing::warn!(%e, "rtl_tcp auth-key regenerate keyring write failed");
                if let Some(overlay) = toast_overlay_for_regen.upgrade() {
                    overlay.add_toast(adw::Toast::new(&format!(
                        "Couldn't save new key to keyring: {e}"
                    )));
                }
            }

            // Step 3: UI mutation after server + persistence
            // settled.
            *current_key_for_regen.borrow_mut() = Some(fresh.clone());
            if revealed_for_regen.get() {
                key_row.set_subtitle(&crate::sidebar::server_panel::auth_key_to_hex(&fresh));
            } else {
                key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
            }
            if let Some(overlay) = toast_overlay_for_regen.upgrade() {
                overlay.add_toast(adw::Toast::new("New key generated"));
            }
        });

    // Listener-cap live-apply. Changes on the spin row take effect
    // on the next client accept without restarting the server. The
    // row also persists to sdr_config via a separate signal
    // attached inside `server_panel.rs`; this handler only cares
    // about the running-server case. Per issue #395.
    panels.server.listener_cap_row.connect_value_notify(move |row| {
        let Ok(handle) = running_for_cap.try_borrow() else {
            // Another handler is holding the `RunningServer` borrow
            // (e.g. the share_row active-notify flipping server
            // start/stop). Skip this tick — the spin row's new
            // value is already persisted via the server_panel
            // signal, and the next accept after start will pick
            // it up through `build_server_config_from_panel`.
            return;
        };
        let Some(handle) = handle.as_ref() else {
            // Server not running — the spin row edit is already
            // persisted; nothing to apply live.
            return;
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to [MIN_LISTENER_CAP, MAX_LISTENER_CAP] at the widget level"
        )]
        let cap = row.value() as usize;
        handle.server.set_listener_cap(cap);
    });
}

/// Cadence for the server-stats poll that renders the "Server
/// status" rows. 500 ms is fast enough that "connected / waiting"
/// transitions feel instant while keeping the `ServerStats` clone +
/// row-subtitle churn off the critical path.
const SERVER_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Bits-per-byte conversion used in the Mbps formatter. Kept behind
/// a named constant so the arithmetic at the call site reads as
/// unit math ("bytes * `BITS_PER_BYTE` / duration / MEGA") instead
/// of opaque `8`s and `1_000_000`s.
const BITS_PER_BYTE: u64 = 8;
/// Megabits divisor for rendering Mbps. `1_000_000` matches
/// telecom/carrier conventions for transport rates.
const BITS_PER_MEGABIT: f64 = 1_000_000.0;

/// Weak references to every widget the server-status poll tick
/// touches. Held by the poll closure INSTEAD of a strong
/// `ServerPanel` clone so the closure doesn't bump the widgets'
/// `GObject` refcounts past window lifetime.
///
/// The original design cloned the whole `ServerPanel` into the
/// closure and relied on a single `widget_weak.upgrade().is_none()`
/// break gate — but the clone held strong refs to every widget,
/// including the group itself, so the weak check could never fire
/// and the 500 ms timer leaked past window close. Every
/// panel-touching closure in this file now uses weak refs for the
/// same reason (see `connect_rtl_tcp_discovery`'s pattern).
struct ServerStatusWidgetsWeak {
    status_row: glib::WeakRef<adw::ExpanderRow>,
    status_client_row: glib::WeakRef<adw::ActionRow>,
    status_uptime_row: glib::WeakRef<adw::ActionRow>,
    status_data_rate_row: glib::WeakRef<adw::ActionRow>,
    status_commanded_row: glib::WeakRef<adw::ActionRow>,
    activity_log_row: glib::WeakRef<adw::ExpanderRow>,
    activity_log_list: glib::WeakRef<gtk4::ListBox>,
    clients_row: glib::WeakRef<adw::ExpanderRow>,
    clients_list: glib::WeakRef<gtk4::ListBox>,
}

/// Snapshot of upgraded strong references held for the duration of
/// a single poll tick. All nine widgets upgrade together or we
/// `Break` the timer — render functions then read these fields
/// directly without needing their own weak-ref fallbacks.
struct ServerStatusWidgets {
    status_row: adw::ExpanderRow,
    status_client_row: adw::ActionRow,
    status_uptime_row: adw::ActionRow,
    status_data_rate_row: adw::ActionRow,
    status_commanded_row: adw::ActionRow,
    activity_log_row: adw::ExpanderRow,
    activity_log_list: gtk4::ListBox,
    clients_row: adw::ExpanderRow,
    clients_list: gtk4::ListBox,
}

impl ServerStatusWidgetsWeak {
    fn from_panel(panel: &sidebar::ServerPanel) -> Self {
        Self {
            status_row: panel.status_row.downgrade(),
            status_client_row: panel.status_client_row.downgrade(),
            status_uptime_row: panel.status_uptime_row.downgrade(),
            status_data_rate_row: panel.status_data_rate_row.downgrade(),
            status_commanded_row: panel.status_commanded_row.downgrade(),
            activity_log_row: panel.activity_log_row.downgrade(),
            activity_log_list: panel.activity_log_list.downgrade(),
            clients_row: panel.clients_row.downgrade(),
            clients_list: panel.clients_list.downgrade(),
        }
    }

    /// Upgrade every weak ref atomically. Returns `None` if any
    /// one widget has been destroyed — the caller breaks its
    /// timer instead of rendering against a partially-dead panel.
    fn upgrade(&self) -> Option<ServerStatusWidgets> {
        Some(ServerStatusWidgets {
            status_row: self.status_row.upgrade()?,
            status_client_row: self.status_client_row.upgrade()?,
            status_uptime_row: self.status_uptime_row.upgrade()?,
            status_data_rate_row: self.status_data_rate_row.upgrade()?,
            status_commanded_row: self.status_commanded_row.upgrade()?,
            activity_log_row: self.activity_log_row.upgrade()?,
            activity_log_list: self.activity_log_list.upgrade()?,
            clients_row: self.clients_row.upgrade()?,
            clients_list: self.clients_list.upgrade()?,
        })
    }
}

/// Poll `Server::stats()` on a fixed cadence, render the four
/// status rows from the snapshot, and auto-stop the server if
/// `has_stopped()` becomes true (e.g. USB dongle unplugged or
/// accept-thread error).
///
/// Auto-stop flips the `share_row` back off, which re-enters the
/// switch's `connect_active_notify` handler — that branch drops the
/// `RunningServer` handle and releases the dongle for subsequent
/// reopens. Without this the UI would lie about the server's
/// running state indefinitely.
///
/// Data-rate is computed from the delta in `bytes_sent` between
/// consecutive poll ticks. Counter resets (on disconnect) produce
/// negative deltas which we clamp to zero so the row reads "0 bps"
/// instead of a bogus megabit-scale number during the transient.
fn connect_server_status_polling(
    panels: &SidebarPanels,
    running: Rc<RefCell<Option<RunningServer>>>,
    apply_visibility: impl Fn() + 'static,
) {
    use std::cell::Cell;

    let widgets_weak = ServerStatusWidgetsWeak::from_panel(&panels.server);
    let share_row_weak = panels.server.share_row.downgrade();
    let last_bytes_sent = Rc::new(Cell::new(0u64));
    // Activity-log diff key: (ring_len, newest_instant). Rendering
    // is cheap but clearing the ListBox resets any user scroll
    // position, so we short-circuit on unchanged ticks.
    let last_activity_key: Rc<Cell<(usize, Option<Instant>)>> = Rc::new(Cell::new((0, None)));
    // Clients-list diff key. Hashes `(id, peer, role, drops,
    // elapsed_secs)` per client so a stable connected set with
    // ticking uptime / incrementing drop counters still triggers
    // a rebuild — the previous id-set-only hash froze row
    // subtitles once the set stabilized, so a 10-minute session
    // would show "0s" uptime forever. `Option<u64>` so the
    // stop/start reset path can invalidate the cache by setting
    // `None`; without that, an "empty set → empty set" transition
    // across stop/start would short-circuit the first post-start
    // render and leave the expander blank (the placeholder row
    // was removed by `reset_clients_list`). Per `CodeRabbit`
    // round 2 on PR #406.
    let last_clients_key: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));

    // Separate subscription on the Stop button. Flipping the switch
    // off is the single canonical stop path — pointing the button
    // there avoids a second teardown codepath that could drift.
    let stop_share_row_weak = share_row_weak.clone();
    panels.server.status_stop_button.connect_clicked(move |_| {
        if let Some(share) = stop_share_row_weak.upgrade() {
            share.set_active(false);
        }
    });

    let _ = glib::timeout_add_local(SERVER_STATUS_POLL_INTERVAL, move || {
        // Upgrade all the status widgets in one shot. If any is gone
        // (window closed → sidebar dropped → widgets orphaned), tear
        // the timer down. Strong refs live only for the duration of
        // this tick — dropped at function return — so they never
        // contribute to the long-running GObject refcount.
        let Some(widgets) = widgets_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // Snapshot `Server::stats()` under the borrow. `stats()`
        // internally locks a Mutex — the return is a Clone, so the
        // borrow scope is tight.
        let snapshot = running
            .borrow()
            .as_ref()
            .map(|h| (h.server.stats(), h.server.has_stopped()));
        let Some((stats, stopped)) = snapshot else {
            // No server running — nothing to render, keep ticking
            // (the share switch handler will spin us up again).
            return glib::ControlFlow::Continue;
        };

        // If the accept thread exited on its own (USB unplug,
        // fatal error), auto-flip the share switch off. Re-enters
        // the switch handler, which drops the server handle.
        if stopped {
            tracing::warn!("rtl_tcp server stopped on its own — flipping share switch off");
            if let Some(share) = share_row_weak.upgrade() {
                share.set_active(false);
            }
            apply_visibility();
            return glib::ControlFlow::Continue;
        }

        render_status_rows(&widgets, &stats, &last_bytes_sent);
        render_activity_log(&widgets, &stats, &last_activity_key);
        render_clients_list(&widgets, &stats, &last_clients_key);
        glib::ControlFlow::Continue
    });
}

/// Write the current `ServerStats` snapshot into the four status
/// rows. Uses `last_bytes_sent` to compute a rolling data-rate from
/// delta-over-poll-interval. Takes upgraded `ServerStatusWidgets`
/// — strong refs held only for this call's duration — so the poll
/// closure itself doesn't contribute to the long-running `GObject`
/// refcount.
///
/// Renders the FIRST connected client in the per-session rows
/// (client peer, uptime, commanded state, activity log). Multi-
/// client per-client UI rows land in PR B of #391; this commit
/// just wires the new `Vec<ClientInfo>` shape into the existing
/// single-client row layout so the server-panel keeps working.
/// The data-rate row switches to the aggregate
/// `total_bytes_sent` so operators see the full server throughput
/// even before PR B's per-client rows arrive.
fn render_status_rows(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_bytes_sent: &Rc<std::cell::Cell<u64>>,
) {
    use crate::sidebar::server_panel::{
        STATUS_IDLE_VALUE_SUBTITLE, STATUS_WAITING_FOR_CLIENT_SUBTITLE,
    };

    let first = stats.connected_clients.first();
    let extra = stats.connected_clients.len().saturating_sub(1);

    // Client row + expander subtitle. When there are N > 1 clients,
    // append "(+N-1 more)" so the row makes the multi-client state
    // visible even before PR B's per-client list exists.
    if let Some(info) = first {
        let peer_str = info.peer.to_string();
        let client_subtitle = if extra > 0 {
            format!("{peer_str} (+{extra} more)")
        } else {
            peer_str.clone()
        };
        widgets.status_client_row.set_subtitle(&client_subtitle);
        let expander_subtitle = if stats.connected_clients.len() == 1 {
            format!("Connected: {peer_str}")
        } else {
            format!("{} clients connected", stats.connected_clients.len())
        };
        widgets.status_row.set_subtitle(&expander_subtitle);
    } else {
        widgets
            .status_client_row
            .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
        widgets
            .status_row
            .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
    }

    // Uptime row — first client's uptime. PR B will show one row
    // per client, each with its own uptime.
    widgets.status_uptime_row.set_subtitle(&first.map_or_else(
        || STATUS_IDLE_VALUE_SUBTITLE.to_string(),
        |info| format_uptime(info.connected_since.elapsed()),
    ));

    // Data-rate row. Uses the cumulative `total_bytes_sent`
    // counter, which is monotonic within a single Server lifetime.
    // After a stop+start cycle the counter resets to 0 while
    // `last_bytes_sent` still holds the previous server's final
    // value — in that case `current < previous` is the restart
    // signal: rebase `last_bytes_sent` to the new counter and
    // report 0 bytes this tick rather than a bogus huge delta or
    // a long "0.0 kbps" flatline until the new server catches up
    // past the old final byte count. Per `CodeRabbit` round 2 on
    // PR #402.
    let current_bytes = stats.total_bytes_sent;
    let previous_bytes = last_bytes_sent.get();
    let delta = if current_bytes < previous_bytes {
        // Restart detected — the new server has already
        // accumulated `current_bytes` worth of traffic since its
        // start, so that's the best available estimate for
        // "bytes this tick". Reporting 0 or the saturating sub
        // would flatline the row until the new server exceeds
        // the old final count. Per `CodeRabbit` round 2 on
        // PR #402.
        current_bytes
    } else {
        current_bytes - previous_bytes
    };
    last_bytes_sent.set(current_bytes);
    widgets
        .status_data_rate_row
        .set_subtitle(&format_data_rate(delta, SERVER_STATUS_POLL_INTERVAL));

    // Commanded-state row — the most-recently-commanding client's
    // state. Pre-#392 any connected client can send `SetX`
    // commands, so picking the oldest client would let a later
    // peer's tune show up as the oldest peer's "stale" state.
    // `pick_most_recent_commander` resolves this by finding the
    // client whose `last_command` timestamp is newest (falls back
    // to the first connected client when nobody has commanded
    // yet). Post-#392, role-gated dispatch means only the
    // controller can record a command, so this helper naturally
    // resolves to the controller. Per `CodeRabbit` round 2 on
    // PR #402.
    let commander = pick_most_recent_commander(&stats.connected_clients);
    widgets
        .status_commanded_row
        .set_subtitle(&format_commanded_state(commander, &stats.initial));
}

/// Select the client whose most recent `last_command` timestamp is
/// newest. Falls back to the first connected client when nobody
/// has issued a command yet, and to `None` when no clients are
/// connected.
///
/// Shared between the commanded-state row and the activity-log
/// renderer so both surfaces track the same "who's actually
/// driving the dongle" peer. Pre-#392 this matters because any
/// client can command; post-#392 role-gated dispatch will make
/// this resolve to the controller every time.
fn pick_most_recent_commander(
    clients: &[sdr_server_rtltcp::ClientInfo],
) -> Option<&sdr_server_rtltcp::ClientInfo> {
    clients
        .iter()
        .filter_map(|c| c.last_command.map(|(_, t)| (c, t)))
        .max_by_key(|&(_, t)| t)
        .map(|(c, _)| c)
        .or_else(|| clients.first())
}

/// Render a `Duration` as `Nh Nm Ns` / `Nm Ns` / `Ns` depending on
/// magnitude. Keeps the row readable at a glance without fighting a
/// full clock component.
fn format_uptime(elapsed: Duration) -> String {
    let total_secs = elapsed.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Render bytes/interval as a human-readable data rate. Picks the
/// right unit automatically: kbps when we're below 1 Mbps (quiet
/// clients), Mbps otherwise. `rtl_tcp` IQ streams at 2.4 MS/s × 2
/// bytes per sample = ~4.8 Mbps, so the Mbps case dominates in
/// practice.
#[allow(
    clippy::cast_precision_loss,
    reason = "intermediate f64 conversion for rate math; Mbps precision is cosmetic"
)]
fn format_data_rate(bytes: u64, interval: Duration) -> String {
    let secs = interval.as_secs_f64();
    if secs <= 0.0 {
        return "—".to_string();
    }
    let bits_per_sec = (bytes as f64 * BITS_PER_BYTE as f64) / secs;
    if bits_per_sec < BITS_PER_MEGABIT {
        format!("{:.1} kbps", bits_per_sec / 1_000.0)
    } else {
        format!("{:.2} Mbps", bits_per_sec / BITS_PER_MEGABIT)
    }
}

/// Render the "Tuned to" row subtitle for the first connected
/// client. Combines frequency, sample rate and gain into one
/// line. Unset `current_*` fields on the client fall back to the
/// server's **configured** `initial` state (what the user set up
/// in the server panel or CLI args), NOT the library's upstream
/// `rtl_tcp.c` defaults. `None` input (no clients connected)
/// renders as the idle placeholder. Per `CodeRabbit` round 1 on
/// PR #402.
fn format_commanded_state(
    info: Option<&sdr_server_rtltcp::ClientInfo>,
    initial: &sdr_server_rtltcp::InitialDeviceState,
) -> String {
    let Some(info) = info else {
        return crate::sidebar::server_panel::STATUS_IDLE_VALUE_SUBTITLE.to_string();
    };
    let freq_hz = info.current_freq_hz.unwrap_or(initial.center_freq_hz);
    let sample_rate_hz = info
        .current_sample_rate_hz
        .unwrap_or(initial.sample_rate_hz);
    let gain_text = match (info.current_gain_auto, info.current_gain_tenths_db) {
        (Some(true), _) => "auto".to_string(),
        (_, Some(gain_tenths)) => {
            #[allow(clippy::cast_precision_loss, reason = "gain tenths-of-dB, cosmetic")]
            let db = f64::from(gain_tenths) / 10.0;
            format!("{db:.1} dB")
        }
        // Client hasn't sent a gain command yet — show whatever
        // the server started with. `initial.gain_tenths_db = None`
        // encodes upstream's "automatic" mode (CLI `-g 0`).
        _ => match initial.gain_tenths_db {
            None => "auto".to_string(),
            Some(gain_tenths) => {
                #[allow(clippy::cast_precision_loss, reason = "gain tenths-of-dB, cosmetic")]
                let db = f64::from(gain_tenths) / 10.0;
                format!("{db:.1} dB")
            }
        },
    };
    format!(
        "{} @ {} • gain {}",
        format_hz(freq_hz),
        format_hz(sample_rate_hz),
        gain_text
    )
}

/// Short Hz formatter — kHz / MHz / GHz depending on magnitude.
/// Kept local to this module because the status row's formatting
/// needs differ from the header-bar frequency selector (which has
/// its own 12-digit grid display).
fn format_hz(hz: u32) -> String {
    let hz_f = f64::from(hz);
    if hz >= 1_000_000_000 {
        format!("{:.3} GHz", hz_f / 1_000_000_000.0)
    } else if hz >= 1_000_000 {
        format!("{:.3} MHz", hz_f / 1_000_000.0)
    } else if hz >= 1_000 {
        format!("{:.3} kHz", hz_f / 1_000.0)
    } else {
        format!("{hz} Hz")
    }
}

/// Rebuild the activity-log list from the most-recently-commanding
/// client's `recent_commands` ring if it has actually changed since
/// the last render. The "changed?" check uses the ring length + the
/// timestamp of the newest entry so we skip the clear-and-rebuild
/// on idle ticks — preserves any scroll position the user has in
/// the `ListBox`.
///
/// Uses [`pick_most_recent_commander`] rather than just the first
/// connected client because pre-#392 any client can send commands
/// — the oldest client would shadow a newer peer's activity. Per
/// `CodeRabbit` round 2 on PR #402. PR B of #391 replaces this with
/// a per-client log tab so every client's commands show under
/// their own row; until then, tracking "whoever's driving right
/// now" is the right single-row compromise.
fn render_activity_log(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_rendered: &Rc<std::cell::Cell<(usize, Option<Instant>)>>,
) {
    use crate::sidebar::server_panel::ACTIVITY_LOG_EMPTY_SUBTITLE;

    let Some(commander) = pick_most_recent_commander(&stats.connected_clients) else {
        // No connected client → clear + show empty subtitle if
        // we're not already in that state. Track the idle cache
        // key as (0, None) so the render skips on subsequent
        // idle ticks.
        let current_key = (0usize, None::<Instant>);
        if current_key == last_rendered.get() {
            return;
        }
        last_rendered.set(current_key);
        while let Some(child) = widgets.activity_log_list.first_child() {
            widgets.activity_log_list.remove(&child);
        }
        widgets
            .activity_log_row
            .set_subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE);
        return;
    };
    let ring: &std::collections::VecDeque<(sdr_server_rtltcp::CommandOp, Instant)> =
        &commander.recent_commands;

    let newest = ring.back().map(|(_, t)| *t);
    let current_key = (ring.len(), newest);
    if current_key == last_rendered.get() {
        return;
    }
    last_rendered.set(current_key);

    // Clear the ListBox children. GTK4 ListBox has no mass-remove,
    // so walk the child list.
    while let Some(child) = widgets.activity_log_list.first_child() {
        widgets.activity_log_list.remove(&child);
    }

    if ring.is_empty() {
        widgets
            .activity_log_row
            .set_subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE);
        return;
    }

    widgets
        .activity_log_row
        .set_subtitle(&format!("{} commands", ring.len()));
    // Newest first so the user doesn't have to scroll to see the
    // most recent activity.
    let now = Instant::now();
    for (op, at) in ring.iter().rev() {
        let row = adw::ActionRow::builder()
            .title(format!("{op:?}"))
            .subtitle(format_log_age(now.saturating_duration_since(*at)))
            .activatable(false)
            .build();
        widgets.activity_log_list.append(&row);
    }
}

/// Render the "Connected clients" list — one row per client
/// with peer, role badge, duration, and drops counter. Empty
/// state: single "No clients connected" placeholder row plus
/// matching expander subtitle.
///
/// **Rebuild trigger.** Hashes `(id, peer, role, drops,
/// elapsed_secs)` for every connected client; rebuilds when
/// the hash changes. That covers both accept/disconnect
/// transitions AND per-row field churn (ticking uptime,
/// incrementing drop counters), so the displayed subtitles
/// stay live throughout a session. Scroll / hover state is
/// preserved on unchanged ticks. Per issue #395 +
/// `CodeRabbit` round 2 on PR #406.
///
/// **Stop/start invalidation.** On server stop
/// `reset_clients_list` empties the `ListBox` but can't reach
/// the cache cell across function boundaries; instead,
/// `render_clients_list` treats `first_child().is_none()` as
/// "reset has run, force rebuild" so an empty→empty session
/// transition still repaints the placeholder. Per `CodeRabbit`
/// round 2 on PR #406.
fn render_clients_list(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_rendered: &Rc<std::cell::Cell<Option<u64>>>,
) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use crate::sidebar::server_panel::CLIENTS_LIST_EMPTY_SUBTITLE;

    // Compute a diff key that bumps on *any* rendered-field
    // change — not just accept / disconnect. Including peer,
    // role, drops, and (rounded-seconds) uptime in the hash
    // means a stable connected set with ticking uptime or
    // incrementing drops still triggers rebuilds. The previous
    // id-set-only key froze row subtitles once the client set
    // stabilized. Per `CodeRabbit` round 2 on PR #406.
    //
    // Rebuild cost is ~N widget builds at 2 Hz (N ≤ 32 at the
    // listener cap); trivial vs. the USB / DSP hot path.
    let now = Instant::now();
    let mut key_fields: Vec<(sdr_server_rtltcp::ClientId, String, u8, u64, u64)> = stats
        .connected_clients
        .iter()
        .map(|c| {
            let role_disc = match c.role {
                sdr_server_rtltcp::extension::Role::Control => 0u8,
                sdr_server_rtltcp::extension::Role::Listen => 1u8,
            };
            let elapsed_secs = now.saturating_duration_since(c.connected_since).as_secs();
            (
                c.id,
                c.peer.to_string(),
                role_disc,
                c.buffers_dropped,
                elapsed_secs,
            )
        })
        .collect();
    key_fields.sort_unstable_by_key(|(id, _, _, _, _)| *id);
    let mut hasher = DefaultHasher::new();
    key_fields.hash(&mut hasher);
    let current_key = hasher.finish();

    // Invalidate the cache when the ListBox has been cleared
    // externally (by `reset_clients_list` on server stop). Without
    // this, an "empty set → empty set" transition across stop/start
    // would match the prior hash and short-circuit the first-tick
    // render, leaving the expander visually blank. The empty
    // state's placeholder row is a single child, so
    // `first_child().is_none()` distinguishes the reset state from
    // the rendered-empty state. Per `CodeRabbit` round 2 on PR #406.
    let list_was_reset = widgets.clients_list.first_child().is_none();
    if !list_was_reset && last_rendered.get() == Some(current_key) {
        return;
    }
    last_rendered.set(Some(current_key));

    // Clear the ListBox. GTK4 ListBox has no mass-remove.
    while let Some(child) = widgets.clients_list.first_child() {
        widgets.clients_list.remove(&child);
    }

    if stats.connected_clients.is_empty() {
        widgets
            .clients_row
            .set_subtitle(CLIENTS_LIST_EMPTY_SUBTITLE);
        let empty_row = adw::ActionRow::builder()
            .title(CLIENTS_LIST_EMPTY_SUBTITLE)
            .activatable(false)
            .css_classes(["dim-label"])
            .build();
        widgets.clients_list.append(&empty_row);
        return;
    }

    // Expander subtitle shows the count so a collapsed expander
    // still communicates whether the server has activity.
    let count = stats.connected_clients.len();
    widgets.clients_row.set_subtitle(&if count == 1 {
        "1 client".to_string()
    } else {
        format!("{count} clients")
    });

    // Build per-client rows. Controller first (if any) so the
    // accent-colored row sits at the top; listeners render below
    // in the order the registry has them (acceptance order, per
    // `ClientRegistry`). Order isn't a hard contract — if a
    // future registry reorders for its own reasons, this just
    // changes visual order.
    let mut ordered: Vec<&sdr_server_rtltcp::ClientInfo> = stats.connected_clients.iter().collect();
    ordered.sort_by_key(|c| match c.role {
        sdr_server_rtltcp::extension::Role::Control => 0u8,
        sdr_server_rtltcp::extension::Role::Listen => 1u8,
    });

    // Reuse the `now` captured for the diff-key hash so the
    // displayed duration and the hashed `elapsed_secs` are
    // sampled from the same instant — avoids a split where
    // the hash matches but the render shows a one-tick-newer
    // duration (or vice-versa).
    for info in ordered {
        let (role_label, role_css) = match info.role {
            sdr_server_rtltcp::extension::Role::Control => ("Controller", "accent"),
            sdr_server_rtltcp::extension::Role::Listen => ("Listener", "dim-label"),
        };
        let duration = format_uptime(now.saturating_duration_since(info.connected_since));
        let subtitle = if info.buffers_dropped > 0 {
            format!(
                "{role_label} · {duration} · {drops} drops",
                drops = info.buffers_dropped
            )
        } else {
            format!("{role_label} · {duration}")
        };
        let row = adw::ActionRow::builder()
            .title(info.peer.to_string())
            .subtitle(&subtitle)
            .activatable(false)
            .build();
        // Prefix badge: a colored dot (accent for Control, dim
        // for Listen). Small and unobtrusive but enough to
        // distinguish the controller at a glance in a dense list.
        let badge = gtk4::Image::from_icon_name("media-record-symbolic");
        badge.add_css_class(role_css);
        row.add_prefix(&badge);
        widgets.clients_list.append(&row);
    }
}

/// Reset activity-log list + subtitle on stop. Without this the
/// list would persist after the server stopped — misleading users
/// into thinking the log reflects a currently-running session.
fn reset_activity_log(panel: &ServerSwitchWidgets) {
    use crate::sidebar::server_panel::ACTIVITY_LOG_EMPTY_SUBTITLE;
    while let Some(child) = panel.activity_log_list.first_child() {
        panel.activity_log_list.remove(&child);
    }
    panel
        .activity_log_row
        .set_subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE);
}

/// Reset the connected-clients list to its empty state. Called on
/// server stop so the next start doesn't surface stale client rows
/// before the first poll tick repopulates. Per issue #395.
fn reset_clients_list(panel: &ServerSwitchWidgets) {
    use crate::sidebar::server_panel::CLIENTS_LIST_EMPTY_SUBTITLE;
    while let Some(child) = panel.clients_list.first_child() {
        panel.clients_list.remove(&child);
    }
    panel.clients_row.set_subtitle(CLIENTS_LIST_EMPTY_SUBTITLE);
}

/// Render an elapsed duration as a compact "age" string for the
/// activity-log rows. Narrower set of buckets than the discovery
/// formatter — commands arrive in bursts during a session, so the
/// "just now" / seconds-ago distinction matters but hours isn't
/// common in a single session.
fn format_log_age(elapsed: Duration) -> String {
    const JUST_NOW_THRESHOLD: Duration = Duration::from_secs(2);
    let secs = elapsed.as_secs();
    if elapsed < JUST_NOW_THRESHOLD {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Reset status rows to their idle-no-client state. Called when the
/// server stops so the user doesn't see stale "connected at 127.0.0.1"
/// / "uptime 5m" data after they flipped the share switch off.
fn reset_status_rows(panel: &ServerSwitchWidgets) {
    use crate::sidebar::server_panel::{
        STATUS_IDLE_VALUE_SUBTITLE, STATUS_WAITING_FOR_CLIENT_SUBTITLE,
    };
    panel
        .status_row
        .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
    panel
        .status_client_row
        .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
    panel
        .status_uptime_row
        .set_subtitle(STATUS_IDLE_VALUE_SUBTITLE);
    panel
        .status_data_rate_row
        .set_subtitle(STATUS_IDLE_VALUE_SUBTITLE);
    panel
        .status_commanded_row
        .set_subtitle(STATUS_IDLE_VALUE_SUBTITLE);
}

/// Upstream `rtl_tcp`'s `-D` flag accepts 0 = off, 2 = Q-branch
/// direct sampling. Only those two values are meaningful for the
/// UI switch; I-branch (1) is deliberately not exposed because
/// upstream's CLI also hardcodes 2 for `-D`.
const DIRECT_SAMPLING_OFF: i32 = 0;
/// See [`DIRECT_SAMPLING_OFF`]. 2 selects the Q branch.
const DIRECT_SAMPLING_Q_BRANCH: i32 = 2;
/// Buffer-capacity sentinel passed to `ServerConfig`. `0` tells
/// the server crate to use its internal `DEFAULT_BUFFER_CAPACITY`,
/// keeping the UI honest about "we're not overriding this" rather
/// than pinning a value the server may later tune.
const SERVER_BUFFER_CAPACITY_DEFAULT: usize = 0;

/// Read the server panel widget values and build a `ServerConfig`
/// off them. Takes the full `ServerPanel` by reference so the arg
/// list stays short and the fn signature documents the "this reads
/// EVERY relevant row" contract clearly.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "spin-row values are bounded to u16/u32 ranges at the widget level"
)]
/// Build a `ServerConfig` from the panel's current widget state.
///
/// **`auth_key` parameter policy**: caller passes the pending
/// key already loaded into the panel's `current_auth_key` cell.
/// This is NOT re-derived inside the function via
/// `ensure_server_auth_key()` — doing so would risk a second
/// generate-and-save call with a different random value if the
/// keyring is unavailable between the UI-seed moment and the
/// server-start moment. Single source of truth: the key shown
/// by the Reveal button is exactly what `Server::start`
/// receives. Per `CodeRabbit` round 1 on PR #406.
fn build_server_config_from_panel(
    panel: &ServerSwitchWidgets,
    pending_auth_key: Option<Vec<u8>>,
) -> ServerConfig {
    use std::net::SocketAddr;

    use crate::sidebar::server_panel::{BIND_ALL_INTERFACES_IDX, BIND_LOOPBACK_IDX};

    let port = panel.port_row.value() as u16;
    // Match arm bodies duplicate between `BIND_LOOPBACK_IDX` and the
    // wildcard intentionally: the explicit arm documents the
    // expected value at a glance, and the wildcard catches transient
    // out-of-range indices GTK can emit during widget churn. Folding
    // them loses the at-a-glance enumeration of legal indices next
    // to the feature-flag constants.
    #[allow(
        clippy::match_same_arms,
        reason = "explicit legal-index arms document the rule"
    )]
    let bind = match panel.bind_row.selected() {
        BIND_LOOPBACK_IDX => SocketAddr::from(([127, 0, 0, 1], port)),
        BIND_ALL_INTERFACES_IDX => SocketAddr::from(([0, 0, 0, 0], port)),
        _ => SocketAddr::from(([127, 0, 0, 1], port)),
    };

    let center_freq_hz = panel.center_freq_row.value() as u32;
    // Sample-rate rows share the SAMPLE_RATES table via
    // `source_panel::build_rtlsdr_rows` ordering. `SAMPLE_RATES`
    // holds f64 values; the server API wants u32 Hz, so round on
    // the way across. Out-of-range selectors fall back on the
    // upstream rtl_tcp.c default.
    let sample_rate_hz = SAMPLE_RATES
        .get(panel.sample_rate_row.selected() as usize)
        .copied()
        .map_or(sdr_server_rtltcp::DEFAULT_SAMPLE_RATE_HZ, |rate| {
            rate.round() as u32
        });

    // UI treats gain = 0.0 as auto (None), matching upstream's
    // `-g 0` semantics. Any positive value becomes tenths-of-dB.
    let gain_db = panel.gain_row.value();
    let gain_tenths_db = if gain_db > 0.0 {
        Some((gain_db * 10.0).round() as i32)
    } else {
        None
    };

    let ppm = panel.ppm_row.value() as i32;
    let bias_tee = panel.bias_tee_row.is_active();
    let direct_sampling = if panel.direct_sampling_row.is_active() {
        DIRECT_SAMPLING_Q_BRANCH
    } else {
        DIRECT_SAMPLING_OFF
    };

    // Compression combo maps index → CodecMask. Unknown / transient
    // indices (GTK can emit garbage during widget-model churn) fall
    // back to `NONE_ONLY` — the wire-safe default that preserves
    // compatibility with every existing rtl_tcp client.
    let compression = match panel.compression_row.selected() {
        crate::sidebar::server_panel::COMPRESSION_LZ4_IDX => {
            sdr_server_rtltcp::codec::CodecMask::NONE_AND_LZ4
        }
        _ => sdr_server_rtltcp::codec::CodecMask::NONE_ONLY,
    };

    ServerConfig {
        bind,
        device_index: 0,
        initial: InitialDeviceState {
            center_freq_hz,
            sample_rate_hz,
            gain_tenths_db,
            ppm,
            bias_tee,
            direct_sampling,
        },
        buffer_capacity: SERVER_BUFFER_CAPACITY_DEFAULT,
        compression,
        // Listener cap pulled from the panel's live widget value so
        // the spin row's current position is the single source of
        // truth at server-start time. Later live-update calls flow
        // through `Server::set_listener_cap` directly. Per #395.
        listener_cap: panel.listener_cap_row.value() as usize,
        // Auth key plumbed from the caller. The panel's
        // `auth_require_row.is_active()` still dictates whether
        // auth is on — caller passes `Some(key)` only when the
        // toggle is active. Caller has already validated the
        // key length via `ensure_server_auth_key()`; `Server::start`
        // re-validates defensively before bind. Per `CodeRabbit`
        // round 1 on PR #406.
        auth_key: if panel.auth_require_row.is_active() {
            pending_auth_key
        } else {
            None
        },
    }
}

/// Start an mDNS advertiser for the running `Server` using the
/// user's chosen nickname (falling back to `local_hostname()` if
/// the entry is empty or whitespace). Errors propagate to the
/// caller so the UI can toast them — the server itself keeps
/// running regardless, just without LAN advertising.
fn build_advertiser(
    server: &Server,
    nickname_raw: &str,
) -> Result<Advertiser, sdr_rtltcp_discovery::DiscoveryError> {
    let nickname = nickname_raw.trim();
    let nickname = if nickname.is_empty() {
        local_hostname()
    } else {
        nickname.to_string()
    };
    let host = local_hostname();
    // DNS-SD instance names must be unique on the LAN. Combine host
    // + nickname the same way the CLI does in
    // `sdr-server-rtltcp/src/bin/sdr-rtl-tcp.rs::announce_over_mdns`.
    let instance_name = if nickname == host {
        nickname.clone()
    } else {
        format!("{host} {nickname}")
    };
    let tuner_info = server.tuner_info();
    let opts = AdvertiseOptions {
        port: server.bind_address().port(),
        instance_name,
        hostname: host.clone(),
        txt: TxtRecord {
            tuner: tuner_info.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            gains: tuner_info.gain_count,
            nickname,
            txbuf: None,
            // Advertise the codec bitmask so our own clients
            // know up-front whether to send an extended-protocol
            // hello (`NONE_ONLY` → no hello, vanilla path).
            // Vanilla mDNS consumers (non-sdr-rs clients that
            // don't know this key) just ignore it. #307.
            codecs: Some(server.compression().to_wire()),
            // Advertise `auth_required=true` when the running
            // server has a key configured so clients can prompt
            // for a key BEFORE dispatching connect. Read from
            // `Server::auth_required()` (not the UI's auth-toggle
            // state) because a future live-update via
            // `Server::set_auth_key` is the single source of truth.
            // #394 + #395.
            auth_required: server.auth_required().then_some(true),
        },
    };
    Advertiser::announce(opts)
}

/// Apply an auth-key change to the running server and refresh
/// the mDNS advertiser atomically. Returns `true` iff the
/// server actually holds the new state; `false` means the
/// caller must revert the UI so it stays in sync.
///
/// **Success cases:**
/// - No server is running (`running` cell contains `None`): no
///   server-side change to apply; caller can proceed with UI.
/// - Server is running and `set_auth_key(new)` returns `Ok`.
///   The advertiser is then rebuilt via
///   `refresh_advertiser_for_auth_change` so the TXT record
///   reflects the new `auth_required` state.
///
/// **Failure cases:**
/// - `try_borrow_mut` on the running-server cell fails (another
///   handler holds a mutable borrow — rare, mid-click race).
///   Caller reverts the switch; next click usually wins.
/// - `set_auth_key` returns `Err` (e.g., mutex poisoned). The
///   toast surfaces the error and the caller reverts UI state.
///
/// Does NOT touch UI state — caller owns the UI mutation gate.
/// Per `CodeRabbit` round 1 on PR #406.
fn apply_live_auth_change(
    running: &Rc<RefCell<Option<RunningServer>>>,
    new_key: Option<Vec<u8>>,
    widgets: Option<&ServerSwitchWidgets>,
    toast_overlay: &glib::WeakRef<adw::ToastOverlay>,
) -> bool {
    let Ok(mut handle_cell) = running.try_borrow_mut() else {
        tracing::warn!("auth change skipped — running-server cell busy");
        return false;
    };
    let Some(handle) = handle_cell.as_mut() else {
        // Server not running — UI-only change is always fine.
        return true;
    };
    if let Err(e) = handle.server.set_auth_key(new_key) {
        tracing::warn!(%e, "Server::set_auth_key failed on live auth change");
        if let Some(overlay) = toast_overlay.upgrade() {
            overlay.add_toast(adw::Toast::new(&format!(
                "Couldn't update auth on the running server: {e}"
            )));
        }
        return false;
    }
    // mDNS TXT refresh. Only meaningful when we have widget refs
    // (caller upgraded `widgets_weak` before the call).
    if let Some(widgets) = widgets {
        refresh_advertiser_for_auth_change(handle, widgets, toast_overlay);
    }
    true
}

/// Tear down and rebuild the running server's mDNS advertiser so
/// its TXT record reflects the current `Server::auth_required()`
/// state. Called after every successful live auth toggle so
/// discovery clients see the new `auth_required=true|absent`
/// flag without waiting for a server restart.
///
/// **No-op when:**
/// - No server is running (`handle` is `None`).
/// - The user has advertising turned off (`advertise_row`
///   inactive). Honors the user's choice — we don't bring
///   advertising back online just because auth flipped.
///
/// **Error path:** `build_advertiser` failures log + toast
/// (same pattern as the initial server-start advertise failure).
/// The server itself keeps running without a fresh TXT; worst
/// case, clients see stale auth metadata until the server is
/// restarted. Never panics, never leaves a half-registered
/// advertiser in place. Per `CodeRabbit` round 1 on PR #406.
fn refresh_advertiser_for_auth_change(
    handle: &mut RunningServer,
    widgets: &ServerSwitchWidgets,
    toast_overlay: &glib::WeakRef<adw::ToastOverlay>,
) {
    if !widgets.advertise_row.is_active() {
        // User turned advertising off — don't sneak it back on.
        return;
    }
    // Drop the old advertiser FIRST so its Drop-based unregister
    // fires before we re-announce under the same instance name.
    // mdns-sd allows back-to-back registers with the same name
    // but cleanly bracketed unregister/register avoids a window
    // where duplicate records briefly coexist on the LAN.
    drop(handle.advertiser.take());
    match build_advertiser(&handle.server, &widgets.nickname_row.text()) {
        Ok(adv) => {
            handle.advertiser = Some(adv);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "mDNS advertiser rebuild after auth toggle failed; TXT auth_required will be stale until next start"
            );
            if let Some(overlay) = toast_overlay.upgrade() {
                overlay.add_toast(adw::Toast::new(&format!(
                    "Couldn't refresh mDNS advertisement after auth toggle: {e}"
                )));
            }
        }
    }
}

/// Lock or unlock the server-config rows. Called with `true` on
/// start (so the user can't mutate config out from under a live
/// session) and `false` on stop. `share_row` itself stays sensitive
/// — that's how the user turns things off.
fn set_controls_locked(panel: &ServerSwitchWidgets, locked: bool) {
    let sensitive = !locked;
    panel.nickname_row.set_sensitive(sensitive);
    panel.port_row.set_sensitive(sensitive);
    panel.bind_row.set_sensitive(sensitive);
    panel.advertise_row.set_sensitive(sensitive);
    panel.compression_row.set_sensitive(sensitive);
    panel.device_defaults_row.set_sensitive(sensitive);
}

/// Format the subtitle string for a discovered `rtl_tcp` server row.
///
/// Emits three pieces separated by ` • `:
///
/// 1. `{connect_target}:{port}` — the address the Connect button will
///    dial (IPv4 address if we have one, otherwise the advertised
///    hostname).
/// 2. advertised mDNS hostname — only when it's non-empty AND
///    genuinely different from the connect target (i.e., we have an
///    IP and want to show the friendly name alongside it). The
///    hostname is stripped of any trailing `.local.` so we show
///    `shack-pi` instead of `shack-pi.local.`.
/// 3. `{tuner} · {gains} gains · seen {age}` — hardware info plus
///    the freshness indicator from `format_age`.
///
/// Kept as a free function (not a method on `DiscoveredServer`) so the
/// age-stamp convention stays a UI concern and the discovery crate
/// doesn't need to think about human-readable timestamps.
fn format_discovery_subtitle(server: &DiscoveredServer, elapsed: Duration) -> String {
    let connect_target = server
        .addresses
        .first()
        .map_or_else(|| server.hostname.clone(), ToString::to_string);
    let bare_hostname = bare_local_host(&server.hostname);
    // Compare `bare_hostname` against a similarly-trimmed view of the
    // connect target so the no-IP fallback (target = hostname) doesn't
    // render "shack-pi.local.:1234 • shack-pi • …" — one name twice.
    let bare_connect_target = bare_local_host(&connect_target);
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(format!("{connect_target}:{}", server.port));
    if !bare_hostname.is_empty() && bare_hostname != bare_connect_target {
        parts.push(bare_hostname.to_string());
    }
    parts.push(format!(
        "{} · {} gains · seen {}",
        server.txt.tuner,
        server.txt.gains,
        format_age(elapsed)
    ));
    parts.join(" • ")
}

/// Strip a trailing `.local.` / `.local` / `.` suffix from an mDNS
/// hostname so the user sees `shack-pi` instead of `shack-pi.local.`.
/// Purely presentational — resolution still happens against the full
/// name in the Connect button's dial path.
fn bare_local_host(host: &str) -> &str {
    host.trim_end_matches('.')
        .trim_end_matches(".local")
        .trim_end_matches('.')
}

/// Render an elapsed duration as a short human-readable age string.
///
/// Buckets:
/// - under 5 s → `"just now"` (avoids flicker on the 200 ms poll tick)
/// - 5 s – 60 s → `"Ns ago"`
/// - 1 m – 60 m → `"Nm ago"`
/// - 60 m and up → `"Nh ago"`
///
/// Coarse by design — the point is to tell "freshly re-announced" from
/// "cached and possibly dead", not to replace an NTP timestamp.
fn format_age(elapsed: Duration) -> String {
    const FRESH_THRESHOLD: Duration = Duration::from_secs(5);
    let secs = elapsed.as_secs();
    if elapsed < FRESH_THRESHOLD {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Subtitle text shown on AGC-mutexed rows in the grayed-out
/// state so the reason for the lock is inline — without it, an
/// insensitive row is easy to mistake for a bug rather than
/// intentional behavior.
const AGC_MUTEX_SUBTITLE: &str = "Disabled while AGC is on";

/// Enforce the tuner AGC ↔ manual gain mutual exclusion on the UI
/// side: when AGC is on, the gain spin row becomes insensitive
/// (grayed out, non-interactive). When AGC is off, the row is
/// fully editable.
///
/// The mutex exists because librtlsdr's `rtlsdr_set_tuner_gain`
/// silently no-ops when AGC mode is active on most RTL variants,
/// and on some oscillates between the manual target and the AGC
/// target in a loop that produces audible artifacts. Preventing
/// the user from editing the control while it would silently fail
/// is the discoverable fix (see #332). Bookmarks restore the full
/// tuning profile with AGC-first-then-gain ordering already, so
/// the restore path still updates `gain_row.set_value` cleanly
/// even when the row is insensitive — the value displays but the
/// user can't edit it until AGC is turned off.
fn apply_agc_gain_mutex(gain_row: &adw::SpinRow, agc_active: bool) {
    gain_row.set_sensitive(!agc_active);
    gain_row.set_subtitle(if agc_active { AGC_MUTEX_SUBTITLE } else { "" });
}

/// Enforce the tuner AGC ↔ squelch mutual exclusion on the UI
/// side: when AGC is on, the squelch controls (manual enable,
/// manual level, auto-squelch enable) become insensitive.
///
/// The mutex exists because RTL-SDR's hardware tuner AGC auto-
/// normalizes the IF signal amplitude — the tuner's internal
/// VGA pushes toward a target level regardless of actual RF
/// input. `PowerSquelch` reads mean IF amplitude and gates
/// against a threshold, so with AGC on every signal (including
/// noise on an empty channel) looks like "above threshold" and
/// the gate stays open. Users see this as "all static all the
/// time" the moment they enable AGC while squelch is on.
///
/// Same UX pattern as `apply_agc_gain_mutex`: gray the rows,
/// set a subtitle on the first row explaining why, restore
/// sensitivity when AGC turns off. Both mutexes share the
/// `AGC_MUTEX_SUBTITLE` string so the explanation reads
/// identically across the panel.
fn apply_agc_squelch_mutex(
    squelch_enabled_row: &adw::SwitchRow,
    squelch_level_row: &adw::SpinRow,
    auto_squelch_row: &adw::SwitchRow,
    agc_active: bool,
) {
    squelch_enabled_row.set_sensitive(!agc_active);
    squelch_level_row.set_sensitive(!agc_active);
    auto_squelch_row.set_sensitive(!agc_active);
    // Only one subtitle — the squelch-enabled row is the
    // "header" of this group in the Radio panel, so that's
    // where the explanation lands. The other two rows stay
    // grayed without extra text to avoid repeating the
    // message three times in a row.
    squelch_enabled_row.set_subtitle(if agc_active { AGC_MUTEX_SUBTITLE } else { "" });
}

/// Interval for refreshing the source combo's RTL-SDR slot label
/// against the live USB bus. Low-frequency enough to be
/// negligible CPU-wise; fast enough that a user plugging in their
/// dongle after app launch sees the slot update to the real
/// device name within a few seconds without having to restart.
///
/// Shares cadence with `SERVER_PANEL_HOTPLUG_POLL_INTERVAL` as a
/// deliberate choice — both pollers watch the same libusb bus for
/// the same vendor/product-filtered device set, so users see
/// both the source combo and the server panel update on the same
/// tick. Kept as a separate constant so each poller's sizing can
/// evolve independently.
const SOURCE_RTLSDR_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Install a hotplug poller on the source panel that keeps the
/// RTL-SDR slot label (`device_row` entry 0) in sync with the
/// live USB bus. Seeded once at build-time (inside
/// `build_source_panel`); this helper adds the ongoing refresh.
///
/// Compared against a cached last-seen label so the `splice` fires
/// only on real edges — plugging in, unplugging, or USB string
/// changing. Without the edge gate we'd churn the combo's model
/// every 3 s and risk transient selection flicker (though GTK's
/// `ComboRow` is robust to same-value splices, the no-op is
/// cheaper to skip than to perform).
///
/// Weak ref on the source panel's `widget` so the poller tears
/// down cleanly on window close — upgrade returns `None` and the
/// `ControlFlow::Break` arm fires.
fn connect_source_rtlsdr_probe(panels: &SidebarPanels) {
    let widget_weak = panels.source.widget.downgrade();
    let model_weak = panels.source.device_model.downgrade();
    // Cached label from the last tick so we only rewrite on a
    // real edge. Seed from the model's current `DEVICE_RTLSDR`
    // entry — NOT from a fresh probe — so we're comparing
    // subsequent probes against what the UI is actually showing.
    //
    // A second probe here would race the USB state: if the user
    // unplugs their dongle between `build_source_panel` (which
    // ran the initial probe + seed) and this wiring point, a
    // second probe would read the new bus state, cache it as
    // `last_label`, and then every subsequent tick's probe would
    // match the cache — the combo would stay on the stale plugged-
    // in name forever (or until the NEXT plug / unplug edge
    // briefly desynced them again). Reading the model directly
    // guarantees first-tick reconciliation.
    let seed_label = panels
        .source
        .device_model
        .string(DEVICE_RTLSDR)
        .map_or_else(String::new, |s| s.to_string());
    let last_label: Rc<RefCell<String>> = Rc::new(RefCell::new(seed_label));
    let _ = glib::timeout_add_local(SOURCE_RTLSDR_PROBE_INTERVAL, move || {
        if widget_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        let Some(model) = model_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let current = sidebar::source_panel::probe_rtlsdr_device_label();
        let mut last = last_label.borrow_mut();
        if *last != current {
            tracing::debug!(
                previous = %*last,
                current = %current,
                "source panel: RTL-SDR slot label updated",
            );
            // Replace the RTL-SDR slot in the StringList.
            // `splice(pos, n, additions)` removes `n` items at
            // `pos` and inserts `additions` — so `(DEVICE_RTLSDR,
            // 1, &[&current])` is a single-entry in-place swap.
            // Using the shared `DEVICE_RTLSDR` constant instead
            // of a literal `0` keeps the probe aligned with the
            // rest of the source-row selection logic; all four
            // `DEVICE_*` indices are the one source of truth for
            // slot positions. Leaves Network / File / RTL-TCP
            // entries untouched.
            model.splice(DEVICE_RTLSDR, 1, &[&current]);
            *last = current;
        }
        glib::ControlFlow::Continue
    });
}

#[allow(
    clippy::too_many_lines,
    reason = "GTK signal-wiring panel; splitting would fragment the control mapping"
)]
fn connect_source_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    toast_overlay: &adw::ToastOverlay,
    server_running: Rc<std::cell::Cell<bool>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
) {
    // Sample rate selector + bandwidth advisory re-render.
    // The advisory visibility depends on BOTH the sample-rate
    // selection AND the device-type selection (only network paths
    // care about wire bandwidth). We clone the helper closure into
    // both notify handlers so either trigger re-evaluates.
    // All three widgets the advisory closure touches are weak-
    // ref'd. The closure is attached to both `sample_rate_row` and
    // `device_row`'s `connect_selected_notify` — strong captures
    // here would create the same self-cycle pattern flagged in
    // `connect_share_switch` / `connect_server_status_polling`:
    // `row → closure → row.clone()` keeps the widget alive forever.
    let advisory_row_weak = panels.source.bandwidth_advisory_row.downgrade();
    let device_row_weak = panels.source.device_row.downgrade();
    let sample_rate_row_weak = panels.source.sample_rate_row.downgrade();
    let apply_source_bandwidth_advisory = {
        let advisory_row_weak = advisory_row_weak.clone();
        let device_row_weak = device_row_weak.clone();
        let sample_rate_row_weak = sample_rate_row_weak.clone();
        move || {
            // Any missing widget means the window has been torn
            // down; skip the render — subsequent notify events
            // won't fire against dead widgets.
            let (Some(advisory), Some(device_row), Some(sample_rate_row)) = (
                advisory_row_weak.upgrade(),
                device_row_weak.upgrade(),
                sample_rate_row_weak.upgrade(),
            ) else {
                return;
            };
            let is_network_path = device_row.selected() == DEVICE_RTLTCP;
            // Bounds-check the sample-rate index: transient
            // out-of-range values from widget-model churn would
            // otherwise satisfy the `>= threshold` compare and
            // flash the advisory visible with no legal selection.
            // Same safety pattern as the server-panel advisory
            // above.
            let selected = sample_rate_row.selected();
            let is_high_rate = (selected as usize) < SAMPLE_RATES.len()
                && selected >= crate::sidebar::source_panel::HIGH_BANDWIDTH_SAMPLE_RATE_IDX;
            advisory.set_visible(is_network_path && is_high_rate);
        }
    };
    // Seed the advisory visibility once at wire-up. Without this,
    // the caption stays hidden until the user nudges one of the
    // two rows — which hides it even when the restored config
    // already has RTL-TCP + a high sample rate selected.
    apply_source_bandwidth_advisory();

    let state_sr = Rc::clone(state);
    let apply_on_sr = apply_source_bandwidth_advisory.clone();
    panels
        .source
        .sample_rate_row
        .connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            if let Some(&rate) = SAMPLE_RATES.get(idx) {
                state_sr.send_dsp(UiToDsp::SetSampleRate(rate));
            }
            apply_on_sr();
        });
    let apply_on_device = apply_source_bandwidth_advisory;
    panels
        .source
        .device_row
        .connect_selected_notify(move |_| apply_on_device());

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

    // Gain control. Sensitivity is gated by AGC — see the `AGC
    // toggle` handler below and `apply_agc_gain_mutex` for the
    // reasoning (librtlsdr silently ignores gain writes when
    // tuner AGC is on; some variants also oscillate between
    // manual and AGC targets on mixed writes).
    //
    // The notify handler checks the AGC state and skips the
    // DSP dispatch when AGC is not Off. `set_sensitive(false)`
    // blocks user interaction but does NOT suppress the notify
    // signal on programmatic `set_value` calls (bookmark
    // restore, future preset-apply paths, etc.), so a pure-
    // sensitivity gate would still let a stream of no-op
    // `SetGain` commands hit the DSP every time a non-Off-AGC
    // bookmark loads. The AGC-state check short-circuits those
    // at the source — both hardware and software AGC
    // renormalize the signal, so any gain write during those
    // modes is discarded downstream anyway.
    let state_gain = Rc::clone(state);
    let agc_row_for_gain = panels.source.agc_row.downgrade();
    panels.source.gain_row.connect_value_notify(move |row| {
        if let Some(agc_row) = agc_row_for_gain.upgrade() {
            let agc_type = sidebar::source_panel::agc_type_from_selected(agc_row.selected());
            if !matches!(agc_type, Some(sidebar::source_panel::AgcType::Off)) {
                return;
            }
        }
        state_gain.send_dsp(UiToDsp::SetGain(row.value()));
    });

    // AGC type selector (Off / Hardware / Software). Dispatches
    // the right `UiToDsp::SetAgc` / `UiToDsp::SetSoftwareAgc`
    // pair on every selection and also fires two mutexes so
    // the UI doesn't lie about controls that EITHER AGC type
    // disables:
    //
    // 1. Gain row — `rtlsdr_set_tuner_gain` silently no-ops on
    //    most RTL variants when hardware AGC is on; software
    //    AGC makes manual gain pointless because the DSP stage
    //    would renormalize it immediately.
    // 2. Squelch rows — both AGC types auto-normalize IF
    //    amplitude, so amplitude-based squelch can't distinguish
    //    signal from noise and the gate just stays open. Without
    //    this mutex users see "all static all the time" the
    //    moment they enable AGC with squelch on.
    //
    // Register the AGC notify handler BEFORE restoring the
    // persisted selection. `set_selected` only fires
    // `selected-notify` when the new index differs from the
    // current one, so the startup-restore path relies on the
    // handler being registered first to dispatch the persisted
    // mode. Without this ordering, fresh installs (persisted
    // matches build-time default) or config match would leave
    // DSP stuck in its all-off default state until the user
    // touched the selector.
    //
    // Handler drops transient out-of-range indices —
    // `agc_type_from_selected` now returns `Option<AgcType>`
    // and we early-return on `None` rather than coercing them
    // to a fallback and persisting a bogus config write during
    // widget-teardown churn.
    let state_agc = Rc::clone(state);
    let config_for_agc = std::sync::Arc::clone(config);
    let gain_row_for_agc = panels.source.gain_row.clone();
    let squelch_enabled_for_agc = panels.radio.squelch_enabled_row.clone();
    let squelch_level_for_agc = panels.radio.squelch_level_row.clone();
    let auto_squelch_for_agc = panels.radio.auto_squelch_row.clone();
    panels.source.agc_row.connect_selected_notify(move |row| {
        let Some(agc_type) = sidebar::source_panel::agc_type_from_selected(row.selected()) else {
            // Transient GTK value (e.g., `INVALID_LIST_POSITION`
            // during model swap). Skip dispatch AND persistence
            // — we'll pick up the next real selection from the
            // follow-up notify event.
            tracing::trace!(
                selected = row.selected(),
                "AGC combo notify with out-of-range index, ignoring"
            );
            return;
        };

        // Dispatch both messages every time so exactly one
        // enable path is active and the other is cleanly off.
        // The engine treats hardware and software AGC as
        // independent flags; the UI is the policy layer that
        // mutually excludes them.
        let (hw, sw) = match agc_type {
            sidebar::source_panel::AgcType::Off => (false, false),
            sidebar::source_panel::AgcType::Hardware => (true, false),
            sidebar::source_panel::AgcType::Software => (false, true),
        };
        state_agc.send_dsp(UiToDsp::SetAgc(hw));
        state_agc.send_dsp(UiToDsp::SetSoftwareAgc(sw));

        // Persist the new selection so the choice sticks
        // across restarts. Cheap — `ConfigManager::write` is an
        // in-memory update with a debounced flush to disk.
        sidebar::source_panel::save_agc_type(&config_for_agc, agc_type);

        let agc_active = !matches!(agc_type, sidebar::source_panel::AgcType::Off);
        apply_agc_gain_mutex(&gain_row_for_agc, agc_active);
        apply_agc_squelch_mutex(
            &squelch_enabled_for_agc,
            &squelch_level_for_agc,
            &auto_squelch_for_agc,
            agc_active,
        );
    });

    // Restore persisted AGC type from config now that the
    // notify handler is wired up. Two scenarios:
    //
    // 1. Persisted index differs from the combo's build-time
    //    default (Software) — `set_selected` fires
    //    `selected-notify`, the handler runs, DSP is
    //    dispatched, mutexes applied.
    // 2. Persisted index matches the default (fresh install
    //    or user previously selected Software) —
    //    `set_selected` is a no-op and `selected-notify`
    //    does NOT fire. We explicitly dispatch so DSP still
    //    gets the initial-state sync and mutexes are applied
    //    against the seeded selection.
    //
    // Both paths run the same dispatch logic; the explicit
    // post-`set_selected` call is idempotent with the notify
    // handler (both `SetAgc` and `SetSoftwareAgc` are
    // idempotent at the controller), so the double-dispatch
    // in scenario 1 is cheap and correct.
    {
        let persisted = sidebar::source_panel::load_agc_type(config);
        panels
            .source
            .agc_row
            .set_selected(sidebar::source_panel::selected_from_agc_type(persisted));

        let (hw, sw) = match persisted {
            sidebar::source_panel::AgcType::Off => (false, false),
            sidebar::source_panel::AgcType::Hardware => (true, false),
            sidebar::source_panel::AgcType::Software => (false, true),
        };
        state.send_dsp(UiToDsp::SetAgc(hw));
        state.send_dsp(UiToDsp::SetSoftwareAgc(sw));
        let agc_active = !matches!(persisted, sidebar::source_panel::AgcType::Off);
        apply_agc_gain_mutex(&panels.source.gain_row, agc_active);
        apply_agc_squelch_mutex(
            &panels.radio.squelch_enabled_row,
            &panels.radio.squelch_level_row,
            &panels.radio.auto_squelch_row,
            agc_active,
        );
    }

    // Shared "last-good auth bytes" cache between the auth-key
    // handler (primary writer) and the role-picker handler
    // (reader). Populated whenever the auth row parses as empty
    // (`None`, intentional clear) or valid hex (`Some(bytes)`);
    // NOT updated on malformed hex. The role handler uses this
    // snapshot when the live auth text is unparseable so it can
    // still propagate the new role to DSP with a coherent
    // auth_key value — without this, flipping role while the
    // key field held a bad paste would skip the whole
    // `SetRtlTcpClientConfig` dispatch and leave DSP on the
    // previous role. Per `CodeRabbit` round 9 on PR #408.
    //
    // `Rc<RefCell<Option<Vec<u8>>>>` on GTK's single-threaded
    // main loop — no lock contention. Declared BEFORE the
    // startup last-connected restore below so that block can
    // seed the cache with the keyring-loaded bytes — per
    // `CodeRabbit` round 10 on PR #408, leaving the cache
    // empty after startup would let a subsequent malformed-hex
    // role flip clear DSP's working auth instead of preserving
    // the startup-restored bytes.
    let last_good_auth_key: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));

    // Restore the rtl_tcp client's last-used role + auth key
    // (#396). Role resolution uses the standard two-tier
    // lookup: per-favorite `requested_role` first (if the
    // LastConnectedServer matches a favorite entry), falling
    // back to the global `KEY_RTL_TCP_CLIENT_LAST_ROLE` default,
    // and finally to `Control` (legacy-safe). The auth key is
    // loaded directly from the per-server keyring using the
    // LastConnectedServer's `host:port`. Pre-CodeRabbit round 2
    // on PR #408 this path hard-set `auth_key: None` and
    // ignored per-favorite role, so pressing Play right after
    // launch against a previously-auth-configured server would
    // drop the saved key and force a redundant `AuthRequired`
    // bounce before reconnecting. With the keyring preload the
    // DSP carries the right bytes from the first Play.
    {
        use crate::sidebar::source_panel::{
            FavoriteRole, KEY_RTL_TCP_CLIENT_LAST_ROLE, RTL_TCP_ROLE_CONTROL_IDX,
            RTL_TCP_ROLE_LISTEN_IDX, load_favorites, load_last_connected,
        };
        let last_connected = load_last_connected(config);
        let favorite_entry = last_connected.as_ref().and_then(|srv| {
            let key = format!("{}:{}", srv.host, srv.port);
            load_favorites(config).into_iter().find(|f| f.key == key)
        });
        let persisted_role: FavoriteRole = favorite_entry
            .as_ref()
            .and_then(|f| f.requested_role)
            .or_else(|| {
                config.read(|v| {
                    v.get(KEY_RTL_TCP_CLIENT_LAST_ROLE)
                        .and_then(|val| serde_json::from_value(val.clone()).ok())
                })
            })
            .unwrap_or(FavoriteRole::Control);
        let idx = match persisted_role {
            FavoriteRole::Control => RTL_TCP_ROLE_CONTROL_IDX,
            FavoriteRole::Listen => RTL_TCP_ROLE_LISTEN_IDX,
        };
        panels.source.rtl_tcp_role_row.set_selected(idx);
        // Load the saved per-server auth key for the last-
        // connected endpoint, if any. Also cache that server's
        // stable id on `AppState` so the first post-Play
        // `AuthRequired` / `AuthFailed` / `Connected` arm
        // already has it and the keyring save / clear paths
        // target the right entry without waiting on the first
        // `apply_rtl_tcp_connect` call.
        //
        // Auth-row visibility + text is resolved deterministically
        // using the same two-input rule as `apply_rtl_tcp_connect`
        // (per `CodeRabbit` round 5 on PR #408): reveal the row
        // when EITHER the favorite advertises `auth_required ==
        // Some(true)` (server requires a key; user should see the
        // field up-front even on a fresh session with no saved
        // key) OR a saved key exists in the keyring (we want to
        // show the pre-loaded value so the user knows the
        // session will auto-auth). Set text from the saved key,
        // or clear when none — so a prior-session auth-required
        // server whose key the user later cleared doesn't leak
        // stale text into the field on the next launch.
        let mut auth_key: Option<Vec<u8>> = None;
        if let Some(srv) = last_connected.as_ref() {
            *state.rtl_tcp_active_server.borrow_mut() = format!("{}:{}", srv.host, srv.port);
            auth_key = load_client_auth_key_from_keyring(&srv.host, srv.port);
            // Seed the round-9 last-good cache with the
            // startup-restored bytes so a subsequent malformed-
            // hex role flip (round 9's fallback path) preserves
            // the auth DSP just received. Without this the
            // cache would stay `None` until the user first
            // edited the auth field, opening a window where a
            // role flip with malformed text in the row silently
            // clears DSP auth. Per `CodeRabbit` round 10 on
            // PR #408.
            last_good_auth_key.borrow_mut().clone_from(&auth_key);
            let has_auth_required = matches!(
                favorite_entry.as_ref().and_then(|f| f.auth_required),
                Some(true)
            );
            let should_reveal = has_auth_required || auth_key.is_some();
            panels
                .source
                .rtl_tcp_auth_key_row
                .set_visible(should_reveal);
            if let Some(bytes) = auth_key.as_ref() {
                panels
                    .source
                    .rtl_tcp_auth_key_row
                    .set_text(&crate::sidebar::server_panel::auth_key_to_hex(bytes));
            } else {
                panels.source.rtl_tcp_auth_key_row.set_text("");
            }
        }
        state.send_dsp(UiToDsp::SetRtlTcpClientConfig {
            requested_role: persisted_role.as_wire_role(),
            auth_key,
        });
    }

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

    // rtl_tcp connection controls — Disconnect + Retry now.
    // Both route to the DSP controller which owns the active
    // Source and performs the stop/start teardown. Buttons are
    // sensitive-gated by the state-change handler in
    // `handle_dsp_message`, so clicks should only ever reach here
    // on legal transitions.
    let state_disconnect = Rc::clone(state);
    panels
        .source
        .rtl_tcp_disconnect_button
        .connect_clicked(move |_| {
            state_disconnect.send_dsp(UiToDsp::DisconnectRtlTcp);
        });
    let state_retry = Rc::clone(state);
    panels
        .source
        .rtl_tcp_retry_button
        .connect_clicked(move |_| {
            state_retry.send_dsp(UiToDsp::RetryRtlTcpNow);
        });

    // Source type selector — guard against transient out-of-range
    // indices AND enforce mutual exclusivity with the rtl_tcp server
    // (the dongle can only serve one master; re-selecting RTL-SDR
    // while the server's accept thread has the USB device would
    // trigger a double-open at the next Play).
    let state_source = Rc::clone(state);
    let toast_overlay_weak = toast_overlay.downgrade();
    // Last-known legal selection. Seeded from the current row state
    // so the revert path on first illegal transition lands on the
    // value the UI already shows. Updated every time the guard
    // accepts a new selection.
    let last_legal_selection: Rc<std::cell::Cell<u32>> =
        Rc::new(std::cell::Cell::new(panels.source.device_row.selected()));
    // Re-entry guard against our own `set_selected` (the revert).
    // Without it the revert would re-enter this handler, see the
    // previous illegal value as "new", and endlessly toggle.
    let reverting: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
    panels
        .source
        .device_row
        .connect_selected_notify(move |row| {
            if reverting.get() {
                // Our own revert fired this notify — drop it.
                return;
            }
            let selected = row.selected();
            // Exclusivity guard: can't re-enter the local-source
            // world while the rtl_tcp server has the dongle claimed.
            if selected == DEVICE_RTLSDR && server_running.get() {
                if let Some(overlay) = toast_overlay_weak.upgrade() {
                    overlay.add_toast(adw::Toast::new(
                        "Stop the network server first before switching to local RTL-SDR.",
                    ));
                }
                reverting.set(true);
                row.set_selected(last_legal_selection.get());
                reverting.set(false);
                return;
            }
            let source_type = match selected {
                DEVICE_RTLSDR => SourceType::RtlSdr,
                DEVICE_NETWORK => SourceType::Network,
                DEVICE_FILE => SourceType::File,
                DEVICE_RTLTCP => SourceType::RtlTcp,
                _ => return, // ignore transient indices
            };
            last_legal_selection.set(selected);
            state_source.send_dsp(UiToDsp::SetSourceType(source_type));
        });

    // Network hostname — send on every edit so Play always has current value
    let state_host = Rc::clone(state);
    let port_for_host = panels.source.port_row.clone();
    let proto_for_host = panels.source.protocol_row.clone();
    let hostname_for_host = panels.source.hostname_row.clone();
    let auth_key_for_host = panels.source.rtl_tcp_auth_key_row.clone();
    panels.source.hostname_row.connect_changed(move |row| {
        // Invalidate the cached `rtl_tcp_active_server` when
        // the widget no longer matches the cached stable id
        // (typically a manual edit; harmless no-op for
        // `apply_rtl_tcp_connect`'s programmatic writes when
        // those match the cache). Per CodeRabbit round 4 on
        // PR #408.
        invalidate_rtl_tcp_active_server_on_edit(
            &state_host,
            &hostname_for_host,
            &port_for_host,
            &auth_key_for_host,
        );
        let hostname = row.text().to_string();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = port_for_host.value() as u16;
        let protocol = if proto_for_host.selected() == NETWORK_PROTOCOL_UDP_IDX {
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
    let port_row_for_port = panels.source.port_row.clone();
    let auth_key_for_port = panels.source.rtl_tcp_auth_key_row.clone();
    panels.source.port_row.connect_value_notify(move |row| {
        invalidate_rtl_tcp_active_server_on_edit(
            &state_port,
            &host_for_port,
            &port_row_for_port,
            &auth_key_for_port,
        );
        let hostname = host_for_port.text().to_string();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = row.value() as u16;
        let protocol = if proto_for_port.selected() == NETWORK_PROTOCOL_UDP_IDX {
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
                NETWORK_PROTOCOL_TCPCLIENT_IDX => sdr_types::Protocol::TcpClient,
                NETWORK_PROTOCOL_UDP_IDX => sdr_types::Protocol::Udp,
                _ => return, // ignore transient indices
            };
            state_proto.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        });

    // Connection-role picker (#396). The selector flips between
    // `Role::Control` (index 0) and `Role::Listen` (index 1); we
    // dispatch a fresh `SetRtlTcpClientConfig` with the new role
    // plus the current auth key (unchanged by a role flip). The
    // role takes effect on the NEXT connect — already-running
    // sessions keep their admitted role because the wire
    // protocol ties role to the hello and doesn't support
    // mid-stream role changes. Persistence has two tiers:
    //
    // - Global `KEY_RTL_TCP_CLIENT_LAST_ROLE` — fallback default
    //   for NEW servers that haven't been favorited yet. The
    //   Connect-from-discovery path reads this to seed the
    //   picker before the user has expressed a per-server
    //   preference. Pre-CodeRabbit round 1 on PR #408 this was
    //   the ONLY persistence tier, which meant changing
    //   Server B's role clobbered Server A's preference.
    // - Per-favorite `FavoriteEntry.requested_role` — wins for
    //   favorited servers. When the current server identity
    //   matches a favorite key, update that entry's role and
    //   save_favorites so the next connect from this favorite
    //   restores the right picker state without touching other
    //   servers.
    let state_role = Rc::clone(state);
    let auth_key_for_role = panels.source.rtl_tcp_auth_key_row.clone();
    let config_for_role = std::sync::Arc::clone(config);
    let hostname_for_role = panels.source.hostname_row.clone();
    let port_for_role = panels.source.port_row.clone();
    let favorites_for_role = Rc::clone(favorites);
    let last_good_for_role = Rc::clone(&last_good_auth_key);
    panels
        .source
        .rtl_tcp_role_row
        .connect_selected_notify(move |row| {
            use crate::sidebar::source_panel::{
                FavoriteRole, KEY_RTL_TCP_CLIENT_LAST_ROLE, RTL_TCP_ROLE_CONTROL_IDX,
                RTL_TCP_ROLE_LISTEN_IDX, save_favorites,
            };
            let fav_role = match row.selected() {
                RTL_TCP_ROLE_CONTROL_IDX => FavoriteRole::Control,
                RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
                _ => return, // transient out-of-range indices
            };
            let requested_role = fav_role.as_wire_role();
            // Resolve the auth_key for this dispatch:
            // - Empty text → `None` (intentional clear).
            // - Valid hex → `Some(bytes)`.
            // - Malformed non-empty text → the cached last-good
            //   bytes (which the auth handler maintains). This
            //   means a role flip with bad hex in the auth field
            //   still pushes the new role to DSP — pre-
            //   `CodeRabbit` round 9 on PR #408 we'd skip the
            //   dispatch entirely, so a user could switch to
            //   Listener, hit Retry / ControllerBusy-toast-
            //   Takeover, and still end up as Controller because
            //   DSP never saw the new role. The auth_key-row
            //   handler still drives the `error` CSS class on
            //   the row so the user sees the malformed input.
            let key_text = auth_key_for_role.text().to_string();
            let auth_key: Option<Vec<u8>> = if key_text.is_empty() {
                None
            } else if let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&key_text) {
                Some(bytes)
            } else {
                last_good_for_role.borrow().clone()
            };
            state_role.send_dsp(UiToDsp::SetRtlTcpClientConfig {
                requested_role,
                auth_key,
            });
            // Tier 1: global default — always written so a fresh
            // server ("never favorited, never configured") picks
            // this up as the picker seed.
            config_for_role.write(|v| {
                v[KEY_RTL_TCP_CLIENT_LAST_ROLE] =
                    serde_json::to_value(fav_role).unwrap_or(serde_json::Value::Null);
            });
            // Tier 2: per-favorite override. Resolve the
            // server key from the cached stable identity first
            // (`state.rtl_tcp_active_server`, written by
            // `apply_rtl_tcp_connect` / the startup restore at
            // connect-setup time) and only fall back to reading
            // the `hostname_row` / `port_row` widgets when the
            // cache is empty (manually-typed Play path, no
            // apply_rtl_tcp_connect). Pre-`CodeRabbit` round 10
            // on PR #408 this handler always rebuilt the key
            // from the widgets, so a discovery connect that
            // persisted `shack-pi.local.:1234` as the favorite
            // identity could silently diverge from whatever
            // resolved-IP value the dial path had pushed into
            // `hostname_row` — the lookup below would miss the
            // favorite, and `requested_role` wouldn't round-
            // trip between discovery, favorites, and reconnects.
            //
            // Then update the matching entry's `requested_role`
            // in the SHARED in-memory map
            // (`connect_rtl_tcp_discovery`'s re-announce path
            // also reads + mutates this map), and persist the
            // full snapshot. Pre-round-8 this handler called
            // `load_favorites` on every fire and saved a fresh
            // `Vec`, diverging from the discovery path's in-
            // memory map — a subsequent `ServerAnnounced` would
            // preserve the stale in-memory role and clobber the
            // just-saved selection. Mutating the shared map
            // keeps both paths honest.
            let server_key = {
                let cached = state_role.rtl_tcp_active_server.borrow().clone();
                if cached.is_empty() {
                    let host = hostname_for_role.text().to_string();
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let port = port_for_role.value() as u16;
                    if host.is_empty() || port == 0 {
                        return;
                    }
                    format!("{host}:{port}")
                } else {
                    cached
                }
            };
            let dirty = {
                let mut favorites = favorites_for_role.borrow_mut();
                if let Some(fav) = favorites.get_mut(&server_key)
                    && fav.requested_role != Some(fav_role)
                {
                    fav.requested_role = Some(fav_role);
                    true
                } else {
                    false
                }
            };
            if dirty {
                let snapshot: Vec<sidebar::source_panel::FavoriteEntry> =
                    favorites_for_role.borrow().values().cloned().collect();
                save_favorites(&config_for_role, &snapshot);
            }
        });

    // Server key entry (#394 + #396). On every edit we rebuild
    // the `SetRtlTcpClientConfig` message with the current role
    // + the new key bytes, so the NEXT connect carries the
    // latest value. The entry accepts hex input (matching what
    // `openssl rand -hex 32` produces and what the server UI's
    // Copy button writes to the clipboard); an empty field
    // clears the key (`auth_key: None`). The key is also saved
    // to the per-server keyring on a successful auth-required
    // connect (wired in the toast-flow commit) — this handler
    // only threads the current-session value through to the
    // DSP.
    let state_auth = Rc::clone(state);
    let role_for_auth = panels.source.rtl_tcp_role_row.clone();
    let last_good_for_auth = Rc::clone(&last_good_auth_key);
    panels
        .source
        .rtl_tcp_auth_key_row
        .connect_changed(move |row| {
            use crate::sidebar::source_panel::{
                FavoriteRole, RTL_TCP_ROLE_CONTROL_IDX, RTL_TCP_ROLE_LISTEN_IDX,
            };
            // Transient out-of-range indices on `ComboRow` can
            // occur during widget teardown; fall back to the
            // legacy-safe `Control` default in that case (same
            // treatment the role_row handler gives with an
            // `early return`, but auth_key edits happen often
            // enough that swallowing one rare transient is
            // fine).
            #[allow(
                clippy::match_same_arms,
                reason = "explicit catch-all matches the Control default"
            )]
            let fav_role = match role_for_auth.selected() {
                RTL_TCP_ROLE_CONTROL_IDX => FavoriteRole::Control,
                RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
                _ => FavoriteRole::Control,
            };
            let text = row.text().to_string();
            // Malformed hex must NOT collapse to `auth_key: None`.
            // Pre-`CodeRabbit` round 7 on PR #408 a bad paste fell
            // into the `auth_key_from_hex(..) -> None` branch and
            // silently cleared DSP auth state — the next Retry /
            // Play would then dispatch an unauthenticated connect,
            // bounce through `AuthRequired`, and the user had to
            // fix the text before realizing the previous saved key
            // had been clobbered. Three cases now:
            //
            // - Empty text: intentional clear. Drop the error
            //   class, dispatch `auth_key: None`, cache `None`.
            // - Valid hex: parsed bytes. Drop the error class,
            //   dispatch `Some(bytes)`, cache `Some(bytes)`.
            // - Malformed non-empty text: add the libadwaita
            //   `error` CSS class so the row reads as invalid,
            //   and RETURN without dispatching or updating the
            //   cache — keeping DSP's last-good auth state
            //   (and the `last_good_auth_key` cache the role
            //   handler reads from) intact until the user
            //   either fixes the text or clears the field.
            //
            // `auth_key_from_hex` treats empty as `None` too, but
            // we handle the empty branch explicitly above so the
            // malformed case is cleanly separable.
            let auth_key: Option<Vec<u8>> = if text.is_empty() {
                row.remove_css_class("error");
                None
            } else if let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&text) {
                row.remove_css_class("error");
                Some(bytes)
            } else {
                row.add_css_class("error");
                return;
            };
            // Update the last-good cache alongside the dispatch
            // so the role handler's fallback path (malformed
            // hex at role-flip time) has a coherent value to
            // dispatch. See `last_good_auth_key` declaration
            // above. Per `CodeRabbit` round 9 on PR #408.
            last_good_for_auth.borrow_mut().clone_from(&auth_key);
            state_auth.send_dsp(UiToDsp::SetRtlTcpClientConfig {
                requested_role: fav_role.as_wire_role(),
                auth_key,
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

/// Tolerance (Hz) for the "bandwidth is at its mode default"
/// comparison. The bandwidth `SpinRow` uses `digits(0)` so values
/// are already integer-aligned; this tolerance is just a
/// float-comparison guard, not a user-visible fuzziness.
const BANDWIDTH_RESET_TOLERANCE_HZ: f64 = 0.5;

/// Update the bandwidth reset button's sensitivity: active only
/// when the spin row's current value differs from the current
/// demod mode's default bandwidth. Called from anywhere either
/// input (current bandwidth OR demod mode) can change. Per
/// issue #341.
fn update_bandwidth_reset_sensitivity(radio: &sidebar::radio_panel::RadioPanel, state: &AppState) {
    let mode = state.demod_mode.get();
    // Conservative fallback: if we can't resolve the mode's
    // default (unreachable today — every DemodMode has a valid
    // ctor), keep the reset button inactive rather than claim
    // a comparison we can't actually compute.
    let Ok(default) = sdr_radio::demod::default_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "default_bandwidth_for_mode failed — disabling bandwidth reset button"
        );
        radio.bandwidth_reset_button.set_sensitive(false);
        return;
    };
    let current = radio.bandwidth_row.value();
    let at_default = (current - default).abs() < BANDWIDTH_RESET_TOLERANCE_HZ;
    radio.bandwidth_reset_button.set_sensitive(!at_default);
}

/// Tolerance (Hz) for the "VFO offset is at 0" comparison in
/// the floating reset button's visibility logic.
const VFO_OFFSET_RESET_TOLERANCE_HZ: f64 = 0.5;

/// Update the floating "Reset VFO" button's visibility — shown
/// only when the VFO is in a non-default state, i.e. bandwidth
/// differs from the mode default OR offset is nonzero. Per
/// issue #341.
fn update_vfo_reset_button_visibility(
    radio: &sidebar::radio_panel::RadioPanel,
    spectrum: &spectrum::SpectrumHandle,
    state: &AppState,
) {
    let mode = state.demod_mode.get();
    // Offset-at-zero is resolvable without the demod lookup, so
    // compute it first. If the bandwidth lookup below fails, we
    // can still decide visibility based on offset alone — the
    // click handler's `SetVfoOffset(0.0)` dispatch remains
    // useful even when the bandwidth reset path is broken.
    let offset_at_zero = spectrum.vfo_offset_hz().abs() < VFO_OFFSET_RESET_TOLERANCE_HZ;
    let Ok(default_bw) = sdr_radio::demod::default_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "default_bandwidth_for_mode failed — floating reset button \
             falls back to offset-only visibility"
        );
        // Button stays available when the user has a nonzero
        // offset to clear; hides when both paths would no-op.
        spectrum.vfo_reset_button.set_visible(!offset_at_zero);
        return;
    };
    let current_bw = radio.bandwidth_row.value();
    let bandwidth_at_default = (current_bw - default_bw).abs() < BANDWIDTH_RESET_TOLERANCE_HZ;
    spectrum
        .vfo_reset_button
        .set_visible(!(bandwidth_at_default && offset_at_zero));
}

/// Connect radio panel controls to DSP commands.
#[allow(clippy::too_many_lines)]
fn connect_radio_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    scanner_force_disable: &Rc<ScannerForceDisable>,
) {
    // Bandwidth. The DSP can originate a change too (VFO drag on
    // the spectrum dispatches `UiToDsp::SetBandwidth` directly,
    // and the controller echoes `DspToUi::BandwidthChanged` so the
    // spin row reflects the drag). The echo path updates this row
    // via `set_value` which re-fires `connect_value_notify` —
    // `suppress_bandwidth_notify` breaks the cycle by telling this
    // handler to skip the DSP dispatch when the change originated
    // on the DSP side.
    let state_bw = Rc::clone(state);
    let force_disable_bw = Rc::clone(scanner_force_disable);
    panels.radio.bandwidth_row.connect_value_notify(move |row| {
        if state_bw.suppress_bandwidth_notify.get() {
            return;
        }
        // Not a DSP echo → this is the user turning the spin row.
        // Force-disable scanner so the new bandwidth applies to
        // the user's chosen channel instead of the scanner's next
        // hop.
        force_disable_bw.trigger("manual bandwidth change");
        state_bw.send_dsp(UiToDsp::SetBandwidth(row.value()));
    });

    // Bandwidth reset button → `SetBandwidth(mode_default)`. Per
    // #341. Routes through DSP so the echo updates the spin row
    // — no direct `set_value` manipulation that would skip the
    // DSP / scanner-mutex / force-disable machinery.
    let state_bw_reset = Rc::clone(state);
    let force_disable_bw_reset = Rc::clone(scanner_force_disable);
    panels
        .radio
        .bandwidth_reset_button
        .connect_clicked(move |_| {
            // Reset is a manual change — stop the scanner first
            // so the cleaned-up bandwidth doesn't race the next
            // scanner retune. Same contract as the manual
            // bandwidth-row edit above.
            force_disable_bw_reset.trigger("manual bandwidth reset");
            let mode = state_bw_reset.demod_mode.get();
            match sdr_radio::demod::default_bandwidth_for_mode(mode) {
                Ok(default) => {
                    state_bw_reset.send_dsp(UiToDsp::SetBandwidth(default));
                }
                Err(e) => {
                    tracing::warn!(
                        ?mode,
                        error = %e,
                        "default_bandwidth_for_mode failed on reset click — no dispatch"
                    );
                }
            }
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

    // CTCSS tone selector
    let state_ctcss = Rc::clone(state);
    let radio_for_ctcss = panels.radio.clone();
    panels.radio.ctcss_row.connect_selected_notify(move |row| {
        let mode = sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(row.selected());
        state_ctcss.send_dsp(UiToDsp::SetCtcssMode(mode));
        // Push the status row label immediately — the detector
        // only emits `CtcssSustainedChanged` on actual gate
        // edges, so without this the label would lag behind a
        // mode change (stay on "Tone detected" after flipping to
        // Off, or stay on "Off" after picking a tone until the
        // first detector window confirms).
        radio_for_ctcss.set_ctcss_sustained(false);
    });

    // CTCSS detection threshold
    let state_ctcss_thresh = Rc::clone(state);
    panels
        .radio
        .ctcss_threshold_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_ctcss_thresh.send_dsp(UiToDsp::SetCtcssThreshold(row.value() as f32));
        });

    // Voice squelch mode
    //
    // On mode change: tell the AF chain to rebuild its detector,
    // reconfigure the threshold spin row (units + range + default
    // value), and push the status row label to the appropriate
    // "waiting" / "Off" text so it doesn't lag behind the first
    // real detector edge.
    //
    // The initial startup layout is Off, so nothing else needs
    // to fire — `apply_voice_squelch_mode_ui(Off)` is called
    // here too to make the starting state consistent.
    panels
        .radio
        .apply_voice_squelch_mode_ui(sdr_dsp::voice_squelch::VoiceSquelchMode::Off);
    let state_vs_mode = Rc::clone(state);
    let radio_for_vs = panels.radio.clone();
    panels
        .radio
        .voice_squelch_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Use the DEFAULT threshold for the target mode, NOT
            // the current spin-row value. The previous mode's
            // threshold is in different units (normalized ratio
            // for Syllabic, dB for Snr), so forwarding it to the
            // new variant would land far outside the new
            // detector's tuning range — e.g. Off → Snr seeding
            // 0.15 dB, or Snr → Syllabic seeding 6.0 as a
            // normalized ratio. Both fail the detector.
            //
            // `apply_voice_squelch_mode_ui` below reconfigures
            // the spin row's adjustment range AND seeds its
            // value from the mode's inline threshold, so the
            // UI and DSP end up aligned on the same default
            // value in the same units.
            let threshold =
                sidebar::radio_panel::RadioPanel::voice_squelch_default_threshold_for_index(idx);
            let mode =
                sidebar::radio_panel::RadioPanel::voice_squelch_mode_from_index(idx, threshold);
            state_vs_mode.send_dsp(UiToDsp::SetVoiceSquelchMode(mode));
            radio_for_vs.apply_voice_squelch_mode_ui(mode);
            radio_for_vs.set_voice_squelch_open(false);
        });

    // Voice squelch threshold
    let state_vs_thresh = Rc::clone(state);
    panels
        .radio
        .voice_squelch_threshold_row
        .connect_value_notify(move |row| {
            #[allow(clippy::cast_possible_truncation)]
            state_vs_thresh.send_dsp(UiToDsp::SetVoiceSquelchThreshold(row.value() as f32));
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
    agc_row: &adw::ComboRow,
) {
    if let Some(sq_en) = bookmark.squelch_enabled {
        state.send_dsp(UiToDsp::SetSquelchEnabled(sq_en));
        radio.squelch_enabled_row.set_active(sq_en);
    }
    if let Some(auto_sq) = bookmark.auto_squelch_enabled {
        state.send_dsp(UiToDsp::SetAutoSquelch(auto_sq));
        radio.auto_squelch_row.set_active(auto_sq);
    }
    if let Some(sq_lvl) = bookmark.squelch_level {
        state.send_dsp(UiToDsp::SetSquelch(sq_lvl));
        #[allow(clippy::cast_lossless)]
        radio.squelch_level_row.set_value(sq_lvl as f64);
    }
    // AGC must be set before gain — switching to manual mode first
    // ensures the saved gain value actually takes effect.
    //
    // New bookmarks carry `agc_type` directly; older ones only
    // have the legacy `agc: Option<bool>` field, which we map to
    // `Hardware` (true) or `Off` (false). The new field wins
    // when both are present. The notify handler on `agc_row`
    // dispatches the right `SetAgc` / `SetSoftwareAgc` pair and
    // applies the mutexes, so we only need to flip the combo
    // selector — no explicit dispatch here.
    let restored_agc_type: Option<sidebar::source_panel::AgcType> =
        bookmark.agc_type.or_else(|| {
            bookmark.agc.map(|on| {
                if on {
                    sidebar::source_panel::AgcType::Hardware
                } else {
                    sidebar::source_panel::AgcType::Off
                }
            })
        });
    if let Some(agc_type) = restored_agc_type {
        agc_row.set_selected(sidebar::source_panel::selected_from_agc_type(agc_type));
    }
    if let Some(gain) = bookmark.gain {
        // `set_value` fires the gain row's `connect_value_notify`
        // handler, which dispatches `SetGain` to the DSP — but
        // only when AGC is currently Off (the handler checks the
        // combo state and short-circuits otherwise). So a single
        // `set_value` call here handles both the "AGC is Off,
        // update the DSP too" path and the "AGC is active, just
        // display the bookmarked value in the locked row" path.
        // No explicit `state.send_dsp(SetGain(...))` needed — it
        // would either duplicate the handler's dispatch (AGC Off
        // case) or be a wasted write the DSP silently ignores
        // (AGC active case).
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
    // Restore CTCSS threshold BEFORE mode so the detector the
    // mode setter builds picks up the saved value instead of
    // defaulting. Mirrors the RadioModule::set_mode order.
    if let Some(threshold) = bookmark.ctcss_threshold {
        state.send_dsp(UiToDsp::SetCtcssThreshold(threshold));
        #[allow(clippy::cast_lossless)]
        radio.ctcss_threshold_row.set_value(threshold as f64);
    }
    if let Some(mode) = bookmark.ctcss_mode {
        state.send_dsp(UiToDsp::SetCtcssMode(mode));
        radio
            .ctcss_row
            .set_selected(sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(
                mode,
            ));
    }
    // Voice squelch mode — the enum carries its threshold
    // inline, so a single field captures both. Dispatch to the
    // DSP first, then update the UI combo + threshold row to
    // reflect the restored state.
    if let Some(mode) = bookmark.voice_squelch_mode {
        state.send_dsp(UiToDsp::SetVoiceSquelchMode(mode));
        let idx = sidebar::radio_panel::RadioPanel::voice_squelch_index_from_mode(mode);
        radio.voice_squelch_row.set_selected(idx);
        let threshold = sidebar::radio_panel::RadioPanel::voice_squelch_threshold_from_mode(mode);
        #[allow(clippy::cast_lossless)]
        radio
            .voice_squelch_threshold_row
            .set_value(threshold as f64);
        // Push the threshold over the wire explicitly too —
        // `SetVoiceSquelchMode` already carries it inline on an
        // active variant, but sending the dedicated threshold
        // message keeps the radio module's cached mode variant
        // in sync in case a future refactor routes the two
        // updates through different code paths.
        state.send_dsp(UiToDsp::SetVoiceSquelchThreshold(threshold));
        radio.apply_voice_squelch_mode_ui(mode);
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
    scanner_force_disable: &Rc<ScannerForceDisable>,
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
    let force_disable_nav = Rc::clone(scanner_force_disable);

    panels.bookmarks.connect_navigate(move |bookmark| {
        // Both bookmark recall AND band-preset selection come in
        // through this callback (the preset handler in
        // `connect_preset_to_bookmarks` invokes `on_navigate` with
        // a synthesized Bookmark). Keep the toast reason neutral
        // so a preset click doesn't claim "bookmark recall".
        force_disable_nav.trigger("preset/bookmark selection");

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
    let bm = &panels.bookmarks;
    let bm_for_add = Rc::clone(bm);
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
            auto_squelch_enabled: radio_bm.auto_squelch_row.is_active(),
            squelch_level: radio_bm.squelch_level_row.value() as f32,
            gain: source_gain_bm.value(),
            // Snapshot the AGC selection at save time. On a
            // transient out-of-range combo index (rare, e.g.
            // user triggering save during a model-swap animation)
            // fall back to the configured default rather than
            // refusing to save — the save is user-initiated and
            // should always produce a bookmark.
            agc_type: sidebar::source_panel::agc_type_from_selected(source_agc_bm.selected())
                .unwrap_or(sidebar::source_panel::AgcType::DEFAULT),
            volume: None, // Volume ScaleButton not in sidebar — don't persist.
            deemphasis: radio_bm.deemphasis_row.selected(),
            nb_enabled: radio_bm.noise_blanker_row.is_active(),
            nb_level: radio_bm.nb_level_row.value() as f32,
            fm_if_nr: radio_bm.fm_if_nr_row.is_active(),
            wfm_stereo: radio_bm.stereo_row.is_active(),
            high_pass: None, // No UI widget yet — don't persist.
            ctcss_mode: Some(sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(
                radio_bm.ctcss_row.selected(),
            )),
            ctcss_threshold: Some(radio_bm.ctcss_threshold_row.value() as f32),
            voice_squelch_mode: Some(
                sidebar::radio_panel::RadioPanel::voice_squelch_mode_from_index(
                    radio_bm.voice_squelch_row.selected(),
                    radio_bm.voice_squelch_threshold_row.value() as f32,
                ),
            ),
        };
        let bookmark =
            sidebar::navigation_panel::Bookmark::with_profile(&name, freq_u64, mode, bw, &profile);
        bm_for_add.bookmarks.borrow_mut().push(bookmark);
        sidebar::navigation_panel::save_bookmarks(&bm_for_add.bookmarks.borrow());
        bm_for_add.rebuild_after_mutation(&name_entry);
        name_entry.set_text("");
    });

    // Save button — update the active bookmark with current settings.
    // Capture the bookmarks panel via `Weak` so the stored closure
    // doesn't keep the panel alive: the closure lives inside
    // `panel.on_save`, and cloning `Rc<BookmarksPanel>` into it
    // would form a cycle (panel → on_save → closure → panel)
    // that prevents the panel from dropping on window teardown.
    let save_bm_weak = std::rc::Rc::downgrade(bm);
    let save_name_entry = nav.name_entry.clone();
    let save_state = Rc::clone(state);
    let save_radio_bw = panels.radio.bandwidth_row.clone();
    let save_radio_sq_en = panels.radio.squelch_enabled_row.clone();
    let save_radio_auto_sq = panels.radio.auto_squelch_row.clone();
    let save_radio_sq_lvl = panels.radio.squelch_level_row.clone();
    let save_radio_deemp = panels.radio.deemphasis_row.clone();
    let save_radio_nben = panels.radio.noise_blanker_row.clone();
    let save_radio_nben_lvl = panels.radio.nb_level_row.clone();
    let save_radio_nr = panels.radio.fm_if_nr_row.clone();
    let save_radio_stereo = panels.radio.stereo_row.clone();
    let save_radio_ctcss = panels.radio.ctcss_row.clone();
    let save_radio_ctcss_threshold = panels.radio.ctcss_threshold_row.clone();
    let save_radio_voice_squelch = panels.radio.voice_squelch_row.clone();
    let save_radio_voice_squelch_threshold = panels.radio.voice_squelch_threshold_row.clone();
    let save_source_gain = panels.source.gain_row.clone();
    let save_source_agc = panels.source.agc_row.clone();
    bm.connect_save(move || {
        // `save_bm_weak` is the ONLY reference this closure holds
        // to the panel. Upgrading on entry gives us a live handle
        // for the duration of the save; dropping it at the end of
        // the call lets the panel drop cleanly on teardown even
        // though the closure itself is stored inside
        // `panel.on_save`.
        let Some(save_bm) = save_bm_weak.upgrade() else {
            return;
        };
        let active = save_bm.active_bookmark.borrow().clone();
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
            auto_squelch_enabled: save_radio_auto_sq.is_active(),
            #[allow(clippy::cast_possible_truncation)]
            squelch_level: save_radio_sq_lvl.value() as f32,
            gain: save_source_gain.value(),
            // Same transient-index fallback as the new-bookmark
            // path above — user-initiated save always produces
            // a bookmark.
            agc_type: sidebar::source_panel::agc_type_from_selected(save_source_agc.selected())
                .unwrap_or(sidebar::source_panel::AgcType::DEFAULT),
            volume: None,
            deemphasis: save_radio_deemp.selected(),
            nb_enabled: save_radio_nben.is_active(),
            #[allow(clippy::cast_possible_truncation)]
            nb_level: save_radio_nben_lvl.value() as f32,
            fm_if_nr: save_radio_nr.is_active(),
            wfm_stereo: save_radio_stereo.is_active(),
            high_pass: None,
            ctcss_mode: Some(sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(
                save_radio_ctcss.selected(),
            )),
            #[allow(clippy::cast_possible_truncation)]
            ctcss_threshold: Some(save_radio_ctcss_threshold.value() as f32),
            voice_squelch_mode: Some({
                #[allow(clippy::cast_possible_truncation)]
                let t = save_radio_voice_squelch_threshold.value() as f32;
                sidebar::radio_panel::RadioPanel::voice_squelch_mode_from_index(
                    save_radio_voice_squelch.selected(),
                    t,
                )
            }),
        };
        // Find and update the active bookmark in the list.
        let mut bms = save_bm.bookmarks.borrow_mut();
        if let Some(bm) = bms
            .iter_mut()
            .find(|b| b.name == active.name && b.frequency == active.frequency)
        {
            bm.frequency = freq_u64;
            bm.demod_mode = sidebar::navigation_panel::demod_mode_to_string(mode);
            bm.bandwidth = bw;
            bm.squelch_enabled = Some(profile.squelch_enabled);
            bm.auto_squelch_enabled = Some(profile.auto_squelch_enabled);
            bm.squelch_level = Some(profile.squelch_level);
            bm.gain = Some(profile.gain);
            // Legacy-compatible AGC save: write both the new
            // `agc_type` AND the legacy `agc: Option<bool>` so
            // a post-#354 bookmark still round-trips through
            // older builds. Software AGC maps to `false` on the
            // legacy path (safer than `true` since hardware AGC
            // is the documented-problem path in #332).
            bm.agc = Some(matches!(
                profile.agc_type,
                sidebar::source_panel::AgcType::Hardware
            ));
            bm.agc_type = Some(profile.agc_type);
            bm.volume = profile.volume;
            bm.deemphasis = Some(profile.deemphasis);
            bm.nb_enabled = Some(profile.nb_enabled);
            bm.nb_level = Some(profile.nb_level);
            bm.fm_if_nr = Some(profile.fm_if_nr);
            bm.wfm_stereo = Some(profile.wfm_stereo);
            bm.high_pass = profile.high_pass;
            bm.ctcss_mode = profile.ctcss_mode;
            bm.ctcss_threshold = profile.ctcss_threshold;
            bm.voice_squelch_mode = profile.voice_squelch_mode;
            // Keep ActiveBookmark in sync with the updated frequency.
            *save_bm.active_bookmark.borrow_mut() = sidebar::navigation_panel::ActiveBookmark {
                name: active.name.clone(),
                frequency: freq_u64,
            };
        }
        sidebar::navigation_panel::save_bookmarks(&bms);
        drop(bms);
        // Rebuild to update subtitle. Fires `on_mutated` so the
        // scanner re-projects — Save can change `scan_enabled` /
        // `priority` / override fields on the bookmark.
        save_bm.rebuild_after_mutation(&save_name_entry);
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

    // Sink type selector — toggles the engine between local
    // audio device and network stream, and shows/hides the
    // network config rows so the sidebar layout reflects the
    // active mode. Per issue #247.
    let state_sink_type = Rc::clone(state);
    let host_row = panels.audio.network_host_row.clone();
    let port_row = panels.audio.network_port_row.clone();
    let proto_row = panels.audio.network_protocol_row.clone();
    let status_row = panels.audio.network_status_row.clone();
    panels
        .audio
        .sink_type_row
        .connect_selected_notify(move |row| {
            // Match explicitly against both legal indices and
            // early-return on anything else. The previous shape
            // mapped any non-Network value to Local, which would
            // silently dispatch a sink swap on a transient or
            // future-added combo entry that this handler doesn't
            // know about. Per `CodeRabbit` round 2 on PR #351.
            let new_type = match row.selected() {
                sidebar::audio_panel::SINK_TYPE_LOCAL_IDX => sdr_core::AudioSinkType::Local,
                sidebar::audio_panel::SINK_TYPE_NETWORK_IDX => sdr_core::AudioSinkType::Network,
                unknown => {
                    tracing::warn!(
                        selected_idx = unknown,
                        "audio sink-type combo emitted unknown index; ignoring"
                    );
                    return;
                }
            };
            let network_visible = matches!(new_type, sdr_core::AudioSinkType::Network);
            host_row.set_visible(network_visible);
            port_row.set_visible(network_visible);
            proto_row.set_visible(network_visible);
            status_row.set_visible(network_visible);
            state_sink_type.send_dsp(UiToDsp::SetAudioSinkType(new_type));
        });

    // Helper closure-builder: any change to the network host /
    // port / protocol triple re-sends the full SetNetworkSinkConfig
    // so the controller can rebuild the sink atomically. The
    // engine handler is idempotent — sending the same values
    // again is harmless. Per issue #247.
    let push_network_config = {
        let state = Rc::clone(state);
        let host_row = panels.audio.network_host_row.clone();
        let port_row = panels.audio.network_port_row.clone();
        let proto_row = panels.audio.network_protocol_row.clone();
        move || {
            let hostname = host_row.text().to_string();
            // SpinRow's adjustment is bounded (1..=65535), and
            // we explicitly clamp again here as belt-and-
            // suspenders against any future code path that
            // hands us a different adjustment. After the clamp
            // the value is finite and in [0, 65535] so the
            // narrowing cast is exact — the clippy lints below
            // are safe to silence with that justification.
            let port_clamped = port_row
                .value()
                .round()
                .clamp(f64::from(u16::MIN), f64::from(u16::MAX));
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to [0, u16::MAX] above"
            )]
            let port = port_clamped as u16;
            let protocol = sidebar::audio_panel::protocol_from_combo_idx(proto_row.selected());
            state.send_dsp(UiToDsp::SetNetworkSinkConfig {
                hostname,
                port,
                protocol,
            });
        }
    };

    // Hostname commits on Enter / focus-out (the AdwEntryRow's
    // `connect_apply` signal). connect_changed would fire per
    // keystroke and reconnect-on-every-character is bad UX.
    {
        let push = push_network_config.clone();
        panels.audio.network_host_row.connect_apply(move |_| push());
    }
    {
        let push = push_network_config.clone();
        panels
            .audio
            .network_port_row
            .connect_value_notify(move |_| push());
    }
    {
        let push = push_network_config.clone();
        panels
            .audio
            .network_protocol_row
            .connect_selected_notify(move |_| push());
    }

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
#[allow(clippy::too_many_arguments)]
fn unlock_transcription_session_rows(
    model_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "whisper")] silence_row: &glib::WeakRef<adw::SpinRow>,
    noise_gate_row: &glib::WeakRef<adw::SpinRow>,
    audio_enhancement_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")] display_mode_row: &glib::WeakRef<adw::ComboRow>,
    #[cfg(feature = "sherpa")] vad_threshold_row: &glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")] auto_break_row: &glib::WeakRef<adw::SwitchRow>,
    #[cfg(feature = "sherpa")] auto_break_min_open_row: &glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")] auto_break_tail_row: &glib::WeakRef<adw::SpinRow>,
    #[cfg(feature = "sherpa")] auto_break_min_segment_row: &glib::WeakRef<adw::SpinRow>,
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
    if let Some(row) = audio_enhancement_row.upgrade() {
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
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_min_open_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_tail_row.upgrade() {
        row.set_sensitive(true);
    }
    #[cfg(feature = "sherpa")]
    if let Some(row) = auto_break_min_segment_row.upgrade() {
        row.set_sensitive(true);
    }
}

/// Connect scanner panel controls to DSP commands.
///
/// Wiring:
/// - master switch → `UiToDsp::SetScannerEnabled`
/// - default dwell / hang sliders → persist to `ConfigManager`
///   and re-project the bookmark list into
///   `UiToDsp::UpdateScannerChannels` so a running scanner picks
///   up the new per-channel dwell/hang on its next tick.
fn connect_scanner_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let scanner = &panels.scanner;

    // Master switch → SetScannerEnabled. Using `connect_active_notify`
    // (not `connect_state_set`) so programmatic toggles fire too:
    //   - F8 shortcut calls `set_active` which changes the active
    //     property and fires notify::active.
    //   - `ScannerForceDisable::trigger` calls `set_active(false)`
    //     on the same switch for manual-tune force-disable.
    //   - DSP-origin widget syncs (ScannerEmptyRotation,
    //     ScannerMutexStopped::ScannerStopped*) call `set_state`,
    //     which also propagates to active and fires notify::active.
    //     The resulting redundant `SetScannerEnabled(false)`
    //     dispatch is idempotent at the engine — it's cheaper to
    //     pay one extra message per event than to add a suppress
    //     flag for every DSP-origin sync site.
    let state_switch = Rc::clone(state);
    scanner.master_switch.connect_active_notify(move |sw| {
        state_switch.send_dsp(UiToDsp::SetScannerEnabled(sw.is_active()));
    });

    // Restore persisted slider values BEFORE wiring the notify
    // handlers below. `set_value` on a SpinRow fires
    // `value-changed`, so if we wired first and restored after
    // we'd trigger a spurious `save_default_*_ms` +
    // `project_and_push_scanner_channels` during window
    // construction — plus `build_window` re-seeds the scanner
    // right after `connect_sidebar_panels` returns, which would
    // pile on a second redundant dispatch per slider.
    let dwell_ms = sidebar::scanner_panel::load_default_dwell_ms(config);
    scanner.default_dwell_row.set_value(f64::from(dwell_ms));
    let hang_ms = sidebar::scanner_panel::load_default_hang_ms(config);
    scanner.default_hang_row.set_value(f64::from(hang_ms));

    // Default dwell slider: persist on every value change, then
    // re-project the bookmark list so `ScannerChannel::dwell_ms`
    // picks up the new default on channels without an override.
    let config_dwell = std::sync::Arc::clone(config);
    let bookmarks_dwell = Rc::clone(&panels.bookmarks);
    let state_dwell = Rc::clone(state);
    let config_dwell_project = std::sync::Arc::clone(config);
    scanner.default_dwell_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = row.value() as u32;
        sidebar::scanner_panel::save_default_dwell_ms(&config_dwell, ms);
        sidebar::navigation_panel::project_and_push_scanner_channels(
            &bookmarks_dwell.bookmarks.borrow(),
            &state_dwell,
            &config_dwell_project,
        );
    });

    // Default hang slider: same pattern as dwell.
    let config_hang = std::sync::Arc::clone(config);
    let bookmarks_hang = Rc::clone(&panels.bookmarks);
    let state_hang = Rc::clone(state);
    let config_hang_project = std::sync::Arc::clone(config);
    scanner.default_hang_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = row.value() as u32;
        sidebar::scanner_panel::save_default_hang_ms(&config_hang, ms);
        sidebar::navigation_panel::project_and_push_scanner_channels(
            &bookmarks_hang.bookmarks.borrow(),
            &state_hang,
            &config_hang_project,
        );
    });

    // Lockout button → `LockoutScannerChannel(key)`. The active
    // channel key is updated on every `ScannerActiveChannelChanged`
    // in `handle_dsp_message` and stashed on `state.scanner_active_key`.
    // The button is hidden whenever that key is `None` (same
    // handler), so a click here is guaranteed to have a key —
    // but we check and early-return defensively in case a click
    // races a state change.
    let state_lockout = Rc::clone(state);
    scanner.lockout_button.connect_clicked(move |_| {
        let Some(key) = state_lockout.scanner_active_key.borrow().clone() else {
            tracing::debug!("lockout clicked with no active key — no-op");
            return;
        };
        state_lockout.send_dsp(UiToDsp::LockoutScannerChannel(key));
    });
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
    #[cfg_attr(not(feature = "sherpa"), allow(unused_variables))]
    squelch_enabled_row: &adw::SwitchRow,
    #[cfg_attr(not(feature = "sherpa"), allow(unused_variables))] toast_overlay: &adw::ToastOverlay,
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
    let audio_enhancement_row = transcript.audio_enhancement_row.clone();
    // Weak refs used by the async event-loop closure to drive the same
    // teardown the synchronous error path does (see below) when the
    // backend fires TranscriptionEvent::Error mid-session. Weak so the
    // timeout closure doesn't keep widgets alive past their UI lifetime.
    let enable_row_weak = transcript.enable_row.downgrade();
    let model_row_weak = model_row.downgrade();
    #[cfg(feature = "whisper")]
    let silence_row_weak = silence_row.downgrade();
    let noise_gate_row_weak = noise_gate_row.downgrade();
    let audio_enhancement_row_weak = audio_enhancement_row.downgrade();
    #[cfg(feature = "sherpa")]
    let display_mode_row = transcript.display_mode_row.clone();
    #[cfg(feature = "sherpa")]
    let vad_threshold_row = transcript.vad_threshold_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_row = transcript.auto_break_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_min_open_row = transcript.auto_break_min_open_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_tail_row = transcript.auto_break_tail_row.clone();
    #[cfg(feature = "sherpa")]
    let auto_break_min_segment_row = transcript.auto_break_min_segment_row.clone();
    #[cfg(feature = "sherpa")]
    let squelch_enabled_row_for_session = squelch_enabled_row.clone();
    #[cfg(feature = "sherpa")]
    let toast_overlay_for_session = toast_overlay.downgrade();
    #[cfg(feature = "sherpa")]
    let live_line_label = transcript.live_line_label.clone();
    #[cfg(feature = "sherpa")]
    let display_mode_row_weak = display_mode_row.downgrade();
    #[cfg(feature = "sherpa")]
    let vad_threshold_row_weak = vad_threshold_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_row_weak = auto_break_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_min_open_row_weak = auto_break_min_open_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_tail_row_weak = auto_break_tail_row.downgrade();
    #[cfg(feature = "sherpa")]
    let auto_break_min_segment_row_weak = auto_break_min_segment_row.downgrade();
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
            // Read the selected model index once at the top of the
            // session-start branch; the Auto Break eligibility check
            // below needs it, and the BackendConfig construction
            // below reuses it.
            let model_idx = model_row.selected() as usize;

            // Auto Break is eligible ONLY when all three conditions
            // hold: (1) the toggle itself is on, (2) the current demod
            // mode is NFM, and (3) the selected sherpa model is offline
            // (Moonshine, Parakeet). The toggle is persisted, so
            // without this computed gate it would still report "on"
            // after a restart into WFM, or after the user switched to
            // streaming Zipformer and the row went invisible — either
            // of which would produce an unsupported session
            // (streaming Zipformer rejects AutoBreak at session start;
            // non-NFM modes never emit squelch edges so the state
            // machine sits in Idle forever). Compute the effective
            // value once here and use it for both the precondition
            // check and the BackendConfig assignment.
            #[cfg(feature = "sherpa")]
            let auto_break_enabled = {
                let selected_is_offline = sdr_transcription::SherpaModel::ALL
                    .get(model_idx)
                    .copied()
                    .is_some_and(|m| !m.supports_partials());
                auto_break_row.is_active()
                    && state_clone.demod_mode.get() == sdr_types::DemodMode::Nfm
                    && selected_is_offline
            };

            // Auto Break precondition: squelch must be enabled so the
            // radio produces the open/close transitions the state
            // machine needs for segmentation. Without squelch enabled,
            // the session would sit in Idle indefinitely producing
            // zero transcripts — silent failure mode. Block session
            // start with an actionable toast.
            #[cfg(feature = "sherpa")]
            if auto_break_enabled && !squelch_enabled_row_for_session.is_active() {
                let toast = adw::Toast::new(
                    "Auto Break needs squelch enabled to detect transmission boundaries. \
                     Enable squelch in the radio panel, or turn off Auto Break to use VAD.",
                );
                if let Some(overlay) = toast_overlay_for_session.upgrade() {
                    overlay.add_toast(toast);
                }
                // Revert the enable toggle so the user can take action first.
                // The OFF branch of the handler is a safe no-op on an
                // inactive session (it just drops any backend channels).
                row.set_active(false);
                return;
            }

            // Lock model and tuning controls while transcription is active.
            model_row.set_sensitive(false);
            #[cfg(feature = "whisper")]
            silence_row.set_sensitive(false);
            noise_gate_row.set_sensitive(false);
            audio_enhancement_row.set_sensitive(false);
            // All settings lock during a session for mid-session fault
            // tolerance — walks back PR 4's earlier display_mode_row
            // exception. User stops, changes, starts.
            #[cfg(feature = "sherpa")]
            display_mode_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            vad_threshold_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_min_open_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_tail_row.set_sensitive(false);
            #[cfg(feature = "sherpa")]
            auto_break_min_segment_row.set_sensitive(false);

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

            #[cfg(feature = "sherpa")]
            let segmentation_mode = if auto_break_enabled {
                sdr_transcription::SegmentationMode::AutoBreak
            } else {
                sdr_transcription::SegmentationMode::Vad
            };
            #[cfg(feature = "whisper")]
            let segmentation_mode = sdr_transcription::SegmentationMode::Vad;

            // Auto Break timing parameters read from the session sliders.
            // Whisper builds hardcode the defaults (these fields are
            // never consumed because Whisper uses a different backend).
            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let auto_break_min_open_ms = auto_break_min_open_row.value() as u32;
            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let auto_break_tail_ms = auto_break_tail_row.value() as u32;
            #[cfg(feature = "sherpa")]
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let auto_break_min_segment_ms = auto_break_min_segment_row.value() as u32;
            #[cfg(feature = "whisper")]
            let auto_break_min_open_ms = sdr_transcription::AUTO_BREAK_MIN_OPEN_MS_DEFAULT;
            #[cfg(feature = "whisper")]
            let auto_break_tail_ms = sdr_transcription::AUTO_BREAK_TAIL_MS_DEFAULT;
            #[cfg(feature = "whisper")]
            let auto_break_min_segment_ms =
                sdr_transcription::AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT;

            // Audio enhancement mode from the transcript panel
            // combo row. The row's persisted index is captured at
            // session start (not subscribed to — matches the
            // existing "lock during session" behavior for all
            // transcription settings).
            let audio_enhancement = match audio_enhancement_row.selected() {
                sidebar::transcript_panel::AUDIO_ENHANCEMENT_BROADBAND_IDX => {
                    sdr_transcription::denoise::AudioEnhancement::Broadband
                }
                sidebar::transcript_panel::AUDIO_ENHANCEMENT_OFF_IDX => {
                    sdr_transcription::denoise::AudioEnhancement::Off
                }
                _ => sdr_transcription::denoise::AudioEnhancement::VoiceBand,
            };

            let config = sdr_transcription::BackendConfig {
                model,
                silence_threshold,
                noise_gate_ratio,
                vad_threshold,
                segmentation_mode,
                auto_break_min_open_ms,
                auto_break_tail_ms,
                auto_break_min_segment_ms,
                audio_enhancement,
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
                    let audio_enhancement_row_weak = audio_enhancement_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let display_mode_row_weak = display_mode_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let vad_threshold_row_weak = vad_threshold_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_row_weak = auto_break_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_min_open_row_weak = auto_break_min_open_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_tail_row_weak = auto_break_tail_row_weak.clone();
                    #[cfg(feature = "sherpa")]
                    let auto_break_min_segment_row_weak =
                        auto_break_min_segment_row_weak.clone();
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
                                            &audio_enhancement_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &display_mode_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &vad_threshold_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_min_open_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_tail_row_weak,
                                            #[cfg(feature = "sherpa")]
                                            &auto_break_min_segment_row_weak,
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
                                        &audio_enhancement_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &display_mode_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &vad_threshold_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_min_open_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_tail_row_weak,
                                        #[cfg(feature = "sherpa")]
                                        &auto_break_min_segment_row_weak,
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
                        &audio_enhancement_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &display_mode_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &vad_threshold_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_min_open_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_tail_row.downgrade(),
                        #[cfg(feature = "sherpa")]
                        &auto_break_min_segment_row.downgrade(),
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
                &audio_enhancement_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &display_mode_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &vad_threshold_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_min_open_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_tail_row.downgrade(),
                #[cfg(feature = "sherpa")]
                &auto_break_min_segment_row.downgrade(),
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

#[cfg(test)]
mod parse_host_port_tests {
    use super::parse_host_port;

    #[test]
    fn round_trips_a_simple_hostname_port_pair() {
        // The mainline case — `favorite_key(server)` today
        // produces exactly this shape, so Connect-from-popover
        // depends on this round-trip working.
        assert_eq!(
            parse_host_port("shack-pi:1234"),
            Some(("shack-pi".to_string(), 1234))
        );
    }

    #[test]
    fn ipv6_literal_with_embedded_colons_splits_on_last_colon() {
        // We don't emit bracketed IPv6 in `favorite_key` today,
        // but the parser should be the conservative half of the
        // contract: `rsplit_once(':')` keeps everything up to the
        // last colon as the host so an IPv6 literal round-trips
        // if we ever start persisting one.
        assert_eq!(
            parse_host_port("fe80::1:8080"),
            Some(("fe80::1".to_string(), 8080))
        );
    }

    #[test]
    fn rejects_missing_colon() {
        assert_eq!(parse_host_port("shack-pi"), None);
    }

    #[test]
    fn rejects_non_numeric_port() {
        assert_eq!(parse_host_port("shack-pi:abc"), None);
    }

    #[test]
    fn rejects_out_of_range_port() {
        // 65536 overflows u16; parse must fail rather than
        // silently truncating.
        assert_eq!(parse_host_port("shack-pi:65536"), None);
    }

    #[test]
    fn rejects_empty_host() {
        // ":1234" shouldn't round-trip as a valid endpoint —
        // callers would dispatch `SetNetworkConfig { hostname:
        // "" }` which is garbage.
        assert_eq!(parse_host_port(":1234"), None);
    }
}

#[cfg(test)]
mod favorite_sort_tests {
    use super::sort_favorites_for_display;
    use crate::sidebar::source_panel::FavoriteEntry;

    fn entry(key: &str, nickname: &str) -> FavoriteEntry {
        FavoriteEntry {
            key: key.into(),
            nickname: nickname.into(),
            tuner_name: None,
            gain_count: None,
            last_seen_unix: None,
            requested_role: None,
            auth_required: None,
        }
    }

    #[test]
    fn primary_order_is_lowercased_nickname() {
        let a = entry("a.local.:1234", "Zeta");
        let b = entry("b.local.:1234", "alpha");
        let c = entry("c.local.:1234", "Beta");
        let mut entries = vec![&a, &b, &c];
        sort_favorites_for_display(&mut entries);
        // Case-insensitive: "alpha" < "Beta" < "Zeta".
        assert_eq!(
            entries.iter().map(|e| &e.key[..]).collect::<Vec<_>>(),
            ["b.local.:1234", "c.local.:1234", "a.local.:1234",]
        );
    }

    #[test]
    fn tie_breaks_on_key_when_nicknames_match() {
        // Duplicate nickname across two servers — the secondary
        // key must pin the order deterministically so two app
        // launches (or two inserts against an unstable HashMap
        // iteration order) render the popover the same way.
        let a = entry("attic-pi.local.:1234", "Shack");
        let b = entry("shack-pi.local.:1234", "Shack");
        let c = entry("basement-pi.local.:1234", "Shack");
        let mut entries = vec![&a, &b, &c];
        sort_favorites_for_display(&mut entries);
        // Alphabetical by `key` — attic < basement < shack.
        assert_eq!(
            entries.iter().map(|e| &e.key[..]).collect::<Vec<_>>(),
            [
                "attic-pi.local.:1234",
                "basement-pi.local.:1234",
                "shack-pi.local.:1234",
            ]
        );
    }

    #[test]
    fn idempotent_when_already_sorted() {
        let a = entry("a.local.:1234", "alpha");
        let b = entry("b.local.:1234", "beta");
        let mut entries = vec![&a, &b];
        sort_favorites_for_display(&mut entries);
        assert_eq!(
            entries.iter().map(|e| &e.key[..]).collect::<Vec<_>>(),
            ["a.local.:1234", "b.local.:1234",]
        );
    }
}

#[cfg(test)]
mod favorite_subtitle_format_tests {
    use super::{format_favorite_subtitle, format_seen_age};
    use crate::sidebar::source_panel::FavoriteEntry;

    /// Fixed "wall-clock now" for the subtitle + age tests. Pinning
    /// this keeps the expected output deterministic; the actual
    /// seconds value is arbitrary (2023-11-14T22:13:20Z) — what
    /// matters is that all test inputs derive their `last_seen`
    /// offsets from here.
    const NOW_UNIX: u64 = 1_700_000_000;

    fn sample_entry(
        tuner: Option<&str>,
        gains: Option<u32>,
        last_seen: Option<u64>,
    ) -> FavoriteEntry {
        FavoriteEntry {
            key: "shack-pi.local.:1234".into(),
            nickname: "Shack Pi".into(),
            tuner_name: tuner.map(str::to_string),
            gain_count: gains,
            last_seen_unix: last_seen,
            requested_role: None,
            auth_required: None,
        }
    }

    #[test]
    fn seen_age_just_now_under_60_seconds() {
        // Sub-minute gap renders as "just now" — avoids "0m ago"
        // churn on freshly-stamped entries.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 30), "just now");
    }

    #[test]
    fn seen_age_minute_bucket() {
        // Integer division, not rounding: 179s → 2m (not 3m).
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 179), "2m ago");
    }

    #[test]
    fn seen_age_hour_bucket() {
        // 3599s → 59m (last second of minute bucket), 3600s → 1h.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 3_600), "1h ago");
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 3_599), "59m ago");
    }

    #[test]
    fn seen_age_day_bucket() {
        // 86_399s → 23h, 86_400s → 1d.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 86_400), "1d ago");
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 86_399), "23h ago");
    }

    #[test]
    fn seen_age_clock_skew_renders_just_now() {
        // `last_seen > now` means the entry was stamped against a
        // clock that was ahead of ours — shouldn't underflow into
        // a garbage value.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX + 60), "just now");
        // Equal case.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX), "just now");
    }

    #[test]
    fn subtitle_includes_all_three_parts_when_metadata_present() {
        // Canonical "rich" entry: key + tuner·gains + seen age,
        // joined by middle-dot separators.
        let entry = sample_entry(Some("R820T"), Some(29), Some(NOW_UNIX - 7_200));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • R820T · 29 gains • seen 2h ago",
        );
    }

    #[test]
    fn subtitle_drops_tuner_segment_when_tuner_missing() {
        // Legacy-upgraded entry with no tuner metadata — the
        // "tuner · gains" middle segment is omitted entirely
        // rather than rendering empty "— · 0 gains" placeholder.
        let entry = sample_entry(None, None, Some(NOW_UNIX - 300));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • seen 5m ago",
        );
    }

    #[test]
    fn subtitle_drops_tuner_segment_when_only_gains_missing() {
        // Partial metadata is still incomplete — `if let (Some,
        // Some)` means both must be present or neither renders.
        let entry = sample_entry(Some("R820T"), None, Some(NOW_UNIX - 300));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • seen 5m ago",
        );
    }

    #[test]
    fn subtitle_shows_offline_when_last_seen_is_none() {
        // Never seen this session → "offline" in the seen slot.
        let entry = sample_entry(Some("R820T"), Some(29), None);
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • R820T · 29 gains • offline",
        );
    }

    #[test]
    fn subtitle_shows_offline_when_last_seen_is_zero() {
        // Zero is treated as "no real stamp" — `format_favorite_
        // subtitle` explicitly gates on `ts > 0` so a corrupt /
        // default-valued timestamp doesn't render as "seen 55y
        // ago" (the 1970 epoch).
        let entry = sample_entry(Some("R820T"), Some(29), Some(0));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • R820T · 29 gains • offline",
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod rtl_tcp_discovery_format_tests {
    use std::net::IpAddr;
    use std::time::{Duration, Instant};

    use sdr_rtltcp_discovery::{DiscoveredServer, TxtRecord};

    use super::{format_age, format_discovery_subtitle};

    fn sample_server(addresses: Vec<IpAddr>, hostname: &str) -> DiscoveredServer {
        DiscoveredServer {
            instance_name: "shack-pi weather._rtl_tcp._tcp.local.".into(),
            hostname: hostname.into(),
            port: 1234,
            addresses,
            txt: TxtRecord {
                tuner: "R820T".into(),
                version: "0.1.0".into(),
                gains: 29,
                nickname: "weather".into(),
                txbuf: None,
                codecs: None,
                auth_required: None,
            },
            last_seen: Instant::now(),
        }
    }

    #[test]
    fn format_age_buckets_seconds_minutes_hours() {
        // < 5 s bucket → "just now" (debounces the 200 ms refresh
        // from showing "0s ago / 1s ago" noise).
        assert_eq!(format_age(Duration::from_millis(0)), "just now");
        assert_eq!(format_age(Duration::from_secs(4)), "just now");
        // 5 s – 59 s → "Ns ago"
        assert_eq!(format_age(Duration::from_secs(5)), "5s ago");
        assert_eq!(format_age(Duration::from_secs(59)), "59s ago");
        // 1 m – 59 m → "Nm ago"
        assert_eq!(format_age(Duration::from_mins(1)), "1m ago");
        assert_eq!(format_age(Duration::from_secs(125)), "2m ago");
        assert_eq!(format_age(Duration::from_secs(3599)), "59m ago");
        // 1 h+ → "Nh ago"
        assert_eq!(format_age(Duration::from_hours(1)), "1h ago");
        assert_eq!(format_age(Duration::from_hours(2)), "2h ago");
    }

    #[test]
    fn subtitle_with_ip_shows_hostname_and_freshness() {
        // When we have a resolved IP, the subtitle includes both the
        // IP (the Connect button's target) AND the advertised
        // hostname (the friendly name the user recognises).
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        let server = sample_server(vec![ip], "shack-pi.local.");
        let subtitle = format_discovery_subtitle(&server, Duration::from_secs(12));
        assert!(
            subtitle.contains("192.168.1.5:1234"),
            "subtitle missing connect target: {subtitle}"
        );
        assert!(
            subtitle.contains("shack-pi"),
            "subtitle missing advertised hostname: {subtitle}"
        );
        assert!(
            !subtitle.contains(".local"),
            "subtitle should strip .local suffix: {subtitle}"
        );
        assert!(
            subtitle.contains("R820T"),
            "subtitle missing tuner: {subtitle}"
        );
        assert!(
            subtitle.contains("29 gains"),
            "subtitle missing gain count: {subtitle}"
        );
        assert!(
            subtitle.contains("seen 12s ago"),
            "subtitle missing freshness: {subtitle}"
        );
    }

    #[test]
    fn subtitle_without_ip_omits_duplicate_hostname_segment() {
        // No resolved addresses: connect target falls back to the
        // hostname itself. Showing it twice (once as target, once as
        // hostname segment) would be noise, so the hostname segment
        // is suppressed when it would duplicate the target.
        let server = sample_server(vec![], "shack-pi.local.");
        let subtitle = format_discovery_subtitle(&server, Duration::from_secs(1));
        assert!(
            subtitle.starts_with("shack-pi.local.:1234"),
            "subtitle should use hostname as target: {subtitle}"
        );
        // Exactly two ` • ` separators: target + hardware/freshness.
        assert_eq!(
            subtitle.matches(" • ").count(),
            1,
            "expected one bullet separator when hostname segment is suppressed: {subtitle}"
        );
    }

    #[test]
    fn subtitle_fresh_announce_reads_just_now() {
        // On the initial announce, elapsed is effectively 0 — the
        // subtitle should say "just now" rather than "0s ago".
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let server = sample_server(vec![ip], "radio.local.");
        let subtitle = format_discovery_subtitle(&server, Duration::from_millis(50));
        assert!(
            subtitle.ends_with("seen just now"),
            "subtitle should read 'seen just now' for sub-5s age: {subtitle}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod server_panel_format_tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use sdr_server_rtltcp::{ClientInfo, InitialDeviceState, codec::Codec};

    use super::{
        SERVER_STATUS_POLL_INTERVAL, format_commanded_state, format_data_rate, format_hz,
        format_uptime,
    };

    // ============================================================
    // Test fixture constants (`CodeRabbit` round 2 on PR #402).
    // Names make each scenario's intent obvious at a glance:
    // "is this testing 145 MHz 2m-band tune or 100 MHz WFM"
    // reads clearer when the literal has a rationale.
    // ============================================================

    /// Placeholder peer port for `ClientInfo` fixtures that don't
    /// exercise the peer address field — any non-privileged port
    /// works, so pick one well above the well-known range.
    const FIXTURE_PEER_PORT: u16 = 42_000;
    /// 2-meter amateur band test frequency (145.5 MHz) — stands in
    /// for "non-default freq the user commanded" in fallback tests.
    const FIXTURE_FREQ_2M_HZ: u32 = 145_500_000;
    /// 100 MHz WFM broadcast band test frequency — second sample
    /// to catch tests that pass on the 2m fixture by coincidence.
    const FIXTURE_FREQ_WFM_HZ: u32 = 100_000_000;
    /// Typical RTL-SDR sample rate (2.4 Msps) — used across tune
    /// fixtures.
    const FIXTURE_SAMPLE_RATE_HZ: u32 = 2_400_000;
    /// Mid-range tuner gain in tenths-of-dB (29.6 dB) — well
    /// inside the R820T table so "auto vs manual" branches aren't
    /// ambiguous.
    const FIXTURE_GAIN_MID_TENTHS: i32 = 296;
    /// Upper-range tuner gain in tenths-of-dB (49.6 dB) — matches
    /// the R820T's documented top step so the "manual gain in dB"
    /// formatter has a realistic ceiling value.
    const FIXTURE_GAIN_TOP_TENTHS: i32 = 496;
    /// Low-but-visible manual gain in tenths-of-dB (20 dB) —
    /// used specifically in the "auto overrides manual" test to
    /// prove the auto flag wins over any set value.
    const FIXTURE_GAIN_LOW_TENTHS: i32 = 200;

    /// Fresh `InitialDeviceState` matching what `Server::start`
    /// stores when the user takes the upstream-default path. Most
    /// format tests use this; the ones that want to prove
    /// fallback-to-initial override the relevant field.
    fn default_initial() -> InitialDeviceState {
        InitialDeviceState::default()
    }

    /// Build a `ClientInfo` fixture for the `format_commanded_state`
    /// tests. Defaults to unset per-session fields (`None` on
    /// `current_freq` / `current_sample_rate` / `current_gain`) so
    /// each test only overrides the fields it's exercising.
    fn info(
        current_freq_hz: Option<u32>,
        current_sample_rate_hz: Option<u32>,
        current_gain_tenths_db: Option<i32>,
        current_gain_auto: Option<bool>,
    ) -> ClientInfo {
        ClientInfo {
            id: 0,
            peer: SocketAddr::from(([127, 0, 0, 1], FIXTURE_PEER_PORT)),
            connected_since: Instant::now(),
            codec: Codec::None,
            role: sdr_server_rtltcp::extension::Role::Control,
            bytes_sent: 0,
            buffers_dropped: 0,
            last_command: None,
            current_freq_hz,
            current_sample_rate_hz,
            current_gain_tenths_db,
            current_gain_auto,
            recent_commands: VecDeque::new(),
        }
    }

    #[test]
    fn format_uptime_uses_compact_unit_picker() {
        // Sub-minute: just seconds.
        assert_eq!(format_uptime(Duration::from_secs(5)), "5s");
        // Sub-hour: minutes + seconds, no hours prefix.
        assert_eq!(format_uptime(Duration::from_secs(61)), "1m 1s");
        assert_eq!(format_uptime(Duration::from_secs(3599)), "59m 59s");
        // Hour+: full triple.
        assert_eq!(format_uptime(Duration::from_secs(3661)), "1h 1m 1s");
        assert_eq!(format_uptime(Duration::from_secs(7322)), "2h 2m 2s");
    }

    #[test]
    fn format_data_rate_picks_kbps_below_mbps_boundary() {
        // 0.5 Mbps worth of bytes over the 500 ms interval → 0.5 Mbps
        // → still kbps under the 1 Mbps switchover. (1 Mbps =
        // 125_000 bytes/s, so 500 ms of 0.5 Mbps is 31_250 bytes.)
        assert_eq!(
            format_data_rate(31_250, SERVER_STATUS_POLL_INTERVAL),
            "500.0 kbps"
        );
        // ~4.8 Mbps (the rtl_tcp canonical rate) over 500 ms.
        // 4.8 Mbps * 0.5 s = 2.4 Mbit = 300_000 bytes.
        assert_eq!(
            format_data_rate(300_000, SERVER_STATUS_POLL_INTERVAL),
            "4.80 Mbps"
        );
        // Zero bytes → "0.0 kbps" not a panic.
        assert_eq!(format_data_rate(0, SERVER_STATUS_POLL_INTERVAL), "0.0 kbps");
    }

    #[test]
    fn format_data_rate_handles_zero_interval() {
        // A degenerate 0-second interval would divide by zero; fn
        // must return a safe sentinel so the row renders instead of
        // crashing.
        assert_eq!(format_data_rate(100, Duration::ZERO), "—");
    }

    #[test]
    fn format_hz_picks_unit_by_magnitude() {
        assert_eq!(format_hz(500), "500 Hz");
        assert_eq!(format_hz(1_500), "1.500 kHz");
        assert_eq!(format_hz(100_300_000), "100.300 MHz");
        assert_eq!(format_hz(1_500_000_000), "1.500 GHz");
    }

    #[test]
    fn format_commanded_state_no_client_renders_idle_placeholder() {
        // `None` means no connected client — the row should show
        // the idle `STATUS_IDLE_VALUE_SUBTITLE` placeholder. Guards
        // against a phantom row when the server is up but nobody's
        // connected.
        let subtitle = format_commanded_state(None, &default_initial());
        assert_eq!(
            subtitle,
            crate::sidebar::server_panel::STATUS_IDLE_VALUE_SUBTITLE
        );
    }

    #[test]
    fn format_commanded_state_falls_back_to_server_initial_when_client_silent() {
        // A connected client that hasn't sent any commands yet —
        // row should render the SERVER'S configured `initial`
        // values (what the user configured at `Server::start`),
        // not the library's upstream `rtl_tcp.c` defaults. Here
        // the initial is a non-default 145 MHz / 2.4 Msps / 29.6 dB,
        // so the subtitle should read those values even though the
        // client hasn't sent any SetX commands yet.
        // Per `CodeRabbit` round 1 on PR #402.
        let initial = InitialDeviceState {
            center_freq_hz: FIXTURE_FREQ_2M_HZ,
            sample_rate_hz: FIXTURE_SAMPLE_RATE_HZ,
            gain_tenths_db: Some(FIXTURE_GAIN_MID_TENTHS),
            ..InitialDeviceState::default()
        };
        let subtitle = format_commanded_state(Some(&info(None, None, None, None)), &initial);
        assert!(
            subtitle.contains("145.500 MHz"),
            "server's configured initial freq should show: {subtitle}"
        );
        assert!(
            subtitle.contains("2.400 MHz"),
            "server's configured initial sample rate should show: {subtitle}"
        );
        assert!(
            subtitle.contains("gain 29.6 dB"),
            "server's configured initial gain should show: {subtitle}"
        );
    }

    #[test]
    fn format_commanded_state_renders_auto_when_initial_gain_is_none() {
        // `initial.gain_tenths_db = None` encodes upstream's
        // automatic-gain mode (the CLI's `-g 0` path). With no
        // client overrides, the gain text should read "auto", not
        // a literal dB value. Regression for the pre-CR "initial"
        // placeholder that was meaningless to users.
        let initial = InitialDeviceState {
            gain_tenths_db: None,
            ..InitialDeviceState::default()
        };
        let subtitle = format_commanded_state(Some(&info(None, None, None, None)), &initial);
        assert!(
            subtitle.contains("gain auto"),
            "initial gain None should render as auto: {subtitle}"
        );
    }

    #[test]
    fn format_commanded_state_renders_client_auto_gain_preference() {
        // When the client has sent SetGainMode(auto), "auto" wins
        // regardless of any previous manual gain value OR the
        // server's configured initial gain.
        let client = info(
            Some(FIXTURE_FREQ_2M_HZ),
            Some(FIXTURE_SAMPLE_RATE_HZ),
            Some(FIXTURE_GAIN_LOW_TENTHS),
            Some(true),
        );
        let subtitle = format_commanded_state(Some(&client), &default_initial());
        assert!(subtitle.contains("145.500 MHz"));
        assert!(subtitle.contains("2.400 MHz"));
        assert!(
            subtitle.contains("gain auto"),
            "client auto should override manual gain value: {subtitle}"
        );
    }

    #[test]
    fn format_commanded_state_renders_manual_gain_in_db() {
        // SetTunerGain records tenths-of-dB; the render converts to
        // full dB with one decimal.
        let client = info(
            Some(FIXTURE_FREQ_WFM_HZ),
            Some(FIXTURE_SAMPLE_RATE_HZ),
            Some(FIXTURE_GAIN_TOP_TENTHS),
            Some(false),
        );
        let subtitle = format_commanded_state(Some(&client), &default_initial());
        assert!(
            subtitle.contains("gain 49.6 dB"),
            "49.6 dB should render from 496 tenths: {subtitle}"
        );
    }

    #[test]
    fn format_log_age_buckets() {
        use super::format_log_age;
        // < 2 s → "just now" debounces the 500 ms poll from showing
        // "0s ago" / "1s ago" noise on the most-recent entry.
        assert_eq!(format_log_age(Duration::from_millis(0)), "just now");
        assert_eq!(format_log_age(Duration::from_millis(1999)), "just now");
        // 2 s – 59 s → "Ns ago"
        assert_eq!(format_log_age(Duration::from_secs(2)), "2s ago");
        assert_eq!(format_log_age(Duration::from_secs(59)), "59s ago");
        // 1 m – 59 m → "Nm ago"
        assert_eq!(format_log_age(Duration::from_mins(1)), "1m ago");
        assert_eq!(format_log_age(Duration::from_secs(3599)), "59m ago");
        // 1 h+ → "Nh ago" (rare — single-session command histories
        // almost never live long enough, but the bucket keeps the
        // formatter total).
        assert_eq!(format_log_age(Duration::from_hours(1)), "1h ago");
    }
}
