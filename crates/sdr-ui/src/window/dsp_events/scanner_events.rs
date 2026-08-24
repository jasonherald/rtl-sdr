//! Scanner-side `DspToUi` handlers: active-channel mirror (widgets +
//! gate rows), scanner state, empty-rotation, and the mutex-stop
//! toast. Split out of `window/dsp_events.rs` per the Codacy
//! 500-NLOC file gate on PR #844.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{
    DspToUi, clear_scanner_active_channel_ui, header, plain_toast, sidebar,
    update_bandwidth_row_range_for_mode,
};
use super::DspEventCtx;

/// `DspToUi::ScannerActiveChannelChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
pub(super) fn on_scanner_active_channel_changed(ctx: &DspEventCtx, msg: DspToUi) {
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
pub(super) fn on_scanner_state_changed(
    ctx: &DspEventCtx,
    scanner_state: sdr_scanner::ScannerState,
) {
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
pub(super) fn on_scanner_empty_rotation(ctx: &DspEventCtx) {
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

/// `DspToUi::ScannerMutexStopped` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
pub(super) fn on_scanner_mutex_stopped(
    ctx: &DspEventCtx,
    reason: sdr_core::messages::ScannerMutexReason,
) {
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
