//! `DspToUi` event dispatch — the poll-loop message handler and its
//! toast helpers.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{
    AppState, DspToUi, Rc, RefCell, StatusBar, UiToDsp, adw, apply_rtl_tcp_connection_state,
    clear_scanner_active_channel_ui, glib, handle_rtl_tcp_state_toast, header, plain_toast,
    sidebar, spectrum, try_collapse_into_existing, update_bandwidth_reset_sensitivity,
    update_bandwidth_row_range_for_mode, update_vfo_reset_button_visibility,
};

/// Handle a single message from the DSP thread.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
        toast_overlay_weak,
        status_bar,
        rtl_tcp_status_row_weak,
        rtl_tcp_disconnect_button_weak,
        rtl_tcp_retry_button_weak,
        rtl_tcp_role_row_weak,
        rtl_tcp_auth_key_row_weak,
        rtl_tcp_hostname_row_weak,
        rtl_tcp_port_row_weak,
        pending_controller_busy_toasts,
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
        handle_rtl_tcp_state_toast(
            conn_state,
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

/// `DspToUi::ScannerActiveChannelChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_scanner_active_channel_changed(ctx: &DspEventCtx, msg: DspToUi) {
    let DspToUi::ScannerActiveChannelChanged {
        key,
        freq_hz,
        demod_mode,
        bandwidth,
        name,
        ctcss,
        voice_squelch,
    } = msg
    else {
        return;
    };
    let DspEventCtx {
        spectrum_handle,
        state,
        scanner_panel,
        ..
    } = ctx;
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

        sync_scanner_channel_widgets(ctx, freq_hz, demod_mode, bandwidth, &name);
        sync_scanner_channel_gates(ctx, ctcss, voice_squelch);
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

/// Widget half of [`on_scanner_active_channel_changed`]: mirror the
/// scanner's channel into the header selector, spectrum, status bar,
/// demod dropdown, and bandwidth row (notify handlers suppressed so
/// the retune doesn't ricochet back into commands). Split out per
/// the 50-NLOC gate (#817).
fn sync_scanner_channel_widgets(
    ctx: &DspEventCtx,
    freq_hz: u64,
    demod_mode: sdr_types::DemodMode,
    bandwidth: f64,
    name: &str,
) {
    let DspEventCtx {
        spectrum_handle,
        state,
        status_bar,
        radio_panel,
        scanner_panel,
        freq_selector,
        demod_dropdown,
        ..
    } = ctx;
    #[allow(clippy::cast_precision_loss)]
    let freq_f64 = freq_hz as f64;
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
}

/// CTCSS / voice-squelch half of [`on_scanner_active_channel_changed`]
/// — keeps Add/Save Bookmark honest for the channel the scanner landed
/// on. Split out per the 50-NLOC gate (#817).
fn sync_scanner_channel_gates(
    ctx: &DspEventCtx,
    ctcss: Option<sdr_radio::af_chain::CtcssMode>,
    voice_squelch: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
) {
    let DspEventCtx { radio_panel, .. } = ctx;
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
    let ctcss_idx = sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(ctcss_for_widget);
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
}

/// `DspToUi::ScannerStateChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_scanner_state_changed(ctx: &DspEventCtx, scanner_state: sdr_scanner::ScannerState) {
    let DspEventCtx { scanner_panel, .. } = ctx;
    let label = match scanner_state {
        sdr_scanner::ScannerState::Idle => "Off",
        sdr_scanner::ScannerState::Retuning => "Scanning…",
        sdr_scanner::ScannerState::Dwelling => "Dwelling…",
        sdr_scanner::ScannerState::Listening => "Listening",
        sdr_scanner::ScannerState::Hanging => "Hang…",
    };
    scanner_panel.state_row.set_subtitle(label);
}

/// `DspToUi::ScannerEmptyRotation` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_scanner_empty_rotation(ctx: &DspEventCtx) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        scanner_panel,
        ..
    } = ctx;
    tracing::info!("scanner rotation empty");
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        overlay.add_toast(plain_toast(
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

/// `DspToUi::ScannerMutexStopped` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_scanner_mutex_stopped(ctx: &DspEventCtx, reason: sdr_core::messages::ScannerMutexReason) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        scanner_panel,
        ..
    } = ctx;
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
        overlay.add_toast(plain_toast(message));
    }
}

/// `DspToUi::AcarsMessage` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_acars_message(ctx: &DspEventCtx, msg: &sdr_acars::AcarsMessage) {
    let DspEventCtx { state, .. } = ctx;
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

        append_acars_viewer_row(handles, msg, &adj, was_at_top);
        update_acars_aircraft_index(handles, msg);
    }

    tracing::trace!(
        "ACARS msg {} ({}, label {:?})",
        state.acars_total_count.get(),
        msg.aircraft.as_str(),
        msg.label
    );
}

/// `DspToUi::AcarsEnabledChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_acars_enabled_changed(
    ctx: &DspEventCtx,
    result: Result<bool, sdr_core::acars_airband_lock::AcarsEnableError>,
) {
    match result {
        Ok(true) => on_acars_engaged(ctx),
        Ok(false) => on_acars_disengaged(ctx),
        Err(err) => on_acars_enable_error(ctx, &err),
    }
}

/// Engage ack (`Ok(true)`) of [`on_acars_enabled_changed`]: mirror the
/// DSP's silent retune to airband center and lock the geometry rows.
fn on_acars_engaged(ctx: &DspEventCtx) {
    let DspEventCtx {
        spectrum_handle,
        state,
        status_bar,
        freq_selector,
        demod_dropdown,
        sample_rate_row,
        decimation_row,
        volume_button,
        ..
    } = ctx;
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

/// Disengage ack (`Ok(false)`) of [`on_acars_enabled_changed`]: restore
/// the pre-engage tune + row sensitivities.
fn on_acars_disengaged(ctx: &DspEventCtx) {
    let DspEventCtx {
        spectrum_handle,
        state,
        status_bar,
        freq_selector,
        demod_dropdown,
        sample_rate_row,
        decimation_row,
        ..
    } = ctx;
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
    restore_acars_saved_volume(ctx);
    drain_deferred_aos_actions(ctx);
    tracing::info!("ACARS disengaged");
}

/// Auto-restore the pre-engage volume on ACARS disengage (issue
/// #588) — skipped when the user manually moved the slider during
/// the session. Split out of [`on_acars_disengaged`] per the 50-NLOC
/// gate (#817).
fn restore_acars_saved_volume(ctx: &DspEventCtx) {
    let DspEventCtx {
        state,
        volume_button,
        ..
    } = ctx;
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
            tracing::debug!(current, "ACARS disengage: keeping user-overridden volume");
        }
    }
}

/// Replay a deferred AOS batch after the disengage ack (issue #589).
/// Split out of [`on_acars_disengaged`] per the 50-NLOC gate (#817).
fn drain_deferred_aos_actions(ctx: &DspEventCtx) {
    let DspEventCtx { state, .. } = ctx;
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
        && let Some(interp_weak) = state.recorder_action_interpreter.borrow().clone()
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
}

/// Engage/disengage failure of [`on_acars_enabled_changed`].
fn on_acars_enable_error(ctx: &DspEventCtx, err: &sdr_core::acars_airband_lock::AcarsEnableError) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        ..
    } = ctx;
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
            crate::sidebar::satellites_recorder::Action::StartAutoRecord { satellite, .. } => {
                Some(satellite.clone())
            }
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
                overlay.add_toast(plain_toast(&format!(
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
        overlay.add_toast(plain_toast(&format!("ACARS: {err}")));
    }
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

/// Row-append half of the ACARS viewer mirror: collapse-into-existing
/// when enabled, cap-bounded append, and the snap-to-top restore.
/// Split out of [`on_acars_message`] per the 50-NLOC gate (#817).
fn append_acars_viewer_row(
    handles: &crate::acars_viewer::ViewerHandles,
    msg: &sdr_acars::AcarsMessage,
    adj: &gtk4::Adjustment,
    was_at_top: bool,
) {
    let collapse_active = handles.collapse_button.is_active();
    let mut collapsed_into: Option<u32> = None;
    if collapse_active {
        collapsed_into = try_collapse_into_existing(&handles.store, msg);
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
            .append(&crate::acars_viewer::AcarsMessageObject::new(msg.clone()));
    }

    // Auto-scroll-to-top: snap back if the user was at
    // the top before the append. Direct adjustment
    // manipulation rather than `ColumnView::scroll_to`:
    // that API is gated behind gtk4 `v4_12` and the
    // workspace pins `v4_10`.
    if was_at_top {
        adj.set_value(adj.lower());
    }
}

/// Aircraft-index half of the ACARS viewer mirror (issue #579).
/// Split out of [`on_acars_message`] per the 50-NLOC gate (#817).
fn update_acars_aircraft_index(
    handles: &crate::acars_viewer::ViewerHandles,
    msg: &sdr_acars::AcarsMessage,
) {
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
            obj.record_message(msg);
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

/// `DspToUi::AcarsChannelStats` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_acars_channel_stats(ctx: &DspEventCtx, ch_stats: Box<[sdr_acars::ChannelStats]>) {
    let DspEventCtx { state, .. } = ctx;
    *state.acars_channel_stats.borrow_mut() = ch_stats.into_vec();
}

/// `DspToUi::AcarsOutputError` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
fn on_acars_output_error(ctx: &DspEventCtx, kind: &'static str, message: &str) {
    let DspEventCtx {
        toast_overlay_weak, ..
    } = ctx;
    tracing::warn!(kind, message, "ACARS output error");
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        overlay.add_toast(plain_toast(&format!(
            "ACARS {kind} output error: {message}"
        )));
    }
}
