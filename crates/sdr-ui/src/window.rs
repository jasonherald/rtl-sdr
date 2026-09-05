//! Main window construction — header bar, split view, breakpoints, DSP bridge.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::viewer::plain_toast;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_core::Engine;
use sdr_pipeline::iq_frontend::FftWindow;
use sdr_radio::DeemphasisMode;
use sdr_radio::af_chain::CtcssMode;
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
use crate::state::{AppState, PendingSstvExport};
use crate::status_bar::{self, StatusBar};

/// Default recording directory under the user's home.
const RECORDING_DIR_NAME: &str = "sdr-recordings";

/// Default window width in pixels.
const DEFAULT_WIDTH: i32 = 1200;
/// Default window height in pixels.
const DEFAULT_HEIGHT: i32 = 800;

/// FFT sizes — re-exported from display panel (single source of truth).
use crate::sidebar::display_panel::FFT_SIZES;
use crate::sidebar::source_panel::DECIMATION_FACTORS;

mod audio;
mod aviation;
mod display;
mod dsp_events;
mod layout;
mod navigation;
mod radio;
mod satellites;
mod scanner;
mod share;
mod source;
mod transcript;

use audio::{connect_audio_panel, connect_volume_persistence};
use aviation::{connect_aviation_panel, try_collapse_into_existing};
use display::connect_display_panel;
use dsp_events::{
    DspEventCtx, TOAST_TIMEOUT_PERSISTENT, TOAST_TIMEOUT_SHORT_SECS, handle_dsp_message,
};
use layout::{
    FavoritesHeaderHandle, LEFT_SIDEBAR_DEFAULT_WIDTH, LayoutHandles, RIGHT_SIDEBAR_DEFAULT_WIDTH,
    apply_sidebar_width, build_breakpoint, build_header_bar, build_layout, build_sidebar_toggle,
    build_toolbar_view, sync_activity_bar_to_sidebar_visibility, wire_activity_bar_clicks,
};
use navigation::connect_navigation_panel;
use radio::{
    connect_distance_estimator_persistence, connect_radio_panel,
    update_bandwidth_reset_sensitivity, update_bandwidth_row_range_for_mode,
    update_vfo_reset_button_visibility,
};
use satellites::connect_satellites_panel;
use scanner::{
    ScannerAxisRefreshOutcome, ScannerForceDisable, clear_scanner_active_channel_ui,
    connect_scanner_panel, refresh_scanner_axis_lock,
};
use share::connect_server_panel;
use source::{
    apply_rtl_tcp_connection_state, connect_rtl_tcp_discovery, connect_source_panel,
    connect_source_rtlsdr_probe, handle_rtl_tcp_state_toast,
};
use transcript::connect_transcript_panel;

/// Interval in milliseconds for polling the DSP→UI channel.
const DSP_POLL_INTERVAL_MS: u64 = 16;

/// Apply a manual tune originated by user UI interaction —
/// shared between the freq-selector `connect_frequency_changed`
/// handler and the scanner-locked spectrum click callback. Both
/// follow the same five-step recipe:
///
/// 1. Force-disable the scanner so the engine sees
///    `SetScannerEnabled(false)` BEFORE the `Tune` lands —
///    avoids racing a scanner retune with the manual tune.
/// 2. Update the cached center frequency in `AppState`.
/// 3. Dispatch `UiToDsp::Tune` to the engine.
/// 4. Sync the status bar's frequency readout.
/// 5. Sync the spectrum widget's centre + the Radio panel's
///    FSPL distance estimator (#164).
///
/// Per-caller specifics that DON'T fit the helper:
/// - The freq-selector widget itself: scanner-locked click
///   needs to push the new value INTO it (the click came from
///   the spectrum); freq-selector handler skips that step (the
///   value originated there).
/// - Bookmark recall: also restores demod mode + bandwidth +
///   tuning profile, which is a different shape and stays
///   inline in `connect_navigation_panel`.
///
/// Per `CodeRabbit` round 1 on PR #565.
fn apply_manual_tune(
    freq_hz: f64,
    reason: &str,
    state: &Rc<AppState>,
    force_disable: &ScannerForceDisable,
    status_bar: &Rc<StatusBar>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    radio_panel: &sidebar::radio_panel::RadioPanel,
) {
    force_disable.trigger(reason);
    state.center_frequency.set(freq_hz);
    state.send_dsp(UiToDsp::Tune(freq_hz));
    status_bar.update_frequency(freq_hz);
    spectrum_handle.set_center_frequency(freq_hz);
    radio_panel.update_distance_frequency(freq_hz);
}

/// Signature of the shared "tune the radio" closure handed to the
/// Satellites panel (per-row play buttons, auto-record AOS, the
/// `app.tune-satellite` action): `(freq_hz, demod_mode, bandwidth_hz)`.
type TuneFn = dyn Fn(u64, sdr_types::DemodMode, u32);

/// Everything the canonical 13-step tune mirror sequence (#509)
/// touches: `AppState` + every UI widget / status indicator that
/// mirrors the radio's tuning state. Consolidating the duplicated
/// sequence into `tune_to_target` requires every captured widget the
/// original call sites held; bundling them here keeps the signatures
/// flat. All fields are cheap clones (`Rc` bumps / `GObject`
/// refcounts), so the ctx itself is `Clone` for capture in closures.
#[derive(Clone)]
struct TuneCtx {
    state: Rc<AppState>,
    freq_selector: header::frequency_selector::FrequencySelector,
    demod_dropdown: gtk4::DropDown,
    spectrum_handle: Rc<spectrum::SpectrumHandle>,
    scanner_force_disable: Rc<ScannerForceDisable>,
    bandwidth_row: adw::SpinRow,
    radio_panel: sidebar::radio_panel::RadioPanel,
    status_bar: Rc<StatusBar>,
}

/// UI-mirror half of the 13-step tune sequence: bandwidth row range +
/// value (with the redundant-`SetBandwidth` notify suppressed),
/// mode-specific control visibility, and the status-bar frequency /
/// demod indicators — done last so a panic anywhere upstream doesn't
/// leave an optimistic value the DSP never received.
fn mirror_tune_ui(ctx: &TuneCtx, mode: sdr_types::DemodMode, bw_hz: f64, freq_f64: f64) {
    let TuneCtx {
        state,
        bandwidth_row,
        radio_panel,
        status_bar,
        ..
    } = ctx;
    update_bandwidth_row_range_for_mode(radio_panel, state, mode);
    // Suppress the bandwidth row's notify around `set_value` so
    // it doesn't redispatch a redundant `SetBandwidth` —
    // `tune_to_target` already sent the canonical command above.
    state.suppress_bandwidth_notify.set(true);
    bandwidth_row.set_value(bw_hz);
    state.suppress_bandwidth_notify.set(false);
    // Mode-specific control visibility (e.g. squelch / deemph rows
    // shown only in NFM/WFM) — must be poked explicitly because
    // the demod-dropdown notify only covers the dropdown's own
    // state.
    radio_panel.apply_demod_visibility(mode);
    // Status bar mirrors. Done last so a panic anywhere upstream
    // doesn't leave the indicator showing an optimistic value that
    // the DSP never received.
    status_bar.update_frequency(freq_f64);
    status_bar.update_demod(header::demod_mode_label(mode), bw_hz);
}

/// Apply the canonical tune-target dispatch — the 13 widget /
/// state / DSP mirror steps that bookmark recall, the satellite
/// play button, and auto-record-on-pass all need to perform when
/// retuning to a new (frequency, demod mode, bandwidth) target.
///
/// This is the single source of truth for the mirror sequence. Each
/// caller layers its own pre/post calls (e.g. bookmark recall calls
/// `restore_bookmark_profile` after; auto-record AOS layers
/// `force_audio_chain_off` + `set_playing(true)` before and
/// `dispatch_vfo_offset(0.0)` after). Per #509.
///
/// `reason` is the context string passed to
/// `ScannerForceDisable::trigger` so the scanner-disabled toast says
/// *why* (`"satellite tune"`, `"preset/bookmark selection"`, etc.).
///
/// Note: `SetDemodMode` is intentionally NOT sent directly here —
/// the demod dropdown's `notify::selected` handler dispatches it as
/// a side effect of `set_selected` below. This mirrors the existing
/// pattern that both call sites historically used.
#[allow(
    clippy::cast_precision_loss,
    reason = "freq_hz is bounded by the user-tunable RTL-SDR range (≤6 GHz) and \
              well below f64's 2^53 mantissa ceiling"
)]
fn tune_to_target(
    ctx: &TuneCtx,
    freq_hz: u64,
    mode: sdr_types::DemodMode,
    bw_hz: f64,
    reason: &'static str,
) {
    let TuneCtx {
        state,
        freq_selector,
        demod_dropdown,
        spectrum_handle,
        scanner_force_disable,
        ..
    } = ctx;
    // Verification logging: print the entire tune request as a
    // single structured line so a `grep tune_to_target ~/.cache/sdr-rs/sdr.log`
    // gives a complete picture of every retune (manual or auto-record).
    // Paired with the DSP-side `SetDemodMode`/`SetBandwidth` info logs,
    // this makes silent-fail demod regressions diagnosable from log
    // alone instead of needing a live debug session.
    tracing::info!(
        target: "tune_to_target",
        freq_hz = freq_hz,
        mode = ?mode,
        bw_hz = bw_hz,
        reason = reason,
        "TUNE_REQUEST"
    );
    scanner_force_disable.trigger(reason);
    let freq_f64 = freq_hz as f64;
    state.center_frequency.set(freq_f64);
    state.demod_mode.set(mode);
    state.send_dsp(UiToDsp::Tune(freq_f64));
    freq_selector.set_frequency(freq_hz);
    spectrum_handle.set_center_frequency(freq_f64);
    // **Order is load-bearing.** `set_selected` triggers the
    // dropdown's `notify::selected` handler which dispatches
    // `UiToDsp::SetDemodMode(mode)` synchronously. The DSP's
    // `SetDemodMode` handler resets `state.bandwidth` to the new
    // mode's `default_bandwidth` (e.g. 12.5 kHz for NFM) — see
    // `crates/sdr-core/src/controller.rs::SetDemodMode`. So the
    // explicit `SetBandwidth(bw_hz)` MUST land *after* the mode
    // switch, otherwise SetDemodMode silently overwrites it and
    // the satellite auto-record path (e.g. NOAA APT at 38 kHz)
    // ends up at NFM's 12.5 kHz default — a 12.5 kHz channel
    // filter throws away most of APT's ~38 kHz signal energy and
    // the decoder hears static. Per silent-fail investigation
    // following the NOAA 15 pass.
    if let Some(idx) = demod_selector::demod_mode_to_index(mode) {
        demod_dropdown.set_selected(idx);
    }
    state.send_dsp(UiToDsp::SetBandwidth(bw_hz));
    // Update the bandwidth row's allowed range for the new mode
    // BEFORE setting the value. The dropdown notify above only
    // queues `SetDemodMode` to the DSP; the range update only
    // happens when DSP echoes `DemodModeChanged` back, which is
    // async. Without this synchronous update, `set_value(bw_hz)`
    // below would clamp to the previous mode's range AND fire
    // its own notify that dispatches a wrong `SetBandwidth`,
    // overriding the correct `SetBandwidth(bw_hz)` we just sent
    // above. WFM→NFM retunes are the common failure case.
    // Per CR round 2 on PR #574.
    mirror_tune_ui(ctx, mode, bw_hz, freq_f64);
    // Companion to the TUNE_REQUEST log above — confirms the dispatch
    // path completed without panic. The DSP-side will emit its own
    // `SetDemodMode`/`SetBandwidth` info logs when it processes the
    // queued messages; cross-referencing those against this line
    // tells us whether requested == applied.
    tracing::info!(
        target: "tune_to_target",
        freq_hz = freq_hz,
        mode = ?mode,
        bw_hz = bw_hz,
        reason = reason,
        "TUNE_DISPATCH_COMPLETE"
    );
}

/// Build the main application window and return the shared
/// [`AppState`]. The caller (currently `app.rs::connect_activate`)
/// decides whether to call `window.present()` based on the
/// `--start-hidden` CLI flag and tray availability — per #512.
///
/// Returns `None` if the DSP engine failed to start; in that case
/// `app.quit()` has already been requested and the caller should
/// skip tray spawn and window present.
#[allow(clippy::too_many_lines)]
pub fn build_window(
    app: &adw::Application,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> Option<std::rc::Rc<crate::state::AppState>> {
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
            return None;
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
        return None;
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
        right_stack,
        panels,
        spectrum_handle: spectrum_handle_raw,
        status_bar,
        transcript_panel,
        general_panel: _general_panel,
    } = build_layout(&state, config);
    let spectrum_handle = Rc::new(spectrum_handle_raw);
    let sidebar_toggle = build_sidebar_toggle(&left_split_view);
    let layout::HeaderBarHandles {
        header,
        play_button,
        demod_dropdown,
        freq_selector,
        screenshot_button,
        rr_button,
        volume_button,
        favorites_handle,
    } = build_header_bar(&sidebar_toggle, &state);

    // Header bookmarks shortcut — a plain click-to-navigate button
    // (not a state toggle). Clicking it routes through the right
    // activity bar's Bookmarks button, which owns the
    // show/hide-and-stack-swap logic. Same pattern as `Ctrl+B` —
    // both go through the activity-bar handler for consistency.
    let bookmarks_toggle = gtk4::Button::builder()
        .icon_name("user-bookmarks-symbolic")
        .tooltip_text("Toggle bookmarks panel (Ctrl+B)")
        .build();
    bookmarks_toggle
        .update_property(&[gtk4::accessible::Property::Label("Toggle bookmarks panel")]);
    header.pack_end(&bookmarks_toggle);

    let right_bookmarks_btn_weak = right_activity_bar
        .buttons
        .get("bookmarks")
        .map(glib::object::ObjectExt::downgrade);
    bookmarks_toggle.connect_clicked(move |_| {
        if let Some(Some(btn)) = right_bookmarks_btn_weak
            .as_ref()
            .map(glib::WeakRef::upgrade)
        {
            btn.emit_clicked();
        }
    });

    // Header transcript shortcut — same click-to-navigate pattern.
    // Drives the right activity bar's Transcript button.
    let transcript_button = gtk4::Button::builder()
        .icon_name("document-page-setup-symbolic")
        .tooltip_text("Toggle transcript panel (Ctrl+Shift+1)")
        .build();
    transcript_button
        .update_property(&[gtk4::accessible::Property::Label("Toggle transcript panel")]);
    header.pack_end(&transcript_button);

    let right_transcript_btn_weak = right_activity_bar
        .buttons
        .get("transcript")
        .map(glib::object::ObjectExt::downgrade);
    transcript_button.connect_clicked(move |_| {
        if let Some(Some(btn)) = right_transcript_btn_weak
            .as_ref()
            .map(glib::WeakRef::upgrade)
        {
            btn.emit_clicked();
        }
    });

    // --- Activity-bar wiring ---
    //
    // Both bars use `wire_activity_bar_clicks`: click on a NEW icon
    // swaps the stack child and opens the panel; click on the
    // CURRENTLY-selected icon toggles the panel while keeping the
    // icon selected (design doc §4.2). `:checked` CSS renders the
    // accent tint via `ToggleButton::active`.
    //
    // Seed ordering (closes #428): load the persisted session,
    // apply to widgets BEFORE wiring the persistence notify
    // handlers, so the initial `set_active` / `set_visible_child` /
    // `set_show_sidebar` calls don't write the same value back
    // through the save path. Matches the "seed-then-wire" pattern
    // `connect_volume_persistence` uses.
    let session = sidebar::activity_bar::load_session(config);
    // Stack visible-child is set unconditionally so the right
    // panel is staged for the next open even when the sidebar
    // restores closed; the icon active state, by contrast, only
    // mirrors actual on-screen panel visibility (issue #518 —
    // an active icon over a hidden panel is misleading).
    left_stack.set_visible_child_name(session.left_selected);
    if session.left_open
        && let Some(btn) = left_activity_bar.buttons.get(session.left_selected)
    {
        btn.set_active(true);
    }
    left_split_view.set_show_sidebar(session.left_open);
    right_stack.set_visible_child_name(session.right_selected);
    if session.right_open
        && let Some(btn) = right_activity_bar.buttons.get(session.right_selected)
    {
        btn.set_active(true);
    }
    right_split_view.set_show_sidebar(session.right_open);

    // Restore saved pixel widths via a one-shot `notify::width`
    // handler — `sidebar_width_fraction` needs the split view's
    // live allocation to convert pixels → fraction, and the
    // allocation isn't settled until the widget has mapped. The
    // `applied` cell flips after the first non-zero width is seen
    // so subsequent width changes (window resize) leave the
    // sidebar's fraction alone.
    //
    // Fresh sessions (`width_px == None`) route the builder-time
    // default through the same post-allocation conversion so the
    // advertised default actually lands: the builder fraction was
    // derived from `DEFAULT_WIDTH = 1200`, but the right split
    // view's parent is the left split view's content area (already
    // narrower by the left sidebar's slice), so the fraction
    // evaluates against a smaller width and the resulting pixel
    // value undershoots the target. Routing defaults through
    // `apply_sidebar_width` with the allocated width fixes that.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    apply_sidebar_width(
        &left_split_view,
        session.left_width_px,
        LEFT_SIDEBAR_DEFAULT_WIDTH as u32,
    );
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    apply_sidebar_width(
        &right_split_view,
        session.right_width_px,
        RIGHT_SIDEBAR_DEFAULT_WIDTH as u32,
    );

    wire_activity_bar_clicks(&left_activity_bar, &left_stack, &left_split_view);
    wire_activity_bar_clicks(&right_activity_bar, &right_stack, &right_split_view);

    // Persistence — wire AFTER the seed so the initial sets don't
    // round-trip back through config. `save_*` writes are cheap
    // (`ConfigManager` batches); every activity click / panel
    // open-close writes.
    for (&name, btn) in &left_activity_bar.buttons {
        let config_weak = std::sync::Arc::clone(config);
        btn.connect_toggled(move |b| {
            if b.is_active() {
                sidebar::activity_bar::save_left_selected(&config_weak, name);
            }
        });
    }
    for (&name, btn) in &right_activity_bar.buttons {
        let config_weak = std::sync::Arc::clone(config);
        btn.connect_toggled(move |b| {
            if b.is_active() {
                sidebar::activity_bar::save_right_selected(&config_weak, name);
            }
        });
    }
    let config_left_open = std::sync::Arc::clone(config);
    left_split_view.connect_show_sidebar_notify(move |sv| {
        sidebar::activity_bar::save_left_open(&config_left_open, sv.shows_sidebar());
    });
    let config_right_open = std::sync::Arc::clone(config);
    right_split_view.connect_show_sidebar_notify(move |sv| {
        sidebar::activity_bar::save_right_open(&config_right_open, sv.shows_sidebar());
    });

    // Sync activity-bar icon active state to the sidebar's
    // `show-sidebar` property regardless of who toggled it
    // (header sidebar button, F9 shortcut, breakpoint collapse,
    // future programmatic callers). Per issue #518: the icon's
    // highlight should mean "this panel is on screen right now",
    // not "this slot will open next" — the click handler in
    // `wire_activity_bar_clicks` owns the user-driven case;
    // these notify handlers cover external toggles. The
    // active-icon update mirrors the click handler's resolution
    // (active iff sidebar shown AND this is the visible-stack
    // child).
    sync_activity_bar_to_sidebar_visibility(&left_split_view, &left_activity_bar, &left_stack);
    sync_activity_bar_to_sidebar_visibility(&right_split_view, &right_activity_bar, &right_stack);

    // Header sidebar toggle ↔ left split view `show-sidebar` sync.
    // Without this, clicking the currently-selected activity icon to
    // collapse the panel leaves the header toggle stuck in `active`;
    // the user's next header click then sets `show-sidebar=false`
    // again (no-op) instead of reopening the panel.
    let sidebar_toggle_weak = sidebar_toggle.downgrade();
    left_split_view.connect_show_sidebar_notify(move |sv| {
        if let Some(toggle) = sidebar_toggle_weak.upgrade()
            && toggle.is_active() != sv.shows_sidebar()
        {
            toggle.set_active(sv.shows_sidebar());
        }
    });
    // Seed the header sidebar toggle to match the restored left
    // panel state so F9's "is it open?" check starts accurate.
    sidebar_toggle.set_active(session.left_open);

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

    // Auto-pause waterfall when the window is minimized (#647).
    // Surface-level state listening: GTK4 exposes `is_active` /
    // `is_maximized` as window properties but minimization lives on
    // the underlying `GdkToplevel` surface. The surface isn't
    // realized until the window is mapped, so we wait for
    // `connect_realize` and then attach a state-notify on the
    // toplevel.
    //
    // **Best-effort.** `GdkToplevelState::MINIMIZED` is reported by
    // most major compositors (Mutter / KWin / sway / Hyprland) but
    // some tiling WMs don't emit it; on those, this handler simply
    // never fires and the user's Display-panel toggle remains the
    // only gate. Audio + recording paths are unaffected regardless
    // — the gate only runs through the FFT compute loop.
    {
        let state_min = Rc::clone(&state);
        let spectrum_min = Rc::clone(&spectrum_handle);
        window.connect_realize(move |w| {
            let Some(surface) = w.surface() else {
                tracing::debug!("waterfall auto-pause: window has no surface yet — skipping");
                return;
            };
            let Some(toplevel) = surface.dynamic_cast_ref::<gtk4::gdk::Toplevel>() else {
                tracing::debug!(
                    "waterfall auto-pause: surface is not a Toplevel — minimize \
                     detection unsupported on this platform"
                );
                return;
            };
            let state_inner = Rc::clone(&state_min);
            let spectrum_inner = Rc::clone(&spectrum_min);

            // Shared logic for both the initial seed and every
            // subsequent state change. `connect_state_notify` only
            // fires when the property changes — not at handler
            // attachment — so a window that's already minimized at
            // realize-time would otherwise leave the FFT gate open
            // until the next state transition. Per CR round 1 on
            // PR #653.
            let apply_minimized =
                |minimized: bool, state: &Rc<AppState>, spectrum: &spectrum::SpectrumHandle| {
                    let prev = state.waterfall_window_minimized.replace(minimized);
                    if prev == minimized {
                        return;
                    }
                    let resolved = state.resolve_and_send_waterfall_gate();
                    // Clear when the gate is going off so the first
                    // paint after restore doesn't show stale data while
                    // the FFT compute is suspended. Per #647.
                    if !resolved {
                        spectrum.clear_displays();
                    }
                    tracing::info!(
                        minimized,
                        "window minimize state changed — waterfall gate resolved (#647)"
                    );
                };

            // Initial seed: read whatever state the toplevel was in
            // at realize time. Most launches start non-minimized
            // (apply_minimized's `prev == minimized` early-return
            // makes this a no-op there); the unusual case is a
            // session-restored minimize from the WM, where this
            // line catches it before any state-notify would.
            apply_minimized(
                toplevel
                    .state()
                    .contains(gtk4::gdk::ToplevelState::MINIMIZED),
                &state_inner,
                &spectrum_inner,
            );

            toplevel.connect_state_notify(move |t| {
                apply_minimized(
                    t.state().contains(gtk4::gdk::ToplevelState::MINIMIZED),
                    &state_inner,
                    &spectrum_inner,
                );
            });
        });
    }

    // Wire `app.apt-open` (Ctrl+Shift+A) — opens the live APT
    // viewer window. Done here rather than in `app.rs::activate`
    // because the action's line-routing handler reads
    // `state.apt_viewer`, and `state` is owned by this window.
    {
        let app_for_provider = app.clone();
        let parent_provider: Rc<dyn Fn() -> Option<gtk4::Window>> =
            Rc::new(move || app_for_provider.windows().into_iter().next());
        // APT viewer wiring (`Ctrl+Shift+A` / `app.apt-open`).
        // NOAA-15 / NOAA-18 / NOAA-19 — the only operational
        // APT transmitters — were decommissioned in mid-2025
        // (see `KNOWN_SATELLITES` doc comment in `sdr-sat`), so
        // there's no satellite in our catalog with
        // `imaging_protocol: Some(Apt)` and the auto-record path
        // never fires APT. We deliberately keep the manual
        // viewer keycombo registered so the user can still tune
        // to a 137 MHz APT-band signal manually (e.g., to test
        // reception against an alternative APT source like a
        // future Cubesat or a SDR replay) and see decoded
        // imagery in the live viewer. Per user request during
        // M2-4 testing.
        crate::apt_viewer::connect_apt_action(app, &parent_provider, &state);
        // Same wiring for the LRPT viewer (`Ctrl+Shift+L` /
        // `app.lrpt-open`). Sharing the parent_provider closure
        // would be possible but each `connect_*_action` clones
        // it internally — passing twice keeps the call sites
        // symmetric. Per epic #469 task 7.5.
        crate::lrpt_viewer::connect_lrpt_action(app, &parent_provider, &state);
        crate::sstv_viewer::connect_sstv_action(app, &parent_provider, &state);
    }

    // Orbcomm wiring (`Ctrl+Shift+O` / `app.orbcomm-open`, epic #867):
    // the floating viewer is retired, so this now selects the docked
    // Orbcomm left activity instead of opening a window. Inline (not
    // via a `select_left_activity` helper) because `ActivityBar` isn't
    // `Clone`; capture the `buttons` map — its `ToggleButton`s are
    // cheap glib refs — plus the left stack and split view.
    {
        let action = gio::SimpleAction::new("orbcomm-open", None);
        let stack = left_stack.clone();
        let bar_buttons = left_activity_bar.buttons.clone();
        let split = left_split_view.clone();
        action.connect_activate(move |_, _| {
            for (n, btn) in &bar_buttons {
                btn.set_active(*n == "orbcomm");
            }
            stack.set_visible_child_name("orbcomm");
            split.set_show_sidebar(true);
        });
        app.add_action(&action);
        app.set_accels_for_action("app.orbcomm-open", &["<Ctrl><Shift>o"]);
    }

    // Set initial status bar values and mode-specific control visibility.
    if let Some(mode) = demod_selector::index_to_demod_mode(demod_dropdown.selected()) {
        let label = header::demod_mode_label(mode);
        panels.radio.apply_demod_visibility(mode);
        // Seed the bandwidth row's allowed range to the initial
        // demod (WFM by default — 50 kHz to 250 kHz). Without
        // this, the row's adjustment carries the panel-level
        // [100 Hz, 250 kHz] envelope until the user changes
        // mode for the first time, letting them dial out-of-range
        // values that the demod silently rejects (issue #505).
        update_bandwidth_row_range_for_mode(&panels.radio, &state, mode);
        // Status-bar bandwidth read AFTER the clamp so a saved
        // out-of-range value (e.g. an older config restoring an
        // 80 kHz NFM bandwidth) shows the corrected value rather
        // than the stale pre-clamp one. Per `CodeRabbit` round 1
        // on PR #548 (outside-diff item).
        let bw = panels.radio.bandwidth_row.value();
        status_bar.update_demod(label, bw);
    }
    #[allow(clippy::cast_precision_loss)]
    status_bar.update_frequency(freq_selector.frequency() as f64);

    setup_app_actions(app, &window, config, &rr_button, &state);

    // Wire transcript panel (separate from sidebar panels).
    let transcription_engine = connect_transcript_panel(
        &transcript_panel,
        &state,
        config,
        &panels.radio.squelch_enabled_row,
        &toast_overlay,
    );

    // On window close, signal the worker to stop without blocking.
    // Tracing line + backtrace on entry so we can pinpoint the
    // cascade when the close was unexpected (a recent user report
    // had the entire app exit after auto-record LOS — only the
    // viewer should have closed). Backtrace is `Backtrace::capture`
    // so it costs nothing unless `RUST_BACKTRACE=1` is set. Per
    // PR #558 auto-record-close investigation.
    //
    // Also unregister `app.tune-satellite` here. The action was
    // registered on the GApplication (because notification action
    // targets resolve against the app's action map), but its
    // closure captures window-owned widgets via `tune_to_satellite`.
    // A pre-pass notification that sits in the daemon and gets
    // clicked AFTER the window closes would fire the closure
    // against destroyed widgets — silent setter calls + a `Tune`
    // dispatch into a torn-down DSP channel. Removing the action
    // on close means that click is treated as "no such action"
    // rather than "tune via stale state". Per CR round 1 on PR #568.
    let app_for_close = app.clone();
    let state_for_close = std::rc::Rc::clone(&state);
    let config_for_close = std::sync::Arc::clone(config);
    let toast_overlay_close = toast_overlay.downgrade();
    let transcription_engine_close = std::rc::Rc::clone(&transcription_engine);
    // The closure receives `&window` as its parameter — use that
    // instead of capturing a strong clone, otherwise the closure
    // (which the window owns) would hold a strong ref back to its
    // own window and form a retention cycle. Per CR round 1 on PR
    // #572.
    window.connect_close_request(move |w| {
        let bt = std::backtrace::Backtrace::capture();
        tracing::info!(backtrace = ?bt, "main window close-request fired");

        // Close-to-tray: hide instead of destroy if both the user
        // toggle is on AND the tray is actually available. If the
        // tray failed to spawn, MUST proceed-to-close — otherwise
        // the user is stuck with an invisible process. Per #512.
        if state_for_close.close_to_tray.get() && state_for_close.tray_available.get() {
            w.set_visible(false);
            // First-close toast: fire exactly once per fresh config.
            if !state_for_close.tray_first_close_seen.get() {
                state_for_close.tray_first_close_seen.set(true);
                config_for_close.write(|v| {
                    v[crate::preferences::general_page::KEY_TRAY_FIRST_CLOSE_SEEN] =
                        serde_json::json!(true);
                });
                if let Some(overlay) = toast_overlay_close.upgrade() {
                    let toast = adw::Toast::builder()
                        .title("App still running in tray — right-click tray icon and choose Quit, or disable in Settings → General → Behavior")
                        .timeout(8)
                        .build();
                    overlay.add_toast(toast);
                }
            }
            return glib::Propagation::Stop;
        }

        // Real close — tray failed to spawn (or the user disabled
        // close-to-tray). Drop the application hold guard so the
        // GApplication can release its reference and exit naturally
        // once the window destroys. Without this, the window would
        // close but `app.hold()` from connect_startup would keep
        // the process alive headless with no way to interact.
        // Per CR round 1 on PR #572.
        let _ = state_for_close.app_hold_guard.borrow_mut().take();
        app_for_close.remove_action(crate::notify::TUNE_SATELLITE_ACTION);
        transcription_engine_close.borrow_mut().shutdown_nonblocking();
        // Same synchronous persist as the tray quit path (#762).
        if let Err(e) = config_for_close.flush() {
            tracing::warn!("config flush at close failed: {e}");
        }
        glib::Propagation::Proceed
    });

    // --- tray-* GIO actions (per #512 close-to-tray) ---
    //
    // These are activated by `app.rs::spawn_tray_and_route` which
    // forwards `sdr_tray::TrayEvent`s from the tray worker thread
    // to the GTK main loop via `app.activate_action(...)`.
    //
    // Registered here (rather than in `setup_app_actions`) because
    // the `tray-quit` handler captures `transcription_engine`, which
    // is only constructed below the `setup_app_actions(...)` call.

    let tray_show = gio::SimpleAction::new("tray-show", None);
    tray_show.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            window.present();
        }
    ));
    app.add_action(&tray_show);

    let tray_hide = gio::SimpleAction::new("tray-hide", None);
    tray_hide.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            window.set_visible(false);
        }
    ));
    app.add_action(&tray_hide);

    let tray_toggle = gio::SimpleAction::new("tray-toggle", None);
    tray_toggle.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            if window.is_visible() {
                window.set_visible(false);
            } else {
                window.present();
            }
        }
    ));
    app.add_action(&tray_toggle);

    // tray-quit: confirm if recording is active, otherwise tear down
    // immediately. The teardown sequence: stop the tray worker thread,
    // drop the application hold guard (which Drop-fires `release()`),
    // remove the tune-satellite action (its closure captures
    // window-owned widgets), shut down transcription, destroy the
    // window. Once the hold guard is dropped and the window is gone,
    // the GApplication's reference count drops to zero and the main
    // loop exits naturally.
    let tray_quit = gio::SimpleAction::new("tray-quit", None);
    let app_for_quit = app.clone();
    let state_for_quit = Rc::clone(&state);
    let window_for_quit = window.clone();
    let transcription_for_quit = Rc::clone(&transcription_engine);
    let config_for_quit = std::sync::Arc::clone(config);
    tray_quit.connect_activate(move |_, _| {
        if state_for_quit.is_recording() {
            // Confirmation modal. WM-close (clicking the dialog's X)
            // maps to "cancel" via `set_close_response`.
            let dialog = adw::MessageDialog::builder()
                .transient_for(&window_for_quit)
                .modal(true)
                .heading("Recording in progress")
                .body("Quit anyway? The current pass will not be saved.")
                .build();
            dialog.add_response("cancel", "_Cancel");
            dialog.add_response("quit", "_Quit anyway");
            dialog.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let app_for_response = app_for_quit.clone();
            let state_for_response = Rc::clone(&state_for_quit);
            let window_for_response = window_for_quit.clone();
            let transcription_for_response = Rc::clone(&transcription_for_quit);
            let config_for_response = std::sync::Arc::clone(&config_for_quit);
            dialog.connect_response(None, move |dlg, response| {
                if response == "quit" {
                    perform_real_quit(
                        &app_for_response,
                        &state_for_response,
                        &window_for_response,
                        &transcription_for_response,
                        &config_for_response,
                    );
                }
                dlg.close();
            });
            dialog.present();
            return;
        }
        perform_real_quit(
            &app_for_quit,
            &state_for_quit,
            &window_for_quit,
            &transcription_for_quit,
            &config_for_quit,
        );
    });
    app.add_action(&tray_quit);

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

    // Header play/stop button as a set-and-forget hook for any
    // wiring that needs to start or stop the radio without bypassing
    // the visible toggle (currently auto-record-on-pass; same idiom
    // would suit any future "schedule the radio on" feature). Going
    // through `set_active` reuses the existing
    // `play_button.connect_toggled` handler — the single place that
    // updates `state.is_running`, sends `UiToDsp::Start` / `Stop`,
    // and swaps the icon — so the DSP, `AppState`, and header
    // button stay aligned. `set_active` is idempotent: GTK only
    // emits `toggled` on a real state change, so a redundant
    // `set_playing(true)` while the radio is already running is a
    // no-op (no duplicate Start dispatch).
    let set_playing: Rc<dyn Fn(bool)> = {
        let play_btn = play_button.clone();
        Rc::new(move |should_play| {
            play_btn.set_active(should_play);
        })
    };

    let tune_ctx = TuneCtx {
        state: Rc::clone(&state),
        freq_selector: freq_selector.clone(),
        demod_dropdown: demod_dropdown.clone(),
        spectrum_handle: Rc::clone(&spectrum_handle),
        scanner_force_disable: Rc::clone(&scanner_force_disable),
        bandwidth_row: panels.radio.bandwidth_row.clone(),
        radio_panel: panels.radio.clone(),
        status_bar: Rc::clone(&status_bar_demod),
    };
    connect_sidebar_panels(
        app,
        &panels,
        &tune_ctx,
        &toast_overlay,
        config,
        &favorites_handle,
        &volume_button,
        &set_playing,
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
    //
    // TODO(#521 follow-up): wire additive `user_reference_offset` into
    // `DopplerTracker` here so a user drag during an active Doppler-
    // tracked pass becomes a per-pass fine-tune instead of getting
    // overwritten on the next 4 Hz recompute. Deferred from Task 7
    // because the spectrum widget's drag handler dispatches
    // `UiToDsp::SetVfoOffset` directly via its own `dsp_tx` clone
    // (bypassing `AppState::send_dsp`), so threading the tracker
    // here would require either hoisting `Rc<RefCell<DopplerTracker>>`
    // onto `AppState` or routing the spectrum drag through the
    // wiring layer. v1 behaviour: user drag wins for the current
    // tick, then the next 4 Hz Doppler recompute reasserts —
    // acceptable per spec §4 note.
    let status_bar_for_vfo = Rc::clone(&status_bar_demod);
    let state_for_vfo = Rc::clone(&state);
    let fs_for_vfo = freq_selector.clone();
    spectrum_handle.connect_vfo_offset_changed(move |offset_hz| {
        // Single source of truth for the actual VFO offset DSP
        // currently holds. Fires from BOTH the DSP echo
        // (`DspToUi::VfoOffsetChanged`) and direct user-drag
        // dispatches, so any path that mutates the VFO offset
        // (auto-record AOS reset, spectrum drag, our own
        // Doppler ticks, click-to-tune, etc.) keeps this in
        // sync. Doppler's rate-limit gate reads from here so it
        // never compares against a stale local baseline. Per CR
        // round 7 on PR #554.
        state_for_vfo.last_dispatched_vfo_offset_hz.set(offset_hz);
        let center = state_for_vfo.center_frequency.get();
        let tuned = center + offset_hz;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let tuned_u64 = tuned.max(0.0) as u64;
        fs_for_vfo.set_frequency(tuned_u64);
        status_bar_for_vfo.update_frequency(tuned);
    });

    // Scanner-locked click-to-tune (#563). When the scanner-axis
    // lock is engaged and the user clicks the spectrum, the
    // gesture handler calls this callback with the absolute
    // frequency under the click. We force-disable the scanner
    // (which tears down the lock via the master switch's
    // `connect_active_notify`), then dispatch a normal manual
    // tune via the shared `apply_manual_tune` helper — same
    // end shape as the freq-selector path below. Locked-click
    // additionally syncs the freq-selector widget (the click
    // came from the spectrum, not the selector, so the
    // selector display needs catching up); the freq-selector
    // handler skips that step because the value originated
    // there. Per `CodeRabbit` round 1 on PR #565.
    let state_for_locked_click = Rc::clone(&state);
    let status_bar_for_locked_click = Rc::clone(&status_bar_demod);
    let spectrum_for_locked_click = Rc::clone(&spectrum_handle);
    let freq_selector_for_locked_click = freq_selector.clone();
    let force_disable_for_locked_click = Rc::clone(&scanner_force_disable);
    let radio_for_locked_click = panels.radio.clone();
    spectrum_handle.connect_locked_click_to_tune(move |freq_hz| {
        tracing::debug!(
            freq_hz,
            "scanner-locked click-to-tune: force-disable + tune"
        );
        apply_manual_tune(
            freq_hz,
            "scanner spectrum click",
            &state_for_locked_click,
            &force_disable_for_locked_click,
            &status_bar_for_locked_click,
            &spectrum_for_locked_click,
            &radio_for_locked_click,
        );
        // The click originated on the spectrum — the freq
        // selector widget didn't move, so push the new value
        // into it manually. (The freq-selector handler skips
        // this step because the value came FROM the selector.)
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let freq_u64 = freq_hz.max(0.0) as u64;
        freq_selector_for_locked_click.set_frequency(freq_u64);
    });

    let state_freq = Rc::clone(&state);
    let status_bar_for_freq = Rc::clone(&status_bar_demod);
    let spectrum_for_freq = Rc::clone(&spectrum_handle);
    let force_disable_freq = Rc::clone(&scanner_force_disable);
    let radio_for_freq = panels.radio.clone();
    freq_selector.connect_frequency_changed(move |freq| {
        tracing::debug!(frequency_hz = freq, "frequency changed");
        #[allow(clippy::cast_precision_loss)]
        let freq_f64 = freq as f64;
        apply_manual_tune(
            freq_f64,
            "manual tune",
            &state_freq,
            &force_disable_freq,
            &status_bar_for_freq,
            &spectrum_for_freq,
            &radio_for_freq,
        );
        // No `freq_selector.set_frequency` here — the value
        // ORIGINATED in the selector, calling set on it would
        // be a no-op at best and a feedback loop at worst.
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
        state_for_vfo_reset.dispatch_vfo_offset(0.0);
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
    let sample_rate_row_for_dsp = panels.source.sample_rate_row.clone();
    let decimation_row_for_dsp = panels.source.decimation_row.clone();
    let volume_button_for_dsp = volume_button.clone();
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
    let dsp_event_ctx = DspEventCtx {
        spectrum_handle: spectrum_handle.clone(),
        play_button_weak: play_button_weak.clone(),
        state: state_rx.clone(),
        toast_overlay_weak: toast_overlay_weak.clone(),
        status_bar: status_bar_demod.clone(),
        gain_row: gain_row_for_dsp.clone(),
        record_audio_row: record_audio_for_dsp.clone(),
        record_iq_row: record_iq_for_dsp.clone(),
        radio_panel: radio_panel_for_dsp.clone(),
        scanner_panel: scanner_panel_for_dsp.clone(),
        freq_selector: freq_selector_for_dsp.clone(),
        demod_dropdown: demod_dropdown_for_dsp.clone(),
        sample_rate_row: sample_rate_row_for_dsp.clone(),
        airspy_device_row: panels.source.airspy_device_row.clone(),
        config: std::sync::Arc::clone(config),
        decimation_row: decimation_row_for_dsp.clone(),
        volume_button: volume_button_for_dsp.clone(),
        rtl_tcp_status_row_weak: rtl_tcp_status_row_weak.clone(),
        rtl_tcp_disconnect_button_weak: rtl_tcp_disconnect_button_weak.clone(),
        rtl_tcp_retry_button_weak: rtl_tcp_retry_button_weak.clone(),
        rtl_tcp_role_row_weak: rtl_tcp_role_row_weak.clone(),
        rtl_tcp_auth_key_row_weak: rtl_tcp_auth_key_row_weak.clone(),
        rtl_tcp_hostname_row_weak: rtl_tcp_hostname_row_weak.clone(),
        rtl_tcp_port_row_weak: rtl_tcp_port_row_weak.clone(),
        pending_controller_busy_toasts: pending_controller_busy_toasts.clone(),
        network_sink_status_row_weak: network_sink_status_row_weak.clone(),
        transcription_enable_row: transcription_enable_for_dsp.clone(),
        #[cfg(feature = "sherpa")]
        auto_break_row: auto_break_row_for_dsp.clone(),
        #[cfg(feature = "sherpa")]
        auto_break_min_open_row: auto_break_min_open_row_for_dsp.clone(),
        #[cfg(feature = "sherpa")]
        auto_break_tail_row: auto_break_tail_row_for_dsp.clone(),
        #[cfg(feature = "sherpa")]
        auto_break_min_segment_row: auto_break_min_segment_row_for_dsp.clone(),
        #[cfg(feature = "sherpa")]
        model_row: model_row_for_dsp.clone(),
    };

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
                    handle_dsp_message(msg, &dsp_event_ctx);
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

    Some(state)
}

// "Tune to satellite" closure used by the Satellites panel's
// per-row play buttons. Mirrors the bookmark-recall dance in
// `connect_navigation_panel` end-to-end: forces the scanner
// off, updates local `AppState`, sends `Tune` + `SetBandwidth`
// to the DSP, and pokes every UI widget / status indicator
// that mirrors the radio's tuning state — spectrum centre
// line, demod dropdown, bandwidth SpinRow, status bar
// frequency / demod-mode label, and the radio panel's mode-
// specific control visibility. The dropdown's
// `selected-notify` and the spin row's `value-notify`
// callbacks fire `SetDemodMode` / a redundant `SetBandwidth`
// themselves — idempotent at the DSP, cheaper than threading
// a suppress flag through here.
fn build_tune_to_satellite(tune_ctx: &TuneCtx) -> Rc<TuneFn> {
    let ctx_t = tune_ctx.clone();
    Rc::new(move |freq_hz, mode, bw_hz| {
        tune_to_target(&ctx_t, freq_hz, mode, f64::from(bw_hz), "satellite tune");
    })
}

// Register `app.tune-satellite` so the "Tune" button on a #510
// pre-pass desktop notification can route back to the same
// tune closure the panel's per-row play buttons use. Action
// target is the satellite's NORAD id (`u32`); the handler
// looks the entry up in `KNOWN_SATELLITES` for downlink /
// demod / bandwidth.
fn register_tune_satellite_action(app: &adw::Application, tune_to_satellite: &Rc<TuneFn>) {
    let tune_for_action = Rc::clone(tune_to_satellite);
    let action = gio::SimpleAction::new(
        crate::notify::TUNE_SATELLITE_ACTION,
        Some(glib::VariantTy::UINT32),
    );
    action.connect_activate(move |_, param| {
        let Some(norad_id) = param.and_then(glib::Variant::get::<u32>) else {
            tracing::warn!("tune-satellite action fired without a u32 target");
            return;
        };
        let Some(known) = sdr_sat::KNOWN_SATELLITES
            .iter()
            .find(|s| s.norad_id == norad_id)
        else {
            tracing::warn!(
                norad_id,
                "tune-satellite action target not in KNOWN_SATELLITES",
            );
            return;
        };
        tune_for_action(known.downlink_hz, known.demod_mode, known.bandwidth_hz);
    });
    app.add_action(&action);
}

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
fn wire_bookmark_mutation_refresh(
    panels: &SidebarPanels,
    tune_ctx: &TuneCtx,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let state = &tune_ctx.state;
    let spectrum_handle = &tune_ctx.spectrum_handle;
    let bookmarks_weak = Rc::downgrade(&panels.bookmarks);
    let state_for_mutated = Rc::clone(state);
    let config_for_mutated = std::sync::Arc::clone(config);
    let scanner_switch_for_mutated = panels.scanner.master_switch.clone();
    let scanner_panel_for_mutated = panels.scanner.clone();
    let spectrum_for_mutated = Rc::clone(spectrum_handle);
    let display_axis_row_for_mutated = panels.display.scanner_axis_row.clone();
    panels.bookmarks.connect_mutated(move || {
        let Some(bookmarks) = bookmarks_weak.upgrade() else {
            return;
        };
        sidebar::navigation_panel::project_and_push_scanner_channels(
            &bookmarks.bookmarks.borrow(),
            &state_for_mutated,
            &config_for_mutated,
        );
        // Mid-scan scan-flag toggle / add / delete: recompute
        // the X-axis envelope while the lock is engaged so the
        // axis tracks the new range without requiring a master-
        // switch off-and-on. No-op when the scanner isn't on
        // — the lock is already disengaged. Per issue #516
        // smoke feedback.
        if scanner_switch_for_mutated.is_active() {
            let active_key = state_for_mutated.scanner_active_key.borrow().clone();
            let outcome = refresh_scanner_axis_lock(
                &bookmarks.bookmarks.borrow(),
                &config_for_mutated,
                &spectrum_for_mutated,
                &display_axis_row_for_mutated,
                active_key.as_ref(),
            );
            // If the helper dropped the previously-active
            // channel (user disabled `scan_enabled` on the
            // active bookmark or deleted it), clear the
            // scanner-sidebar surfaces so the displayed
            // channel name + lockout-row visibility match.
            // Without this, the spectrum highlight clears
            // immediately but the sidebar stays stale until
            // the next `ScannerActiveChannelChanged` event.
            // Per `CodeRabbit` round 5 on PR #562.
            if outcome == ScannerAxisRefreshOutcome::ActiveChannelDropped {
                clear_scanner_active_channel_ui(&scanner_panel_for_mutated, &state_for_mutated);
            }
        }
    });
}

/// Load the shared favorites map (stable `hostname:port` key → rich
/// `FavoriteEntry`) once, so the source panel's role picker and the
/// discovery re-announce path mutate the SAME `Rc<RefCell<..>>` and
/// persistence stays consistent. Per `CodeRabbit` round 8 on PR #408.
#[allow(clippy::type_complexity)]
fn load_favorites_map(
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> Rc<RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>> {
    Rc::new(RefCell::new(
        crate::sidebar::source_panel::load_favorites(config)
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect(),
    ))
}

/// Connect all sidebar panel controls to dispatch `UiToDsp` commands.
#[allow(
    clippy::too_many_arguments,
    reason = "top-level wiring fan-out: each arg is a distinct owned subsystem handle \
              (app, panels, tune ctx, toasts, config, favorites, volume, play state)"
)]
fn connect_sidebar_panels(
    app: &adw::Application,
    panels: &SidebarPanels,
    tune_ctx: &TuneCtx,
    toast_overlay: &adw::ToastOverlay,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites_header: &FavoritesHeaderHandle,
    volume_button: &gtk4::ScaleButton,
    set_playing: &Rc<dyn Fn(bool)>,
) {
    let state = &tune_ctx.state;
    let spectrum_handle = &tune_ctx.spectrum_handle;
    let scanner_force_disable = &tune_ctx.scanner_force_disable;
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
    let favorites = load_favorites_map(config);

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
    connect_display_panel(panels, state, spectrum_handle, config);
    connect_audio_panel(panels, state);
    connect_volume_persistence(panels, state, config, volume_button);
    connect_distance_estimator_persistence(panels, config);
    connect_scanner_panel(panels, state, config, spectrum_handle);
    let tune_to_satellite = build_tune_to_satellite(tune_ctx);
    register_tune_satellite_action(app, &tune_to_satellite);

    connect_satellites_panel(
        panels,
        config,
        tune_ctx,
        toast_overlay,
        &tune_to_satellite,
        set_playing,
    );
    connect_aviation_panel(&panels.aviation, state, config, toast_overlay);
    crate::sidebar::orbcomm_panel::connect_orbcomm_panel(panels, state);
    // Transcript panel is wired separately (not in SidebarPanels).
    connect_navigation_panel(panels, tune_ctx, volume_button);

    wire_bookmark_mutation_refresh(panels, tune_ctx, config);
}

/// Register application-level actions (Preferences, About, Quit).
fn setup_app_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    rr_button: &gtk4::Button,
    state: &Rc<AppState>,
) {
    // Quit action — Ctrl+Q and the menu's "Quit" entry. Routes
    // through `tray-quit` (registered in `build_window`) so explicit
    // quit goes through the same recording-confirmation modal +
    // perform_real_quit teardown as the tray menu's Quit. Without
    // this redirect, `window.close()` would hit the close-request
    // handler and get swallowed into "hide to tray" — the user's
    // Ctrl+Q would silently hide instead of exit. Per CR round 1
    // on PR #572.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| {
            app.activate_action("tray-quit", None);
        }
    ));
    app.add_action(&quit_action);
    app.set_accels_for_action("app.quit", &["<Ctrl>q"]);

    // Preferences action
    let prefs_action = gio::SimpleAction::new("preferences", None);
    let config_for_prefs = std::sync::Arc::clone(config);
    let rr_button_prefs = rr_button.clone();
    let state_for_prefs = Rc::clone(state);
    prefs_action.connect_activate(glib::clone!(
        #[weak]
        window,
        move |_, _| {
            let prefs_window = crate::preferences::build_preferences_window(
                &window,
                &config_for_prefs,
                &state_for_prefs,
            );
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

/// Tear down the application after a tray-Quit confirmation. Joins
/// the tray worker thread, drops the `app.hold()` guard so the
/// `GApplication` can release naturally, removes the notification
/// action whose closure captures window-owned widgets, shuts down
/// the transcription engine, and destroys the window. Per #512.
fn perform_real_quit(
    app: &adw::Application,
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    transcription_engine: &Rc<RefCell<sdr_transcription::TranscriptionEngine>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    tracing::info!("tray-quit: shutting down");
    // Persist synchronously: the auto-save handle's `Drop` only runs
    // once every `Arc` clone captured by GTK closures and timers has
    // died, which is too late for a setting changed < 1 s ago (#762).
    if let Err(e) = config.flush() {
        tracing::warn!("config flush at quit failed: {e}");
    }
    // Join the tray worker thread first so its callbacks can't fire
    // against torn-down state during the rest of this teardown.
    if let Some(mut handle) = state.tray_handle.borrow_mut().take() {
        handle.shutdown();
    }
    // Drop the app-hold guard. Its `Drop` impl calls
    // `g_application_release()`, which decrements the application's
    // reference count. Combined with the upcoming `window.destroy()`
    // (no other windows alive), this lets the GApplication's main
    // loop exit naturally on the next iteration.
    let _ = state.app_hold_guard.borrow_mut().take();
    // Same teardown the original close-request handler did before
    // close-to-tray took it over.
    app.remove_action(crate::notify::TUNE_SATELLITE_ACTION);
    transcription_engine.borrow_mut().shutdown_nonblocking();
    window.destroy();
}
