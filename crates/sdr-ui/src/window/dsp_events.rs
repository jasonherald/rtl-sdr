//! `DspToUi` event dispatch — the poll-loop message handler and its
//! toast helpers.

use gtk4::prelude::*;
use libadwaita::prelude::*;

mod acars_events;
mod scanner_events;
use acars_events::{
    on_acars_channel_stats, on_acars_enabled_changed, on_acars_message, on_acars_output_error,
};
use scanner_events::{
    on_scanner_active_channel_changed, on_scanner_empty_rotation, on_scanner_mutex_stopped,
    on_scanner_state_changed,
};

use super::{
    AppState, DspToUi, Rc, RefCell, StatusBar, adw, apply_rtl_tcp_connection_state, glib,
    handle_rtl_tcp_state_toast, header, plain_toast, sidebar, spectrum,
    update_bandwidth_reset_sensitivity, update_bandwidth_row_range_for_mode,
    update_vfo_reset_button_visibility,
};

/// Widget/state references the `DspToUi` poll loop hands to
/// [`handle_dsp_message`]. Built once in `build_window` and moved
/// into the poll closure — same clones, same lifecycles as the
/// former 30-parameter signature, minus the signature.
pub(super) struct DspEventCtx {
    pub(super) spectrum_handle: Rc<spectrum::SpectrumHandle>,
    pub(super) play_button_weak: glib::WeakRef<gtk4::ToggleButton>,
    pub(super) state: Rc<AppState>,
    pub(super) toast_overlay_weak: glib::WeakRef<adw::ToastOverlay>,
    pub(super) status_bar: Rc<StatusBar>,
    pub(super) gain_row: adw::SpinRow,
    pub(super) record_audio_row: adw::SwitchRow,
    pub(super) record_iq_row: adw::SwitchRow,
    pub(super) radio_panel: sidebar::radio_panel::RadioPanel,
    pub(super) scanner_panel: sidebar::scanner_panel::ScannerPanel,
    pub(super) freq_selector: header::frequency_selector::FrequencySelector,
    pub(super) demod_dropdown: gtk4::DropDown,
    pub(super) sample_rate_row: adw::ComboRow,
    pub(super) decimation_row: adw::ComboRow,
    pub(super) volume_button: gtk4::ScaleButton,
    pub(super) rtl_tcp_status_row_weak: glib::WeakRef<adw::ActionRow>,
    pub(super) rtl_tcp_disconnect_button_weak: glib::WeakRef<gtk4::Button>,
    pub(super) rtl_tcp_retry_button_weak: glib::WeakRef<gtk4::Button>,
    pub(super) rtl_tcp_role_row_weak: glib::WeakRef<adw::ComboRow>,
    pub(super) rtl_tcp_auth_key_row_weak: glib::WeakRef<adw::PasswordEntryRow>,
    pub(super) rtl_tcp_hostname_row_weak: glib::WeakRef<adw::EntryRow>,
    pub(super) rtl_tcp_port_row_weak: glib::WeakRef<adw::SpinRow>,
    pub(super) pending_controller_busy_toasts: Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
    pub(super) network_sink_status_row_weak: glib::WeakRef<adw::ActionRow>,
    pub(super) transcription_enable_row: adw::SwitchRow,
    #[cfg(feature = "sherpa")]
    pub(super) auto_break_row: adw::SwitchRow,
    #[cfg(feature = "sherpa")]
    pub(super) auto_break_min_open_row: adw::SpinRow,
    #[cfg(feature = "sherpa")]
    pub(super) auto_break_tail_row: adw::SpinRow,
    #[cfg(feature = "sherpa")]
    pub(super) auto_break_min_segment_row: adw::SpinRow,
    #[cfg(feature = "sherpa")]
    pub(super) model_row: adw::ComboRow,
}

/// Handle a single message from the DSP thread — a one-line-arm
/// dispatch into the `on_*` handlers below.
pub(super) fn handle_dsp_message(msg: DspToUi, ctx: &DspEventCtx) {
    match msg {
        DspToUi::FftData(_) => {
            // FFT data now comes via SharedFftBuffer, not the channel.
            // This variant is kept for backward compatibility but shouldn't
            // be sent in normal operation.
        }
        DspToUi::SignalLevel(level) => on_signal_level(ctx, level),
        DspToUi::Error(err_msg) => on_error(ctx, &err_msg),
        DspToUi::SourceStopped => on_source_stopped(ctx),
        DspToUi::SampleRateChanged(rate) => on_sample_rate_changed(ctx, rate),
        DspToUi::DisplayBandwidth(raw_rate) => on_display_bandwidth(ctx, raw_rate),
        DspToUi::DeviceInfo(info) => on_device_info(ctx, &info),
        DspToUi::GainList(gains) => on_gain_list(ctx, &gains),
        DspToUi::AudioRecordingStarted(path) => on_audio_recording_started(ctx, &path),
        DspToUi::AudioRecordingStopped => on_audio_recording_stopped(ctx),
        DspToUi::IqRecordingStarted(path) => on_iq_recording_started(ctx, &path),
        DspToUi::IqRecordingStopped => on_iq_recording_stopped(ctx),
        DspToUi::DemodModeChanged(new_mode) => on_demod_mode_changed(ctx, new_mode),
        DspToUi::BandwidthChanged(bw) => on_bandwidth_changed(ctx, bw),
        DspToUi::VfoOffsetChanged(offset) => on_vfo_offset_changed(ctx, offset),
        DspToUi::CtcssSustainedChanged(sustained) => on_ctcss_sustained_changed(ctx, sustained),
        DspToUi::VoiceSquelchOpenChanged(open) => on_voice_squelch_open_changed(ctx, open),
        DspToUi::RtlTcpConnectionState(conn_state) => on_rtl_tcp_connection_state(ctx, &conn_state),
        DspToUi::NetworkSinkStatus(status) => on_network_sink_status(ctx, &status),
        // --- Scanner (#317) ---
        msg @ DspToUi::ScannerActiveChannelChanged { .. } => {
            on_scanner_active_channel_changed(ctx, msg);
        }
        DspToUi::ScannerStateChanged(scanner_state) => on_scanner_state_changed(ctx, scanner_state),
        DspToUi::ScannerEmptyRotation => on_scanner_empty_rotation(ctx),
        DspToUi::AptLine(line) => on_apt_line(ctx, &line),
        DspToUi::SstvVisDetected { mode_label } => {
            on_sstv_vis_detected(ctx, mode_label);
        }
        DspToUi::SstvLineDecoded(line_index) => on_sstv_line_decoded(ctx, line_index),
        DspToUi::SstvImageComplete {
            width,
            height,
            pixels,
        } => {
            on_sstv_image_complete(ctx, width, height, pixels);
        }
        DspToUi::ScannerMutexStopped(reason) => on_scanner_mutex_stopped(ctx, reason),
        DspToUi::AcarsMessage(msg) => on_acars_message(ctx, &msg),
        DspToUi::AcarsChannelStats(ch_stats) => on_acars_channel_stats(ctx, ch_stats),
        DspToUi::AcarsEnabledChanged(result) => on_acars_enabled_changed(ctx, result),
        // Output-writer errors (issue #578). Handler wired in Task 8;
        // stub here keeps the match exhaustive. Surfaces the kind-scoped
        // message as a toast so the user sees misconfigured paths / DNS
        // failures without having to consult the log.
        DspToUi::AcarsOutputError { kind, message } => {
            on_acars_output_error(ctx, kind, &message);
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
pub(super) fn apply_network_sink_status(
    row: &adw::ActionRow,
    status: &sdr_core::NetworkSinkStatus,
) {
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

/// `adw::Toast::set_timeout(0)` keeps a toast on screen until
/// the user dismisses it or an explicit `dismiss()` fires. Used
/// for the two `ControllerBusy` action toasts — the stakes are
/// high enough (the user has to actively choose between Take-
/// control, Listener, or abandoning the connect) that a
/// time-limited toast would feel like silent retry behavior.
/// Per `CodeRabbit` round 12 on PR #408.
pub(super) const TOAST_TIMEOUT_PERSISTENT: u32 = 0;

/// Short toast timeout in seconds for transient-acknowledgement
/// notices — the `AuthRequired` / `AuthFailed` copy that
/// complements a revealed key-entry row. Long enough to read, short
/// enough to clear without user interaction once the user has
/// moved on to typing. Per `CodeRabbit` round 12 on PR #408.
pub(super) const TOAST_TIMEOUT_SHORT_SECS: u32 = 5;

/// `DspToUi::SourceStopped` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_source_stopped(ctx: &DspEventCtx) {
    let DspEventCtx {
        play_button_weak,
        state,
        record_audio_row,
        record_iq_row,
        transcription_enable_row,
        ..
    } = ctx;
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

/// `DspToUi::GainList` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_gain_list(ctx: &DspEventCtx, gains: &[f64]) {
    let DspEventCtx { gain_row, .. } = ctx;
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

/// `DspToUi::AudioRecordingStarted` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_audio_recording_started(ctx: &DspEventCtx, path: &std::path::Path) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        ..
    } = ctx;
    tracing::info!(?path, "audio recording started");
    // Mirror into AppState so is_recording() (used by the
    // close-to-tray Quit confirmation modal) reflects reality.
    // Per #512.
    state.audio_recording_active.set(true);
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let name = path
            .file_name()
            .map_or("file".to_string(), |n| n.to_string_lossy().to_string());
        let toast = plain_toast(&format!("Recording audio: {name}"));
        overlay.add_toast(toast);
    }
}

/// `DspToUi::AudioRecordingStopped` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_audio_recording_stopped(ctx: &DspEventCtx) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        record_audio_row,
        ..
    } = ctx;
    tracing::info!("audio recording stopped");
    // Mirror into AppState. Per #512.
    state.audio_recording_active.set(false);
    record_audio_row.set_active(false);
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let toast = plain_toast("Audio recording saved");
        overlay.add_toast(toast);
    }
}

/// `DspToUi::IqRecordingStarted` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_iq_recording_started(ctx: &DspEventCtx, path: &std::path::Path) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        ..
    } = ctx;
    tracing::info!(?path, "IQ recording started");
    // Mirror into AppState so is_recording() (used by the
    // close-to-tray Quit confirmation modal) reflects reality.
    // Per #512.
    state.iq_recording_active.set(true);
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let name = path
            .file_name()
            .map_or("file".to_string(), |n| n.to_string_lossy().to_string());
        let toast = plain_toast(&format!("Recording IQ: {name}"));
        overlay.add_toast(toast);
    }
}

/// `DspToUi::IqRecordingStopped` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_iq_recording_stopped(ctx: &DspEventCtx) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        record_iq_row,
        ..
    } = ctx;
    tracing::info!("IQ recording stopped");
    // Mirror into AppState. Per #512.
    state.iq_recording_active.set(false);
    record_iq_row.set_active(false);
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let toast = plain_toast("IQ recording saved");
        overlay.add_toast(toast);
    }
}

/// `DspToUi::DemodModeChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_demod_mode_changed(ctx: &DspEventCtx, new_mode: sdr_types::DemodMode) {
    let DspEventCtx {
        spectrum_handle,
        state,
        toast_overlay_weak,
        radio_panel,
        transcription_enable_row,
        ..
    } = ctx;
    #[cfg(feature = "sherpa")]
    let DspEventCtx {
        auto_break_row,
        auto_break_min_open_row,
        auto_break_tail_row,
        auto_break_min_segment_row,
        model_row,
        ..
    } = ctx;
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
            let toast =
                plain_toast("Transcription stopped — demod mode changed. Press Start to resume.");
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

/// `DspToUi::BandwidthChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_bandwidth_changed(ctx: &DspEventCtx, bw: f64) {
    let DspEventCtx {
        spectrum_handle,
        state,
        radio_panel,
        ..
    } = ctx;
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

/// `DspToUi::VfoOffsetChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_vfo_offset_changed(ctx: &DspEventCtx, offset: f64) {
    let DspEventCtx {
        spectrum_handle,
        state,
        status_bar,
        radio_panel,
        freq_selector,
        ..
    } = ctx;
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

/// `DspToUi::RtlTcpConnectionState` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_rtl_tcp_connection_state(ctx: &DspEventCtx, conn_state: &sdr_types::RtlTcpConnectionState) {
    let DspEventCtx {
        state,
        status_bar,
        rtl_tcp_status_row_weak,
        rtl_tcp_disconnect_button_weak,
        rtl_tcp_retry_button_weak,
        ..
    } = ctx;
    tracing::debug!(?conn_state, "rtl_tcp connection state");
    // Upgrade all three weak refs atomically; any missing
    // widget means the window's gone, so we drop the event
    // rather than render a ghost status row.
    if let (Some(status_row), Some(disconnect), Some(retry)) = (
        rtl_tcp_status_row_weak.upgrade(),
        rtl_tcp_disconnect_button_weak.upgrade(),
        rtl_tcp_retry_button_weak.upgrade(),
    ) {
        apply_rtl_tcp_connection_state(&status_row, &disconnect, &retry, conn_state);
    }
    // #396 toast surface: fire toast + manipulate widgets
    // on the EDGE of every transition into a role-denial
    // terminal state (or into Connected from one of those
    // states, for the keyring save path). Edge detection
    // uses a u8-discriminant cell on AppState so we don't
    // re-fire the toast on every same-state republish.
    let prev_disc = state.last_rtl_tcp_state_disc.get();
    let now_disc = crate::state::rtl_tcp_state_discriminant(conn_state);
    if prev_disc != now_disc {
        state.last_rtl_tcp_state_disc.set(now_disc);
        handle_rtl_tcp_state_toast(conn_state, prev_disc, ctx);
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
    status_bar.update_role(rtl_tcp_role_badge(conn_state));
}

/// Map the server-granted role to the status-bar badge: `Some(true)`
/// → Controller, `Some(false)` → Listener, `None`/non-Connected →
/// hidden. Split out of [`on_rtl_tcp_connection_state`] per the
/// 50-NLOC gate (#817).
fn rtl_tcp_role_badge(
    conn_state: &sdr_types::RtlTcpConnectionState,
) -> Option<crate::status_bar::RtlTcpRoleBadge> {
    match conn_state {
        sdr_types::RtlTcpConnectionState::Connected {
            granted_role: Some(true),
            ..
        } => Some(crate::status_bar::RtlTcpRoleBadge::Controller),
        sdr_types::RtlTcpConnectionState::Connected {
            granted_role: Some(false),
            ..
        } => Some(crate::status_bar::RtlTcpRoleBadge::Listener),
        _ => None,
    }
}

/// `DspToUi::AptLine` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_apt_line(ctx: &DspEventCtx, line: &sdr_core::messages::AptLine) {
    let DspEventCtx { state, .. } = ctx;
    // Route the freshly-decoded APT line into the open
    // viewer, if any. When no viewer is open we silently
    // drop — the decoder always runs (it's cheap) so the
    // user can open the viewer mid-pass and start seeing
    // lines from that moment on, rather than having to
    // pre-arm before AOS.
    if let Some(view) = state.apt_viewer.borrow().as_ref() {
        view.push_line(line);
    }
}

/// `DspToUi::SstvVisDetected` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_sstv_vis_detected(ctx: &DspEventCtx, mode_label: &'static str) {
    let DspEventCtx { state, .. } = ctx;
    // The decoder identified an SSTV mode from a fresh VIS
    // header. Surface it in the viewer's title so the user
    // can see whether they're getting PD120 / PD180 / PD240
    // (or a future slowrx mode) without having to read
    // `tracing::info!` in the journal. No-op when the
    // viewer isn't open. Per epic #472 mode-display
    // follow-up.
    if let Some(view) = state.sstv_viewer.borrow().as_ref() {
        view.set_mode_label(mode_label);
    }
}

/// `DspToUi::SstvLineDecoded` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_sstv_line_decoded(ctx: &DspEventCtx, _line_index: u32) {
    let DspEventCtx { state, .. } = ctx;
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

/// `DspToUi::SstvImageComplete` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_sstv_image_complete(ctx: &DspEventCtx, width: u32, height: u32, pixels: Vec<[u8; 3]>) {
    let DspEventCtx { state, .. } = ctx;
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

/// `DspToUi::SignalLevel` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_signal_level(ctx: &DspEventCtx, level: f32) {
    let DspEventCtx {
        spectrum_handle,
        state,
        status_bar,
        radio_panel,
        ..
    } = ctx;
    status_bar.update_signal_level(level);
    spectrum_handle.push_signal_level(level);
    // Feed the FSPL distance estimator in the Radio panel
    // (ticket #164). The panel caches the level + current
    // frequency so the display refreshes if the user later
    // tweaks ERP / calibration.
    radio_panel.update_distance_from_signal(level, state.center_frequency.get());
}

/// `DspToUi::Error` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_error(ctx: &DspEventCtx, err_msg: &str) {
    let DspEventCtx {
        toast_overlay_weak, ..
    } = ctx;
    tracing::warn!(error = %err_msg, "DSP error");
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let toast = plain_toast(err_msg);
        overlay.add_toast(toast);
    }
}

/// `DspToUi::SampleRateChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_sample_rate_changed(ctx: &DspEventCtx, rate: f64) {
    let DspEventCtx { status_bar, .. } = ctx;
    tracing::info!(effective_sample_rate = rate, "sample rate changed");
    status_bar.update_sample_rate(rate);
}

/// `DspToUi::DisplayBandwidth` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_display_bandwidth(ctx: &DspEventCtx, raw_rate: f64) {
    let DspEventCtx {
        spectrum_handle, ..
    } = ctx;
    tracing::info!(raw_sample_rate = raw_rate, "display bandwidth updated");
    spectrum_handle.set_display_bandwidth(raw_rate);
}

/// `DspToUi::DeviceInfo` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_device_info(_ctx: &DspEventCtx, info: &str) {
    tracing::info!(device_info = %info, "device info received");
}

/// `DspToUi::CtcssSustainedChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_ctcss_sustained_changed(ctx: &DspEventCtx, sustained: bool) {
    let DspEventCtx { radio_panel, .. } = ctx;
    tracing::debug!(sustained, "CTCSS sustained-gate edge");
    radio_panel.set_ctcss_sustained(sustained);
}

/// `DspToUi::VoiceSquelchOpenChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_voice_squelch_open_changed(ctx: &DspEventCtx, open: bool) {
    let DspEventCtx { radio_panel, .. } = ctx;
    tracing::debug!(open, "voice squelch gate edge");
    radio_panel.set_voice_squelch_open(open);
}

/// `DspToUi::NetworkSinkStatus` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_network_sink_status(ctx: &DspEventCtx, status: &sdr_core::sink_slot::NetworkSinkStatus) {
    let DspEventCtx {
        network_sink_status_row_weak,
        ..
    } = ctx;
    tracing::debug!(?status, "network sink status");
    if let Some(row) = network_sink_status_row_weak.upgrade() {
        apply_network_sink_status(&row, status);
    }
}
