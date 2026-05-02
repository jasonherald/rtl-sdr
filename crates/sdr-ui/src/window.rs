//! Main window construction — header bar, split view, breakpoints, DSP bridge.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::ObjectSubclassIsExt;
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
/// Sidebar collapse breakpoint width in pixels.
const SIDEBAR_BREAKPOINT_PX: f64 = 800.0;

/// FFT sizes — re-exported from display panel (single source of truth).
use crate::sidebar::display_panel::FFT_SIZES;
#[cfg(feature = "sherpa")]
use crate::sidebar::transcript_panel::DISPLAY_MODE_FINAL_IDX;

use crate::sidebar::source_panel::DECIMATION_FACTORS;

/// Interval in milliseconds for polling the DSP→UI channel.
const DSP_POLL_INTERVAL_MS: u64 = 16;

/// Toast display time (seconds) for scanner "force-disable" notices.
const SCANNER_TOAST_TIMEOUT_SECS: u32 = 3;

/// Cadence of the Satellites panel's countdown ticker — 1 line/sec
/// is the smallest interval that produces a visible change in the
/// pass-row title (which renders to 1-minute granularity for far
/// passes and to seconds only inside the "starting now" window).
/// Smaller would burn cycles for no visible benefit.
const SATELLITES_COUNTDOWN_TICK: Duration = Duration::from_secs(1);

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

/// Outcome of `save_sstv_batches` reported back to the GTK main
/// thread. Used by the `RecorderAction::SaveSstvPass` arm. Per CR
/// round 6 #21 on PR #599.
struct SstvSaveOutcome {
    /// User-facing toast text summarising the per-batch save
    /// results.
    message: String,
    /// `true` iff every image in the *current* pass batch saved
    /// cleanly. Drives the compare-and-clear of
    /// `state.sstv_completed_images` and viewer auto-close.
    current_ok: bool,
    /// Batches that still need to be saved on a future attempt:
    /// any prior pending batch where at least one image failed,
    /// plus the current batch if it had any failures (re-keyed
    /// to the *current* `dir`). On the next `SaveSstvPass` each
    /// retained batch is retried against its own preserved `dir`
    /// — never the new pass's directory.
    retained: Vec<PendingSstvExport>,
}

/// Worker-thread save routine: iterate prior failed batches first
/// (each into its own original `dir`), then save the current
/// pass's images into `current_dir`. Retain any batch that had
/// any per-image failures so the next `SaveSstvPass` can retry it
/// in its own folder. Per CR round 6 #21 on PR #599.
fn save_sstv_batches(
    pending_batches: Vec<PendingSstvExport>,
    current_images: Vec<sdr_radio::sstv_image::CompletedSstvImage>,
    current_dir: std::path::PathBuf,
) -> SstvSaveOutcome {
    let mut retained: Vec<PendingSstvExport> = Vec::new();
    let mut total_saved = 0_usize;
    let mut total_failed = 0_usize;
    let mut error_summary: Vec<String> = Vec::new();

    // Save each previously-retained batch to its own directory.
    for batch in pending_batches {
        let (saved, errs) = save_sstv_batch(&batch.dir, &batch.images);
        total_saved += saved;
        let failed = errs.len();
        total_failed += failed;
        if failed > 0 {
            error_summary.extend(errs.iter().map(|e| format!("{}: {e}", batch.dir.display())));
            retained.push(batch);
        }
    }

    // Save the current pass.
    let current_dir_display = current_dir.display().to_string();
    let current_image_count = current_images.len();
    let (cur_saved, cur_errs) = save_sstv_batch(&current_dir, &current_images);
    total_saved += cur_saved;
    total_failed += cur_errs.len();
    let current_ok = cur_errs.is_empty() && (cur_saved > 0 || current_image_count == 0);
    if !cur_errs.is_empty() {
        error_summary.extend(
            cur_errs
                .iter()
                .map(|e| format!("{current_dir_display}: {e}")),
        );
        retained.push(PendingSstvExport {
            dir: current_dir,
            images: current_images,
        });
    }

    let message = if total_saved == 0 && total_failed == 0 {
        // No prior pending batches and the current pass produced
        // no images — same warn-and-skip semantics as before.
        tracing::warn!(
            "auto-record SaveSstvPass but no SSTV images were decoded — pass produced no imagery",
        );
        format!(
            "Pass complete, but no SSTV images decoded — nothing saved to {current_dir_display}"
        )
    } else if total_failed == 0 {
        format!("Pass complete — {total_saved} SSTV image(s) saved")
    } else {
        format!(
            "Pass complete — {total_saved} image(s) saved, {total_failed} failed: {}",
            error_summary.join("; ")
        )
    };

    SstvSaveOutcome {
        message,
        current_ok,
        retained,
    }
}

/// Save a single batch of SSTV images into `dir`. Returns
/// `(saved_count, per_image_error_messages)`. A directory-creation
/// failure surfaces as one error covering the whole batch; image
/// write failures surface per image.
fn save_sstv_batch(
    dir: &std::path::Path,
    images: &[sdr_radio::sstv_image::CompletedSstvImage],
) -> (usize, Vec<String>) {
    if images.is_empty() {
        return (0, Vec::new());
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("auto-record SaveSstvPass: failed to create directory {dir:?}: {e}",);
        return (0, vec![format!("create_dir_all failed: {e}")]);
    }
    let mut saved = 0_usize;
    let mut errors: Vec<String> = Vec::new();
    for (idx, img) in images.iter().enumerate() {
        let path = dir.join(format!("img{idx}.png"));
        match crate::sstv_viewer::write_sstv_rgb_png(&path, &img.pixels, img.width, img.height) {
            Ok(()) => {
                tracing::info!(
                    ?path,
                    width = img.width,
                    height = img.height,
                    "auto-record SSTV image saved",
                );
                saved += 1;
            }
            Err(e) => {
                tracing::warn!("auto-record SSTV export img{idx} to {path:?} failed: {e}",);
                errors.push(format!("img{idx}: {e}"));
            }
        }
    }
    (saved, errors)
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
    clippy::too_many_arguments,
    reason = "consolidating the duplicated 13-step mirror sequence into a single \
              source of truth requires every captured widget / handle the two \
              call sites used to capture; threading them through is the point"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "freq_hz is bounded by the user-tunable RTL-SDR range (≤6 GHz) and \
              well below f64's 2^53 mantissa ceiling"
)]
fn tune_to_target(
    state: &Rc<AppState>,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    scanner_force_disable: &ScannerForceDisable,
    bandwidth_row: &adw::SpinRow,
    radio_panel: &sidebar::radio_panel::RadioPanel,
    status_bar: &Rc<StatusBar>,
    freq_hz: u64,
    mode: sdr_types::DemodMode,
    bw_hz: f64,
    reason: &'static str,
) {
    scanner_force_disable.trigger(reason);
    let freq_f64 = freq_hz as f64;
    state.center_frequency.set(freq_f64);
    state.demod_mode.set(mode);
    state.send_dsp(UiToDsp::Tune(freq_f64));
    state.send_dsp(UiToDsp::SetBandwidth(bw_hz));
    freq_selector.set_frequency(freq_hz);
    spectrum_handle.set_center_frequency(freq_f64);
    if let Some(idx) = demod_selector::demod_mode_to_index(mode) {
        demod_dropdown.set_selected(idx);
    }
    // Update the bandwidth row's allowed range for the new mode
    // BEFORE setting the value. The dropdown notify above only
    // queues `SetDemodMode` to the DSP; the range update only
    // happens when DSP echoes `DemodModeChanged` back, which is
    // async. Without this synchronous update, `set_value(bw_hz)`
    // below would clamp to the previous mode's range AND fire
    // its own notify that dispatches a wrong `SetBandwidth`,
    // overriding the correct `SetBandwidth(bw_hz)` we just sent
    // at line 202. WFM→NFM retunes are the common failure case.
    // Per CR round 2 on PR #574.
    update_bandwidth_row_range_for_mode(radio_panel, state, mode);
    // Suppress the bandwidth row's notify around `set_value` so
    // it doesn't redispatch a redundant `SetBandwidth` —
    // `tune_to_target` already sent the canonical command at
    // line 202 above.
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
    let (
        header,
        play_button,
        demod_dropdown,
        freq_selector,
        screenshot_button,
        rr_button,
        volume_button,
        favorites_handle,
    ) = build_header_bar(&sidebar_toggle, &state);

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

    wire_activity_bar_clicks(
        &left_activity_bar,
        &left_stack,
        &left_split_view,
        session.left_selected,
    );
    wire_activity_bar_clicks(
        &right_activity_bar,
        &right_stack,
        &right_split_view,
        session.right_selected,
    );

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

    // Wire `app.apt-open` (Ctrl+Shift+A) — opens the live APT
    // viewer window. Done here rather than in `app.rs::activate`
    // because the action's line-routing handler reads
    // `state.apt_viewer`, and `state` is owned by this window.
    {
        let app_for_provider = app.clone();
        let parent_provider: Rc<dyn Fn() -> Option<gtk4::Window>> =
            Rc::new(move || app_for_provider.windows().into_iter().next());
        crate::apt_viewer::connect_apt_action(app, &parent_provider, &state);
        // Same wiring for the LRPT viewer (`Ctrl+Shift+L` /
        // `app.lrpt-open`). Sharing the parent_provider closure
        // would be possible but each `connect_*_action` clones
        // it internally — passing twice keeps the call sites
        // symmetric. Per epic #469 task 7.5.
        crate::lrpt_viewer::connect_lrpt_action(app, &parent_provider, &state);
        crate::sstv_viewer::connect_sstv_action(app, &parent_provider, &state);
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
            dialog.connect_response(None, move |dlg, response| {
                if response == "quit" {
                    perform_real_quit(
                        &app_for_response,
                        &state_for_response,
                        &window_for_response,
                        &transcription_for_response,
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

    connect_sidebar_panels(
        app,
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
                        &sample_rate_row_for_dsp,
                        &decimation_row_for_dsp,
                        &volume_button_for_dsp,
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

    Some(state)
}

/// Outcome of a [`refresh_scanner_axis_lock`] call. Lets the
/// bookmark-mutation caller (which has access to the scanner
/// sidebar widgets) keep `state.scanner_active_key`,
/// `active_channel_row`, and `lockout_row` in sync when a
/// refresh drops the previously-active channel from the
/// rotation — otherwise the sidebar would still show the old
/// channel name (and the lockout button stays visible) until
/// the next `ScannerActiveChannelChanged` event arrived. The
/// master-switch caller ignores the return because it engages
/// the lock from a clean slate (no prior active to drop). Per
/// `CodeRabbit` round 5 on PR #562.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScannerAxisRefreshOutcome {
    /// Lock state didn't change in a way the sidebar cares
    /// about: either the prior active channel still exists in
    /// the new set (highlight reinstated), or there was no
    /// prior active to begin with, or the lock was disengaged
    /// because the channel set went empty.
    Unchanged,
    /// The prior active channel is no longer in the refreshed
    /// scanner set (user disabled `scan_enabled` on it or
    /// deleted the bookmark mid-scan). Caller should clear the
    /// scanner-sidebar surfaces via
    /// `clear_scanner_active_channel_ui` so the displayed
    /// channel name + lockout-row visibility match the actual
    /// "scanner on, no active channel" state.
    ActiveChannelDropped,
}

/// Recompute the scanner X-axis envelope from the live
/// bookmark list and refresh the spectrum's lock + Display
/// panel status row. Called from both the scanner master-
/// switch handler (when the user flips it on) AND the bookmark
/// mutation callback (when the user toggles `scan_enabled` /
/// deletes / adds a bookmark mid-scan, which can shift the
/// envelope without the master switch moving). Without the
/// second call site, scan-list edits silently let the lock
/// stay pinned to a stale range until the user flips the
/// master switch off-and-on. Per #516 smoke feedback.
///
/// Returns [`ScannerAxisRefreshOutcome::ActiveChannelDropped`]
/// when the previously-active channel got removed from the new
/// scanner set (so the caller can clear its sidebar surfaces).
fn refresh_scanner_axis_lock(
    bookmarks: &[sidebar::navigation_panel::Bookmark],
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    spectrum_handle: &spectrum::SpectrumHandle,
    status_row: &adw::ActionRow,
) -> ScannerAxisRefreshOutcome {
    let default_dwell_ms = sidebar::scanner_panel::load_default_dwell_ms(config);
    let default_hang_ms = sidebar::scanner_panel::load_default_hang_ms(config);
    let channels = sidebar::navigation_panel::project_scanner_channels(
        bookmarks,
        default_dwell_ms,
        default_hang_ms,
    );
    if let Some((min_hz, max_hz)) = sidebar::navigation_panel::scanner_channel_envelope(&channels) {
        // Snapshot the active-channel context BEFORE
        // `enter_scanner_mode` resets it to `None`. Without
        // this, a mid-scan bookmark mutation (the user toggles
        // a `scan_enabled` flag while a channel is being
        // sampled) would briefly clear the FFT highlight band
        // and waterfall projection until the next
        // `ScannerActiveChannelChanged` event arrived — a
        // visually jarring blink during live editing. Per
        // `CodeRabbit` round 2 on PR #562.
        let prior_active = spectrum_handle
            .scanner_axis_lock()
            .and_then(|lock| lock.active_channel_hz.zip(lock.active_channel_bw_hz));
        spectrum_handle.enter_scanner_mode(min_hz, max_hz);
        // Reapply only if the prior active channel still exists
        // in the refreshed scanner set. If the user just
        // disabled or deleted the bookmark that was the active
        // channel, reinstating its highlight would resurrect a
        // channel that's no longer being sampled — the
        // highlight would linger until the next DSP hop event
        // arrived. Match by exact (frequency, bandwidth) tuple
        // — same fields the prior tuple was sourced from, so
        // float equality is safe (identical bit patterns
        // round-trip through the SpectrumHandle store). Per
        // `CodeRabbit` round 4 on PR #562.
        let outcome = if let Some((freq_hz, bw_hz)) = prior_active {
            #[allow(clippy::cast_precision_loss)]
            let still_present = channels.iter().any(|ch| {
                let center = ch.key.frequency_hz as f64;
                (center - freq_hz).abs() < f64::EPSILON
                    && (ch.bandwidth - bw_hz).abs() < f64::EPSILON
            });
            if still_present {
                spectrum_handle.set_scanner_active_channel(freq_hz, bw_hz);
                ScannerAxisRefreshOutcome::Unchanged
            } else {
                // Leave `active_channel_*` cleared by
                // `enter_scanner_mode` above (matches the
                // "scanner on, no active channel" state) AND
                // tell the caller the active was dropped, so
                // the sidebar widgets get cleared in the same
                // tick instead of waiting for the next DSP
                // `ScannerActiveChannelChanged` event.
                ScannerAxisRefreshOutcome::ActiveChannelDropped
            }
        } else {
            ScannerAxisRefreshOutcome::Unchanged
        };
        update_scanner_axis_status_row(status_row, Some((min_hz, max_hz)));
        outcome
    } else {
        // No channels left in the scanner set. The lock
        // disengages, so the sidebar's active-channel surfaces
        // also belong cleared — but `clear_scanner_active_channel_ui`
        // already runs on the engine-side `ScannerEmptyRotation`
        // event that this code path implies. Reporting
        // `Unchanged` here keeps the bookmark-mutation
        // callback from double-clearing.
        spectrum_handle.exit_scanner_mode();
        update_scanner_axis_status_row(status_row, None);
        ScannerAxisRefreshOutcome::Unchanged
    }
}

/// Sync the Display panel's read-only "Scanner axis" status row
/// to match the current scanner-axis-lock state. Called from
/// every site that engages / disengages the lock — the master
/// switch handler in `connect_scanner_panel`, plus the DSP
/// scanner-stop fan-out (`ScannerEmptyRotation`,
/// `ScannerMutexStopped`) so the row tracks the actual lock
/// state instead of just the master switch position. Per issue
/// #516.
fn update_scanner_axis_status_row(row: &adw::ActionRow, range_hz: Option<(f64, f64)>) {
    if let Some((min_hz, max_hz)) = range_hz {
        let subtitle = format!(
            "{} – {}",
            spectrum::frequency_axis::format_frequency(min_hz),
            spectrum::frequency_axis::format_frequency(max_hz),
        );
        row.set_subtitle(&subtitle);
        row.set_visible(true);
    } else {
        row.set_subtitle("");
        row.set_visible(false);
    }
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
    // Drop any buffered channel-marker hop so the next transcript
    // text doesn't inherit a divider from a channel that's no
    // longer active. Reaches every stop path that funnels through
    // this helper (`ScannerActiveChannelChanged { key: None }`,
    // `ScannerEmptyRotation`, `ScannerMutexStopped`). Per
    // CodeRabbit round 1 on PR #558.
    *state.pending_channel_marker.borrow_mut() = None;
    scanner_panel
        .active_channel_row
        .set_subtitle(sidebar::scanner_panel::ACTIVE_CHANNEL_PLACEHOLDER);
    scanner_panel.lockout_row.set_visible(false);
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
    sample_rate_row: &adw::ComboRow,
    decimation_row: &adw::ComboRow,
    volume_button: &gtk4::ScaleButton,
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
            // Feed the FSPL distance estimator in the Radio panel
            // (ticket #164). The panel caches the level + current
            // frequency so the display refreshes if the user later
            // tweaks ERP / calibration.
            radio_panel.update_distance_from_signal(level, state.center_frequency.get());
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
            // Mirror into AppState so is_recording() (used by the
            // close-to-tray Quit confirmation modal) reflects reality.
            // Per #512.
            state.audio_recording_active.set(true);
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
            // Mirror into AppState. Per #512.
            state.audio_recording_active.set(false);
            record_audio_row.set_active(false);
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::new("Audio recording saved");
                overlay.add_toast(toast);
            }
        }
        DspToUi::IqRecordingStarted(path) => {
            tracing::info!(?path, "IQ recording started");
            // Mirror into AppState so is_recording() (used by the
            // close-to-tray Quit confirmation modal) reflects reality.
            // Per #512.
            state.iq_recording_active.set(true);
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
            // Mirror into AppState. Per #512.
            state.iq_recording_active.set(false);
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
            // Retune the bandwidth row's allowed range to the
            // new mode's [min, max] so the user can't dial a
            // value the demod will silently reject. Helper
            // self-suppresses around its auto-clamp — see issue
            // #505 + CR round 1 on PR #548 for why.
            update_bandwidth_row_range_for_mode(radio_panel, state, new_mode);
        }
        DspToUi::BandwidthChanged(bw) => {
            // DSP-confirmed bandwidth change. Update BOTH the
            // Radio panel's spin row AND the spectrum's visible
            // VFO width so they stay in lockstep with the active
            // filter regardless of where the change originated:
            //
            // - VFO drag on the spectrum: the drag handler
            //   already mutated `vfo_state.bandwidth_hz` inline
            //   for instant visual feedback, so the
            //   `set_vfo_bandwidth` below is a redundant
            //   confirm. Cheap.
            // - Radio panel `AdwSpinRow` / reset button /
            //   scanner retune / mode switch: those paths only
            //   sent the `SetBandwidth` command. Without the
            //   spectrum update here, the visible VFO width
            //   stays at whatever the previous drag put it at
            //   — which was issue #504.
            //
            // Set the `suppress_bandwidth_notify` flag around
            // the spin row's `set_value` so its
            // `connect_value_notify` handler knows this update
            // is DSP-originated and doesn't dispatch a redundant
            // `UiToDsp::SetBandwidth` back to the controller.
            // Restored after the set_value returns so
            // user-originated edits from the next event loop tick
            // are dispatched normally.
            state.suppress_bandwidth_notify.set(true);
            radio_panel.bandwidth_row.set_value(bw);
            state.suppress_bandwidth_notify.set(false);
            spectrum_handle.set_vfo_bandwidth(bw);
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
            // Buffer the channel name + hop time for lazy marker
            // emission. The transcription text-event handler will
            // consume this when the next transcribed line arrives
            // — that way markers only appear when there's actual
            // audio to attribute. If the scanner hops past a quiet
            // channel before any text fires, the next channel's
            // name overwrites the buffer and the silent channel
            // never gets a marker. If transcription is off, no
            // text events fire at all, so the buffered hop simply
            // stays unconsumed (gets overwritten on each hop) and
            // no markers ever appear in the panel. The hop time
            // is captured here (`chrono::Local::now()`) — not at
            // render time — so the marker reflects when the
            // scanner actually switched, even if the transcription
            // backend lags by a few seconds. Per issue #517 +
            // initial-smoke feedback on PR #558 + CodeRabbit
            // round 1 on PR #558.
            *state.pending_channel_marker.borrow_mut() =
                key.as_ref().map(|_| (chrono::Local::now(), name.clone()));
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
                // Push the active-channel context to the
                // scanner-axis lock — drives the highlight
                // band over the channel's bandwidth and the
                // narrow-FFT projection into the locked X
                // axis. No-op when the lock isn't engaged.
                // Per issue #516.
                spectrum_handle.set_scanner_active_channel(freq_f64, bandwidth);

                scanner_panel.active_channel_row.set_subtitle(&format!(
                    "{} — {}",
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
                // Retune the bandwidth row's range to the new
                // mode BEFORE the set_value below — otherwise a
                // scanner channel with bandwidth outside the
                // previous mode's range would be silently
                // clamped here and the displayed value would
                // drift from the actually-applied filter (#505).
                // The helper self-suppresses around its own
                // auto-clamp, so we don't need to wrap that call
                // in the suppress flag — only the explicit
                // `set_value` for the scanner-supplied bandwidth
                // below needs suppression. Per `CodeRabbit`
                // round 1 on PR #548.
                update_bandwidth_row_range_for_mode(radio_panel, state, demod_mode);
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

                scanner_panel.lockout_row.set_visible(true);
            } else {
                // Scanner went idle but lock stays engaged
                // (between rotations or before engine flips
                // back to Idle). Drop the active-channel
                // context so the highlight band + narrow-data
                // projection clear; wide axis stays pinned.
                // Per `CodeRabbit` round 1 on PR #562.
                spectrum_handle.clear_scanner_active_channel();
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
            scanner_panel.state_row.set_subtitle(label);
        }
        DspToUi::ScannerEmptyRotation => {
            tracing::info!("scanner rotation empty");
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(adw::Toast::new(
                    "Scanner has no active channels (all locked or disabled)",
                ));
            }
            // Engine is already back to Idle — drop the master
            // switch to match. Use `set_active(false)` (NOT
            // `set_state(false)`): per GtkSwitch semantics,
            // `set_state` only fires `notify::state` and leaves
            // `active` decoupled, so the master switch's
            // `connect_active_notify` handler — which now also
            // tears down the scanner-axis lock + Display panel
            // status row (#516) — wouldn't run. `set_active`
            // updates both properties and fires `notify::active`,
            // dispatching a redundant `SetScannerEnabled(false)`
            // (idempotent at the engine — scanner's already
            // Idle) AND triggering the lock teardown. Per
            // `CodeRabbit` round 3 on PR #562.
            scanner_panel.master_switch.set_active(false);
            // Clear the active-channel surfaces locally rather
            // than waiting for a separate `ActiveChannelChanged
            // { key: None }` event — the engine sends it today,
            // but relying on that ordering across four stop
            // sites was brittle.
            clear_scanner_active_channel_ui(scanner_panel, state);
        }
        DspToUi::AptLine(line) => {
            // Route the freshly-decoded APT line into the open
            // viewer, if any. When no viewer is open we silently
            // drop — the decoder always runs (it's cheap) so the
            // user can open the viewer mid-pass and start seeing
            // lines from that moment on, rather than having to
            // pre-arm before AOS.
            if let Some(view) = state.apt_viewer.borrow().as_ref() {
                view.push_line(&line);
            }
        }
        DspToUi::SstvLineDecoded(_line_index) => {
            // A new SSTV scan line has arrived — refresh the open
            // viewer (if any) from the shared SstvImage handle.
            // The viewer polls the handle via `update_from_handle`
            // which reads whatever the DSP tap has written since
            // the last call.  When no viewer is open we silently
            // drop, mirroring APT semantics above.
            if let Some(view) = state.sstv_viewer.borrow().as_ref() {
                view.update_from_handle(&state.sstv_image.handle());
            }
        }
        DspToUi::SstvImageComplete {
            width,
            height,
            pixels,
        } => {
            // The SSTV decoder has closed out a full image frame.
            // Accumulate it into the pass buffer so the
            // `SaveSstvPass` interpreter can write every image that
            // arrived during the pass to disk.
            //
            // We deliberately do NOT call `view.update_from_handle`
            // here: by the time this message arrives the controller's
            // tap has already called `SstvImageHandle::take_completed`,
            // which clears the in-flight pixel buffer for the next
            // VIS detection. Reading from the now-empty handle would
            // either no-op (snapshot returns None) or actively wipe
            // the displayed frame. The final row was already rendered
            // by the previous `SstvLineDecoded` refresh, so the
            // viewer already shows the correct end state.
            // Per CR round 3 on PR #599.
            let completed = sdr_radio::sstv_image::CompletedSstvImage {
                width,
                height,
                pixels,
            };
            state.sstv_completed_images.borrow_mut().push(completed);
            tracing::info!(
                width,
                height,
                "SSTV image complete; {} in buffer",
                state.sstv_completed_images.borrow().len()
            );
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
                sdr_core::messages::ScannerMutexReason::ScannerStoppedForRecording => {
                    // `set_active(false)` (NOT `set_state(false)`)
                    // so `connect_active_notify` fires and tears
                    // down the scanner-axis lock + status row.
                    // See `ScannerEmptyRotation` for the full
                    // rationale. Per `CodeRabbit` round 3 on
                    // PR #562.
                    scanner_panel.master_switch.set_active(false);
                    clear_scanner_active_channel_ui(scanner_panel, state);
                    "Scanner stopped — recording started"
                }
            };
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(adw::Toast::new(message));
            }
        }
        DspToUi::AcarsMessage(msg) => {
            // Bounded ring: pop oldest if at cap.
            let cap = crate::acars_config::default_recent_keep() as usize;
            let mut ring = state.acars_recent.borrow_mut();
            if ring.len() >= cap {
                ring.pop_front();
            }
            ring.push_back((*msg).clone());
            drop(ring);
            state
                .acars_total_count
                .set(state.acars_total_count.get().saturating_add(1));

            // Mirror to the viewer store if a viewer is open and
            // not paused. Pause semantic per
            // `acars_viewer.rs::build_acars_viewer_window`:
            // toggle active = skip append; the bounded ring keeps
            // growing regardless.
            //
            // Bounded retention: cap the visible store at the same
            // ceiling as `acars_recent` so multi-hour sessions
            // don't grow UI memory + filter cost without bound.
            // Splice from the front (oldest first) before append
            // so the new row lands at the bottom.
            //
            // Collapse-duplicates (#586): when the viewer's
            // collapse toggle is active, walk the most recent
            // rows for a `(aircraft, mode, label, text)` key
            // match within `ACARS_COLLAPSE_WINDOW`. On hit, bump
            // the existing wrapper's count + last_seen and emit
            // an `items_changed` so the row re-binds with the
            // new `(×N)` prefix instead of appending a duplicate.
            //
            // Auto-scroll-to-top: if the viewer is scrolled to
            // the top, scroll back to position 0 after the
            // append/mutate so new rows flow into view. If the
            // user has scrolled down to read older rows, freeze
            // until they scroll back up.
            if let Some(handles) = state.acars_viewer_handles.borrow().as_ref()
                && !handles.pause_button.is_active()
            {
                // Capture scroll state BEFORE the append. With the
                // GtkStack wrap (issue #579), GTK shifts the visible
                // area to preserve content when a new row lands at
                // position 0 under the descending-time sort. Checking
                // adj.value() AFTER the append would see the shifted
                // value and skip the snap-to-top.
                let adj = handles.scrolled_window.vadjustment();
                let was_at_top = (adj.value() - adj.lower()).abs() < 1.0;

                let collapse_active = handles.collapse_button.is_active();
                let mut collapsed_into: Option<u32> = None;
                if collapse_active {
                    collapsed_into = try_collapse_into_existing(&handles.store, &msg);
                }

                if let Some(idx) = collapsed_into {
                    handles.store.items_changed(idx, 1, 1);
                } else {
                    let cap = crate::acars_config::default_recent_keep();
                    let n = handles.store.n_items();
                    if n >= cap {
                        let excess = n - cap + 1;
                        handles
                            .store
                            .splice(0, excess, &[] as &[gtk4::glib::Object]);
                    }
                    handles
                        .store
                        .append(&crate::acars_viewer::AcarsMessageObject::new(
                            (*msg).clone(),
                        ));
                }

                // Auto-scroll-to-top: snap back if the user was at
                // the top before the append. Direct adjustment
                // manipulation rather than `ColumnView::scroll_to`:
                // that API is gated behind gtk4 `v4_12` and the
                // workspace pins `v4_10`.
                if was_at_top {
                    adj.set_value(adj.lower());
                }

                // Aircraft-index update (issue #579). Find or
                // insert the AircraftEntryObject for this tail.
                // New tails initialize with msg_count=1 (already
                // counting this message) so the column view's bind
                // reads the correct value on first paint. Existing
                // tails bump in place via record_message, then we
                // nudge the filter/sort models via items_changed
                // since GListStore doesn't fire that signal on
                // field mutation of an already-stored object.
                {
                    let mut idx = handles.aircraft_index.borrow_mut();
                    if let Some(obj) = idx.get(&msg.aircraft) {
                        obj.record_message(&msg);
                        // O(n) over ~50 aircraft is fine; Clear
                        // invalidates positions otherwise so we
                        // re-find each time rather than tracking
                        // a position field on the object.
                        if let Some(pos) = handles.aircraft_store.find(obj) {
                            handles.aircraft_store.items_changed(pos, 1, 1);
                        }
                    } else {
                        let entry = crate::acars_viewer::AircraftEntry {
                            tail: msg.aircraft,
                            last_seen: msg.timestamp,
                            msg_count: 1,
                            last_label: msg.label,
                        };
                        let obj = crate::acars_viewer::AircraftEntryObject::new(entry);
                        handles.aircraft_store.append(&obj);
                        idx.insert(msg.aircraft, obj);
                    }
                }
            }

            tracing::trace!(
                "ACARS msg {} ({}, label {:?})",
                state.acars_total_count.get(),
                msg.aircraft.as_str(),
                msg.label
            );
        }
        DspToUi::AcarsChannelStats(ch_stats) => {
            *state.acars_channel_stats.borrow_mut() = ch_stats.into_vec();
        }
        DspToUi::AcarsEnabledChanged(result) => {
            match result {
                Ok(true) => {
                    state.acars_enabled.set(true);
                    state.acars_pending.set(false);
                    state.acars_total_count.set(0);
                    state.acars_recent.borrow_mut().clear();
                    // Mirror the DSP's silent retune to airband
                    // center on the header freq selector + status
                    // bar + spectrum, and disable user input
                    // since DSP rejects geometry commands while
                    // engaged (round 14 on PR #584). Stash the
                    // pre-engage `(center, vfo_offset)` tuple
                    // so disengage can restore both — the
                    // controller's restore path reapplies the
                    // snapshot offset (CR round 13 on PR #584)
                    // and `state.center_frequency` would
                    // otherwise drift from the DSP snapshot.
                    state.acars_saved_tune.set(Some((
                        state.center_frequency.get(),
                        spectrum_handle.vfo_offset_hz(),
                    )));
                    let center_hz = sdr_core::acars_airband_lock::ACARS_CENTER_HZ;
                    state.center_frequency.set(center_hz);
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                    freq_selector.set_frequency(center_hz as u64);
                    spectrum_handle.set_center_frequency(center_hz);
                    freq_selector.widget.set_sensitive(false);
                    // Mirror the DSP's airband lock on the other
                    // geometry-mutating widgets (rounds 14-15 on
                    // PR #584): SetDemodMode, SetSampleRate, and
                    // SetDecimation are all rejected while engaged.
                    demod_dropdown.set_sensitive(false);
                    sample_rate_row.set_sensitive(false);
                    decimation_row.set_sensitive(false);
                    status_bar.update_frequency(center_hz);
                    // Auto-mute the speaker (issue #588). With ACARS
                    // engaged the demod is parked on the user's
                    // single pre-engage VFO position, which is
                    // unrelated to the 6 ACARS channels being
                    // decoded silently in parallel — so whatever
                    // comes out of the speaker is at best an
                    // unrelated airband channel and at worst static.
                    // Capture pre-engage volume + flip to 0; the
                    // suppress flag prevents the value-changed
                    // handler from persisting 0.0 to config or
                    // double-dispatching SetVolume. We send
                    // SetVolume(0.0) explicitly here.
                    #[allow(clippy::cast_possible_truncation)]
                    let pre_engage_volume = volume_button.value() as f32;
                    state.acars_saved_volume.set(Some(pre_engage_volume));
                    state.suppress_volume_notify.set(true);
                    volume_button.set_value(0.0);
                    state.suppress_volume_notify.set(false);
                    state.send_dsp(UiToDsp::SetVolume(0.0));
                    tracing::info!("ACARS engaged");
                }
                Ok(false) => {
                    state.acars_enabled.set(false);
                    state.acars_pending.set(false);
                    state.acars_recent.borrow_mut().clear();
                    state.acars_total_count.set(0);
                    state.acars_channel_stats.borrow_mut().clear();
                    // Restore the pre-engage tune snapshot. DSP
                    // retunes silently and reapplies its own
                    // snapshot offset, but doesn't emit Tune /
                    // VfoOffsetChanged echoes — so restore the
                    // UI mirrors here. Order matches what a
                    // user-driven `Tune` would do:
                    // `state.center_frequency`, spectrum center,
                    // then offset (which the freq selector +
                    // status bar derive from `center + offset`).
                    if let Some((center_hz, offset_hz)) = state.acars_saved_tune.take() {
                        state.center_frequency.set(center_hz);
                        spectrum_handle.set_center_frequency(center_hz);
                        spectrum_handle.set_vfo_offset(offset_hz);
                        let tuned_hz = center_hz + offset_hz;
                        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                        let tuned_u64 = tuned_hz.max(0.0) as u64;
                        freq_selector.set_frequency(tuned_u64);
                        status_bar.update_frequency(tuned_hz);
                    }
                    freq_selector.widget.set_sensitive(true);
                    demod_dropdown.set_sensitive(true);
                    sample_rate_row.set_sensitive(true);
                    decimation_row.set_sensitive(true);
                    // Auto-restore volume (issue #588) — but only
                    // if the user didn't manually move it during
                    // the session. We muted to 0.0 on engage; if
                    // current value is still ≈ 0, no override
                    // happened, restore the saved value. If the
                    // user moved it (current > tolerance), respect
                    // their explicit choice and skip restore.
                    // Tolerance 0.01 (1%) is well above ScaleButton
                    // popover step granularity. Don't suppress on
                    // restore: the value-changed handler's
                    // dispatch + persist of the restored value is
                    // exactly what we want.
                    if let Some(saved) = state.acars_saved_volume.take() {
                        const VOLUME_OVERRIDE_TOLERANCE: f64 = 0.01;
                        let current = volume_button.value();
                        if current.abs() < VOLUME_OVERRIDE_TOLERANCE {
                            volume_button.set_value(f64::from(saved));
                        } else {
                            tracing::debug!(
                                current,
                                "ACARS disengage: keeping user-overridden volume"
                            );
                        }
                    }
                    // Drain a deferred AOS batch (issue #589). When
                    // a satellite auto-record tick fired during an
                    // engaged session, the recorder tick site
                    // stashed the entire `Vec<RecorderAction>`
                    // and dispatched SetAcarsEnabled(false) — now
                    // that the controller has acked the disengage
                    // we replay every action through the same
                    // recorder interpreter, in the original order.
                    // Defer to next idle so we're outside the
                    // dispatch borrow.
                    let pending = state.pending_aos_actions.borrow_mut().take();
                    if let Some(actions) = pending
                        && let Some(interp_weak) =
                            state.recorder_action_interpreter.borrow().clone()
                        && let Some(interp) = interp_weak.upgrade()
                    {
                        tracing::info!(
                            "AOS replay: ACARS disengaged, executing {} deferred action(s)",
                            actions.len()
                        );
                        glib::idle_add_local_once(move || {
                            for action in actions {
                                interp(action);
                            }
                        });
                    }
                    tracing::info!("ACARS disengaged");
                }
                Err(err) => {
                    tracing::warn!("ACARS enable failed: {err}");
                    // Clear the in-flight flag so the panel
                    // refresh tick stops suppressing the
                    // switch-state mirror. State.acars_enabled
                    // is intentionally NOT mutated here per CR
                    // round 1 on PR #584 — Err doesn't
                    // disambiguate engage-vs-disengage failure.
                    // The next refresh tick will resync the
                    // switch to the unchanged
                    // `state.acars_enabled` value, undoing the
                    // user's failed toggle.
                    state.acars_pending.set(false);
                    // `acars_saved_volume` (and `acars_saved_tune`)
                    // are intentionally NOT cleared here. Err
                    // doesn't disambiguate engage-vs-disengage
                    // failure: a failed disengage on an already-
                    // engaged session needs the saved snapshots
                    // preserved so the eventual successful
                    // disengage can restore them; a failed engage
                    // simply never set them.
                    //
                    // Abort any deferred AOS batch (issue #589).
                    // The disengage couldn't complete, so the
                    // satellite tune would still be rejected by
                    // the airband lock. Drop the stashed batch +
                    // clear the round-trip flag so LOS doesn't
                    // try to re-engage onto an unstable state,
                    // and surface a dedicated toast naming the
                    // affected satellite (looked up from the
                    // batch's `StartAutoRecord` entry).
                    let aborted = state.pending_aos_actions.borrow_mut().take();
                    if let Some(actions) = aborted {
                        let satellite = actions.iter().find_map(|a| match a {
                            crate::sidebar::satellites_recorder::Action::StartAutoRecord {
                                satellite,
                                ..
                            } => Some(satellite.clone()),
                            _ => None,
                        });
                        state.acars_was_engaged_pre_pass.set(false);
                        if let Some(satellite) = satellite {
                            tracing::warn!(
                                satellite = %satellite,
                                error = %err,
                                "AOS aborted: ACARS disengage failed",
                            );
                            if let Some(overlay) = toast_overlay_weak.upgrade() {
                                overlay.add_toast(adw::Toast::new(&format!(
                                    "Pass {satellite} aborted: ACARS disengage failed"
                                )));
                            }
                        }
                    }
                    // Surface the original engage/disengage
                    // failure as a toast too so the user sees
                    // the actionable error (e.g. "scanner is
                    // running" or "RTL-SDR required").
                    if let Some(overlay) = toast_overlay_weak.upgrade() {
                        overlay.add_toast(adw::Toast::new(&format!("ACARS: {err}")));
                    }
                }
            }
        }
        // Output-writer errors (issue #578). Handler wired in Task 8;
        // stub here keeps the match exhaustive. Surfaces the kind-scoped
        // message as a toast so the user sees misconfigured paths / DNS
        // failures without having to consult the log.
        DspToUi::AcarsOutputError { kind, message } => {
            tracing::warn!(kind, message, "ACARS output error");
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(adw::Toast::new(&format!(
                    "ACARS {kind} output error: {message}"
                )));
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

/// How much wider than its default a sidebar may be dragged. 2× the
/// default feels natural — "a little bigger" and "a lot bigger"
/// without letting the panel overrun the spectrum.
const SIDEBAR_MAX_WIDTH_MULTIPLIER: f64 = 2.0;
/// Minimum `sidebar-width-fraction` we write. Guards against the
/// `AdwOverlaySplitView` pspec's rejection of exactly 0 and the
/// animator's visual collapse at very small values.
const SIDEBAR_FRACTION_MIN: f64 = 0.01;
/// Maximum `sidebar-width-fraction` — symmetric sibling of
/// [`SIDEBAR_FRACTION_MIN`]. Prevents the content area from being
/// squeezed to zero even if a pixel clamp miscomputes.
const SIDEBAR_FRACTION_MAX: f64 = 0.99;

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
    /// General activity panel — landing view. Hosts band presets
    /// and source as flat `AdwPreferencesGroup`s on an
    /// `AdwPreferencesPage`. Bookmarks live in the right activity
    /// stack (not here); `rtl_tcp` share controls live in the Share
    /// left activity (not here).
    general_panel: sidebar::GeneralPanel,
}

#[allow(clippy::too_many_lines)]
fn build_layout(
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) -> LayoutHandles {
    // Sidebar panels — constructed flat; each lives in its own
    // activity stack child (no shared scroll wrapper). The
    // General activity composes band presets + source into an
    // `AdwPreferencesPage`; Radio / Audio / Display / Scanner /
    // Share host their respective panel widgets directly until
    // sub-tickets #423-#426 refactor each into the expander-row
    // layout. Bookmarks lives in the right activity stack.
    let panels = sidebar::build_panels();
    sidebar::server_panel::connect_server_panel_persistence(&panels.server, config);

    let general_panel = sidebar::build_general_panel(&panels.navigation, &panels.source);

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

    // Transcript panel — the real widget, not a placeholder. Its
    // root is already an `AdwPreferencesGroup` so it slots straight
    // into the page wrapper in the right stack, inheriting the same
    // chrome as every other activity panel.
    let transcript_panel = sidebar::transcript_panel::build_transcript_panel(config);

    // Left panel stack — one real panel widget per activity. General
    // hosts the composed `GeneralPanel` (band presets + bookmarks +
    // source + rtl_tcp share as expander rows); Radio / Audio /
    // Display / Scanner host their existing panel widget wrapped in
    // a scroll so long pages can scroll internally without resizing
    // the panel's width (design doc §2.4). Sub-tickets #423-#426
    // later refactor each of those widgets into the expander-row
    // layout the General panel demonstrates; the `name` strings MUST
    // remain stable because they're the config-persistence keys
    // (§5 of the design doc).
    let left_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::None)
        .hexpand(true)
        .vexpand(true)
        .build();
    left_stack.add_named(&general_panel.widget, Some("general"));
    left_stack.add_named(&panels.radio.widget, Some("radio"));
    left_stack.add_named(&panels.audio.widget, Some("audio"));
    left_stack.add_named(&panels.display.widget, Some("display"));
    left_stack.add_named(&panels.scanner.widget, Some("scanner"));
    left_stack.add_named(&page_from_group(&panels.server.widget), Some("share"));
    left_stack.add_named(&panels.satellites.widget, Some("satellites"));
    left_stack.add_named(&panels.aviation.widget, Some("aviation"));

    // Right panel stack — single child today, hosts the real
    // transcript widget (not a placeholder) so transcription keeps
    // working during the migration window.
    let right_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::None)
        .hexpand(true)
        .vexpand(true)
        .build();
    right_stack.add_named(
        &page_from_group(&transcript_panel.widget),
        Some("transcript"),
    );
    right_stack.add_named(
        &page_from_group(&panels.bookmarks.widget),
        Some("bookmarks"),
    );
    // Explicitly pin the initial visible child so a future
    // additional right-activity inserted before transcript doesn't
    // silently shift what the first `Ctrl+Shift+1` press (or the
    // header transcript button's click) shows. Matches the contract
    // `wire_activity_bar_clicks(..., "transcript")` relies on below.
    right_stack.set_visible_child_name("transcript");

    // Inner (right) split view — sidebar sits on the trailing edge
    // so the right activity bar is the rightmost element on-screen.
    //
    // `sidebar_width_fraction` is `[0, 1]` regardless of the
    // `sidebar-width-unit` we set; the unit only changes how
    // `min`/`max-sidebar-width` are interpreted. Passing a pixel
    // value as the fraction panics at property-set even with
    // `unit = Px` (verified on libadwaita 1.9). So the default
    // here is a fraction; under nested splits its pixel result
    // is approximate, and `min-sidebar-width` clamps the transcript
    // panel up to its 360 px floor when the math would otherwise
    // leave it narrower. User-driven resize + persistence come from
    // the drag handle wired below (#429).
    let right_split_view = adw::OverlaySplitView::builder()
        .sidebar_position(gtk4::PackType::End)
        .content(&content_box)
        .show_sidebar(false)
        .min_sidebar_width(RIGHT_SIDEBAR_MIN_WIDTH)
        .max_sidebar_width(RIGHT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER)
        .sidebar_width_fraction(RIGHT_SIDEBAR_DEFAULT_WIDTH / f64::from(DEFAULT_WIDTH))
        .build();

    // Compose the right sidebar with its resize handle on the
    // leading edge (the boundary with the content area). Dragging
    // the handle LEFT widens the sidebar; drag-end persists the
    // new pixel width; double-click resets to the default.
    let config_right_resize = std::sync::Arc::clone(config);
    let save_right_width: std::rc::Rc<dyn Fn(u32)> = std::rc::Rc::new(move |px| {
        sidebar::activity_bar::save_right_width_px(&config_right_resize, px);
    });
    let right_handle = build_resize_handle(
        &right_split_view,
        ResizeDirection::LeftGrowsSidebar,
        RIGHT_SIDEBAR_MIN_WIDTH,
        RIGHT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER,
        RIGHT_SIDEBAR_DEFAULT_WIDTH,
        &save_right_width,
    );
    let right_sidebar_wrap = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .build();
    right_sidebar_wrap.append(&right_handle);
    right_sidebar_wrap.append(&right_stack);
    right_split_view.set_sidebar(Some(&right_sidebar_wrap));

    // Outer (left) split view — sidebar hosts the left activity
    // stack. Starts open with "general" visible so a fresh launch
    // lands on the General panel instead of an empty frame.
    let left_split_view = adw::OverlaySplitView::builder()
        .content(&right_split_view)
        .show_sidebar(true)
        .min_sidebar_width(LEFT_SIDEBAR_MIN_WIDTH)
        .max_sidebar_width(LEFT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER)
        .sidebar_width_fraction(LEFT_SIDEBAR_DEFAULT_WIDTH / f64::from(DEFAULT_WIDTH))
        .build();

    // Compose the left sidebar with its resize handle on the
    // trailing edge. Dragging the handle RIGHT widens the sidebar.
    let config_left_resize = std::sync::Arc::clone(config);
    let save_left_width: std::rc::Rc<dyn Fn(u32)> = std::rc::Rc::new(move |px| {
        sidebar::activity_bar::save_left_width_px(&config_left_resize, px);
    });
    let left_handle = build_resize_handle(
        &left_split_view,
        ResizeDirection::RightGrowsSidebar,
        LEFT_SIDEBAR_MIN_WIDTH,
        LEFT_SIDEBAR_DEFAULT_WIDTH * SIDEBAR_MAX_WIDTH_MULTIPLIER,
        LEFT_SIDEBAR_DEFAULT_WIDTH,
        &save_left_width,
    );
    let left_sidebar_wrap = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .hexpand(true)
        .vexpand(true)
        .build();
    left_sidebar_wrap.append(&left_stack);
    left_sidebar_wrap.append(&left_handle);
    left_split_view.set_sidebar(Some(&left_sidebar_wrap));
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
        general_panel,
    }
}

/// Wrap an `AdwPreferencesGroup` in its own `AdwPreferencesPage`
/// so every activity stack child inherits the same margin/spacing
/// rhythm as the General panel (Apple-style header padding + group
/// titles). `AdwPreferencesPage` is itself scrollable internally,
/// so no extra `GtkScrolledWindow` wrapper is needed.
fn page_from_group(group: &adw::PreferencesGroup) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.add(group);
    page
}

/// Apply a pixel width to an `AdwOverlaySplitView` sidebar after
/// the split view has a real allocation. A single `notify::width`
/// handler fires once the first non-zero width lands, converts
/// the target pixels into the `[0, 1]` fraction the widget
/// accepts, applies it, and then disarms (`applied` flag) so
/// subsequent width notifications (window resize) leave the
/// sidebar's fractional preference alone.
///
/// `saved_px == Some(px)` uses the persisted value; `None` falls
/// back to `default_px`. Both cases go through the same
/// post-allocation conversion so the advertised pixel default
/// actually lands — builder-time fractions are derived from
/// `DEFAULT_WIDTH` and evaluate against the split view's
/// narrower-than-window allocation, so without this the fresh-
/// session defaults under-shoot their targets.
fn apply_sidebar_width(split_view: &adw::OverlaySplitView, saved_px: Option<u32>, default_px: u32) {
    let target_px = saved_px.unwrap_or(default_px);
    let applied: std::rc::Rc<std::cell::Cell<bool>> = std::rc::Rc::new(std::cell::Cell::new(false));
    split_view.connect_notify_local(Some("width"), move |sv, _| {
        if applied.get() {
            return;
        }
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let fraction =
            (f64::from(target_px) / sv_w).clamp(SIDEBAR_FRACTION_MIN, SIDEBAR_FRACTION_MAX);
        sv.set_sidebar_width_fraction(fraction);
        applied.set(true);
    });
}

/// Width of the invisible drag strip at a sidebar's inner edge
/// (design doc §4.4 calls for "thin (4–6 px)"). 6 px gives the
/// user a forgiving hit target without stealing pixels from the
/// panel content.
const RESIZE_HANDLE_WIDTH_PX: i32 = 6;

/// Which direction of drag grows the sidebar. The LEFT split
/// view's sidebar sits on the leading edge — dragging the handle
/// right pushes the sidebar-content boundary right and widens the
/// sidebar. The RIGHT split view's sidebar sits on the trailing
/// edge (`sidebar_position=End`) — the handle is on its leading
/// edge, and dragging LEFT widens the sidebar.
#[derive(Clone, Copy, Debug)]
enum ResizeDirection {
    /// Positive `offset_x` widens the sidebar (left split view).
    RightGrowsSidebar,
    /// Negative `offset_x` widens the sidebar (right split view).
    LeftGrowsSidebar,
}

/// Build an invisible drag-handle widget sized to
/// [`RESIZE_HANDLE_WIDTH_PX`] and wire it to resize an
/// `AdwOverlaySplitView` sidebar. Live-resizes during drag,
/// persists the final width on drag-end via `save_width_px`,
/// and resets to `default_px` on a left-button double-click
/// (standard GTK paned-divider pattern).
///
/// `AdwOverlaySplitView` only exposes `sidebar-width-fraction`
/// (range `[0, 1]`); pixel min/max/default are converted to the
/// fraction against the split view's live allocation every time
/// the gesture fires, so the clamp reacts correctly to window
/// resizes.
fn build_resize_handle(
    split_view: &adw::OverlaySplitView,
    direction: ResizeDirection,
    min_px: f64,
    max_px: f64,
    default_px: f64,
    save_width_px: &std::rc::Rc<dyn Fn(u32)>,
) -> gtk4::Box {
    let handle = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .width_request(RESIZE_HANDLE_WIDTH_PX)
        .css_classes(["sidebar-resize-handle"])
        .build();
    if let Some(cursor) = gtk4::gdk::Cursor::from_name("col-resize", None) {
        handle.set_cursor(Some(&cursor));
    }

    // Captured at drag-begin so every `drag-update` computes the
    // new width from the stable starting fraction rather than
    // integrating floating-point deltas. Without this the gesture
    // would drift 1–2 px per drag cycle.
    let start_fraction: std::rc::Rc<std::cell::Cell<f64>> =
        std::rc::Rc::new(std::cell::Cell::new(0.0));

    // Gesture closures capture `split_view` via `WeakRef` to
    // break an otherwise-real retain cycle: `split_view` owns
    // `sidebar`, `sidebar` owns `handle`, `handle` owns the
    // gesture controllers, the controllers own their closures,
    // and a strong `split_view.clone()` inside the closures would
    // close the loop and leak the whole sidebar subtree on window
    // teardown. Matches the `glib::WeakRef` idiom used elsewhere
    // in this file (scanner force-disable, RTL-TCP handlers).
    let drag_gesture = gtk4::GestureDrag::new();

    let split_view_weak = split_view.downgrade();
    let start_fraction_begin = std::rc::Rc::clone(&start_fraction);
    drag_gesture.connect_drag_begin(move |_, _, _| {
        if let Some(sv) = split_view_weak.upgrade() {
            start_fraction_begin.set(sv.sidebar_width_fraction());
        }
    });

    let split_view_weak = split_view.downgrade();
    let start_fraction_update = std::rc::Rc::clone(&start_fraction);
    drag_gesture.connect_drag_update(move |_, offset_x, _| {
        let Some(sv) = split_view_weak.upgrade() else {
            return;
        };
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let start_px = start_fraction_update.get() * sv_w;
        let signed_offset = match direction {
            ResizeDirection::RightGrowsSidebar => offset_x,
            ResizeDirection::LeftGrowsSidebar => -offset_x,
        };
        let new_px = (start_px + signed_offset).clamp(min_px, max_px);
        // Fraction pspec is `[0, 1]`; guard against 0 which the
        // widget treats as "collapsed" at the animator level.
        let new_fraction = (new_px / sv_w).clamp(SIDEBAR_FRACTION_MIN, SIDEBAR_FRACTION_MAX);
        sv.set_sidebar_width_fraction(new_fraction);
    });

    let split_view_weak = split_view.downgrade();
    let save_end = std::rc::Rc::clone(save_width_px);
    drag_gesture.connect_drag_end(move |_, _, _| {
        let Some(sv) = split_view_weak.upgrade() else {
            return;
        };
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let final_px = sv.sidebar_width_fraction() * sv_w;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let px = final_px.round().max(0.0) as u32;
        save_end(px);
    });
    handle.add_controller(drag_gesture);

    // Double-click = reset to default width. Matches the GTK paned-
    // divider convention users expect ("I messed up my drag, take
    // me back"). A single click does nothing — the drag gesture
    // already handles press/release.
    let click_gesture = gtk4::GestureClick::new();
    click_gesture.set_button(gtk4::gdk::BUTTON_PRIMARY);
    let split_view_weak = split_view.downgrade();
    let save_click = std::rc::Rc::clone(save_width_px);
    click_gesture.connect_released(move |_, n_press, _, _| {
        if n_press != 2 {
            return;
        }
        let Some(sv) = split_view_weak.upgrade() else {
            return;
        };
        let sv_w = f64::from(sv.width());
        if sv_w <= 0.0 {
            return;
        }
        let fraction = (default_px / sv_w).clamp(SIDEBAR_FRACTION_MIN, SIDEBAR_FRACTION_MAX);
        sv.set_sidebar_width_fraction(fraction);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let px = default_px.round().max(0.0) as u32;
        save_click(px);
    });
    handle.add_controller(click_gesture);

    handle
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
    gtk4::ScaleButton,
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
    // Initial value + `connect_value_changed` handler are wired in
    // `build_window` after `connect_audio_panel` runs, so the
    // persistence + audio-panel mirror rely on the full handle set.
    volume_button.set_tooltip_text(Some("Volume"));
    // Explicit accessibility label — tooltip text alone isn't
    // announced reliably by screen readers for icon-only header
    // controls (same idiom as the bookmarks / transcript / pinned-
    // servers buttons).
    volume_button.update_property(&[gtk4::accessible::Property::Label("Volume")]);

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
        volume_button,
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
                // Clicking the already-selected icon toggles the
                // panel open/closed. The icon's `active` property
                // tracks the panel's NEW visibility — active
                // when shown, inactive when hidden — so the
                // highlight always reflects "this panel is on
                // screen right now". Per issue #518: the earlier
                // `set_active(true)` unconditionally re-asserted
                // the highlight even after a close, which was
                // misleading (icon glowed but panel was gone).
                //
                // GTK's default click handler already flipped
                // `active`, so we'd otherwise see two flips per
                // click. Setting it explicitly here pins the
                // icon state to the resolved sidebar visibility
                // regardless of GTK's intermediate flip.
                if let Some(sv) = split_view_weak.upgrade() {
                    let new_shown = !sv.shows_sidebar();
                    sv.set_show_sidebar(new_shown);
                    clicked_btn.set_active(new_shown);
                } else {
                    // Split view torn down — preserve the
                    // previous "icon stays active" behaviour
                    // since we can't observe panel state.
                    clicked_btn.set_active(true);
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

/// Wire a `connect_show_sidebar_notify` that keeps the activity
/// bar's icon active state in sync with the sidebar's visibility
/// regardless of who toggled it. Companion to
/// [`wire_activity_bar_clicks`] — that handles the user-clicks-
/// the-icon path; this handles every OTHER way `show-sidebar`
/// can flip (header sidebar button, F9 keyboard shortcut, the
/// breakpoint collapsing the sidebar at narrow widths, future
/// programmatic toggles).
///
/// Without this, an external toggle would leave the icon stale —
/// e.g. user opens panel via icon (icon active), closes panel
/// via header button (sidebar gone, icon still active). Per
/// issue #518.
fn sync_activity_bar_to_sidebar_visibility(
    split_view: &adw::OverlaySplitView,
    bar: &sidebar::ActivityBar,
    stack: &gtk4::Stack,
) {
    let buttons: Vec<(&'static str, glib::WeakRef<gtk4::ToggleButton>)> = bar
        .buttons
        .iter()
        .map(|(n, b)| (*n, b.downgrade()))
        .collect();
    let stack_weak = stack.downgrade();
    split_view.connect_show_sidebar_notify(move |sv| {
        let shown = sv.shows_sidebar();
        let visible_name = stack_weak
            .upgrade()
            .and_then(|s| s.visible_child_name().map(|gs| gs.to_string()));
        for (name, weak) in &buttons {
            let Some(btn) = weak.upgrade() else { continue };
            let should_be_active = shown && visible_name.as_deref() == Some(*name);
            if btn.is_active() != should_be_active {
                btn.set_active(should_be_active);
            }
        }
    });
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn connect_sidebar_panels(
    app: &adw::Application,
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
    volume_button: &gtk4::ScaleButton,
    set_playing: &Rc<dyn Fn(bool)>,
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
    connect_volume_persistence(panels, state, config, volume_button);
    connect_distance_estimator_persistence(panels, config);
    connect_scanner_panel(panels, state, config, spectrum_handle);
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
    let tune_to_satellite: Rc<dyn Fn(u64, sdr_types::DemodMode, u32)> = {
        let state_t = Rc::clone(state);
        let freq_selector_t = freq_selector.clone();
        let demod_dropdown_t = demod_dropdown.clone();
        let spectrum_t = Rc::clone(spectrum_handle);
        let force_disable_t = Rc::clone(scanner_force_disable);
        let bandwidth_row_t = panels.radio.bandwidth_row.clone();
        let radio_panel_t = panels.radio.clone();
        let status_bar_t = Rc::clone(status_bar);
        Rc::new(move |freq_hz, mode, bw_hz| {
            tune_to_target(
                &state_t,
                &freq_selector_t,
                &demod_dropdown_t,
                &spectrum_t,
                &force_disable_t,
                &bandwidth_row_t,
                &radio_panel_t,
                &status_bar_t,
                freq_hz,
                mode,
                f64::from(bw_hz),
                "satellite tune",
            );
        })
    };
    // Register `app.tune-satellite` so the "Tune" button on a #510
    // pre-pass desktop notification can route back to the same
    // tune closure the panel's per-row play buttons use. Action
    // target is the satellite's NORAD id (`u32`); the handler
    // looks the entry up in `KNOWN_SATELLITES` for downlink /
    // demod / bandwidth.
    {
        let tune_for_action = Rc::clone(&tune_to_satellite);
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

    connect_satellites_panel(
        panels,
        config,
        state,
        toast_overlay,
        spectrum_handle,
        &tune_to_satellite,
        set_playing,
        status_bar,
    );
    connect_aviation_panel(&panels.aviation, state, config, toast_overlay);
    // Transcript panel is wired separately (not in SidebarPanels).
    connect_navigation_panel(
        panels,
        state,
        freq_selector,
        demod_dropdown,
        status_bar,
        spectrum_handle,
        scanner_force_disable,
        volume_button,
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
            let outcome = refresh_scanner_axis_lock(
                &bookmarks.bookmarks.borrow(),
                &config_for_mutated,
                &spectrum_for_mutated,
                &display_axis_row_for_mutated,
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
    // Only hydrate the shared host / port / protocol row triple
    // with the last-connected RTL-TCP server when the persisted
    // source type is actually RTL-TCP. If the user was last on
    // raw Network, the values restored by `connect_source_panel`
    // a moment earlier (KEY_SOURCE_NETWORK_*) are the right ones
    // to keep visible — overwriting them with an unrelated
    // RTL-TCP endpoint just because one was once connected would
    // surprise the user on every restart. Per `CodeRabbit` round
    // 2 on PR #558.
    let restored_source_is_rtl_tcp =
        sidebar::source_panel::load_source_device_index(&config_for_discovery)
            == sidebar::source_panel::DEVICE_RTLTCP;
    if restored_source_is_rtl_tcp
        && let Some(last) = crate::sidebar::source_panel::load_last_connected(&config_for_discovery)
    {
        // Same guarded-rewrite idiom as `apply_rtl_tcp_connect`:
        // hydrating the last-connected RTL-TCP server must not
        // overwrite `KEY_SOURCE_NETWORK_*` (the raw-Network
        // triple). The persistence handlers for those rows
        // observe the flag and skip the disk-write, AND skip
        // the `SetNetworkConfig` dispatch so the three row
        // mutations don't kick three intermediate reconnects
        // against a partially-rewritten triple. Per `CodeRabbit`
        // rounds 1 and 2 on PR #558.
        state.rtl_tcp_hydration_in_progress.set(true);
        protocol_row.set_selected(NETWORK_PROTOCOL_TCPCLIENT_IDX);
        hostname_row.set_text(&last.host);
        port_row.set_value(f64::from(last.port));
        state.rtl_tcp_hydration_in_progress.set(false);
        // Emit the canonical `SetNetworkConfig` for the restored
        // RTL-TCP endpoint *after* the flag clears, mirroring
        // `apply_rtl_tcp_connect`'s own post-hydration dispatch.
        // Without this, the only `SetNetworkConfig` the DSP saw
        // came from `connect_source_panel`'s raw-Network restore
        // a moment earlier — so first Play on a persisted
        // RTL-TCP session would dial the stale raw-Network
        // endpoint until the user nudged a row by hand. Per
        // `CodeRabbit` round 4 on PR #558.
        state.send_dsp(UiToDsp::SetNetworkConfig {
            hostname: last.host.clone(),
            port: last.port,
            protocol: sdr_types::Protocol::TcpClient,
        });
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
    // Guard the programmatic row rewrites so the per-field
    // handlers don't clobber `KEY_SOURCE_NETWORK_*` (which belong
    // to the user's independent raw-Network selection) with the
    // RTL-TCP endpoint. While the hydration flag is set, the
    // handlers suppress BOTH the persistence write AND the
    // per-edit `SetNetworkConfig` dispatch — three sequential row
    // mutations otherwise fan out to three intermediate
    // reconnects against a partially-rewritten triple. A single
    // canonical `SetNetworkConfig` is dispatched further down
    // (after the flag clears) so the DSP gets the fully-formed
    // endpoint exactly once. Per `CodeRabbit` rounds 1, 2, and 5
    // on PR #558.
    state.rtl_tcp_hydration_in_progress.set(true);
    protocol_row.set_selected(NETWORK_PROTOCOL_TCPCLIENT_IDX);
    hostname_row.set_text(host);
    port_row.set_value(f64::from(port));
    state.rtl_tcp_hydration_in_progress.set(false);
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

/// Wire the server panel end-to-end: the master share-over-network
/// switch (start/stop + control locking), periodic `Server::stats()`
/// polling (rendered status rows + auto-stop on `has_stopped()`), and
/// the bandwidth advisory that toggles on the device-default sample
/// rate. Errors surface via the `toast_overlay`, and the switch
/// auto-reverts to its off state on start failure so the UI never
/// lies about whether a server is actually running.
///
/// The panel itself is always visible — Share is its own activity on
/// the left activity bar (📡), so the legacy hotplug-gated
/// hide/show timer, the device-count cache, and the
/// `device_row.connect_selected_notify` handler that fed it are gone.
/// The start path still rejects a "local RTL-SDR is the active
/// source" conflict via an exclusivity toast inside the share-switch
/// handler — that guard is independent of the removed machinery.
fn connect_server_panel(
    panels: &SidebarPanels,
    toast_overlay: &adw::ToastOverlay,
    server_running: Rc<std::cell::Cell<bool>>,
) {
    let running: Rc<RefCell<Option<RunningServer>>> = Rc::new(RefCell::new(None));

    // Share is now an activity on the left activity bar — always
    // reachable via the 📡 icon. The legacy hotplug-driven
    // hide/show timer + device-count cache + device-row notify
    // were removed with that migration; `sdr_rtlsdr::get_device_count`
    // is no longer polled on the GTK main loop for visibility,
    // and the start-server path still rejects the "local dongle is
    // the active source" conflict via its own exclusivity guard.

    // Wire the master share-over-network switch. The handler is the
    // authority on server lifecycle — on toggle we either start a
    // new `Server` (+ optional `Advertiser`) and store the handle,
    // or drop the handle so the accept thread tears down.
    connect_share_switch(panels, toast_overlay, Rc::clone(&running), server_running);

    // Poll `Server::stats()` on a timer, render the status rows,
    // and auto-stop the server if `has_stopped()` becomes true
    // (e.g. USB unplug or accept-thread failure).
    connect_server_status_polling(panels, Rc::clone(&running));

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
/// Previously shared cadence with a server-panel hotplug poll that
/// drove panel visibility — that poll was removed when Share became
/// its own activity icon, but this source-combo poller's 3 s cadence
/// was tuned for the same reason (user plugs in a dongle, sees the
/// slot update by the time they reach for the sidebar) so the value
/// remains a good fit on its own.
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
            // Raw Network (TCP/UDP IQ) has the same wire-bandwidth
            // cost profile as rtl_tcp — a high-sample-rate pull
            // across the network will saturate a 100 Mbit link
            // either way. The advisory applies equally to both
            // network-backed source types.
            let is_network_path = matches!(device_row.selected(), DEVICE_NETWORK | DEVICE_RTLTCP);
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

    // Sample rate selector. Restore-then-wire (#552).
    {
        let persisted_idx = sidebar::source_panel::load_source_sample_rate_index(config);
        if (persisted_idx as usize) < SAMPLE_RATES.len() {
            panels.source.sample_rate_row.set_selected(persisted_idx);
            if let Some(&rate) = SAMPLE_RATES.get(persisted_idx as usize) {
                state.send_dsp(UiToDsp::SetSampleRate(rate));
            }
        }
    }
    let state_sr = Rc::clone(state);
    let config_sr = std::sync::Arc::clone(config);
    let apply_on_sr = apply_source_bandwidth_advisory.clone();
    panels
        .source
        .sample_rate_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting. GTK can briefly emit
            // out-of-range values during widget-model churn (e.g.
            // teardown / rebuild on style changes); persisting
            // those would corrupt the config file across restart.
            // Mirror the protocol_row pattern further down: bail
            // when the index doesn't map to a real sample rate.
            // Per CodeRabbit round 1 on PR #558.
            let Some(&rate) = SAMPLE_RATES.get(idx as usize) else {
                return;
            };
            sidebar::source_panel::save_source_sample_rate_index(&config_sr, idx);
            state_sr.send_dsp(UiToDsp::SetSampleRate(rate));
            apply_on_sr();
        });
    // Source-type (device) selector. Restore-then-wire (#552).
    // The restore SETs the row's selected index, which fires
    // `connect_selected_notify` and thus re-applies the bandwidth
    // advisory; that's intentional (it wires up the correct
    // visibility for the persisted source type at startup). The
    // source-type swap itself is handled by an UPSTREAM
    // `connect_selected_notify` (around the per-source-type
    // visibility block); this handler only wires the persistence
    // save + bandwidth-advisory refresh. The dedicated swap
    // dispatch lives at the end of `connect_source_panel`.
    {
        let persisted_idx = sidebar::source_panel::load_source_device_index(config);
        // Bound check via `DEVICE_RTLTCP` (the highest valid
        // index) — fails closed if a stale config carries an
        // out-of-range value (e.g. a future build added more
        // source types and the user rolled back).
        if persisted_idx <= sidebar::source_panel::DEVICE_RTLTCP {
            panels.source.device_row.set_selected(persisted_idx);
            // Dispatch the restored source type to the DSP so a
            // saved Network / File / RTL-TCP selection takes
            // effect at startup. The change-notify handler that
            // dispatches `SetSourceType` from user clicks is
            // wired AFTER this restore block runs, and even if it
            // were wired first, programmatic `set_selected` to a
            // value that already matches the row's default (0 =
            // RTL-SDR) wouldn't fire it. Explicit dispatch closes
            // both gaps. Per CodeRabbit round 1 on PR #558.
            let source_type = match persisted_idx {
                sidebar::source_panel::DEVICE_RTLSDR => Some(SourceType::RtlSdr),
                sidebar::source_panel::DEVICE_NETWORK => Some(SourceType::Network),
                sidebar::source_panel::DEVICE_FILE => Some(SourceType::File),
                sidebar::source_panel::DEVICE_RTLTCP => Some(SourceType::RtlTcp),
                _ => None,
            };
            if let Some(source_type) = source_type {
                state.send_dsp(UiToDsp::SetSourceType(source_type));
            }
        }
    }
    let config_device = std::sync::Arc::clone(config);
    let apply_on_device = apply_source_bandwidth_advisory;
    panels
        .source
        .device_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (same rationale as the
            // sample-rate row above). `DEVICE_RTLTCP` is the
            // highest valid index. Per CodeRabbit round 1 on
            // PR #558.
            if idx > sidebar::source_panel::DEVICE_RTLTCP {
                return;
            }
            sidebar::source_panel::save_source_device_index(&config_device, idx);
            apply_on_device();
        });

    // DC blocking toggle. Restore-then-wire (#552). Same idiom
    // as bias-T / gain / PPM: programmatic `set_active` fires
    // `connect_active_notify`, which would re-save the loaded
    // value AND re-dispatch `SetDcBlocking` — both cheap, but
    // the duplicate dispatch in tracing logs is misleading. So
    // restore first, then wire.
    {
        let persisted = sidebar::source_panel::load_source_dc_blocking(config);
        panels.source.dc_blocking_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetDcBlocking(persisted));
    }
    let state_dc_block = Rc::clone(state);
    let config_dc_block = std::sync::Arc::clone(config);
    panels
        .source
        .dc_blocking_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_dc_blocking(&config_dc_block, enabled);
            state_dc_block.send_dsp(UiToDsp::SetDcBlocking(enabled));
        });

    // Bias-T toggle (#537). Powers an inline LNA over the
    // RTL-SDR's coax. The startup restore must run BEFORE
    // wiring the change-notify handler — same idiom as the
    // satellites-panel auto-record toggle: a programmatic
    // `set_active` fires `connect_active_notify`, which would
    // otherwise re-save the just-loaded value (cheap) AND
    // dispatch a redundant `SetBiasTee` (also cheap, but
    // misleading in tracing logs).
    {
        let persisted = sidebar::source_panel::load_source_rtl_bias_tee(config);
        panels.source.bias_tee_row.set_active(persisted);
        // Dispatch the persisted value once at startup so the
        // dongle's GPIO matches the UI from the first source
        // open, not just after the user toggles. The
        // `SetBiasTee` handler stores the value in `DspState`
        // up-front, and `open_source` re-applies it to the
        // freshly-opened RTL-SDR source — so this dispatch
        // works regardless of whether a source is open at
        // startup. Per CR on PR #550.
        state.send_dsp(UiToDsp::SetBiasTee(persisted));
    }
    let state_bias_tee = Rc::clone(state);
    let config_bias_tee = std::sync::Arc::clone(config);
    panels
        .source
        .bias_tee_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_rtl_bias_tee(&config_bias_tee, enabled);
            state_bias_tee.send_dsp(UiToDsp::SetBiasTee(enabled));
        });

    // Direct sampling combo (#538). Same restore-then-wire idiom
    // as bias-T above. The persisted value is the combo index
    // (0/1/2), which is also the `rtlsdr_set_direct_sampling`
    // mode argument — cast straight to `i32` for the dispatch.
    {
        let persisted = sidebar::source_panel::load_source_rtl_direct_sampling_mode(config);
        if persisted <= sidebar::source_panel::DIRECT_SAMPLING_MAX_IDX {
            panels.source.direct_sampling_row.set_selected(persisted);
            #[allow(clippy::cast_possible_wrap, reason = "u32 <= 2 fits in i32 trivially")]
            state.send_dsp(UiToDsp::SetDirectSampling(persisted as i32));
        }
    }
    let state_direct = Rc::clone(state);
    let config_direct = std::sync::Arc::clone(config);
    let toast_overlay_direct = toast_overlay.downgrade();
    panels
        .source
        .direct_sampling_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (mirrors the
            // protocol_row / sample-rate / device / decimation
            // early-return-on-invalid pattern). GTK can briefly
            // emit out-of-range values during widget-model
            // churn; persisting them would leave the next
            // restart pinned to a non-existent direct-sampling
            // mode. Per `CodeRabbit` round 3 on PR #558.
            if idx > sidebar::source_panel::DIRECT_SAMPLING_MAX_IDX {
                return;
            }
            sidebar::source_panel::save_source_rtl_direct_sampling_mode(&config_direct, idx);
            #[allow(clippy::cast_possible_wrap, reason = "idx <= 2 fits in i32 trivially")]
            state_direct.send_dsp(UiToDsp::SetDirectSampling(idx as i32));
            // Surface a tune-guidance toast: enabling direct
            // sampling routes the antenna straight to the ADC,
            // which silences VHF/UHF (the R820T tuner is now
            // bypassed); disabling it puts the tuner back in
            // path, which silences HF. Either direction needs a
            // manual retune to be useful, and a toast saves the
            // user from staring at noise wondering why. Per
            // `CodeRabbit` round 1 on PR #559 / closes #538
            // objective.
            if let Some(overlay) = toast_overlay_direct.upgrade() {
                let msg = if idx == sidebar::source_panel::DIRECT_SAMPLING_DISABLED_IDX {
                    "Direct Sampling off — retune to VHF/UHF."
                } else {
                    "Direct Sampling on — retune to an HF frequency (< 28 MHz)."
                };
                overlay.add_toast(adw::Toast::new(msg));
            }
        });

    // Offset tuning toggle (#539). Same restore-then-wire idiom
    // as bias-T above. The controller bridge
    // (`UiToDsp::SetOffsetTuning`) was already plumbed; only
    // wiring is new here.
    //
    // Only DISPATCH the persisted value when it's `true`. The
    // librtlsdr R820T-family branch returns `InvalidParameter`
    // for every `set_offset_tuning` call regardless of value —
    // dispatching `false` at startup (the default for users
    // who've never touched the toggle) generates a spurious
    // "Offset tuning failed" toast on the vast majority of
    // dongles. The driver default already matches `false`, so
    // skipping the dispatch is semantically a no-op. Per issue
    // #564.
    {
        let persisted = sidebar::source_panel::load_source_rtl_offset_tuning(config);
        panels.source.offset_tuning_row.set_active(persisted);
        if persisted {
            state.send_dsp(UiToDsp::SetOffsetTuning(true));
        }
    }
    let state_offset = Rc::clone(state);
    let config_offset = std::sync::Arc::clone(config);
    panels
        .source
        .offset_tuning_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_rtl_offset_tuning(&config_offset, enabled);
            state_offset.send_dsp(UiToDsp::SetOffsetTuning(enabled));
        });

    // IQ inversion toggle. Restore-then-wire (#552).
    {
        let persisted = sidebar::source_panel::load_source_iq_inversion(config);
        panels.source.iq_inversion_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetIqInversion(persisted));
    }
    let state_iq_inv = Rc::clone(state);
    let config_iq_inv = std::sync::Arc::clone(config);
    panels
        .source
        .iq_inversion_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_iq_inversion(&config_iq_inv, enabled);
            state_iq_inv.send_dsp(UiToDsp::SetIqInversion(enabled));
        });

    // Decimation selector. Restore-then-wire (#552). The
    // decimation index also feeds the bandwidth-advisory
    // recompute via `apply_source_bandwidth_advisory`, so
    // restoring here BEFORE wiring keeps the advisory pristine
    // on first launch.
    {
        let persisted_idx = sidebar::source_panel::load_source_decimation_index(config);
        if (persisted_idx as usize) < DECIMATION_FACTORS.len() {
            panels.source.decimation_row.set_selected(persisted_idx);
            if let Some(&factor) = DECIMATION_FACTORS.get(persisted_idx as usize) {
                state.send_dsp(UiToDsp::SetDecimation(factor));
            }
        }
    }
    let state_decim = Rc::clone(state);
    let config_decim = std::sync::Arc::clone(config);
    panels
        .source
        .decimation_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (same rationale as the
            // sample-rate row above). Per CodeRabbit round 1 on
            // PR #558.
            let Some(&factor) = DECIMATION_FACTORS.get(idx as usize) else {
                return;
            };
            sidebar::source_panel::save_source_decimation_index(&config_decim, idx);
            state_decim.send_dsp(UiToDsp::SetDecimation(factor));
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
    // Restore persisted manual gain BEFORE wiring the notify
    // handler — otherwise the programmatic `set_value` fires
    // `connect_value_notify` and the in-flight `set_value`
    // re-dispatches with the freshly-loaded value redundantly.
    // Same idiom as the bias-T restore. Per #551.
    {
        let persisted_gain = sidebar::source_panel::load_source_rtl_gain_db(config);
        panels.source.gain_row.set_value(persisted_gain);
        state.send_dsp(UiToDsp::SetGain(persisted_gain));
    }
    let state_gain = Rc::clone(state);
    let agc_row_for_gain = panels.source.agc_row.downgrade();
    let config_gain = std::sync::Arc::clone(config);
    panels.source.gain_row.connect_value_notify(move |row| {
        // Persist the slider value even when AGC is on — the
        // user's last manual gain should survive an AGC-on /
        // restart / AGC-off cycle. Per #551.
        sidebar::source_panel::save_source_rtl_gain_db(&config_gain, row.value());
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

    // IQ correction toggle. Restore-then-wire (#552).
    {
        let persisted = sidebar::source_panel::load_source_iq_correction(config);
        panels.source.iq_correction_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetIqCorrection(persisted));
    }
    let state_iq_corr = Rc::clone(state);
    let config_iq_corr = std::sync::Arc::clone(config);
    panels
        .source
        .iq_correction_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_iq_correction(&config_iq_corr, enabled);
            state_iq_corr.send_dsp(UiToDsp::SetIqCorrection(enabled));
        });

    // PPM correction. Restore persisted value before wiring
    // the notify handler — same idiom as bias-T / gain. Per
    // #551.
    {
        let persisted_ppm = sidebar::source_panel::load_source_rtl_ppm(config);
        panels.source.ppm_row.set_value(f64::from(persisted_ppm));
        state.send_dsp(UiToDsp::SetPpmCorrection(persisted_ppm));
    }
    let state_ppm = Rc::clone(state);
    let config_ppm = std::sync::Arc::clone(config);
    panels.source.ppm_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        let ppm = row.value() as i32;
        sidebar::source_panel::save_source_rtl_ppm(&config_ppm, ppm);
        state_ppm.send_dsp(UiToDsp::SetPpmCorrection(ppm));
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

    // Raw-Network source config (hostname / port / protocol).
    // Restore all three widgets atomically BEFORE wiring the
    // change-notify handlers, then dispatch one
    // `SetNetworkConfig` with the loaded values so Play picks up
    // the right destination on first launch. Per #552. (rtl_tcp
    // client maintains its own per-server hostname/port via the
    // favorites list — these keys are for the raw IQ-stream
    // Network source only; on a launch where the user was last
    // on rtl_tcp the favorites system also restores its own
    // hostname/port and the two are independent.)
    {
        let hostname = sidebar::source_panel::load_source_network_hostname(config);
        let port = sidebar::source_panel::load_source_network_port(config);
        let protocol_idx = sidebar::source_panel::load_source_network_protocol_index(config);
        panels.source.hostname_row.set_text(&hostname);
        panels.source.port_row.set_value(f64::from(port));
        if protocol_idx == NETWORK_PROTOCOL_UDP_IDX
            || protocol_idx == NETWORK_PROTOCOL_TCPCLIENT_IDX
        {
            panels.source.protocol_row.set_selected(protocol_idx);
        }
        let protocol = if protocol_idx == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        state.send_dsp(UiToDsp::SetNetworkConfig {
            hostname,
            port,
            protocol,
        });
    }

    // Network hostname — send on every edit so Play always has current value
    let state_host = Rc::clone(state);
    let config_host = std::sync::Arc::clone(config);
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
        //
        // Skip the invalidation during RTL-TCP hydration: the
        // startup hydration in `connect_rtl_tcp_discovery`
        // rewrites this row from the last-connected RTL-TCP
        // server (only when the persisted source type is
        // RTL-TCP), and `apply_rtl_tcp_connect` writes the
        // cache *after* the row writes — so an unguarded
        // invalidate would clear the cache the hydration just
        // restored AND blank the auth row before the auth-row
        // handler had a chance to push the saved key. The
        // `apply_rtl_tcp_connect` path handles cache and auth
        // row deterministically itself; we just need to stay
        // out of its way here. Per `CodeRabbit` round 3 on PR
        // #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            invalidate_rtl_tcp_active_server_on_edit(
                &state_host,
                &hostname_for_host,
                &port_for_host,
                &auth_key_for_host,
            );
        }
        let hostname = row.text().to_string();
        // Skip the raw-Network disk-write when this change came
        // from an RTL-TCP hydration. The user's independent
        // raw-Network hostname stays in `KEY_SOURCE_NETWORK_*`
        // and round-trips across restart on its own. Per
        // CodeRabbit round 1 on PR #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            sidebar::source_panel::save_source_network_hostname(&config_host, &hostname);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = port_for_host.value() as u16;
        let protocol = if proto_for_host.selected() == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        // Suppress per-edit `SetNetworkConfig` dispatch while a
        // hydration is rewriting all three rows in sequence. The
        // sequence would otherwise cause three intermediate
        // reconnect attempts (one per row), each against a
        // partially-rewritten triple. `apply_rtl_tcp_connect`
        // dispatches a single canonical `SetNetworkConfig` after
        // clearing the flag, so the final state still reaches
        // the DSP. Per `CodeRabbit` round 2 on PR #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            state_host.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        }
    });

    // Network port
    let state_port = Rc::clone(state);
    let config_port = std::sync::Arc::clone(config);
    let host_for_port = panels.source.hostname_row.clone();
    let proto_for_port = panels.source.protocol_row.clone();
    let port_row_for_port = panels.source.port_row.clone();
    let auth_key_for_port = panels.source.rtl_tcp_auth_key_row.clone();
    panels.source.port_row.connect_value_notify(move |row| {
        // Skip the invalidation during RTL-TCP hydration; see
        // hostname handler above for the rationale. Per
        // `CodeRabbit` round 3 on PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            invalidate_rtl_tcp_active_server_on_edit(
                &state_port,
                &host_for_port,
                &port_row_for_port,
                &auth_key_for_port,
            );
        }
        let hostname = host_for_port.text().to_string();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = row.value() as u16;
        // Skip the raw-Network disk-write during RTL-TCP
        // hydration; see hostname handler above. Per CodeRabbit
        // round 1 on PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            sidebar::source_panel::save_source_network_port(&config_port, port);
        }
        let protocol = if proto_for_port.selected() == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        // Suppress per-edit dispatch during hydration; see
        // hostname handler above. Per `CodeRabbit` round 2 on
        // PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            state_port.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        }
    });

    // Network protocol
    let state_proto = Rc::clone(state);
    let config_proto = std::sync::Arc::clone(config);
    let host_for_proto = panels.source.hostname_row.clone();
    let port_for_proto = panels.source.port_row.clone();
    panels
        .source
        .protocol_row
        .connect_selected_notify(move |row| {
            let hostname = host_for_proto.text().to_string();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let port = port_for_proto.value() as u16;
            let selected = row.selected();
            // Validate the selected index BEFORE persisting so a
            // transient out-of-range value during widget churn
            // can't land in config (matches the sample-rate /
            // device / decimation handlers' early-return pattern).
            // Per `CodeRabbit` round 3 on PR #558.
            let protocol = match selected {
                NETWORK_PROTOCOL_TCPCLIENT_IDX => sdr_types::Protocol::TcpClient,
                NETWORK_PROTOCOL_UDP_IDX => sdr_types::Protocol::Udp,
                _ => return, // ignore transient indices
            };
            // Skip the raw-Network disk-write during RTL-TCP
            // hydration; see hostname handler above. Per
            // `CodeRabbit` round 1 on PR #558.
            if !state_proto.rtl_tcp_hydration_in_progress.get() {
                sidebar::source_panel::save_source_network_protocol_index(&config_proto, selected);
            }
            // Suppress per-edit dispatch during hydration; see
            // hostname handler above. Per `CodeRabbit` round 2 on
            // PR #558.
            if !state_proto.rtl_tcp_hydration_in_progress.get() {
                state_proto.send_dsp(UiToDsp::SetNetworkConfig {
                    hostname,
                    port,
                    protocol,
                });
            }
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

    // File path — send on every edit so Play always has current
    // value. Restore-then-wire (#552). Empty saved string is the
    // default and means "no file selected" — re-set the widget
    // to empty too so the placeholder stays correct.
    {
        let persisted = sidebar::source_panel::load_source_file_path(config);
        panels.source.file_path_row.set_text(&persisted);
        state.send_dsp(UiToDsp::SetFilePath(std::path::PathBuf::from(&persisted)));
    }
    let state_file = Rc::clone(state);
    let config_file = std::sync::Arc::clone(config);
    panels.source.file_path_row.connect_changed(move |row| {
        let text = row.text().to_string();
        sidebar::source_panel::save_source_file_path(&config_file, &text);
        state_file.send_dsp(UiToDsp::SetFilePath(std::path::PathBuf::from(text)));
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

/// Retune the bandwidth `AdwSpinRow`'s allowed range to the
/// active demod's `[min_bandwidth, max_bandwidth]`. Called
/// whenever the demod mode changes so the row can't accept
/// values the demod will silently reject — that mismatch was
/// the root cause of issue #505 (audio stutter at panel-
/// bandwidth > per-mode max). The panel-level constants
/// `MIN_BANDWIDTH_HZ` / `MAX_BANDWIDTH_HZ` set the absolute
/// envelope the row can ever cover (covers WFM's full
/// 1-250 kHz range); this helper narrows it to the active
/// mode's actual range on every demod change.
///
/// Also clamps the row's current value into the new range —
/// without that, switching from WFM at 200 kHz to NFM would
/// leave the displayed 200 kHz reading stale until the user
/// manually adjusts.
///
/// **Self-suppresses `value-notify` around the auto-clamp
/// `set_value`.** Without that suppression, the clamp would
/// route through the spin-row's `connect_value_notify` handler
/// — which is the MANUAL-bandwidth-change path. That path
/// fires `force_disable.trigger("manual bandwidth change")`,
/// which would stop the scanner mid-retune (the scanner-driven
/// mode-change path calls this helper before its own
/// `set_value`), and dispatch a redundant `SetBandwidth`
/// command. Per `CodeRabbit` round 1 on PR #548. The clamp is
/// programmatic (the UI snapping to a mode change), not user
/// input, so no manual-side effects should fire.
///
/// The DSP doesn't need to be told about the clamp — the
/// caller is responsible for sending its own `SetBandwidth`
/// (or, in the DSP-echo case, the controller already changed
/// the bandwidth as part of the mode change).
fn update_bandwidth_row_range_for_mode(
    radio: &sidebar::radio_panel::RadioPanel,
    state: &AppState,
    mode: sdr_types::DemodMode,
) {
    let Ok(min_bw) = sdr_radio::demod::min_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "min_bandwidth_for_mode failed — leaving bandwidth row range unchanged"
        );
        return;
    };
    let Ok(max_bw) = sdr_radio::demod::max_bandwidth_for_mode(mode) else {
        tracing::warn!(
            ?mode,
            "max_bandwidth_for_mode failed — leaving bandwidth row range unchanged"
        );
        return;
    };
    let adj = radio.bandwidth_row.adjustment();
    adj.set_lower(min_bw);
    adj.set_upper(max_bw);
    let current = radio.bandwidth_row.value();
    let target = if current < min_bw {
        Some(min_bw)
    } else if current > max_bw {
        Some(max_bw)
    } else {
        None
    };
    if let Some(new_value) = target {
        // Suppress only around the actual `set_value` — keep
        // the suppress window as narrow as possible so a
        // genuinely-user-driven `value-notify` racing the GTK
        // main loop can't accidentally get swallowed.
        state.suppress_bandwidth_notify.set(true);
        radio.bandwidth_row.set_value(new_value);
        state.suppress_bandwidth_notify.set(false);
    }
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
    volume_button: &gtk4::ScaleButton,
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
        // Route through the header `ScaleButton` so the restored
        // level flows through the single source of truth
        // `connect_volume_persistence` established: the button's
        // `value_changed` handler dispatches `SetVolume`, writes
        // `KEY_AUDIO_VOLUME`, and mirrors into the audio panel's
        // `volume_row`. Calling `send_dsp(SetVolume(vol))` directly
        // here would leave the button + audio row + persisted key
        // showing stale state until the next user edit flicked
        // them back. `set_value` fires the handler only if the new
        // value differs from the current one — same idempotency
        // story as the gain row above.
        #[allow(clippy::cast_lossless)]
        volume_button.set_value(vol as f64);
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
    volume_button: &gtk4::ScaleButton,
) {
    // Navigation callback: restore full tuning profile from bookmark.
    let state_nav = Rc::clone(state);
    let fs = freq_selector.clone();
    // Strong clone — single-threaded GTK main loop, the closure
    // outlives the dropdown only at teardown which drops both at
    // once. Pre-#509 this was a `WeakRef` upgraded inside the
    // closure; `tune_to_target` takes `&gtk4::DropDown`, and the
    // strong clone keeps the call shape uniform with the satellite
    // closure (which has always held a strong clone).
    let dd = demod_dropdown.clone();
    let sb = Rc::clone(status_bar);
    let spectrum_nav = Rc::clone(spectrum_handle);
    let radio_nav = panels.radio.clone();
    let source_nav_gain = panels.source.gain_row.clone();
    let source_nav_agc = panels.source.agc_row.clone();
    let force_disable_nav = Rc::clone(scanner_force_disable);
    let volume_button_nav = volume_button.clone();
    let bandwidth_row_nav = panels.radio.bandwidth_row.clone();

    panels.bookmarks.connect_navigate(move |bookmark| {
        // Both bookmark recall AND band-preset selection come in
        // through this callback (the preset handler in
        // `connect_preset_to_bookmarks` invokes `on_navigate` with
        // a synthesized Bookmark). Keep the toast reason neutral
        // so a preset click doesn't claim "bookmark recall".
        let freq = bookmark.frequency;
        let mode = sidebar::navigation_panel::parse_demod_mode(&bookmark.demod_mode);
        let bw = bookmark.bandwidth;

        // Canonical 13-step mirror sequence — single source of
        // truth shared with the satellite tune path. Per #509.
        tune_to_target(
            &state_nav,
            &fs,
            &dd,
            &spectrum_nav,
            &force_disable_nav,
            &bandwidth_row_nav,
            &radio_nav,
            &sb,
            freq,
            mode,
            bw,
            "preset/bookmark selection",
        );

        // Restore optional tuning-profile settings (squelch, gain,
        // etc.). Bookmark-specific layer on top of the canonical
        // mirror sequence — auto-record / satellite play don't
        // need this.
        restore_bookmark_profile(
            bookmark,
            &state_nav,
            &radio_nav,
            &source_nav_gain,
            &source_nav_agc,
            &volume_button_nav,
        );

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

/// Epsilon (in fractional volume units, i.e. `[0.0, 1.0]`) below
/// which the mirrored volume widgets are considered "already at the
/// target" and the sync side skips its `set_value` call. Prevents
/// floating-point round-trip artefacts from causing a trivial
/// mirror loop between the header `GtkScaleButton` (0.0..=1.0 step
/// 0.05) and the audio panel `AdwSpinRow` (0..=100 step 1 →
/// 0.01-per-step when scaled). A half-step worth of slack sits
/// comfortably below the smallest user-perceptible change.
const VOLUME_SYNC_EPSILON: f64 = 0.005;

/// Wire volume persistence (closes #419) and two-way sync between
/// the header `GtkScaleButton` and the audio panel's
/// `volume_row` `AdwSpinRow`.
///
/// The header button is the single source of truth: its
/// `connect_value_changed` handler is the ONLY path that dispatches
/// `UiToDsp::SetVolume` and writes to the config. The audio-panel
/// row drives the button via `set_value` — its own
/// `connect_value_notify` just mirrors into the button and lets the
/// button's handler do the real work. That keeps one handler owning
/// dispatch + persist, and the mirror path stays idempotent.
///
/// Startup ordering (load-bearing):
///   1. Seed both widgets with the saved volume (no handlers yet,
///      so no dispatch or cascade).
///   2. Explicit `state.send_dsp(UiToDsp::SetVolume(saved))` —
///      guarantees the DSP starts at the restored level regardless
///      of `ScaleButton::set_value` being a no-op on same-value
///      (closes #424's "no 1-frame blast while config loads"
///      requirement).
///   3. Wire the handlers.
///
/// Any other code path that mutates volume (bookmark recall,
/// preferences restore, etc.) must go through
/// `volume_button.set_value(vol)` so this handler runs — direct
/// `send_dsp(SetVolume(..))` would leave the button / row / config
/// showing stale state until the user's next edit.
fn connect_volume_persistence(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    volume_button: &gtk4::ScaleButton,
) {
    let saved_volume = config.read(|v| {
        v.get(sidebar::audio_panel::KEY_AUDIO_VOLUME)
            .and_then(serde_json::Value::as_f64)
            .map_or(1.0, |f| f.clamp(0.0, 1.0))
    });

    // Seed both widgets BEFORE wiring handlers. Setting values
    // first means the initial state isn't observed as a
    // "user change" — no handlers fire, no duplicate dispatch,
    // no mirror-path cascade. Dispatch is done explicitly below.
    volume_button.set_value(saved_volume);
    panels
        .audio
        .volume_row
        .set_value(saved_volume * sidebar::audio_panel::VOLUME_PERCENT_MAX);

    // Guaranteed initial dispatch to the DSP so audio starts at
    // the restored level regardless of how `ScaleButton::set_value`
    // interacts with its default (closes #424's "no 1-frame blast
    // while config loads" requirement).
    #[allow(clippy::cast_possible_truncation)]
    state.send_dsp(UiToDsp::SetVolume(saved_volume as f32));

    // Button is the single source of truth: its handler owns
    // dispatch + persist + mirror-to-row.
    let state_vol = Rc::clone(state);
    let config_vol = std::sync::Arc::clone(config);
    let volume_row_weak = panels.audio.volume_row.downgrade();
    volume_button.connect_value_changed(move |_btn, value| {
        // Audio-panel slider mirror runs unconditionally so the
        // header `ScaleButton` stays the single source of truth
        // for the audio panel's percent slider — including
        // during the ACARS programmatic mute/restore where
        // dispatch + persist are suppressed below. Without
        // mirroring here, the panel slider would still show the
        // pre-mute percent while the header sits at 0.0, and a
        // user touching the panel slider could fight ACARS by
        // dispatching the stale value. CR round 1 on PR #590.
        if let Some(row) = volume_row_weak.upgrade() {
            let target_pct = value * sidebar::audio_panel::VOLUME_PERCENT_MAX;
            if (row.value() - target_pct).abs()
                > VOLUME_SYNC_EPSILON * sidebar::audio_panel::VOLUME_PERCENT_MAX
            {
                row.set_value(target_pct);
            }
        }
        // Suppress fires when the ACARS engage path programmatically
        // sets the value to 0 (or restores it on disengage); without
        // this guard the auto-mute would persist 0.0 to config and
        // double-dispatch SetVolume. The engage / disengage arms in
        // `handle_dsp_message` take responsibility for the explicit
        // SetVolume dispatch. Mirrors `suppress_bandwidth_notify` /
        // `suppress_demod_notify`.
        if state_vol.suppress_volume_notify.get() {
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        state_vol.send_dsp(UiToDsp::SetVolume(value as f32));
        config_vol.write(|v| {
            v[sidebar::audio_panel::KEY_AUDIO_VOLUME] = serde_json::json!(value);
        });
    });

    // Audio panel row is mirror-only. Drives the button, which
    // runs the dispatch + config write. The idempotency check
    // breaks the `btn.set_value → row.set_value → btn.set_value`
    // loop when the two widgets are already in sync.
    let volume_button_weak = volume_button.downgrade();
    panels.audio.volume_row.connect_value_notify(move |row| {
        let value = (row.value() / sidebar::audio_panel::VOLUME_PERCENT_MAX).clamp(0.0, 1.0);
        if let Some(btn) = volume_button_weak.upgrade()
            && (btn.value() - value).abs() > VOLUME_SYNC_EPSILON
        {
            btn.set_value(value);
        }
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
    let network_group = panels.audio.network_sink_group.clone();
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
            // Toggle the whole Network-sink section instead of its
            // four rows individually — same pattern as the Radio
            // panel's De-emphasis / CTCSS group-level hides.
            network_group.set_visible(network_visible);
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

/// Load saved transmitter ERP and receiver calibration offset
/// into the Radio panel's FSPL distance estimator rows, and wire
/// value-change handlers to persist any edits back to the config
/// (ticket #164). The distance display refresh wiring lives
/// inside `build_radio_panel` — this function is only about
/// config ↔ row synchronisation.
fn connect_distance_estimator_persistence(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use sidebar::radio_panel::{KEY_RADIO_DISTANCE_CALIBRATION_DB, KEY_RADIO_DISTANCE_ERP_WATTS};

    // Seed the rows with the saved values (clamped to the spin
    // rows' own adjustment bounds via their `set_value`). The
    // default spin row values were already applied at
    // `build_radio_panel` time, so `config.read` here only
    // overrides when a saved value exists.
    let saved_erp = config.read(|v| {
        v.get(KEY_RADIO_DISTANCE_ERP_WATTS)
            .and_then(serde_json::Value::as_f64)
    });
    let saved_cal = config.read(|v| {
        v.get(KEY_RADIO_DISTANCE_CALIBRATION_DB)
            .and_then(serde_json::Value::as_f64)
    });
    if let Some(erp) = saved_erp {
        panels.radio.erp_row.set_value(erp);
    }
    if let Some(cal) = saved_cal {
        panels.radio.calibration_row.set_value(cal);
    }

    // Persist-on-change. Uses `value_notify` (not the adjustment's
    // `value_changed`) to match the signal the in-panel distance
    // refresh handler is already listening to — both fire on the
    // same user edit.
    let config_erp = std::sync::Arc::clone(config);
    panels.radio.erp_row.connect_value_notify(move |row| {
        config_erp.write(|v| {
            v[KEY_RADIO_DISTANCE_ERP_WATTS] = serde_json::json!(row.value());
        });
    });
    let config_cal = std::sync::Arc::clone(config);
    panels
        .radio
        .calibration_row
        .connect_value_notify(move |row| {
            config_cal.write(|v| {
                v[KEY_RADIO_DISTANCE_CALIBRATION_DB] = serde_json::json!(row.value());
            });
        });
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
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
) {
    let scanner = &panels.scanner;

    // Master switch → SetScannerEnabled. Using `connect_active_notify`
    // (not `connect_state_set`) so programmatic toggles fire too:
    //   - F8 shortcut calls `set_active` which changes the active
    //     property and fires notify::active.
    //   - `ScannerForceDisable::trigger` calls `set_active(false)`
    //     on the same switch for manual-tune force-disable.
    //   - DSP-origin widget syncs (ScannerEmptyRotation,
    //     ScannerMutexStopped::ScannerStopped*) call
    //     `set_active(false)` so notify::active fires here.
    //     `set_state(false)` would NOT trigger this handler —
    //     per GtkSwitch semantics, `state` and `active` are
    //     separate properties; `set_state` fires only
    //     notify::state. The previous comment claimed
    //     otherwise; corrected per `CodeRabbit` round 3 on PR
    //     #562 once the post-stop scanner-axis-lock teardown
    //     started depending on this handler running.
    //     The resulting redundant `SetScannerEnabled(false)`
    //     dispatch is idempotent at the engine — it's cheaper
    //     to pay one extra message per event than to add a
    //     suppress flag for every DSP-origin sync site.
    // Master switch dispatches `SetScannerEnabled` AND drives
    // the spectrum's scanner-axis lock. On enable, compute the
    // (min, max) envelope of all scanner-flagged bookmarks and
    // push it to the spectrum so the X axis pins to that range
    // until the scanner stops. On disable, clear the lock so
    // the spectrum reverts to "current channel ± half BW".
    // The display panel's status row mirrors the lock state via
    // the `update_scanner_axis_status_row` helper. Per issue
    // #516.
    let state_switch = Rc::clone(state);
    let bookmarks_for_switch = Rc::clone(&panels.bookmarks);
    let config_for_switch = std::sync::Arc::clone(config);
    let spectrum_for_switch = Rc::clone(spectrum_handle);
    let display_axis_row = panels.display.scanner_axis_row.clone();
    scanner.master_switch.connect_active_notify(move |sw| {
        let enabled = sw.is_active();
        state_switch.send_dsp(UiToDsp::SetScannerEnabled(enabled));
        if enabled {
            // Compute envelope from the LIVE bookmark list so
            // mid-scan scan-flag toggles + adds/deletes pick up
            // on the next master-switch flip. The same helper
            // also fires from the bookmark mutation callback to
            // refresh while the scanner is already running.
            // Outcome is irrelevant here: this enable path
            // engages the lock from a clean slate (no prior
            // active to drop), so `ActiveChannelDropped` can't
            // fire. Per issue #516.
            let _ = refresh_scanner_axis_lock(
                &bookmarks_for_switch.bookmarks.borrow(),
                &config_for_switch,
                &spectrum_for_switch,
                &display_axis_row,
            );
        } else {
            spectrum_for_switch.exit_scanner_mode();
            update_scanner_axis_status_row(&display_axis_row, None);
        }
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

/// Wire the Satellites scheduler panel to its config-persistence
/// layer, the [`sdr_sat::TleCache`], and a 1 Hz countdown timer.
///
/// Two pieces of shared state plumb the handlers together:
///
/// * `displayed: Rc<RefCell<Vec<DisplayedPass>>>` — the list of
///   pass rows currently in `passes_group`. Walked by the 1 Hz
///   ticker (to update title-line countdowns) and rebuilt by
///   `recompute` whenever lat/lon/alt changes or a TLE refresh
///   completes.
/// * `cache: Arc<TleCache>` — `Arc` (not `Rc`) because the
///   refresh button hands a clone to `gio::spawn_blocking`, which
///   requires `Send`. `TleCache` is `Send + Sync`.
///
/// The 1 Hz timer holds a `glib::WeakRef<adw::PreferencesGroup>`
/// to the passes group so it returns `ControlFlow::Break` once
/// the window is destroyed; same lifecycle pattern as the DSP
/// poll loop.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn connect_satellites_panel(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    state: &Rc<AppState>,
    toast_overlay: &adw::ToastOverlay,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    tune_to_satellite: &Rc<dyn Fn(u64, sdr_types::DemodMode, u32)>,
    set_playing: &Rc<dyn Fn(bool)>,
    status_bar: &Rc<StatusBar>,
) {
    use sdr_sat::{GroundStation, KNOWN_SATELLITES, Pass, TleCache};
    use sidebar::satellites_notify::{Action as NotifyAction, NotifyScheduler};
    use sidebar::satellites_panel::{
        AutoRecordQuality, KEY_STATION_ALT_M, KEY_STATION_LAT_DEG, KEY_STATION_LON_DEG,
        SatellitesPanelWeak, enumerate_upcoming_passes, format_downlink_mhz, format_last_refresh,
        format_pass_subtitle, format_pass_title, load_auto_record_apt, load_auto_record_audio,
        load_auto_record_composites, load_auto_record_quality, load_notify_lead_min,
        load_station_alt_m, load_station_lat_deg, load_station_lon_deg, load_watched_satellites,
        norad_id_for_pass, save_auto_record_apt, save_auto_record_audio,
        save_auto_record_composites, save_f64, save_tle_last_refresh, save_watched_satellites,
        tune_target_for_pass,
    };
    use sidebar::satellites_recorder::{
        Action as RecorderAction, AutoRecorder, SavedTune, ToastKind,
    };

    // One pass row + its source `Pass` so the 1 Hz ticker can
    // refresh the title without re-running pass enumeration. The
    // optional bell `ToggleButton` is held so the watch-toggle
    // handler can mirror the active state across every row whose
    // satellite matches — multiple NOAA 19 passes in the visible
    // list must all reflect the same subscription state. `None`
    // for off-catalog passes (no NORAD id → no notify).
    struct DisplayedPass {
        row: adw::ActionRow,
        pass: Pass,
        bell_btn: Option<gtk4::ToggleButton>,
    }

    // Borrow the panel for synchronous setup, then capture only
    // weak refs in long-lived closures. Cloning the strong panel
    // into a closure stored on its own widget creates a refcount
    // cycle (widget → handler → closure → cloned panel → widget)
    // that prevents teardown — see `SatellitesPanelWeak`'s doc for
    // the full chain.
    let panel = &panels.satellites;
    let panel_weak: SatellitesPanelWeak = panel.downgrade();

    // Restore persisted values BEFORE wiring change-notify handlers,
    // matching the scanner-panel pattern: `set_value` on a SpinRow
    // fires `value-changed`, so wiring first would trigger spurious
    // saves + recomputes during window construction.
    panel.lat_row.set_value(load_station_lat_deg(config));
    panel.lon_row.set_value(load_station_lon_deg(config));
    panel.alt_row.set_value(load_station_alt_m(config));
    panel
        .notify_lead_row
        .set_value(f64::from(load_notify_lead_min(config)));
    panel
        .auto_record_switch
        .set_active(load_auto_record_apt(config));
    panel
        .auto_record_audio_switch
        .set_active(load_auto_record_audio(config));
    panel
        .auto_record_composites_switch
        .set_active(load_auto_record_composites(config));
    let initial_quality = load_auto_record_quality(config);
    panel
        .auto_record_quality_row
        .set_selected(initial_quality.to_index());
    // Sensitivity is wired in `build_satellites_panel` via the
    // auto-record switch's `connect_active_notify` handler — it
    // fires on every toggle, including the one triggered above
    // by `auto_record_switch.set_active(load_auto_record_apt(...))`,
    // so the persisted switch state propagates to the combo's
    // sensitivity automatically. No re-sync needed here. Per CR
    // round 2 on PR #574.
    panel
        .last_refresh_row
        .set_subtitle(&format_last_refresh(config));

    {
        let config_lead = std::sync::Arc::clone(config);
        panel.notify_lead_row.connect_value_notify(move |row| {
            #[allow(
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "SpinRow is bounded NOTIFY_LEAD_MIN_LOWER..=UPPER \
                          (positive, < u32::MAX)"
            )]
            let value = row.value().round() as u32;
            sidebar::satellites_panel::save_notify_lead_min(&config_lead, value);
        });
    }

    // `Option<Arc<TleCache>>`. `None` means the platform refused us
    // a cache directory (rare; sandboxed minimal environments).
    // Disable TLE-specific UI but keep ground-station persistence,
    // ZIP lookup, and the auto-record toggle wired — those don't
    // depend on TLEs and shouldn't go inert just because the cache
    // is gone.
    let cache: Option<std::sync::Arc<TleCache>> = match TleCache::new() {
        Ok(c) => Some(std::sync::Arc::new(c)),
        Err(e) => {
            tracing::warn!("Satellites panel: TLE cache unavailable — {e}");
            panel.refresh_button.set_sensitive(false);
            panel
                .last_refresh_row
                .set_subtitle("Cache directory unavailable");
            None
        }
    };

    let displayed: Rc<RefCell<Vec<DisplayedPass>>> = Rc::new(RefCell::new(Vec::new()));

    // #510 — per-satellite watched-set + notify scheduler. Loaded
    // from config so the user's selections survive restarts. The
    // set is mutated from two sites: (a) the bell toggle on each
    // pass row (write-through to config); (b) read-only by the
    // 1 Hz tick that drives the scheduler.
    let watched: Rc<RefCell<std::collections::HashSet<u32>>> =
        Rc::new(RefCell::new(load_watched_satellites(config)));
    let notify_scheduler: Rc<RefCell<NotifyScheduler>> =
        Rc::new(RefCell::new(NotifyScheduler::new()));

    // `recompute` is built unconditionally — when the cache is
    // unavailable it's a no-op so the lat/lon/alt notify handlers
    // can call it without branching. When the cache is available
    // it does the real pass-enumeration + row-rebuild work.
    let recompute: Rc<dyn Fn()> = if let Some(cache) = cache.as_ref() {
        let cache_recompute = std::sync::Arc::clone(cache);
        let panel_weak_recompute = panel_weak.clone();
        let displayed_recompute = Rc::clone(&displayed);
        let tune_for_recompute = Rc::clone(tune_to_satellite);
        let watched_for_recompute = Rc::clone(&watched);
        let config_for_recompute = std::sync::Arc::clone(config);
        Rc::new(move || {
            let Some(panel) = panel_weak_recompute.upgrade() else {
                return;
            };
            // Drop the previous pass rows — these are throwaway,
            // built fresh per recompute.
            for entry in displayed_recompute.borrow_mut().drain(..) {
                panel.passes_group.remove(&entry.row);
            }
            // `passes_status_row` is the always-present empty-state
            // placeholder. We toggle its *visibility* rather than
            // detach + reattach. Once a widget is unparented, its
            // last strong ref lives only on the SatellitesPanel
            // struct — and that struct is dropped when
            // `build_window` returns. Toggling visibility keeps the
            // row parented (and therefore alive) for the lifetime
            // of the window.
            let station = GroundStation::new(
                panel.lat_row.value(),
                panel.lon_row.value(),
                panel.alt_row.value(),
            );
            let now = chrono::Utc::now();
            let passes = enumerate_upcoming_passes(&cache_recompute, &station, now);

            if passes.is_empty() {
                panel.passes_status_row.set_visible(true);
                return;
            }

            panel.passes_status_row.set_visible(false);
            let mut new_rows = Vec::with_capacity(passes.len());
            for pass in passes {
                let row = adw::ActionRow::builder()
                    .title(format_pass_title(&pass, now))
                    .subtitle(format_pass_subtitle(&pass))
                    .build();
                // Per-row play button — one-click tune to the
                // satellite's downlink with the right demod / BW.
                // Skipped when the satellite isn't in the catalog
                // (impossible in practice but the lookup type is
                // `Option`, so we fail closed — no button rather
                // than a button that does nothing).
                // Per-row play button: ignore the 4th element
                // (`Option<ImagingProtocol>`) — manual tune is a
                // user-initiated action and works on any catalog
                // entry. Only the auto-record path filters on
                // `Some(protocol)`.
                if let Some((freq_hz, mode, bw_hz, _protocol, _norad_id)) =
                    tune_target_for_pass(&pass)
                {
                    let play_btn = gtk4::Button::builder()
                        .icon_name("media-playback-start-symbolic")
                        .tooltip_text(format!(
                            "Tune to {} ({})",
                            pass.satellite,
                            format_downlink_mhz(freq_hz),
                        ))
                        .valign(gtk4::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    // Tooltips aren't read by screen readers — set
                    // the accessible label too, matching the
                    // project rule for icon-only buttons.
                    let a11y_label = format!("Tune to {} downlink", pass.satellite);
                    play_btn
                        .update_property(&[gtk4::accessible::Property::Label(a11y_label.as_str())]);
                    let tune_for_click = Rc::clone(&tune_for_recompute);
                    play_btn.connect_clicked(move |_| {
                        tune_for_click(freq_hz, mode, bw_hz);
                    });
                    row.add_suffix(&play_btn);
                }
                // 🔔 watch-toggle (#510) — per-satellite, NOT
                // per-pass. Toggling on row N flips the user's
                // subscription for THIS satellite. Mirrored across
                // sibling rows in the toggle handler so two rows
                // of the same satellite (NOAA 19 typically has 4-6
                // passes per day) stay in sync. `None` for
                // off-catalog passes — no NORAD id, no
                // notification target, no button.
                let bell_btn = if let Some(norad_id) = norad_id_for_pass(&pass) {
                    let initial_active = watched_for_recompute.borrow().contains(&norad_id);
                    let bell_btn = gtk4::ToggleButton::builder()
                        .icon_name("alarm-symbolic")
                        .active(initial_active)
                        .tooltip_text(format!(
                            "Notify before {} passes (T-pre-pass alert)",
                            pass.satellite,
                        ))
                        .valign(gtk4::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    let a11y_label = format!("Notify before {} passes", pass.satellite);
                    bell_btn
                        .update_property(&[gtk4::accessible::Property::Label(a11y_label.as_str())]);
                    let watched_for_toggle = Rc::clone(&watched_for_recompute);
                    let config_for_toggle = std::sync::Arc::clone(&config_for_recompute);
                    // `Weak` (not strong `Rc`) breaks the cycle:
                    // bell_btn → handler closure → Rc → Vec
                    // <DisplayedPass> → bell_btn. With a strong ref
                    // here, removing rows from `passes_group`
                    // wouldn't drop the bell_btn, which would keep
                    // the closure (and the Vec) pinned forever.
                    let displayed_for_toggle = Rc::downgrade(&displayed_recompute);
                    bell_btn.connect_toggled(move |b| {
                        let active = b.is_active();
                        {
                            let mut set = watched_for_toggle.borrow_mut();
                            // `HashSet::insert` / `HashSet::remove`
                            // return whether membership actually
                            // changed. Skip the config write when it
                            // didn't — sibling-mirror re-enters this
                            // handler for every other row of the
                            // same satellite, and without the guard
                            // every mirror would issue an identical
                            // save_watched_satellites call. Per CR
                            // round 3 on PR #568.
                            let changed = if active {
                                set.insert(norad_id)
                            } else {
                                set.remove(&norad_id)
                            };
                            if changed {
                                save_watched_satellites(&config_for_toggle, &set);
                            }
                        }
                        // Mirror across sibling rows. `set_active`
                        // is a no-op when the state already matches,
                        // so the recursion terminates after one
                        // round-trip per sibling. The pointer
                        // compare keeps us from re-entering THIS
                        // button's own handler. If the displayed
                        // Vec has already been dropped (window
                        // teardown), the upgrade fails and we
                        // simply skip mirroring — the watched-set
                        // write above is the only persistent
                        // effect that matters at that point.
                        //
                        // Match siblings by NORAD id, not display
                        // name: the watched set is keyed by id, and
                        // any future catalog drift where two entries
                        // share a label (alternate names, alias
                        // entries) would otherwise toggle the wrong
                        // satellite's bells. Per CR round 1 on PR
                        // #568.
                        let Some(displayed) = displayed_for_toggle.upgrade() else {
                            return;
                        };
                        for entry in displayed.borrow().iter() {
                            if norad_id_for_pass(&entry.pass) == Some(norad_id)
                                && let Some(other) = &entry.bell_btn
                                && other.as_ptr() != b.as_ptr()
                                && other.is_active() != active
                            {
                                other.set_active(active);
                            }
                        }
                    });
                    row.add_suffix(&bell_btn);
                    Some(bell_btn)
                } else {
                    None
                };
                panel.passes_group.add(&row);
                new_rows.push(DisplayedPass {
                    row,
                    pass,
                    bell_btn,
                });
            }
            *displayed_recompute.borrow_mut() = new_rows;
        })
    } else {
        // No cache → no enumeration. Lat/lon/alt notify handlers
        // still call this on every change; making it a no-op
        // keeps the call sites branch-free.
        Rc::new(|| {})
    };

    // Initial paint — show passes immediately if we already have
    // cached TLEs from a prior session. (No-op if cache is None.)
    recompute();

    // Lat / lon / alt — persist on change and re-run pass
    // enumeration. Cheap: a single SGP4 sweep across ~7
    // satellites takes well under a millisecond.
    {
        let config_lat = std::sync::Arc::clone(config);
        let recompute_lat = Rc::clone(&recompute);
        panel.lat_row.connect_value_notify(move |row| {
            save_f64(&config_lat, KEY_STATION_LAT_DEG, row.value());
            recompute_lat();
        });
    }
    {
        let config_lon = std::sync::Arc::clone(config);
        let recompute_lon = Rc::clone(&recompute);
        panel.lon_row.connect_value_notify(move |row| {
            save_f64(&config_lon, KEY_STATION_LON_DEG, row.value());
            recompute_lon();
        });
    }
    {
        let config_alt = std::sync::Arc::clone(config);
        let recompute_alt = Rc::clone(&recompute);
        panel.alt_row.connect_value_notify(move |row| {
            save_f64(&config_alt, KEY_STATION_ALT_M, row.value());
            recompute_alt();
        });
    }

    // Auto-record toggle — persist only. The actual "tune the
    // radio + start APT decoding when a NOAA pass starts" wiring
    // lands in #482 and reads from the same config key.
    {
        let config_auto = std::sync::Arc::clone(config);
        panel.auto_record_switch.connect_active_notify(move |sw| {
            save_auto_record_apt(&config_auto, sw.is_active());
        });
    }

    // "Also save audio" toggle — persist only. The recorder's
    // 1 Hz tick samples this switch's `is_active()` at AOS.
    // Per #533.
    {
        let config_audio = std::sync::Arc::clone(config);
        panel
            .auto_record_audio_switch
            .connect_active_notify(move |sw| {
                save_auto_record_audio(&config_audio, sw.is_active());
            });
    }

    // "Save false-colour composites" toggle — persist only. The
    // `RecorderAction::SaveLrptPass` handler reads
    // `panel.auto_record_composites_switch.is_active()` at LOS
    // (mirrors the audio-save sampling pattern — flipping
    // mid-pass doesn't retroactively start or stop anything).
    // Per #547.
    {
        let config_comp = std::sync::Arc::clone(config);
        panel
            .auto_record_composites_switch
            .connect_active_notify(move |sw| {
                save_auto_record_composites(&config_comp, sw.is_active());
                tracing::info!(on = sw.is_active(), "auto_record_composites persisted");
            });
    }

    // Persist the quality threshold on change via the symmetric
    // writer. Validating through `AutoRecordQuality::from_index`
    // before the write protects the config against transient
    // out-of-range indices that GTK can emit during model churn —
    // an unrecognized value would round-trip back to the default
    // tier on the next read otherwise. Per CR round 1 on PR #574.
    {
        let config_quality = std::sync::Arc::clone(config);
        panel
            .auto_record_quality_row
            .connect_selected_notify(move |row| {
                let raw = row.selected();
                let quality = crate::sidebar::satellites_panel::AutoRecordQuality::from_index(raw);
                if quality.to_index() != raw {
                    // Transient model-churn value (e.g. mid-rebuild
                    // selection-cleared). Skip the write so we don't
                    // overwrite a valid persisted index with garbage.
                    tracing::debug!(raw, "auto_record_quality: ignoring transient combo index");
                    return;
                }
                crate::sidebar::satellites_panel::save_auto_record_quality(
                    &config_quality,
                    quality,
                );
                tracing::info!(idx = quality.to_index(), "auto_record_quality persisted");
            });
    }

    // Doppler-correction tracker (#521).
    //
    // Two-layer wiring:
    //   1. `restore_doppler_switch` runs ALWAYS — restores the
    //      persisted master-switch value to the widget and
    //      wires its change-notify to save back. This way the
    //      user's preference survives a launch even when the
    //      TLE cache is unavailable. Per CR round 1 on PR #554.
    //   2. `connect_doppler_tracker` runs only when the TLE
    //      cache is available — without TLEs we can't propagate
    //      SGP4 to evaluate the trigger or compute the offset,
    //      so there's nothing for the *behavior* to do.
    restore_doppler_switch(panels, config);
    if let Some(cache_doppler) = cache.as_ref() {
        connect_doppler_tracker(panels, state, cache_doppler, status_bar);
    }

    // Refresh button — re-download every known satellite's TLE on
    // a worker thread, update the timestamp row, and rebuild the
    // pass list. Same `spawn_future_local` + `spawn_blocking`
    // pattern as the RadioReference search button. Wired only
    // when the cache is available; otherwise the button was
    // already disabled above.
    if let Some(cache_outer) = cache.as_ref() {
        let cache_refresh = std::sync::Arc::clone(cache_outer);
        let config_refresh = std::sync::Arc::clone(config);
        let panel_weak_refresh = panel_weak.clone();
        let recompute_refresh = Rc::clone(&recompute);
        panel.refresh_button.connect_clicked(move |_| {
            let Some(panel) = panel_weak_refresh.upgrade() else {
                return;
            };
            panel.refresh_spinner.set_visible(true);
            panel.refresh_spinner.start();
            panel.refresh_button.set_sensitive(false);

            let cache_task = std::sync::Arc::clone(&cache_refresh);
            let config_done = std::sync::Arc::clone(&config_refresh);
            let panel_weak_done = panel_weak_refresh.clone();
            let recompute_done = Rc::clone(&recompute_refresh);

            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    // `force_refresh` — NOT `tle_text` — because the
                    // user clicked Refresh and a fresh-cache fast-path
                    // would let us mark "Last refreshed: now" without
                    // any actual network fetch. `force_refresh` always
                    // hits the network and never falls back to a stale
                    // cache, so a successful return means a real round
                    // trip happened. A per-satellite failure is logged
                    // and skipped so a single decommissioned /
                    // rate-limited entry can't break the whole
                    // refresh: the user still gets the rest.
                    let mut last_err: Option<sdr_sat::TleCacheError> = None;
                    let mut succeeded = 0usize;
                    for known in KNOWN_SATELLITES {
                        match cache_task.force_refresh(known.norad_id) {
                            Ok(_) => succeeded += 1,
                            Err(e) => {
                                tracing::warn!(
                                    "TLE refresh for {} (NORAD {}) failed: {e}",
                                    known.name,
                                    known.norad_id,
                                );
                                last_err = Some(e);
                            }
                        }
                    }
                    if succeeded == 0 {
                        // Every fetch failed — surface the last error so
                        // the UI can show it. (If at least one
                        // succeeded, treat the refresh as "done" so
                        // the user sees the timestamp tick forward.)
                        Err(last_err.unwrap_or_else(|| {
                            sdr_sat::TleCacheError::Fetch(
                                "refresh produced no successful fetches".to_string(),
                            )
                        }))
                    } else {
                        Ok(())
                    }
                })
                .await;

                let Some(panel) = panel_weak_done.upgrade() else {
                    return;
                };
                panel.refresh_spinner.stop();
                panel.refresh_spinner.set_visible(false);
                panel.refresh_button.set_sensitive(true);

                match result {
                    Ok(Ok(())) => {
                        let now = chrono::Utc::now();
                        save_tle_last_refresh(&config_done, now);
                        panel
                            .last_refresh_row
                            .set_subtitle(&format_last_refresh(&config_done));
                        recompute_done();
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("TLE refresh failed: {e}");
                        panel
                            .last_refresh_row
                            .set_subtitle(&format!("Refresh failed: {e}"));
                    }
                    Err(_) => {
                        tracing::warn!("TLE refresh task panicked");
                        panel
                            .last_refresh_row
                            .set_subtitle("Refresh failed: background task panicked");
                    }
                }
            });
        });
    }

    // ZIP code → lat/lon shortcut. We wire BOTH `apply` (apply
    // button click / Enter when apply-button is sensitive) and
    // `entry_activated` (Enter, unconditional), then dedupe by an
    // "in-flight" flag — `apply` won't fire if AdwEntryRow's
    // internal "has the text been edited?" tracking is in a state
    // where the apply button is insensitive, but `entry_activated`
    // fires on Enter regardless. Belt-and-braces is cheaper than
    // chasing libadwaita's internal sensitivity rules.
    //
    // Result text goes to `zip_status_row` (AdwEntryRow has no
    // subtitle slot of its own). Wired regardless of TLE cache
    // availability — the ZIP lookup is independent.
    {
        let in_flight: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        let run_lookup: Rc<dyn Fn(adw::EntryRow)> = {
            let panel_weak_zip = panel_weak.clone();
            let in_flight_run = Rc::clone(&in_flight);
            Rc::new(move |entry: adw::EntryRow| {
                if in_flight_run.get() {
                    tracing::debug!("Satellites: ZIP lookup ignored — already in flight");
                    return;
                }
                let Some(panel) = panel_weak_zip.upgrade() else {
                    return;
                };
                // Trim once, here, so the trimmed value is what
                // flows through the lookup. `lookup_us_zip` does its
                // own trim internally too, but a paste of "  24068 "
                // showing up as `length=8` in the debug log reads
                // worse than `length=5`.
                let zip = entry.text().trim().to_string();
                if zip.is_empty() {
                    // Empty entry — nothing to do; treat as a no-op so a
                    // stray Enter doesn't reset the status row.
                    return;
                }
                in_flight_run.set(true);
                tracing::debug!("Satellites: ZIP lookup triggered (length={})", zip.len());
                entry.set_sensitive(false);
                panel.zip_status_row.set_title("Looking up…");

                let panel_weak_done = panel_weak_zip.clone();
                let in_flight_done = Rc::clone(&in_flight_run);
                let zip_for_task = zip.clone();
                glib::spawn_future_local(async move {
                    // Chain the two lookups on the worker thread so the
                    // UI side just gets one result. ZIP failure is fatal
                    // for this run; elevation failure is logged and
                    // demoted to `Ok(_, None)` — altitude is best-effort
                    // since it barely matters for pass prediction
                    // anyway, and we'd rather populate lat/lon than
                    // leave the user staring at an error toast.
                    let result = gio::spawn_blocking(move || -> Result<
                    (sdr_sat::PostalLocation, Result<f64, String>),
                    sdr_sat::PostalLookupError,
                > {
                    let loc = sdr_sat::lookup_us_zip(&zip_for_task)?;
                    // Elevation lookup is best-effort — a failure here
                    // shouldn't fail the whole flow, but we DO want
                    // the provider error to reach the UI so the user
                    // can see why altitude didn't update. Pass it
                    // back as `Err(String)` (cheap to send across
                    // thread, decoupled from `ElevationLookupError`'s
                    // type, and the log already scrubbed lat/lon).
                    let elevation = match sdr_sat::lookup_elevation_m(loc.lat_deg, loc.lon_deg)
                    {
                        Ok(m) => Ok(m),
                        Err(e) => {
                            // Don't include lat/lon in the log — that's user
                            // location data. The error message itself is
                            // safe (it's the upstream HTTP error / parse
                            // error / dataset-coverage error).
                            tracing::warn!("elevation lookup failed: {e}");
                            Err(e.to_string())
                        }
                    };
                    Ok((loc, elevation))
                })
                .await;

                    in_flight_done.set(false);
                    let Some(panel) = panel_weak_done.upgrade() else {
                        return;
                    };
                    panel.zip_row.set_sensitive(true);

                    match result {
                        Ok(Ok((loc, elevation))) => {
                            // Order matters slightly: setting lat/lon/alt
                            // fires `value-notify`, which persists the
                            // value and triggers `recompute`. Three
                            // recomputes back-to-back is fine —
                            // sub-millisecond each.
                            panel.lat_row.set_value(loc.lat_deg);
                            panel.lon_row.set_value(loc.lon_deg);
                            let where_text = if loc.region.is_empty() {
                                loc.place
                            } else {
                                format!("{place}, {region}", place = loc.place, region = loc.region)
                            };
                            let status = match elevation {
                                Ok(alt_m) => {
                                    panel.alt_row.set_value(alt_m);
                                    format!("Resolved: {where_text} ({alt_m:.0} m)")
                                }
                                Err(e) => {
                                    // Leave altitude alone but
                                    // surface the provider error
                                    // so the user knows what to
                                    // try next (e.g. retry on a
                                    // bad network).
                                    format!("Resolved: {where_text} (altitude unchanged: {e})")
                                }
                            };
                            panel.zip_status_row.set_title(&status);
                        }
                        Ok(Err(e)) => {
                            // Don't include the ZIP in the log — user
                            // location data, already surfaced inline in
                            // the status row. Provider error alone is
                            // enough.
                            tracing::warn!("ZIP lookup failed: {e}");
                            panel.zip_status_row.set_title(&e.to_string());
                        }
                        Err(_) => {
                            tracing::warn!("ZIP lookup task panicked");
                            panel
                                .zip_status_row
                                .set_title("Lookup failed: background task panicked");
                        }
                    }
                });
            })
        };
        // Wire both signal paths to the same closure: `apply` for
        // the apply button click (when libadwaita has flagged the
        // text as edited), `entry-activated` for raw Enter keys
        // (always fires, regardless of edit state).
        {
            let run = Rc::clone(&run_lookup);
            panel.zip_row.connect_apply(move |entry| run(entry.clone()));
        }
        {
            let run = Rc::clone(&run_lookup);
            panel
                .zip_row
                .connect_entry_activated(move |entry| run(entry.clone()));
        }
    }

    // 1 Hz countdown ticker. Only scheduled when the cache is
    // available — without it `displayed` stays empty forever and
    // the timer would tick uselessly. Captures the panel weakly
    // so the source returns `ControlFlow::Break` once any panel
    // widget has been dropped (otherwise GLib runs it forever,
    // holding a strong chain into the `displayed` vec and its
    // widgets).
    // Auto-record-on-pass state machine (#482b). Driven from the
    // same 1 Hz tick that updates pass-row countdowns — no second
    // GLib source. The recorder itself is pure (returns
    // `Vec<RecorderAction>`); the closure below interprets each
    // action against the live UI / DSP / filesystem.
    let recorder: Rc<RefCell<AutoRecorder>> = Rc::new(RefCell::new(AutoRecorder::new()));

    // Parent-window resolver for the auto-open-viewer side effect.
    // Walks up the widget tree from the satellites page; falls
    // back to `None` if the widget has been detached, in which
    // case the open is silently skipped. Holds a `WeakRef` so the
    // 1 Hz timer's `panel_weak.upgrade() == None` exit gate can
    // actually fire — a strong clone here would keep the panel
    // widget alive and the timer would never break.
    let parent_provider_for_recorder: Rc<dyn Fn() -> Option<gtk4::Window>> = {
        let widget_weak = panel.widget.downgrade();
        Rc::new(move || {
            widget_weak
                .upgrade()
                .and_then(|w| w.root())
                .and_then(|r| r.downcast::<gtk4::Window>().ok())
        })
    };

    let interpret_action: Rc<dyn Fn(RecorderAction)> = {
        let state_a = Rc::clone(state);
        let tune_a = Rc::clone(tune_to_satellite);
        let set_playing_a = Rc::clone(set_playing);
        // Optional TLE cache — used by the SavePng wiring to compute
        // `is_ascending` for the rotate-180 flag (B2 of the noaa-apt
        // parity work). `None` when the host platform refused us a
        // cache directory; the rotate path falls back to "no rotation"
        // in that case.
        let cache_a: Option<std::sync::Arc<TleCache>> = cache.as_ref().map(std::sync::Arc::clone);
        // Weak ref for the same lifecycle reason as
        // `parent_provider_for_recorder` — strong clone would pin
        // the toast overlay alive past window close.
        let toast_overlay_weak = toast_overlay.downgrade();
        let parent_provider_a = Rc::clone(&parent_provider_for_recorder);
        // Scanner master switch handle for the LOS-side restore.
        // Set-active here fires the switch's `connect_active_notify`
        // handler, which re-dispatches `SetScannerEnabled(true)` and
        // re-arms the engine — same path the user takes when they
        // flip the switch by hand.
        let scanner_switch_a = panels.scanner.master_switch.clone();
        // Audio-chain widgets — force-disabled at AOS, restored at
        // LOS via the same `set_*` notify chain that any user flip
        // takes. Per #555 (squelch + CTCSS) and #556 (FM IF NR).
        let squelch_enabled_row_a = panels.radio.squelch_enabled_row.clone();
        let auto_squelch_row_a = panels.radio.auto_squelch_row.clone();
        let squelch_level_row_a = panels.radio.squelch_level_row.clone();
        let ctcss_row_a = panels.radio.ctcss_row.clone();
        let fm_if_nr_row_a = panels.radio.fm_if_nr_row.clone();
        // Composite-save toggle — read at LOS by the
        // `SaveLrptPass` handler. Strong clone so the closure
        // doesn't need to upgrade a weak ref against panel
        // teardown ordering: every other panel widget the
        // closure captures is a strong clone too. Per #547.
        let auto_record_composites_switch_a = panel.auto_record_composites_switch.clone();
        let post_toast = move |overlay_weak: &glib::WeakRef<adw::ToastOverlay>, msg: &str| {
            if let Some(overlay) = overlay_weak.upgrade() {
                overlay.add_toast(adw::Toast::new(msg));
            }
        };
        // Compute the rotate-180 flag for the currently-recording APT
        // pass: `true` when the satellite is on the ascending leg of
        // its orbit, which means the assembled image is upside-down +
        // mirrored east/west (per `sdr_radio::apt_image::rotate_180_per_channel`).
        // Falls back to `false` (no rotation) on any failure — TLE
        // cache miss, parse failure, propagation error, or
        // recording-pass info missing. The default is safe: NOAA
        // satellites are sun-synchronous, so the descending pass is
        // the typical case for daytime captures and no-rotation
        // preserves north-at-top.
        // Takes the recording-pass tuple directly (`norad_id`, `aos`)
        // rather than reading it from `AppState`, so callers compute
        // rotation against an explicit snapshot. Without this, the
        // SavePng handler could read a freshly-overwritten slot if a
        // back-to-back AOS landed between this pass's LOS dispatch
        // and the rotation compute — exporting the older pass's
        // image with the newer pass's orientation. Per CR round 6
        // on PR #571.
        let compute_apt_rotate_180_for_pass = {
            let cache_a = cache_a.clone();
            move |norad_id: u32, aos: chrono::DateTime<chrono::Utc>| -> bool {
                let Some(cache) = cache_a.as_ref() else {
                    return false;
                };
                // Look up by stable NORAD id (not display name) so
                // a catalog rename doesn't silently break this
                // path. Per CR round 2 on PR #571.
                let Some(known) = sdr_sat::KNOWN_SATELLITES
                    .iter()
                    .find(|s| s.norad_id == norad_id)
                else {
                    tracing::debug!(
                        norad_id,
                        "APT rotate-180: satellite not in catalog; defaulting to no rotation",
                    );
                    return false;
                };
                let (line1, line2) = match cache.cached_tle_for(known.norad_id) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(
                            norad_id,
                            error = %e,
                            "APT rotate-180: TLE unavailable; defaulting to no rotation",
                        );
                        return false;
                    }
                };
                let parsed = match sdr_sat::Satellite::from_tle(known.name, &line1, &line2) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(
                            norad_id,
                            error = %e,
                            "APT rotate-180: TLE parse failed; defaulting to no rotation",
                        );
                        return false;
                    }
                };
                match sdr_sat::is_ascending(&parsed, aos) {
                    Ok(asc) => asc,
                    Err(e) => {
                        tracing::debug!(
                            norad_id,
                            error = %e,
                            "APT rotate-180: SGP4 propagate failed; defaulting to no rotation",
                        );
                        false
                    }
                }
            }
        };
        Rc::new(move |action: RecorderAction| match action {
            RecorderAction::StartAutoRecord {
                satellite,
                norad_id,
                freq_hz,
                mode,
                bandwidth_hz,
                protocol,
            } => {
                tracing::info!(
                    "auto-record AOS: tuning to {satellite} @ {freq_hz} Hz, BW {bandwidth_hz} Hz, protocol {protocol:?}",
                );
                // ACARS-engaged gating happens at the recorder
                // tick site (the for-loop calling
                // `interpret_action`), not here — that's the
                // only level that has visibility into the
                // entire `Vec<RecorderAction>` batch and can
                // defer it as a unit. CR round 1 on PR #591
                // flagged the gap: stashing only this single
                // `StartAutoRecord` while iterating the rest
                // of the batch left `StartAutoAudioRecord` etc.
                // running while ACARS was still engaged.
                // Per-protocol viewer dispatch. Adding a new
                // protocol means adding a match arm here +
                // flipping `imaging_protocol` on the catalog
                // entry — no recorder change needed. Per #514.
                //
                // **Fail closed on unsupported protocols.** All
                // AOS side effects (set_playing, tune, zero VFO,
                // open viewer) live INSIDE each arm rather than
                // unconditionally before the match. Per CR
                // round 1 on PR #541: if a catalog entry is
                // flipped to `Some(Lrpt)` ahead of Task 7 wiring
                // the LRPT viewer, the user's tune state must
                // NOT be hijacked just to land in a no-op
                // branch.
                // Audio-chain force-off helper. Flips the three
                // user-visible widgets to disabled; each one's
                // `connect_*_notify` handler then dispatches the
                // corresponding `UiToDsp::Set*` message — same
                // path a manual flip takes. Per #555 (squelch +
                // CTCSS) / #556 (FM IF NR). Idempotent: a
                // `set_active(false)` on an already-false switch
                // is a no-op (no notify fired, no redundant
                // dispatch).
                //
                // Called inside each protocol arm rather than
                // before the match so an unsupported protocol
                // doesn't hijack the user's audio chain (mirrors
                // the AOS side-effect-isolation rule from CR
                // round 1 on PR #541).
                let force_audio_chain_off = || {
                    if squelch_enabled_row_a.is_active() {
                        squelch_enabled_row_a.set_active(false);
                    }
                    if auto_squelch_row_a.is_active() {
                        auto_squelch_row_a.set_active(false);
                    }
                    let off_idx =
                        sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(CtcssMode::Off);
                    if ctcss_row_a.selected() != off_idx {
                        ctcss_row_a.set_selected(off_idx);
                    }
                    if fm_if_nr_row_a.is_active() {
                        fm_if_nr_row_a.set_active(false);
                    }
                };
                match protocol {
                    sdr_sat::ImagingProtocol::Apt => {
                        // **Order is load-bearing.** Force the
                        // audio chain off BEFORE start/tune so
                        // the very first samples through the
                        // freshly-tuned demod aren't gated by a
                        // stale squelch / CTCSS / FM-IF-NR
                        // setting. Otherwise low-SNR AOS rows
                        // race against the change-notify chain
                        // dispatching the SetX messages and the
                        // first few scan lines could be silenced
                        // before the demod even sees them. Per
                        // CR round 1 on PR #557.
                        force_audio_chain_off();
                        // Drive Start through the header play
                        // button — its `connect_toggled` handler
                        // is the single place that updates
                        // `state.is_running`, dispatches
                        // `UiToDsp::Start`, and swaps the
                        // play/stop icon. `set_active` is a
                        // no-op when the radio is already
                        // running, so this is safe to call
                        // unconditionally without a duplicate
                        // Start. The pre-AOS `was_running` flag
                        // (captured in `SavedTune` by the 1 Hz
                        // tick before this action fires) drives
                        // the corresponding LOS-side stop.
                        set_playing_a(true);
                        tune_a(freq_hz, mode, bandwidth_hz);
                        // Zero the live VFO offset for the
                        // auto-record pass. The user's pre-AOS
                        // offset (a manual VFO drag away from
                        // centre) is preserved in `SavedTune`
                        // for the LOS restore, but during the
                        // pass the demod must align *exactly*
                        // with the satellite's downlink —
                        // otherwise we'd demod at `freq_hz +
                        // saved_offset` and the APT subcarrier
                        // would land outside the channel
                        // filter. The DSP's
                        // `DspToUi::VfoOffsetChanged` echo
                        // updates the spectrum widget, freq
                        // selector, and status bar; no manual
                        // mirror needed.
                        state_a.dispatch_vfo_offset(0.0);
                        crate::apt_viewer::open_apt_viewer_if_needed(&parent_provider_a, &state_a);
                        // Clear the canvas at AOS so a back-to-back
                        // pass (e.g. NOAA 18 → NOAA 19 with
                        // overlapping viewer sessions) starts on a
                        // clean image. The viewer was either just
                        // opened above (already empty) or carried
                        // over from a previous pass — either way,
                        // an explicit clear keeps the image we're
                        // about to save scoped to *this* pass.
                        if let Some(view) = state_a.apt_viewer.borrow().as_ref() {
                            view.clear();
                        }
                        // Stash recording-pass info for the LOS-side
                        // SavePng wiring to compute the auto-rotation
                        // flag (B2 of the noaa-apt parity work). The
                        // exact AOS time matters less than "around
                        // when" — `is_ascending` checks the lat
                        // derivative over a 30 s window, which is
                        // valid anywhere mid-pass. NORAD id arrives
                        // pre-resolved on the recorder action — no
                        // name → catalog lookup at this layer means
                        // no silent rotation breakage if the catalog
                        // ever picks up alias drift. Per CR round 3
                        // on PR #571.
                        let aos = chrono::Utc::now();
                        *state_a.apt_recording_pass.borrow_mut() = Some((norad_id, aos));
                        // Push the rotate-180 flag down to the
                        // renderer so the toolbar's manual `Export
                        // PNG` button matches the auto-record
                        // orientation. Without this, manual exports
                        // of ascending passes come out upside-down
                        // even though the auto-record save rotates
                        // them. Per CR round 1 on PR #571.
                        let rotate_180 = compute_apt_rotate_180_for_pass(norad_id, aos);
                        if let Some(view) = state_a.apt_viewer.borrow().as_ref() {
                            view.set_rotate_180(rotate_180);
                        }
                    }
                    sdr_sat::ImagingProtocol::Lrpt => {
                        // **Order is load-bearing.** Reset the
                        // shared image AND the viewer canvas
                        // BEFORE starting playback / retuning
                        // so the freshly-tuned LRPT decoder
                        // can't push pass-1 leftover rows (or
                        // race the clear and erase the first
                        // few rows of the new pass). Per
                        // CodeRabbit round 8 on PR #543.
                        //
                        // The viewer is opened first so the
                        // `view.clear()` call below can target
                        // it — the open path also sends
                        // `UiToDsp::SetLrptImage(handle)` to
                        // the DSP thread, which lazy-inits the
                        // decoder against the (now-cleared)
                        // shared image. Catalog's
                        // `demod_mode: Lrpt` lines up the
                        // controller's IF rate (144 ksps) with
                        // the QPSK demod's expected sample rate
                        // — without that, `radio_input` would
                        // be at the wrong rate and the demod's
                        // resampler would sit at the wrong
                        // setpoint. Per epic #469 task 7.
                        crate::lrpt_viewer::open_lrpt_viewer_if_needed(
                            &parent_provider_a,
                            &state_a,
                        );
                        state_a.lrpt_image.clear();
                        if let Some(view) = state_a.lrpt_viewer.borrow().as_ref() {
                            view.clear();
                        }
                        // Force audio chain off BEFORE start/tune
                        // so the freshly-tuned demod isn't gated
                        // by a stale squelch / CTCSS / FM-IF-NR
                        // setting. Per CR round 1 on PR #557 —
                        // see APT arm above for the full
                        // rationale.
                        force_audio_chain_off();
                        // Now safe to start playback + retune;
                        // any decoded rows from this point
                        // forward land in the cleared image.
                        set_playing_a(true);
                        tune_a(freq_hz, mode, bandwidth_hz);
                        state_a.dispatch_vfo_offset(0.0);
                        // Mirror into AppState so is_recording() (used by
                        // the close-to-tray Quit confirmation modal)
                        // reflects an in-progress LRPT pass. Per #512.
                        // The `(norad_id, aos)` tuple lets the LOS
                        // completion path snapshot-and-compare so an
                        // overlapping pass-N+1 AOS that starts during
                        // pass-N's encode doesn't have its `is_recording`
                        // flag clobbered when pass-N's completion
                        // fires. Mirrors the APT
                        // `apt_recording_pass` pattern from PR #571
                        // round 4. Per CR round 2 on PR #575.
                        let aos = chrono::Utc::now();
                        *state_a.lrpt_recording_pass.borrow_mut() = Some((norad_id, aos));
                    }
                    sdr_sat::ImagingProtocol::Sstv => {
                        // **Order is load-bearing.** Open the viewer
                        // (which sends `UiToDsp::SetSstvImage`) and
                        // clear both the shared handle and the canvas
                        // BEFORE starting playback / retuning so no
                        // leftover rows from a previous pass land in
                        // the fresh image buffer.  Mirrors the LRPT
                        // arm's clear-before-start discipline from
                        // CR round 8 on PR #543.
                        crate::sstv_viewer::open_sstv_viewer_if_needed(
                            &parent_provider_a,
                            &state_a,
                        );
                        state_a.sstv_image.clear();
                        // Failed-pass images now live in
                        // `sstv_pending_export` keyed by their
                        // original pass directory (moved there at
                        // `SaveSstvPass` time, not here at AOS).
                        // The current-pass buffer is the round-4
                        // simple clear. Per CR round 6 #21 on
                        // PR #599 (refines round 5 #20).
                        state_a.sstv_completed_images.borrow_mut().clear();
                        if let Some(view) = state_a.sstv_viewer.borrow().as_ref() {
                            view.clear();
                        }
                        // ISS SSTV is audible NFM — do NOT force the
                        // audio chain off.  The user's squelch /
                        // CTCSS / FM-IF-NR settings are correct for
                        // the mode; we only suppress them for
                        // silent-passthrough decoders (LRPT) and the
                        // APT path (subcarrier noise). Per CLAUDE.md
                        // and step 8 of epic #472 task spec.
                        set_playing_a(true);
                        tune_a(freq_hz, mode, bandwidth_hz);
                        state_a.dispatch_vfo_offset(0.0);
                        let aos = chrono::Utc::now();
                        *state_a.sstv_recording_pass.borrow_mut() = Some((norad_id, aos));
                    }
                }
            }
            RecorderAction::StartAutoAudioRecord(path) => {
                tracing::info!("auto-record AOS: opening WAV writer at {path:?}");
                state_a.send_dsp(UiToDsp::StartAudioRecording(path));
            }
            RecorderAction::StopAutoAudioRecord => {
                tracing::info!("auto-record LOS: closing WAV writer");
                state_a.send_dsp(UiToDsp::StopAudioRecording);
            }
            RecorderAction::ResetImagingDecoders => {
                // Between-pass decoder flush. The state machine
                // emits this at every `Recording → Finalizing`
                // transition (LOS), AFTER the save action's
                // snapshot read of the shared `LrptImage`. When
                // `was_running == true` pre-AOS this is the only
                // hook between passes — `RestoreTune` keeps the
                // source open across LOS → AOS, so the
                // source-stop reset never fires. When
                // `was_running == false` the subsequent
                // `set_playing(false)` in `RestoreTune` triggers
                // the source-stop path which resets again —
                // idempotent (`reset_imaging_decoders` only
                // touches in-flight buffers), so the
                // double-reset is harmless. Per issue #544.
                tracing::info!("auto-record LOS: resetting imaging decoders");
                state_a.send_dsp(UiToDsp::ResetImagingDecoders);
            }
            RecorderAction::SavePng(path) => {
                // Snapshot the recording-pass tuple FIRST so every
                // pass-derived value (rotation flag, slot-clear
                // check) reads from this stable view. If a new AOS
                // overwrites the slot between this dispatch and the
                // export — a back-to-back-pass race — we must use
                // the snapshot, not the live slot, otherwise the
                // older pass's image gets exported with the newer
                // pass's orientation. Per CR round 6 on PR #571.
                //
                // The same snapshot also drives the "only clear if
                // still-equal" guard on `apt_recording_pass` in both
                // the early-return path below and the async-callback
                // completion. Per CR rounds 4 and 5 on PR #571.
                let exported_pass = *state_a.apt_recording_pass.borrow();
                // Compute the rotate-180 flag for ascending passes
                // (B2 of the noaa-apt parity work) FROM THE SNAPSHOT,
                // not from the live `state_a.apt_recording_pass`. The
                // helper resolves the satellite's TLE from the cache
                // and calls `sdr_sat::is_ascending` at the snapshotted
                // AOS sample point. Defaults to `false` (no rotation)
                // if any step fails — descending-pass orientation is
                // the safer default since it preserves north-at-top.
                let rotate_180 = exported_pass
                    .is_some_and(|(norad_id, aos)| compute_apt_rotate_180_for_pass(norad_id, aos));
                let mode = sdr_radio::apt_image::BrightnessMode::default();
                // Async export: snapshot the AptImage on the main
                // thread NOW, hand the snapshot to a worker via
                // `gio::spawn_blocking`. The encode for a 1500-line
                // pass is multi-hundred-ms — synchronously running
                // it here would freeze GTK during LOS, exactly when
                // the user wants to see the toast and have the
                // window auto-close cleanly. Per CR round 1 on PR
                // #571.
                let view_opt = state_a.apt_viewer.borrow().as_ref().cloned();
                let Some(view) = view_opt else {
                    tracing::warn!(
                        "auto-record SavePng but no APT viewer is open (user closed mid-pass)",
                    );
                    post_toast(
                        &toast_overlay_weak,
                        "Pass complete, but the APT viewer was closed — no image saved",
                    );
                    // Same overlap-guard as the async-callback path:
                    // only clear the slot if it still holds the pass
                    // we entered this branch with. If a new AOS
                    // wrote a fresh tuple in the meantime, leave it
                    // alone.
                    {
                        let mut slot = state_a.apt_recording_pass.borrow_mut();
                        if *slot == exported_pass {
                            *slot = None;
                        }
                    }
                    return;
                };
                // Capture state needed by the async on_complete
                // callback (the rest can be moved into the closure).
                let path_for_msg = path.clone();
                let path_for_export = path;
                let toast_overlay_for_complete = toast_overlay_weak.clone();
                let state_for_complete = Rc::clone(&state_a);
                // Snapshot the *current* viewer-window WeakRef BEFORE
                // spawning the worker. If the user closes the viewer
                // mid-export and reopens it, `state.apt_viewer_window`
                // will point at the new window by the time the
                // callback fires; reading from there could close the
                // wrong window. Cloning the WeakRef pins the
                // identity of the window we'll attempt to close, while
                // staying weak so a closed/dropped window upgrades to
                // None and we no-op. Per CR round 3 on PR #571.
                let exported_window_weak = state_a.apt_viewer_window.borrow().as_ref().cloned();
                view.export_png_full_async(path_for_export, mode, rotate_180, move |result| {
                    let (export_ok, msg) = match result {
                        Ok(()) => {
                            tracing::info!(
                                rotate_180,
                                ?mode,
                                "auto-record PNG saved to {}",
                                path_for_msg.display()
                            );
                            (
                                true,
                                format!(
                                    "Pass complete — image saved to {}",
                                    path_for_msg.display()
                                ),
                            )
                        }
                        Err(e) => {
                            tracing::warn!(
                                "auto-record PNG export to {} failed: {e}",
                                path_for_msg.display()
                            );
                            (false, format!("Pass complete but PNG save failed: {e}"))
                        }
                    };
                    post_toast(&toast_overlay_for_complete, &msg);
                    // Close the APT viewer window now that the PNG
                    // is on disk — resets the viewer for the next
                    // pass instead of carrying stale lines forward.
                    // Per a user request during PR #554 live
                    // testing.
                    //
                    // Only close on a successful export — if the
                    // save failed (Cairo error, disk full, etc.)
                    // the user probably wants to inspect the
                    // in-memory image and manually retry the
                    // export. Per CR round 9 on PR #554.
                    if export_ok {
                        // Use the WeakRef we snapshotted at export
                        // start (not the current `state.apt_viewer_window`)
                        // so a viewer reopen during the async save
                        // can't trick us into closing the wrong
                        // window. Upgrade-or-skip — if the user
                        // already closed it, the upgrade returns None
                        // and we simply do nothing. Per CR round 3 on
                        // PR #571.
                        if let Some(window) = exported_window_weak
                            .as_ref()
                            .and_then(glib::WeakRef::upgrade)
                        {
                            tracing::info!(
                                "auto-record LOS: closing APT viewer window after PNG save",
                            );
                            window.close();
                        }
                    }
                    // Clear the recording-pass info now that the
                    // export is done — but ONLY if the slot still
                    // holds the same pass we just saved. If a new
                    // AOS overwrote it while we were encoding, that
                    // new pass owns the slot now and clearing it
                    // would silently break the next LOS-side
                    // rotate-180 lookup. Per CR round 4 on PR #571.
                    {
                        let mut slot = state_for_complete.apt_recording_pass.borrow_mut();
                        if *slot == exported_pass {
                            *slot = None;
                        }
                    }
                });
            }
            RecorderAction::SaveLrptPass(dir) => {
                // Walk every APID present in the SHARED `LrptImage`
                // (the DSP-side decoder's destination — the source
                // of truth) and write one PNG per channel into the
                // per-pass directory (creating it lazily). Decoupled
                // from the live viewer in `CodeRabbit` round 7 on
                // PR #543: the previous implementation went through
                // `state.lrpt_viewer` and produced "no image saved"
                // toasts whenever the user dismissed the live
                // window mid-pass — even though the DSP had been
                // happily decoding into the shared image the
                // whole time. Reading directly from
                // `state.lrpt_image` makes the LOS save robust
                // against viewer close: the decoder runs as long
                // as the demod mode is `Lrpt`, and the captured
                // imagery survives any number of viewer cycles.
                // Snapshot every non-empty APID's pixel buffer
                // on the main thread (cheap — `snapshot_channel`
                // clones the per-channel `Vec<u8>` under a brief
                // mutex hold), then move the encoding + file
                // I/O off to a worker via `gio::spawn_blocking`.
                // PNG encoding for a full multi-channel pass is
                // multiple MB per APID and can take seconds; doing
                // it inline on the 1 Hz countdown tick would
                // freeze the UI right when the auto-record toast
                // and tune-restore should be landing. Per
                // CodeRabbit round 8 on PR #543. Established
                // pattern in this file (TLE refresh @ 8678,
                // bookmark import @ 8805).
                let snapshots: Vec<(u16, sdr_lrpt::image::ChannelBuffer)> = {
                    let mut sorted = state_a.lrpt_image.channel_apids();
                    sorted.sort_unstable();
                    sorted
                        .into_iter()
                        .filter_map(|apid| {
                            state_a
                                .lrpt_image
                                .snapshot_channel(apid)
                                .filter(|s| s.lines > 0)
                                .map(|s| (apid, s))
                        })
                        .collect()
                };
                // Composite snapshots — only when the user opted in
                // via the panel toggle. Each entry is the recipe +
                // a [`CompositeSnapshot`] (cloned source channel
                // pixel buffers + truncated height). Built on the
                // GTK main thread but the assembler lock is held
                // only long enough to memcpy the source channels
                // (~5 ms per recipe for full-pass data, vs ~30 ms
                // for the full RGB interleave that happens in the
                // worker). The expensive per-pixel interleave +
                // PNG encode runs inside `gio::spawn_blocking`
                // below. Per CR round 1 on PR #575.
                let composites_on = auto_record_composites_switch_a.is_active();
                let composite_snapshots: Vec<(
                    crate::lrpt_viewer::CompositeRecipe,
                    sdr_lrpt::image::CompositeSnapshot,
                )> = if composites_on {
                    state_a.lrpt_image.with_assembler(|a| {
                        crate::lrpt_viewer::COMPOSITE_CATALOG
                            .iter()
                            .filter_map(|recipe| {
                                a.clone_channels_for_composite(
                                    recipe.r_apid,
                                    recipe.g_apid,
                                    recipe.b_apid,
                                )
                                .map(|snap| (*recipe, snap))
                            })
                            .collect()
                    })
                } else {
                    Vec::new()
                };
                let toast_overlay_weak_for_save = toast_overlay_weak.clone();
                // Clone state for the post-save viewer-close
                // — we need to read `state.lrpt_recording_pass`
                // after the spawn_blocking completes, which
                // requires capturing state into the future.
                let state_lrpt_close = Rc::clone(&state_a);
                // Snapshot the *current* viewer-window WeakRef BEFORE
                // spawning the worker, mirroring the APT path's
                // pattern from PR #571 round 3. If the user closes
                // the LRPT viewer mid-export and reopens it,
                // `state.lrpt_viewer_window` will point at the new
                // window by the time the callback fires; reading
                // from there could close the wrong window. Cloning
                // the WeakRef pins the identity of the window we'll
                // attempt to close, while staying weak so a
                // closed/dropped window upgrades to None and we
                // no-op. Per CR round 2 on PR #575.
                let exported_lrpt_window_weak =
                    state_a.lrpt_viewer_window.borrow().as_ref().cloned();
                // Snapshot the recording-pass tuple FIRST so the
                // post-save clear is gated on "this is still the
                // pass we entered with". An overlapping pass-N+1
                // AOS that starts while pass-N is still encoding
                // would otherwise have its slot clobbered when
                // pass-N's completion callback fires `*slot =
                // None`. Same shape as the APT compare-and-clear
                // at `RecorderAction::SavePng`. Per CR round 2 on
                // PR #575.
                let exported_lrpt_pass = *state_a.lrpt_recording_pass.borrow();
                glib::spawn_future_local(async move {
                    let dir_for_msg = dir.clone();
                    // Tuple return: (toast message, saved-at-least-one).
                    // The flag gates the post-save viewer close —
                    // we keep the viewer open on total-failure
                    // outcomes so the user can inspect the in-
                    // memory image and manually retry the export.
                    // Per CR round 9 on PR #554.
                    let (result_msg, save_ok) = gio::spawn_blocking(move || {
                        if snapshots.is_empty() {
                            tracing::warn!(
                                "auto-record SaveLrptPass but no APIDs were decoded — pass produced no imagery",
                            );
                            return (
                                format!(
                                    "Pass complete, but no LRPT channels decoded — nothing saved to {}",
                                    dir.display()
                                ),
                                false,
                            );
                        }
                        if let Err(e) = std::fs::create_dir_all(&dir) {
                            // Per-pass directory created up
                            // front so a disk-full / permissions
                            // failure surfaces as a single
                            // observable error rather than `N`
                            // per-channel warnings. Per
                            // CodeRabbit round 1 on PR #543.
                            tracing::warn!(
                                "auto-record SaveLrptPass: failed to create directory {dir:?}: {e}",
                            );
                            return (
                                format!("Pass complete but couldn't create {}: {e}", dir.display()),
                                false,
                            );
                        }
                        let mut saved = 0_usize;
                        let mut errors: Vec<String> = Vec::new();
                        for (apid, snap) in snapshots {
                            let path = dir.join(format!("apid{apid}.png"));
                            match crate::lrpt_viewer::write_greyscale_png(
                                &path,
                                &snap.pixels,
                                sdr_lrpt::image::IMAGE_WIDTH,
                                snap.lines,
                            ) {
                                Ok(()) => {
                                    tracing::info!(
                                        ?path,
                                        apid,
                                        lines = snap.lines,
                                        "auto-record LRPT channel saved",
                                    );
                                    saved += 1;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "auto-record LRPT export for APID {apid} to {path:?} failed: {e}",
                                    );
                                    errors.push(format!("APID {apid}: {e}"));
                                }
                            }
                        }
                        // Composite PNGs alongside the per-APID
                        // files. Filename is `composite-{slug}.png`
                        // where `slug` is the recipe name with
                        // spaces replaced by `-` and path
                        // separators replaced by `_` so the disk
                        // layout is portable across filesystems.
                        //
                        // The RGB interleave runs HERE — inside the
                        // `gio::spawn_blocking` worker — so the
                        // ~30 ms per-recipe per-pixel walk doesn't
                        // block the GTK main thread. The assembler
                        // lock was released after the cheap channel
                        // memcpy in the snapshot phase above. Per
                        // CR round 1 on PR #575.
                        for (recipe, snap) in composite_snapshots {
                            let rgb = sdr_lrpt::image::assemble_rgb_composite(
                                &snap.r_pixels,
                                &snap.g_pixels,
                                &snap.b_pixels,
                                snap.height,
                            );
                            let width = sdr_lrpt::image::IMAGE_WIDTH;
                            let height = snap.height;
                            let slug = recipe
                                .name
                                .replace(' ', "-")
                                .replace(['/', '\\'], "_");
                            let path = dir.join(format!("composite-{slug}.png"));
                            match crate::lrpt_viewer::write_rgb_png(&path, &rgb, width, height) {
                                Ok(()) => {
                                    tracing::info!(
                                        ?path,
                                        recipe = recipe.name,
                                        width,
                                        height,
                                        "auto-record LRPT composite saved",
                                    );
                                    saved += 1;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "auto-record LRPT composite {} to {path:?} failed: {e}",
                                        recipe.name,
                                    );
                                    errors.push(format!("Composite {}: {e}", recipe.name));
                                }
                            }
                        }
                        let msg = if errors.is_empty() {
                            format!(
                                "Pass complete — {saved} LRPT file(s) saved to {}",
                                dir.display()
                            )
                        } else {
                            format!(
                                "Pass complete — {saved} file(s) saved, {} failed: {}",
                                errors.len(),
                                errors.join("; ")
                            )
                        };
                        // Treat "at least one channel saved" as
                        // success for close-purposes — partial-
                        // success outcomes still produced disk
                        // artifacts the user can inspect.
                        (msg, saved > 0)
                    })
                    .await
                    .unwrap_or_else(|e| {
                        // `gio::spawn_blocking`'s join error is a
                        // panic payload (`Box<dyn Any + Send>`),
                        // which doesn't implement `Display`.
                        // Format via `Debug` on the worker side
                        // and just report a generic message to
                        // the user — a panicking PNG encoder is
                        // a logic bug, not something the user
                        // can act on.
                        tracing::warn!(
                            "auto-record SaveLrptPass: worker thread panicked: {e:?}",
                        );
                        (
                            format!(
                                "Pass complete but PNG worker panicked (target was {})",
                                dir_for_msg.display()
                            ),
                            false,
                        )
                    });
                    post_toast(&toast_overlay_weak_for_save, &result_msg);
                    // Mark the LRPT pass as no-longer-recording for
                    // the close-to-tray Quit-confirmation predicate.
                    // We clear regardless of save_ok — the pass itself
                    // is over (LOS already happened); save_ok only
                    // controls whether to close the viewer. Per #512.
                    //
                    // **Compare-and-clear:** only clear the slot if
                    // it still holds the same pass we entered this
                    // branch with. If a new AOS overwrote it
                    // mid-export (overlapping passes can happen now
                    // that composites widen the LOS window), that
                    // new pass owns the slot — wiping it would lie
                    // to the close-to-tray predicate about the
                    // in-flight pass. Mirrors the APT
                    // `apt_recording_pass` compare-and-clear from
                    // PR #571 round 4. Per CR round 2 on PR #575.
                    {
                        let mut slot = state_lrpt_close.lrpt_recording_pass.borrow_mut();
                        if *slot == exported_lrpt_pass {
                            *slot = None;
                        }
                    }
                    // Close the LRPT viewer window now that the
                    // PNGs are on disk — resets the viewer for
                    // the next pass instead of carrying stale
                    // APIDs forward. Per a user request during
                    // PR #554 live testing.
                    //
                    // Only close when at least one channel
                    // actually saved (`save_ok`) — on total-
                    // failure outcomes (no APIDs decoded, dir
                    // create failed, every channel errored, or
                    // worker panicked) keep the viewer open so
                    // the user can inspect the in-memory image
                    // and manually retry the export. Per CR
                    // round 9 on PR #554.
                    //
                    // Runs on the GLib main loop (we re-entered
                    // it via `spawn_future_local`), so the weak
                    // upgrade + `.close()` is main-thread-safe.
                    // Weak-ref upgrade fails closed: if the user
                    // already dismissed the window, there's
                    // nothing to close. The close-request
                    // handler in `open_lrpt_viewer_if_needed`
                    // clears the AppState slots so the next AOS
                    // opens a fresh viewer.
                    //
                    // Use the WeakRef we snapshotted at export
                    // start (not the current
                    // `state.lrpt_viewer_window`) so a viewer
                    // reopen during the async save can't trick us
                    // into closing the wrong window — same shape
                    // as the APT path's snapshot pattern. Per CR
                    // round 2 on PR #575.
                    if save_ok
                        && let Some(window) = exported_lrpt_window_weak
                            .as_ref()
                            .and_then(glib::WeakRef::upgrade)
                    {
                        tracing::info!(
                            "auto-record LOS: closing LRPT viewer window after PNG save"
                        );
                        window.close();
                    }
                });
            }
            RecorderAction::SaveSstvPass(dir) => {
                // Per-pass auto-record save. Each pass's images are
                // written into their own `sstv-iss-{ts}` directory.
                // Failed-pass batches are kept in
                // `sstv_pending_export` keyed by their *original*
                // `dir`, then retried separately against that dir
                // at the next LOS — they never bleed into the
                // current pass's directory. Per CR round 6 #21 on
                // PR #599.
                //
                // Reading from `state.sstv_completed_images`
                // (rather than the shared `SstvImage` handle)
                // mirrors the LRPT design from CodeRabbit round 7
                // on PR #543: the save path is decoupled from the
                // live viewer so closing the viewer window
                // mid-pass doesn't lose the imagery.
                //
                // Encoding + file I/O is offloaded to
                // `gio::spawn_blocking` so multi-image PNG encoding
                // doesn't freeze the UI right when the auto-record
                // toast is landing. Per CodeRabbit #9 on PR #599.
                let pending_batches: Vec<PendingSstvExport> =
                    std::mem::take(&mut *state_a.sstv_pending_export.borrow_mut());
                let current_images: Vec<sdr_radio::sstv_image::CompletedSstvImage> = state_a
                    .sstv_completed_images
                    .borrow()
                    .iter()
                    .cloned()
                    .collect();
                // Snapshot count of the *current* pass so the
                // success path can drain only those — late frames
                // pushed by `DspToUi::SstvImageComplete` while we
                // were awaiting the worker stay buffered for the
                // next save cycle. Per CR round 4 on PR #599.
                let exported_image_count = current_images.len();
                let toast_overlay_weak_for_save = toast_overlay_weak.clone();
                let state_sstv_close = Rc::clone(&state_a);
                // Snapshot the WeakRef BEFORE spawning so a
                // viewer reopen during the async save can't
                // trick us into closing the wrong window.
                // Mirrors the LRPT pattern from CR round 2 on
                // PR #575.
                let exported_sstv_window_weak =
                    state_a.sstv_viewer_window.borrow().as_ref().cloned();
                // Snapshot the recording-pass tuple for
                // compare-and-clear on completion — mirrors the
                // LRPT and APT patterns from PR #571 / #575.
                let exported_sstv_pass = *state_a.sstv_recording_pass.borrow();
                glib::spawn_future_local(async move {
                    let dir_for_msg = dir.clone();
                    let join = gio::spawn_blocking(move || {
                        save_sstv_batches(pending_batches, current_images, dir)
                    })
                    .await;
                    let SstvSaveOutcome {
                        message,
                        current_ok,
                        retained,
                    } = join.unwrap_or_else(|e| {
                        tracing::warn!("auto-record SaveSstvPass: worker thread panicked: {e:?}",);
                        SstvSaveOutcome {
                            message: format!(
                                "Pass complete but PNG worker panicked (target was {})",
                                dir_for_msg.display()
                            ),
                            current_ok: false,
                            // On panic we don't know which batches
                            // failed — keep the (already-drained)
                            // pending list empty rather than guess.
                            // The current pass's images stay in
                            // `sstv_completed_images` and are
                            // dropped at next AOS.
                            retained: Vec::new(),
                        }
                    });
                    post_toast(&toast_overlay_weak_for_save, &message);
                    // Restore retained batches (pending that still
                    // failed + the current batch if it failed) into
                    // `sstv_pending_export`. New pending items
                    // queued by a parallel AOS slip in *after* the
                    // retained set so retry order honours
                    // chronological pass start.
                    if !retained.is_empty() {
                        let mut pending = state_sstv_close.sstv_pending_export.borrow_mut();
                        let mut combined = retained;
                        combined.append(&mut pending);
                        *pending = combined;
                    }
                    // Drain only the current-pass images we
                    // actually snapshotted. Late frames pushed
                    // while the worker was running stay buffered
                    // for the next save cycle. Compare-and-clear
                    // by the recording-pass tuple so an
                    // overlapping pass's buffer/slot isn't wiped
                    // by a late completion callback. Per CR round
                    // 4 on PR #599.
                    let mut slot = state_sstv_close.sstv_recording_pass.borrow_mut();
                    if *slot == exported_sstv_pass {
                        if current_ok {
                            let mut completed = state_sstv_close.sstv_completed_images.borrow_mut();
                            let to_drain = exported_image_count.min(completed.len());
                            completed.drain(..to_drain);
                            if completed.is_empty() {
                                *slot = None;
                            }
                        } else {
                            // Failure path: clear the slot so the
                            // recorder isn't stuck in a permanent
                            // "pass in flight" state. The current
                            // images are already in `retained`
                            // (queued for retry under their own
                            // `dir`), so the buffer can be safely
                            // drained too — keeping them would
                            // duplicate-save on the next attempt.
                            let mut completed = state_sstv_close.sstv_completed_images.borrow_mut();
                            let to_drain = exported_image_count.min(completed.len());
                            completed.drain(..to_drain);
                            *slot = None;
                        }
                    }
                    drop(slot);
                    // Close the viewer on successful save AND only
                    // when the buffer is empty — if late frames
                    // arrived while saving, keep the viewer open
                    // so the user can see them rather than burying
                    // a tail. On failure: also keep open so the
                    // user can inspect the in-memory image and
                    // retry. Mirrors LRPT semantics from CR round
                    // 9 on PR #554, refined per CR round 4 #18 on
                    // PR #599.
                    if current_ok
                        && state_sstv_close.sstv_completed_images.borrow().is_empty()
                        && let Some(window) = exported_sstv_window_weak
                            .as_ref()
                            .and_then(glib::WeakRef::upgrade)
                    {
                        tracing::info!(
                            "auto-record LOS: closing SSTV viewer window after PNG save"
                        );
                        window.close();
                    }
                });
            }
            RecorderAction::RestoreTune(saved) => {
                tracing::info!(
                    "auto-record LOS: restoring tune to {} Hz (offset {} Hz), BW {} Hz",
                    saved.freq_hz,
                    saved.vfo_offset_hz,
                    saved.bandwidth_hz,
                );
                #[allow(
                    clippy::cast_sign_loss,
                    clippy::cast_possible_truncation,
                    reason = "saved freq came from the same widget we're feeding back; \
                              non-negative and well within u64"
                )]
                let freq_hz = saved.freq_hz as u64;
                tune_a(freq_hz, saved.mode, saved.bandwidth_hz);
                // Replay the user's pre-AOS VFO offset so a
                // dragged-from-centre carrier comes back. The
                // existing `DspToUi::VfoOffsetChanged` handler
                // updates the spectrum + freq selector + status
                // bar when the DSP echoes the change, so we
                // don't have to mirror those widgets manually.
                state_a.dispatch_vfo_offset(saved.vfo_offset_hz);
                // Re-engage ACARS if it was on before the pass.
                // Goes after the tune so the airband lock retunes
                // from the user's just-restored freq rather than
                // racing against it. Symmetric with the AOS arm.
                if state_a.acars_was_engaged_pre_pass.replace(false) {
                    tracing::info!("auto-record LOS: re-engaging ACARS (was on pre-pass)");
                    state_a.send_dsp(UiToDsp::SetAcarsEnabled(true));
                }
                // If the user had playback off pre-AOS, we
                // started the radio at AOS to make audio flow —
                // honour that round trip and stop it now. A user
                // who explicitly turned playback off during the
                // pass (rare) loses that intent here, but the
                // expected case (set-and-forget overnight) gets
                // the right behaviour. Routed through
                // `set_playing` (header play button) so the icon,
                // `state.is_running`, and DSP all move together.
                if !saved.was_running {
                    tracing::info!("auto-record: stopping source (was stopped pre-AOS)");
                    set_playing_a(false);
                }
                // **Order is load-bearing.** Restore the audio-
                // chain settings BEFORE re-arming the scanner.
                // A scanner that wakes back up while the audio
                // chain is still in its forced-off pass state
                // would briefly run with wrong squelch / CTCSS /
                // FM-IF-NR — wasting the first dwell or two on
                // un-gated noise. Per CR round 1 on PR #557.
                //
                // Same `set_*`-fires-notify pattern the rest of
                // this handler uses (and that the AOS-side
                // `force_audio_chain_off` mirrors). Squelch
                // level is restored unconditionally (cheap,
                // idempotent on equal value); the toggles are
                // guarded against no-op writes for cleaner
                // tracing logs.
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "saved.squelch_db came from the same SpinRow we're \
                              feeding back into; round-trip preserves precision \
                              well within the row's range"
                )]
                let squelch_db_f64 = f64::from(saved.squelch_db);
                squelch_level_row_a.set_value(squelch_db_f64);
                if saved.squelch_enabled != squelch_enabled_row_a.is_active() {
                    squelch_enabled_row_a.set_active(saved.squelch_enabled);
                }
                if saved.auto_squelch_enabled != auto_squelch_row_a.is_active() {
                    auto_squelch_row_a.set_active(saved.auto_squelch_enabled);
                }
                let saved_ctcss_idx =
                    sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(saved.ctcss_mode);
                if ctcss_row_a.selected() != saved_ctcss_idx {
                    ctcss_row_a.set_selected(saved_ctcss_idx);
                }
                if saved.fm_if_nr_enabled != fm_if_nr_row_a.is_active() {
                    fm_if_nr_row_a.set_active(saved.fm_if_nr_enabled);
                }
                // Re-arm the scanner if it was running pre-AOS.
                // The AOS-side `tune_a` call goes through
                // `tune_to_satellite`, which fires
                // `ScannerForceDisable::trigger("satellite tune")`
                // as a manual-tune side effect — without this
                // restore, an active pre-AOS scan would be left
                // permanently off after the pass. Same idiom as
                // `was_running`: snapshot the user's pre-AOS
                // state, return them to it. `set_active(true)`
                // fires the switch's notify handler, which
                // dispatches `SetScannerEnabled(true)` to the
                // engine — same path a manual flip takes.
                if saved.scanner_running && !scanner_switch_a.is_active() {
                    tracing::info!("auto-record: re-arming scanner (was running pre-AOS)");
                    scanner_switch_a.set_active(true);
                }
            }
            RecorderAction::Toast { message, kind } => {
                if matches!(kind, ToastKind::Warn) {
                    // No dedicated warn styling on AdwToast; the
                    // message itself carries the severity. Tracing
                    // captures it for the log either way.
                    tracing::warn!("auto-record: {message}");
                }
                post_toast(&toast_overlay_weak, &message);
            }
        })
    };

    // Stash a `Weak` handle to the interpreter on AppState so
    // the `AcarsEnabledChanged(Ok(false))` arm in
    // `handle_dsp_message` can replay deferred AOS actions
    // without needing the closure plumbed through its parameter
    // list. Stored weakly to avoid an `AppState` ↔ closure
    // retain cycle (the closure captures `Rc<AppState>`
    // transitively); the strong owner is the recorder tick
    // `glib::timeout_add_local`. Issue #589 / CR round 1 on
    // PR #591.
    *state.recorder_action_interpreter.borrow_mut() = Some(Rc::downgrade(&interpret_action));

    if cache.is_some() {
        let panel_weak_tick = panel_weak.clone();
        let state_for_recorder = Rc::clone(state);
        let displayed_tick = Rc::clone(&displayed);
        let recompute_tick = Rc::clone(&recompute);
        let recorder_tick = Rc::clone(&recorder);
        let interpret_tick = Rc::clone(&interpret_action);
        let state_tick = Rc::clone(state);
        let bandwidth_row_tick = panels.radio.bandwidth_row.clone();
        let spectrum_tick = Rc::clone(spectrum_handle);
        // #510 — notify scheduler + watched-set + lead time. The
        // lead time is read fresh from config on every tick so a
        // user edit (once we expose a setting) takes effect
        // immediately without restarting the timer.
        let watched_tick = Rc::clone(&watched);
        let notify_scheduler_tick = Rc::clone(&notify_scheduler);
        let config_tick = std::sync::Arc::clone(config);
        // Scanner master switch — read for the per-tick snapshot so
        // SavedTune carries scanner state across AOS → LOS, written
        // by `interpret_action::RestoreTune` to re-arm the scanner
        // if it was running pre-AOS. Strong clone because the tick
        // already captures other panel widgets (bandwidth_row);
        // when the panel is dropped the tick's `panel_weak.upgrade`
        // returns None and we Break, dropping the chain.
        let scanner_switch_tick = panels.scanner.master_switch.clone();
        // Audio-chain widgets snapshotted into SavedTune so the
        // pre-AOS state can be restored at LOS. AOS force-disables
        // these because they're destructive to data-bearing FM
        // signals (squelch / CTCSS gate audio; FM IF NR zeros the
        // sidebands the APT subcarrier lives in). Per #555 / #556.
        let squelch_enabled_row_tick = panels.radio.squelch_enabled_row.clone();
        let auto_squelch_row_tick = panels.radio.auto_squelch_row.clone();
        let squelch_level_row_tick = panels.radio.squelch_level_row.clone();
        let ctcss_row_tick = panels.radio.ctcss_row.clone();
        let fm_if_nr_row_tick = panels.radio.fm_if_nr_row.clone();
        let _ = glib::timeout_add_local(SATELLITES_COUNTDOWN_TICK, move || {
            let Some(panel) = panel_weak_tick.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = chrono::Utc::now();
            let mut needs_recompute = false;
            for entry in displayed_tick.borrow().iter() {
                if entry.pass.end <= now {
                    needs_recompute = true;
                    continue;
                }
                entry.row.set_title(&format_pass_title(&entry.pass, now));
            }
            // Drive the auto-record state machine. Snapshot the
            // pass list (cloned out of the displayed vec to keep
            // the borrow short) and the current tune so the
            // recorder gets a consistent view. Capture the VFO
            // offset alongside centre frequency — a user-dragged
            // carrier position needs to survive the AOS→LOS round
            // trip.
            let passes_snapshot: Vec<Pass> = displayed_tick
                .borrow()
                .iter()
                .map(|e| e.pass.clone())
                .collect();
            let auto_record_on = panel.auto_record_switch.is_active();
            // Per #533: the "also save audio" toggle is sampled
            // exclusively at AOS by the state machine; flipping
            // it mid-pass does NOT retroactively start or stop
            // recording (matches `auto_record_on`'s
            // "in-flight pass keeps running" semantics).
            let audio_record_on = panel.auto_record_audio_switch.is_active();
            // Round f64 SpinRow value to u32 at the snapshot
            // boundary so SavedTune carries a clean integer for
            // the eventual restore — no per-restore rounding.
            #[allow(
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "user-set bandwidth is non-negative and \
                          fits in u32 for any realistic SDR channel \
                          width; the SpinRow's own min is positive"
            )]
            let bandwidth_hz_u32 = bandwidth_row_tick.value().round() as u32;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "squelch SpinRow value is in dBFS, bounded by the row's \
                          configured min/max (well within f32 range)"
            )]
            let squelch_db_f32 = squelch_level_row_tick.value() as f32;
            let now_tune = SavedTune {
                freq_hz: state_tick.center_frequency.get(),
                vfo_offset_hz: spectrum_tick.vfo_offset_hz(),
                mode: state_tick.demod_mode.get(),
                bandwidth_hz: bandwidth_hz_u32,
                was_running: state_tick.is_running.get(),
                scanner_running: scanner_switch_tick.is_active(),
                squelch_enabled: squelch_enabled_row_tick.is_active(),
                auto_squelch_enabled: auto_squelch_row_tick.is_active(),
                squelch_db: squelch_db_f32,
                ctcss_mode: sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(
                    ctcss_row_tick.selected(),
                ),
                fm_if_nr_enabled: fm_if_nr_row_tick.is_active(),
            };
            // Read the user's selected quality tier on every
            // tick — cheap (just a ComboRow.selected() call), and
            // means a mid-pass change applies immediately to the
            // next eligible pass without a restart. Per #511.
            let min_elev_deg =
                AutoRecordQuality::from_index(panel.auto_record_quality_row.selected())
                    .min_elev_deg();
            let actions = recorder_tick.borrow_mut().tick(
                now,
                &passes_snapshot,
                auto_record_on,
                audio_record_on,
                min_elev_deg,
                now_tune,
            );
            // ACARS-disengage gate (issue #589): if any action
            // in this tick is `StartAutoRecord` AND ACARS is
            // currently engaged, stash the **whole batch** and
            // dispatch `SetAcarsEnabled(false)`. The
            // `AcarsEnabledChanged(Ok(false))` arm in
            // `handle_dsp_message` will drain the batch and
            // replay every action through `interpret_tick` once
            // the controller acks the disengage.
            //
            // Stashing the whole batch (not just
            // `StartAutoRecord`) makes the disengage ack a real
            // gate: same-tick siblings like
            // `StartAutoAudioRecord` and `ResetImagingDecoders`
            // would otherwise execute while the source was
            // still on airband geometry, capturing audio from
            // the wrong frequency until the disengage lands.
            // CR round 1 on PR #591.
            let needs_acars_gate = state_for_recorder.acars_enabled.get()
                && actions
                    .iter()
                    .any(|a| matches!(a, RecorderAction::StartAutoRecord { .. }));
            if needs_acars_gate {
                tracing::info!(
                    "auto-record AOS: gating {} action(s) on ACARS disengage ack",
                    actions.len()
                );
                state_for_recorder.acars_was_engaged_pre_pass.set(true);
                *state_for_recorder.pending_aos_actions.borrow_mut() = Some(actions);
                state_for_recorder.send_dsp(UiToDsp::SetAcarsEnabled(false));
            } else {
                for action in actions {
                    interpret_tick(action);
                }
            }

            // #510 — pre-pass desktop alerts. Walk the displayed
            // pass list, map each to (norad_id, &Pass), feed the
            // scheduler. Pure function in / pure actions out;
            // notification I/O happens in the action loop below.
            let lead_min = load_notify_lead_min(&config_tick);
            let lead = chrono::Duration::minutes(i64::from(lead_min));
            let watched_snapshot = watched_tick.borrow().clone();
            let notify_actions = {
                let displayed_borrow = displayed_tick.borrow();
                let pairs: Vec<(u32, &Pass)> = displayed_borrow
                    .iter()
                    .filter_map(|e| norad_id_for_pass(&e.pass).map(|id| (id, &e.pass)))
                    .collect();
                notify_scheduler_tick
                    .borrow_mut()
                    .tick(now, lead, lead_min, pairs, |id| {
                        watched_snapshot.contains(&id)
                    })
            };
            for action in notify_actions {
                match action {
                    NotifyAction::Fire {
                        norad_id,
                        pass,
                        lead_min,
                    } => {
                        crate::notify::send_pass_alert(&pass, norad_id, lead_min);
                    }
                }
            }

            if needs_recompute {
                recompute_tick();
            }
            glib::ControlFlow::Continue
        });
    }
}

/// Cadence of the Doppler tracker's trigger re-evaluation —
/// 1 Hz. Spec §2's overhead-and-frequency-match test only
/// needs to flip on horizon crossing / dial change, which is
/// always slower than 1 s. Cheap: one SGP4 propagate per
/// catalog entry within the ±20 kHz window — typically zero
/// or one sat at a time.
const DOPPLER_TRIGGER_TICK: Duration = Duration::from_secs(1);

/// Cadence of the Doppler tracker's offset recompute — 4 Hz
/// (250 ms). Per spec §3, fast enough that the residual
/// frequency error between updates stays inside the channel
/// filter, slow enough that the bus + status-bar updates
/// don't hammer GTK.
const DOPPLER_RECOMPUTE_TICK: Duration = Duration::from_millis(250);

/// Wire the Aviation sidebar panel: toggle switch → DSP, 4 Hz tick
/// for status/channel-row refresh, the open-viewer button, and the
/// output-formatter controls (station ID, JSONL log, network feeder).
#[allow(clippy::too_many_lines)]
fn connect_aviation_panel(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    toast_overlay: &adw::ToastOverlay,
) {
    use crate::sidebar::aviation_panel::{
        GLYPH_IDLE, GLYPH_LOCKED, GLYPH_SIGNAL, SIDEBAR_STATUS_REFRESH_MS, rebuild_channel_rows,
        region_combo_index, region_from_combo_index,
    };
    use sdr_acars::ChannelLockState;
    use sdr_core::acars_airband_lock::{AcarsRegion, validate_custom_channels};

    // ─── Region selector seed + signal (issue #581 / #592) ───
    // Read the persisted region, dispatch it to DSP at startup,
    // and seed the combo index BEFORE wiring the change handler
    // — otherwise the seed itself would fire a redundant
    // dispatch + persist round-trip.
    let saved_region_id = crate::acars_config::read_acars_channel_set(config);
    let initial_region = if saved_region_id == "custom" {
        // Two-key load: read channels, validate, then build
        // Custom — or fall back to default on bad/stale config.
        // `Custom([])` would reach the engage guard as invalid
        // (validate_custom_channels rejects empty), so we must
        // not dispatch it at startup.
        let saved_chans = crate::acars_config::read_acars_custom_channels(config);
        match validate_custom_channels(&saved_chans) {
            Ok(()) => AcarsRegion::Custom(saved_chans.into_boxed_slice()),
            Err(e) => {
                tracing::warn!(
                    "saved custom ACARS channels invalid ({e}); \
                     falling back to default region"
                );
                AcarsRegion::default()
            }
        }
    } else {
        AcarsRegion::from_config_id(saved_region_id.as_str())
    };
    panel
        .region_row
        .set_selected(region_combo_index(&initial_region));
    // Rebuild channel rows to match the seeded region's channel
    // count (predefined = 6; Custom = saved list len, possibly 0).
    rebuild_channel_rows(panel, initial_region.channels().len());
    // Hydrate the Custom EntryRow text from saved config so the
    // user sees their last entry next to the (now-visible) row.
    {
        let saved_chans = crate::acars_config::read_acars_custom_channels(config);
        if !saved_chans.is_empty() {
            let csv = saved_chans
                .iter()
                .map(|hz| format!("{:.3}", hz / 1_000_000.0))
                .collect::<Vec<_>>()
                .join(", ");
            panel.custom_channels_row.set_text(&csv);
        }
    }
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsRegion(initial_region));
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        // Capture the panel widgets needed by the rebuild path.
        // Each widget already holds a strong ref into the GTK
        // tree, so cloning here is the same cheap GObject ref
        // bump the rest of `connect_aviation_panel` uses. The
        // `channel_rows` Rc is the SAME inner cell the 4 Hz
        // tick reads — this is what keeps that tick from
        // operating on a stale row snapshot.
        let channels_group = panel.channels_group.clone();
        let channel_rows_cell = std::rc::Rc::clone(&panel.channel_rows);
        let custom_row_for_dispatch = panel.custom_channels_row.clone();
        panel.region_row.connect_selected_notify(move |row| {
            // Guard against transient ComboRow indices. The model
            // briefly emits selection notifications during
            // teardown / repopulate that don't correspond to a
            // real `REGION_OPTIONS` slot. If the round-trip
            // through `region_from_combo_index` lands on a
            // different index than the selector reported, the
            // value is transient — skip persistence + dispatch
            // so a UI quirk doesn't churn config or fire a
            // needless DSP command. CR round 1 on PR #593.
            let selected = row.selected();
            let mut region = region_from_combo_index(selected);
            if region_combo_index(&region) != selected {
                tracing::debug!(
                    selected,
                    "acars region combo emitted transient index; ignoring"
                );
                return;
            }
            // Custom slot: hydrate from saved config (or the
            // current EntryRow text if the user just typed
            // something but hasn't pressed Enter yet). Empty
            // list is fine — the variant is dispatched as
            // `Custom([])` and the apply handler later
            // replaces it with a real list.
            if matches!(region, AcarsRegion::Custom(_)) {
                let saved = crate::acars_config::read_acars_custom_channels(&config);
                region = AcarsRegion::Custom(saved.into_boxed_slice());
                // Make the EntryRow visible immediately on
                // selection — the visibility-binding closure in
                // the panel builder also fires on this same
                // notify, but invoking the binding side-effect
                // here would require ordering guarantees we
                // don't have. Cheap idempotent set is fine.
                custom_row_for_dispatch.set_visible(true);
            }
            crate::acars_config::save_acars_channel_set(&config, region.config_id());
            // Rebuild channel rows to match the new region's
            // channel count — borrow_mut + adw mutation must
            // happen before send_dsp because the 4 Hz tick will
            // start consulting the new row count almost
            // immediately.
            let new_count = region.channels().len();
            // Inline the rebuild (the helper takes
            // `&AviationPanel`, but we only have the
            // individual fields here in the closure).
            {
                let mut rows = channel_rows_cell.borrow_mut();
                for row in rows.iter() {
                    channels_group.remove(row);
                }
                rows.clear();
                for _ in 0..new_count {
                    let row = adw::ActionRow::builder().title("—").subtitle("—").build();
                    channels_group.add(&row);
                    rows.push(row);
                }
            }
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsRegion(region));
        });
    }

    // ─── Custom-channels apply handler (issue #592) ───
    // Fires on Enter or focus-out. Parses CSV → multiplies by
    // 1e6 → validates via `validate_custom_channels`. On success
    // persists + dispatches a `Custom` variant with real
    // frequencies. On failure: toast naming the problem + add
    // the `error` CSS class (cleared on success).
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        let toast_overlay = toast_overlay.clone();
        let channels_group = panel.channels_group.clone();
        let channel_rows_cell = std::rc::Rc::clone(&panel.channel_rows);
        panel.custom_channels_row.connect_apply(move |row| {
            let text = row.text();
            // Stage 1: parse CSV → Vec<f64> (Hz). Empty entries
            // (e.g. trailing comma) are silently skipped; a
            // truly empty list will be caught in stage 2 by
            // `validate_custom_channels`.
            let parsed: Result<Vec<f64>, String> = text
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<f64>()
                        .map(|mhz| mhz * 1_000_000.0)
                        .map_err(|e| format!("'{s}': {e}"))
                })
                .collect();
            let chans = match parsed {
                Ok(v) => v,
                Err(e) => {
                    row.add_css_class("error");
                    let toast = adw::Toast::builder()
                        .title(format!("Invalid custom channels: {e}"))
                        .timeout(5)
                        .build();
                    toast_overlay.add_toast(toast);
                    return;
                }
            };
            // Stage 2: domain validation (size, finite, span).
            if let Err(e) = validate_custom_channels(&chans) {
                row.add_css_class("error");
                let toast = adw::Toast::builder()
                    .title(e.to_string())
                    .timeout(5)
                    .build();
                toast_overlay.add_toast(toast);
                return;
            }
            // Validated — clear any prior error styling and
            // commit (persist + rebuild rows + dispatch).
            row.remove_css_class("error");
            crate::acars_config::save_acars_custom_channels(&config, &chans);
            // Rebuild channel rows to match the new custom-
            // channel count — same inline pattern as the
            // region-change handler above.
            let new_count = chans.len();
            {
                let mut rows = channel_rows_cell.borrow_mut();
                for row in rows.iter() {
                    channels_group.remove(row);
                }
                rows.clear();
                for _ in 0..new_count {
                    let r = adw::ActionRow::builder().title("—").subtitle("—").build();
                    channels_group.add(&r);
                    rows.push(r);
                }
            }
            let region = AcarsRegion::Custom(chans.into_boxed_slice());
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsRegion(region));
        });
    }

    // ─── Toggle: switch-row → SetAcarsEnabled ───
    // Set `acars_pending` BEFORE dispatching so the 4 Hz mirror
    // tick (below) skips the switch-state mirror until the
    // AcarsEnabledChanged ack lands. Without this, the tick can
    // see the not-yet-updated `acars_enabled` cell, flip the
    // switch back to its old state, and re-enter this same
    // notify handler with the inverse SetAcarsEnabled —
    // racing the original request.
    {
        let state = Rc::clone(state);
        panel.enable_switch.connect_active_notify(move |row| {
            state.acars_pending.set(true);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsEnabled(
                row.is_active(),
            ));
        });
    }

    // ─── 4 Hz tick: AppState → switch row + status subtitle + per-channel rows ───
    // Hold weak refs only so closing the window drops the panel
    // widgets and the timer self-cancels via Break on the next
    // fire. Strong refs would keep the panel + AppState alive
    // for the rest of the process. The channel-rows view is an
    // `Rc<RefCell<…>>` clone so the rebuild handler can swap the
    // row list under us without the tick needing a re-snapshot.
    // We hold a STRONG ref to the cell here — the cell itself
    // is small (one `Vec<ActionRow>` plus a borrow flag) and
    // the rows it owns are dropped in lock-step with the panel
    // widgets when the window closes; the switch/status weak
    // refs gate Break on the next fire either way.
    let switch_weak = panel.enable_switch.downgrade();
    let status_weak = panel.status_row.downgrade();
    let channel_rows_for_tick = std::rc::Rc::clone(&panel.channel_rows);
    let state_for_tick = Rc::clone(state);
    glib::timeout_add_local(
        std::time::Duration::from_millis(SIDEBAR_STATUS_REFRESH_MS),
        move || {
            let (Some(switch), Some(status)) = (switch_weak.upgrade(), status_weak.upgrade())
            else {
                return glib::ControlFlow::Break;
            };

            let enabled = state_for_tick.acars_enabled.get();

            // Mirror Cell→switch one direction only — but only
            // when no SetAcarsEnabled is in flight. While
            // pending, the user's just-typed switch value is
            // authoritative; mirroring back from the still-old
            // `enabled` Cell would race the in-flight command.
            // Cleared by every AcarsEnabledChanged arm.
            if !state_for_tick.acars_pending.get() && switch.is_active() != enabled {
                switch.set_active(enabled);
            }

            // Status subtitle.
            let total = state_for_tick.acars_total_count.get();
            let last_label = state_for_tick
                .acars_recent
                .borrow()
                .back()
                .map(|m| format!("Last: {}", format_relative_age(m.timestamp)));
            let subtitle = if enabled {
                match last_label {
                    Some(s) => format!("Decoded {total} · {s}"),
                    None => format!("Decoded {total} · Awaiting first message"),
                }
            } else {
                "Disabled".to_string()
            };
            status.set_subtitle(&subtitle);

            // Per-channel rows. Read the LIVE row list each tick
            // so a region swap (which drops + recreates rows
            // under `channel_rows_for_tick`) is reflected
            // immediately — a snapshot taken at wire time would
            // hold weak refs into the OLD generation of rows.
            // `channel_stats` is a `Vec` sized from the active
            // region (Task 9 migration); the two lengths can
            // transiently differ around a region swap (panel
            // rebuild lags the next DSP-side stats emission).
            // Zip over the shorter of the two so neither side
            // panics during the window. Issue #592.
            let channel_stats = state_for_tick.acars_channel_stats.borrow();
            let rows = channel_rows_for_tick.borrow();
            for (row, ch) in rows.iter().zip(channel_stats.iter()) {
                let glyph = match ch.lock_state {
                    ChannelLockState::Locked => GLYPH_LOCKED,
                    ChannelLockState::Idle => GLYPH_IDLE,
                    ChannelLockState::Signal => GLYPH_SIGNAL,
                };
                row.set_title(&format!("{glyph}  {:.3} MHz", ch.freq_hz / 1_000_000.0));
                row.set_subtitle(&format!(
                    "{} msgs · {:.1} dB · {}",
                    ch.msg_count,
                    ch.level_db,
                    ch.last_msg_at
                        .map_or_else(|| "—".to_string(), format_relative_age)
                ));
            }
            glib::ControlFlow::Continue
        },
    );

    // ─── Open ACARS window button ───
    {
        let state = Rc::clone(state);
        panel.open_viewer_button.connect_clicked(move |_| {
            crate::acars_viewer::open_acars_viewer_if_needed(&state);
        });
    }

    // ─── Output-formatter widget seed + wiring (issue #578) ───
    // Seed text fields and toggles BEFORE wiring signal handlers so
    // the initial set_text / set_active calls do NOT fire save or
    // dispatch. The explicit send_dsp block at the end delivers the
    // persisted state to the controller unconditionally.

    // Seed text fields first. Station ID is normalized to
    // the same 8-char cap the DSP enforces (see
    // `controller.rs::handle_set_acars_station_id`), and
    // healed back to config when the persisted value is over
    // the cap — otherwise the field, the config, and the
    // DSP would briefly hold three different values until
    // the user touches the row. CR round 5 on PR #595.
    let raw_station_id = crate::acars_config::read_acars_station_id(config);
    let normalized_station_id: String = raw_station_id.chars().take(8).collect();
    if normalized_station_id != raw_station_id {
        crate::acars_config::save_acars_station_id(config, &normalized_station_id);
    }
    panel.station_id_row.set_text(&normalized_station_id);
    panel
        .jsonl_path_row
        .set_text(&crate::acars_config::read_acars_jsonl_path(config));
    panel
        .network_addr_row
        .set_text(&crate::acars_config::read_acars_network_addr(config));

    // Seed toggles (bind_property in the panel builder already handles
    // path/addr row visibility, so these calls also show/hide them).
    panel
        .jsonl_enable_row
        .set_active(crate::acars_config::read_acars_jsonl_enabled(config));
    panel
        .network_enable_row
        .set_active(crate::acars_config::read_acars_network_enabled(config));

    // Set subtitle to match current toggle state — signal
    // handlers only fire on user-initiated changes, so we
    // need to seed the subtitle explicitly. CR round 1 on
    // PR #595.
    {
        let path = panel.jsonl_path_row.text();
        let active = panel.jsonl_enable_row.is_active();
        let subtitle = if active {
            if path.is_empty() {
                "~/sdr-recordings/acars.jsonl".to_string()
            } else {
                path.to_string()
            }
        } else {
            "Off".to_string()
        };
        panel.jsonl_enable_row.set_subtitle(&subtitle);
    }
    {
        let addr = panel.network_addr_row.text();
        let active = panel.network_enable_row.is_active();
        let subtitle = if active {
            if addr.is_empty() {
                "feed.airframes.io:5550".to_string()
            } else {
                addr.to_string()
            }
        } else {
            "Off".to_string()
        };
        panel.network_enable_row.set_subtitle(&subtitle);
    }

    // Wire station_id_row — per-keystroke save + dispatch.
    // Station ID changes are cheap on the DSP side (just
    // updates a string field, no reopen), so live updates
    // are fine and match the codebase's existing pattern for
    // text-row config (server_panel.rs nickname,
    // source_panel.rs file path). CR round 4 on PR #595 —
    // `connect_apply` only fires on Enter, so it would lose
    // edits committed via focus-out or app close.
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        panel.station_id_row.connect_changed(move |row| {
            let value = row.text().to_string();
            crate::acars_config::save_acars_station_id(&config, &value);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsStationId(value));
        });
    }

    // Wire jsonl_enable_row toggle.
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        let path_row = panel.jsonl_path_row.clone();
        panel.jsonl_enable_row.connect_active_notify(move |row| {
            let active = row.is_active();
            let path = path_row.text().to_string();
            // Persist + dispatch the latest path on EVERY
            // toggle edge so an edit-then-toggle-off sequence
            // doesn't leave config / DSP holding the old
            // value. The user's typed text is always the
            // source of truth. CR round 3 on PR #595.
            crate::acars_config::save_acars_jsonl_path(&config, &path);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlPath(path.clone()));
            crate::acars_config::save_acars_jsonl_enabled(&config, active);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlEnabled(active));
            // Subtitle reflects current path or "Off".
            let subtitle = if active {
                if path.is_empty() {
                    "~/sdr-recordings/acars.jsonl".to_string()
                } else {
                    path
                }
            } else {
                "Off".to_string()
            };
            row.set_subtitle(&subtitle);
        });
    }

    // Wire jsonl_path_row — per-keystroke save to config so
    // edits never get lost on focus-out / app close, plus
    // explicit-commit DSP dispatch on Enter (apply) so we
    // don't reopen the writer 17 times while the user types
    // a path. Toggle handlers read the live row text
    // separately, so toggling on after typing without Enter
    // dispatches the latest value too. CR round 4 on PR #595.
    {
        let config = std::sync::Arc::clone(config);
        panel.jsonl_path_row.connect_changed(move |row| {
            crate::acars_config::save_acars_jsonl_path(&config, &row.text());
        });
    }
    {
        let state = Rc::clone(state);
        let enable_row = panel.jsonl_enable_row.clone();
        panel.jsonl_path_row.connect_apply(move |row| {
            let value = row.text().to_string();
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlPath(
                value.clone(),
            ));
            // Keep the enable-row subtitle in sync — when the
            // toggle is on, the subtitle shows the current
            // path. CR round 2 on PR #595.
            if enable_row.is_active() {
                let subtitle = if value.is_empty() {
                    "~/sdr-recordings/acars.jsonl".to_string()
                } else {
                    value
                };
                enable_row.set_subtitle(&subtitle);
            }
        });
    }

    // Wire network_enable_row toggle.
    {
        let state = Rc::clone(state);
        let config = std::sync::Arc::clone(config);
        let addr_row = panel.network_addr_row.clone();
        panel.network_enable_row.connect_active_notify(move |row| {
            let active = row.is_active();
            let addr = addr_row.text().to_string();
            // Persist + dispatch the latest addr on EVERY
            // toggle edge — same pattern as jsonl above.
            // CR round 3 on PR #595.
            crate::acars_config::save_acars_network_addr(&config, &addr);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkAddr(
                addr.clone(),
            ));
            crate::acars_config::save_acars_network_enabled(&config, active);
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkEnabled(active));
            // Subtitle reflects current addr or "Off".
            let subtitle = if active {
                if addr.is_empty() {
                    "feed.airframes.io:5550".to_string()
                } else {
                    addr
                }
            } else {
                "Off".to_string()
            };
            row.set_subtitle(&subtitle);
        });
    }

    // Wire network_addr_row — same split as jsonl_path_row:
    // per-keystroke save to config (no DNS/dial spam), DSP
    // dispatch on apply. CR round 4 on PR #595.
    {
        let config = std::sync::Arc::clone(config);
        panel.network_addr_row.connect_changed(move |row| {
            crate::acars_config::save_acars_network_addr(&config, &row.text());
        });
    }
    {
        let state = Rc::clone(state);
        let enable_row = panel.network_enable_row.clone();
        panel.network_addr_row.connect_apply(move |row| {
            let value = row.text().to_string();
            state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkAddr(
                value.clone(),
            ));
            // Keep the enable-row subtitle in sync — when the
            // toggle is on, the subtitle shows the current
            // addr. CR round 2 on PR #595.
            if enable_row.is_active() {
                let subtitle = if value.is_empty() {
                    "feed.airframes.io:5550".to_string()
                } else {
                    value
                };
                enable_row.set_subtitle(&subtitle);
            }
        });
    }

    // Initial dispatch — deliver persisted values to the controller so
    // writers reopen on launch if previously enabled. Path/addr/station_id
    // are dispatched BEFORE enabled so the controller's handler finds the
    // pending paths already set when it processes the enabled flags.
    // Use the (already-normalized) row text rather than the
    // raw config value so the initial DSP-side state matches
    // exactly what the user sees. CR round 5 on PR #595.
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsStationId(
        panel.station_id_row.text().to_string(),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlPath(
        crate::acars_config::read_acars_jsonl_path(config),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkAddr(
        crate::acars_config::read_acars_network_addr(config),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsJsonlEnabled(
        crate::acars_config::read_acars_jsonl_enabled(config),
    ));
    state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsNetworkEnabled(
        crate::acars_config::read_acars_network_enabled(config),
    ));
}

/// Format a `SystemTime` as a relative age string ("5s ago",
/// "2m ago", "1h ago"). Returns "—" if the timestamp is in the
/// future or unrepresentable.
/// Walk the most recent rows of the viewer store backwards from
/// the end and check for a `(aircraft, mode, label, text)` key
/// match within `ACARS_COLLAPSE_WINDOW`. Returns the matched
/// row's index after bumping its count + `last_seen` in place,
/// or `None` if no in-window match — in which case the caller
/// appends a fresh row. Stops walking as soon as it sees a row
/// older than the recency window (rows are insertion-ordered
/// in the underlying store, oldest at index 0). Issue #586.
fn try_collapse_into_existing(
    store: &gtk4::gio::ListStore,
    msg: &sdr_acars::AcarsMessage,
) -> Option<u32> {
    use gtk4::prelude::ListModelExt;
    let n = store.n_items();
    if n == 0 {
        return None;
    }
    let cutoff = msg
        .timestamp
        .checked_sub(crate::acars_viewer::ACARS_COLLAPSE_WINDOW)?;
    let mut idx = n;
    while idx > 0 {
        idx -= 1;
        let Some(item) = store.item(idx) else {
            continue;
        };
        let Some(obj) = item.downcast_ref::<crate::acars_viewer::AcarsMessageObject>() else {
            continue;
        };
        // Skip rows older than the recency window. We can't
        // early-exit here even though insertion order would
        // suggest later rows have even older `last_seen`:
        // `record_duplicate` updates an existing row's
        // `last_seen` IN PLACE (no store reorder), so the
        // "monotonic by index" invariant doesn't hold once any
        // collapse has fired. CR round 1 on PR #591.
        if obj.last_seen() < cutoff {
            continue;
        }
        let inner = obj.imp().inner.borrow();
        let Some(existing) = inner.as_ref() else {
            continue;
        };
        if existing.aircraft == msg.aircraft
            && existing.mode == msg.mode
            && existing.label == msg.label
            && existing.text == msg.text
        {
            // Drop the borrow before mutating via the public
            // API (which doesn't actually need the borrow held,
            // but keeping the scope tight is cleaner).
            drop(inner);
            obj.record_duplicate(msg.timestamp);
            return Some(idx);
        }
    }
    None
}

fn format_relative_age(ts: std::time::SystemTime) -> String {
    let Ok(elapsed) = ts.elapsed() else {
        return "—".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Minimum |Δoffset| (Hz) before re-dispatching `SetVfoOffset`
/// from the 4 Hz recompute tick. Sub-5-Hz changes are below
/// the channel filter's pass-band granularity for any LEO
/// imaging downlink we care about, so suppressing them is
/// pure bus-traffic relief.
const DOPPLER_DISPATCH_THRESHOLD_HZ: f64 = 5.0;

/// Restore the persisted Doppler master-switch state to the
/// widget and wire change-notify to save back. Always called,
/// regardless of TLE-cache availability — the user's preference
/// must survive a launch where the cache happened to be
/// unavailable. The behavioral wiring (timers + tracker) lives
/// in [`connect_doppler_tracker`] and is gated separately.
/// Per CR round 1 on PR #554.
fn restore_doppler_switch(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let persisted = sidebar::satellites_panel::load_doppler_tracking_enabled(config);
    panels.satellites.doppler_switch.set_active(persisted);

    let config = std::sync::Arc::clone(config);
    panels
        .satellites
        .doppler_switch
        .connect_active_notify(move |row| {
            sidebar::satellites_panel::save_doppler_tracking_enabled(&config, row.is_active());
        });
}

/// Wire the [`DopplerTracker`](crate::doppler_tracker::DopplerTracker):
/// 1 Hz trigger re-evaluation tick, 4 Hz offset-recompute
/// tick, status-bar update, [`UiToDsp::SetVfoOffset`] dispatch
/// (rate-limited to changes >`DOPPLER_DISPATCH_THRESHOLD_HZ`).
/// Per #521 and the design spec at
/// `docs/superpowers/specs/2026-04-26-doppler-correction-design.md`.
///
/// Master-switch persistence + initial restore happens in
/// [`restore_doppler_switch`], which is called unconditionally
/// from [`connect_satellites_panel`]. This function adds a
/// **second** change-notify handler on the same widget that
/// drives the tracker model — multiple GTK signal handlers on
/// one widget fire independently, no conflict. Wired only when
/// the TLE cache is available; without TLEs the trigger
/// re-evaluate has no candidate to engage.
#[allow(
    clippy::too_many_lines,
    reason = "three chained closures (master-switch handler + two timers) all \
              live in one function so they share the `tracker` and \
              `last_dispatched` Rcs by direct clone; splitting would mean \
              hoisting those onto AppState, which the design spec §4 \
              already explicitly defers"
)]
fn connect_doppler_tracker(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    status_bar: &Rc<StatusBar>,
) {
    use crate::doppler_tracker::{
        Candidate, DopplerTracker, FREQ_MATCH_TOLERANCE_HZ, compute_doppler_offset_hz,
        pick_active_satellite, should_tick,
    };
    use sdr_sat::{GroundStation, KNOWN_SATELLITES, Satellite, track};

    // Read the widget's current state — it was already restored
    // (and a persistence handler wired) by `restore_doppler_switch`,
    // which runs unconditionally before we enter this cache-gated
    // path. Per CR round 1 on PR #554.
    let initial = panels.satellites.doppler_switch.is_active();

    let tracker: Rc<RefCell<DopplerTracker>> = Rc::new(RefCell::new(DopplerTracker::new(initial)));

    // The dispatch baseline lives on `AppState` as
    // `last_dispatched_vfo_offset_hz` — written by the
    // `connect_vfo_offset_changed` callback, which fires from
    // BOTH the DSP echo (`DspToUi::VfoOffsetChanged`) and direct
    // user-drag dispatches. The tracker reads from there for
    // its rate-limit gate, so external writes (auto-record AOS
    // reset, spectrum drag) keep the baseline in sync — no
    // stale local value to worry about. Per CR round 7 on PR
    // #554. The fallback paths below also write the baseline
    // directly when they dispatch a `SetVfoOffset(user_ref)`
    // flush, so re-engagement within `DOPPLER_DISPATCH_THRESHOLD_HZ`
    // of the prior live value isn't suppressed.

    // Master-switch handler that drives the tracker. (A separate
    // change-notify handler in `restore_doppler_switch` already
    // persists the value — multiple GTK signal handlers fire
    // independently, no conflict.) On disable, `set_master_enabled`
    // atomically clears `active`, captures and resets
    // `user_reference_offset_hz`, and returns the captured value
    // for us to flush to DSP.
    {
        let tracker = Rc::clone(&tracker);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        panels
            .satellites
            .doppler_switch
            .connect_active_notify(move |row| {
                let enabled = row.is_active();
                let mut t = tracker.borrow_mut();
                let was_active = t.active().is_some();
                let final_offset = t.set_master_enabled(enabled);
                drop(t);
                // Only dispatch the fallback `SetVfoOffset` when
                // a satellite was actually being tracked. Without
                // this guard, toggling Doppler off while no
                // satellite is engaged would still send
                // `SetVfoOffset(0.0)` and clobber any non-zero
                // VFO offset the user had set independently. Per
                // CR round 3 on PR #554.
                if was_active && let Some(offset) = final_offset {
                    state.dispatch_vfo_offset(offset);
                    status_bar.update_doppler(None);
                }
            });
    }

    // 1 Hz trigger re-evaluation tick: rebuild the candidate
    // list from catalog × frequency match × ground station ×
    // cached TLEs, run `pick_active_satellite`, and call
    // `set_active` on the tracker. On a transition to None
    // (e.g. user retunes off the satellite, or the satellite
    // sets), dispatch a final SetVfoOffset(user_reference) and
    // clear the status bar — same teardown the master-switch
    // handler does for the off-while-active case.
    {
        let tracker = Rc::clone(&tracker);
        let cache = std::sync::Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let panel_weak = panels.satellites.downgrade();
        let _ = glib::timeout_add_local(DOPPLER_TRIGGER_TICK, move || {
            let Some(panel) = panel_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let mut t = tracker.borrow_mut();
            // Lifecycle gate: master + running. While stopped,
            // no candidate rebuild + no `set_active` transition,
            // so a satellite setting below the horizon mid-stop
            // doesn't fire a spurious disengage dispatch into a
            // stopped DSP. On resume, this tick re-evaluates and
            // engages / disengages naturally against the live
            // geometry. Per #567.
            if !should_tick(t.master_enabled(), state.is_running.get()) {
                return glib::ControlFlow::Continue;
            }
            // Build the ground station from the live panel
            // values — the user can edit lat/lon/alt mid-pass
            // and the tracker should follow.
            let station = GroundStation::new(
                panel.lat_row.value(),
                panel.lon_row.value(),
                panel.alt_row.value(),
            );
            let now = chrono::Utc::now();
            let current_freq = state.center_frequency.get();

            // Build the candidate list: every catalog entry
            // whose downlink is within ±FREQ_MATCH_TOLERANCE_HZ
            // of the radio's current centre frequency, paired
            // with its currently-evaluated elevation. Iterate
            // in `KNOWN_SATELLITES` order so the spec §2
            // tie-break (earlier entry wins) is deterministic.
            let mut candidates: Vec<Candidate> = Vec::new();
            for sat in KNOWN_SATELLITES {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "catalog downlinks sit in the 100s of MHz, well \
                              below f64's 2^53 mantissa ceiling"
                )]
                let downlink = sat.downlink_hz as f64;
                if (downlink - current_freq).abs() > FREQ_MATCH_TOLERANCE_HZ {
                    continue;
                }
                let Ok((line1, line2)) = cache.cached_tle_for(sat.norad_id) else {
                    continue;
                };
                let Ok(parsed) = Satellite::from_tle(sat.name, &line1, &line2) else {
                    continue;
                };
                let Ok(track) = track(&station, &parsed, now) else {
                    continue;
                };
                candidates.push(Candidate {
                    satellite: sat,
                    elevation_deg: track.elevation_deg,
                });
            }

            let new_active = pick_active_satellite(t.master_enabled(), &candidates);
            // Capture pre-`set_active` state so we can:
            //   1. Flush back to the prior user reference on a
            //      Some → None disengage (`set_active` resets
            //      `user_reference_offset_hz` to 0 on any change,
            //      so reading it AFTER would always give 0).
            //   2. Decide whether this is a fresh engagement
            //      (None → Some) vs. a satellite swap
            //      (Some(A) → Some(B)) — only the former should
            //      seed `user_reference_offset_hz` from the live
            //      spectrum offset. On a swap, the live offset
            //      is `prior_user_ref + prior_doppler`; reseeding
            //      with that would copy the previous pass's
            //      Doppler into the new pass's baseline (a
            //      double-count). Per CR round 4 on PR #554.
            let prior_user_ref = t.user_reference_offset_hz();
            let prior_active_some = t.active().is_some();
            let changed = t.set_active(new_active);
            if changed {
                if new_active.is_some() {
                    if prior_active_some {
                        // Some(A) → Some(B) satellite swap.
                        // Restore the pre-swap user_reference
                        // (which `set_active` just reset to 0)
                        // so it survives the satellite change.
                        // Per CR round 5 on PR #554.
                        t.set_user_reference_offset_hz(prior_user_ref);
                    } else {
                        // None → Some fresh engagement. Seed
                        // `user_reference_offset_hz` from the
                        // synchronously-tracked DSP baseline on
                        // `AppState` so this pass's Doppler tracks
                        // ON TOP of any offset the user had set
                        // before AOS — and so disengage at LOS
                        // restores that exact value via the
                        // Some → None flush path.
                        //
                        // Round 6 deferred this seed because the
                        // only available source was `spectrum.vfo_offset_hz()`,
                        // which lags DSP echoes — auto-record's
                        // AOS-side `SetVfoOffset(0.0)` would not yet
                        // be reflected when the trigger tick fired,
                        // so we'd capture the stale pre-AOS value.
                        // Round 7 added `state.last_dispatched_vfo_offset_hz`,
                        // which the `connect_vfo_offset_changed`
                        // callback updates on every DSP echo (and
                        // every direct user-drag dispatch). That
                        // gives us the synchronously-tracked source
                        // of truth the deferral was waiting for.
                        // Per CR round 9 on PR #554.
                        let baseline = state.last_dispatched_vfo_offset_hz.get();
                        t.set_user_reference_offset_hz(baseline);
                    }
                    // No dispatch here — the next 4 Hz tick will
                    // dispatch `live = user_reference + doppler`.
                } else {
                    // Disengaged — flush the live offset back to
                    // the pre-engage user reference (captured
                    // before `set_active` reset it) and clear
                    // the status badge.
                    //
                    // We don't need to explicitly clear the
                    // tracker's `user_reference_offset_hz` here
                    // — `set_active(None)` already did it on
                    // line 216 of `doppler_tracker.rs` (the
                    // `if changed { self.user_reference_offset_hz = 0.0; }`
                    // branch), and the
                    // `satellite_to_none_resets_user_reference_offset`
                    // unit test pins that invariant. The
                    // `prior_user_ref` we dispatch is the value
                    // captured pre-`set_active`, so DSP gets the
                    // user's pre-engage baseline; the tracker's
                    // own field is already 0 for the next
                    // engagement. Per CR round 8 on PR #554.
                    drop(t);
                    state.dispatch_vfo_offset(prior_user_ref);
                    status_bar.update_doppler(None);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // 4 Hz offset-recompute tick: while a satellite is active,
    // recompute the Doppler shift and dispatch a SetVfoOffset
    // (rate-limited to changes >DOPPLER_DISPATCH_THRESHOLD_HZ
    // to avoid spamming the bus). Update the status-bar label
    // every tick — the kHz/0.1 rounded format already hides
    // sub-100-Hz wobble, no further suppression needed.
    {
        let tracker = Rc::clone(&tracker);
        let cache = std::sync::Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let panel_weak = panels.satellites.downgrade();
        let _ = glib::timeout_add_local(DOPPLER_RECOMPUTE_TICK, move || {
            let Some(panel) = panel_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // Lifecycle gate: master + running. The status-bar
            // badge clears on the first not-running tick so the
            // user gets immediate "Doppler is idle" feedback when
            // they press Stop — `update_doppler(None)` is
            // idempotent (set_visible(false) on an already-hidden
            // label is a no-op). Per #567.
            if !should_tick(tracker.borrow().master_enabled(), state.is_running.get()) {
                status_bar.update_doppler(None);
                return glib::ControlFlow::Continue;
            }
            let active_sat = tracker.borrow().active();
            let Some(sat) = active_sat else {
                return glib::ControlFlow::Continue;
            };
            // Has the user retuned away from the active satellite?
            // If so, disengage NOW rather than wait up to 1 s for
            // the trigger tick — otherwise stale Doppler keeps
            // dispatching against the new center frequency for up
            // to a full second. Per CR round 5 on PR #554.
            #[allow(
                clippy::cast_precision_loss,
                reason = "catalog downlinks sit in the 100s of MHz, well \
                          below f64's 2^53 mantissa ceiling"
            )]
            let downlink = sat.downlink_hz as f64;
            let current_freq = state.center_frequency.get();
            if (downlink - current_freq).abs() > FREQ_MATCH_TOLERANCE_HZ {
                let mut t = tracker.borrow_mut();
                let prior_user_ref = t.user_reference_offset_hz();
                let _ = t.set_active(None);
                drop(t);
                state.dispatch_vfo_offset(prior_user_ref);
                status_bar.update_doppler(None);
                return glib::ControlFlow::Continue;
            }
            let station = GroundStation::new(
                panel.lat_row.value(),
                panel.lon_row.value(),
                panel.alt_row.value(),
            );
            let Ok((line1, line2)) = cache.cached_tle_for(sat.norad_id) else {
                // TLE evicted between trigger evaluation and
                // recompute — dormant for this tick; the next
                // 1 Hz trigger tick will drop the active sat
                // since `cached_tle_for` will fail there too.
                return glib::ControlFlow::Continue;
            };
            let Ok(parsed) = Satellite::from_tle(sat.name, &line1, &line2) else {
                return glib::ControlFlow::Continue;
            };
            let now = chrono::Utc::now();
            #[allow(
                clippy::cast_precision_loss,
                reason = "catalog downlinks sit in the 100s of MHz, well \
                          below f64's 2^53 mantissa ceiling"
            )]
            let carrier = sat.downlink_hz as f64;
            let Ok(doppler) = compute_doppler_offset_hz(&parsed, &station, now, carrier) else {
                tracing::debug!(
                    satellite = sat.name,
                    "Doppler recompute: SGP4 propagate failed; skipping tick"
                );
                return glib::ControlFlow::Continue;
            };
            let live = tracker.borrow().live_offset_hz(doppler);
            // Status bar updates every tick — the kHz/0.1
            // format hides sub-100-Hz jitter naturally.
            status_bar.update_doppler(Some(doppler));
            // SetVfoOffset is rate-limited to material changes.
            // Baseline lives on `AppState` and is kept in sync by
            // the `connect_vfo_offset_changed` callback (fires on
            // both DSP echo and direct user-drag dispatches). Per
            // CR round 7 on PR #554. We also write it eagerly at
            // dispatch so a fast back-to-back tick before the
            // echo round-trip doesn't over-dispatch — the echo
            // arrives later with the same value, harmless.
            let baseline = state.last_dispatched_vfo_offset_hz.get();
            if (live - baseline).abs() > DOPPLER_DISPATCH_THRESHOLD_HZ {
                state.dispatch_vfo_offset(live);
            }
            glib::ControlFlow::Continue
        });
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
                    // Drop any channel-marker buffered while
                    // transcription was off — the first text
                    // event after re-enable should attribute to
                    // the *next* hop, not whichever channel the
                    // scanner happened to land on during the
                    // off period. Per CodeRabbit round 1 on PR
                    // #558.
                    *state_clone.pending_channel_marker.borrow_mut() = None;

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
                    // State handle for the lazy channel-marker
                    // emission (#517) — the closure consumes
                    // `state_clone.pending_channel_marker` from
                    // the `TranscriptionEvent::Text` arm below.
                    let state_for_marker = Rc::clone(&state_clone);

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
                                        // Drain the pending channel-marker
                                        // (#517) BEFORE inserting the
                                        // transcribed text — the marker
                                        // belongs ABOVE the text it
                                        // precedes. Lazy emission means
                                        // markers only land when there's
                                        // actual audio to attribute, so
                                        // a quiet channel never produces
                                        // a divider.
                                        if let Some((switched_at, channel_name)) =
                                            state_for_marker
                                                .pending_channel_marker
                                                .borrow_mut()
                                                .take()
                                        {
                                            sidebar::transcript_panel::push_channel_marker(
                                                &tv,
                                                switched_at,
                                                &channel_name,
                                            );
                                        }
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
            // Drop any pending channel-marker so a stray scanner
            // hop that landed during the live session doesn't
            // poison the next enable's first text event. Per
            // CodeRabbit round 1 on PR #558.
            *state_clone.pending_channel_marker.borrow_mut() = None;
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
) {
    tracing::info!("tray-quit: shutting down");
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
