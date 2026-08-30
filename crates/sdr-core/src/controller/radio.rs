//! Demodulator-facing command handlers (mode, bandwidth).

use super::{
    DspState, DspToUi, acars_lock_rejects_geometry_change, auto_decimation_ratio, mpsc,
    on_tune_change, orbcomm_lock_rejects_geometry_change, rebuild_vfo_echoing,
};
use sdr_types::DemodMode;

/// Handler for `UiToDsp::SetDemodMode`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_demod_mode(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    mode: DemodMode,
) {
    if acars_lock_rejects_geometry_change(state, dsp_tx, "SetDemodMode") {
        return;
    }
    // Mode switches auto-adjust frontend decimation for the new IF
    // rate below — that would silently walk it away from Orbcomm's
    // forced 1 while engaged. Issue #865, CR round 4.
    if orbcomm_lock_rejects_geometry_change(state, dsp_tx, "SetDemodMode") {
        return;
    }
    // INFO-level so silent-fail demod regressions can be diagnosed
    // by grepping the log alone. Pairs with `tune_to_target`'s
    // TUNE_REQUEST line on the UI side.
    tracing::info!(target: "set_demod_mode", ?mode, "DSP_APPLY_REQUEST");
    on_tune_change(state);
    let old_mode = state.radio.current_mode();
    if let Err(e) = state.radio.set_mode(mode) {
        tracing::warn!("set demod mode failed: {e}");
        let _ = dsp_tx.send(DspToUi::Error(format!("Mode switch failed: {e}")));
    } else {
        // Reset bandwidth to the new mode's default.
        state.bandwidth = state.radio.demod_config().default_bandwidth;

        // Auto-adjust decimation for the new demod's IF rate.
        let if_rate = state.radio.demod_config().if_sample_rate;
        let auto_decim = auto_decimation_ratio(state.sample_rate, if_rate);
        if auto_decim != state.frontend.decim_ratio() {
            tracing::info!(auto_decim, if_rate, "auto-adjusting decimation for mode");
            if let Err(e) = state.frontend.set_decimation(auto_decim) {
                tracing::warn!("auto-decimation on mode switch failed: {e}");
            }
        }

        // Rebuild the RxVfo for the new demod's IF rate and bandwidth.
        if let Err(e) = rebuild_vfo_echoing(state, dsp_tx) {
            tracing::warn!("VFO rebuild on mode switch failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("VFO rebuild failed: {e}")));
        }
        let _ = dsp_tx.send(DspToUi::SampleRateChanged(
            state.frontend.effective_sample_rate(),
        ));
        let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));
        // The bandwidth was just reset to the new mode's default;
        // echo it so the Radio-panel row, status bar and spectrum
        // VFO width track the engine instead of keeping the old
        // mode's value whenever it happens to fall inside the new
        // range. Same echo `SetBandwidth` already emits. Per #697.
        let _ = dsp_tx.send(DspToUi::BandwidthChanged(state.bandwidth));

        // Notify the UI of the mode transition (edge detection — only
        // when the mode actually changed so idempotent refreshes do not
        // trigger the transcript-session boundary logic).
        //
        // The UI layer's response to `DemodModeChanged` is to toggle
        // the transcription enable row off, which eventually drops
        // the transcription channel via `DisableTranscription`. That
        // round-trip is async — until it completes the DSP thread
        // would otherwise keep pushing post-switch audio into the old
        // session, violating the "band change = hard session
        // boundary" contract in the Auto Break design spec. Drop the
        // tap locally FIRST so no post-switch samples leak into the
        // old backend, then notify the UI. The UI's eventual
        // `DisableTranscription` is idempotent on an already-cleared
        // tap.
        // Pin the post-apply DSP state so we can confirm the
        // mode + bandwidth + IF rate the engine actually
        // committed to. Pairs with the DSP_APPLY_REQUEST log
        // above for diagnose-from-log. Note: bandwidth has
        // just been reset to the new mode's default; an
        // upcoming SetBandwidth from the UI / recorder will
        // override it with the desired value.
        tracing::info!(
            target: "set_demod_mode",
            applied_mode = ?mode,
            applied_bandwidth_hz = state.bandwidth,
            applied_if_sample_rate = state.radio.demod_config().if_sample_rate,
            "DSP_APPLIED"
        );
        if old_mode != mode {
            reset_mode_session_boundaries(state, dsp_tx, mode);
        }
    }
}
/// Hard session-boundary reset for an actual mode transition in
/// [`handle_set_demod_mode`]: drop the transcription and generic
/// audio taps locally FIRST (before the UI round-trip), reset the
/// squelch/CTCSS/voice-squelch edge trackers to the rebuilt AF
/// chain's state, then notify the UI. Split out per the 50-NLOC
/// gate (#816 PR B).
fn reset_mode_session_boundaries(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    mode: DemodMode,
) {
    state.transcription_tx = None;
    // Same hard-boundary treatment for the generic
    // audio tap. A recognizer session downstream
    // treats every mode change as an utterance
    // boundary — letting post-switch
    // audio leak into the old session until the
    // UI round-trip sends DisableAudioTap would
    // corrupt the transcript across the mode
    // transition. Per CodeRabbit round 1 on PR
    // #349.
    state.audio_tap_tx = None;
    // Reset the decimation phase so a subsequent
    // EnableAudioTap starts at a clean 3:1
    // alignment instead of carrying a stale phase
    // from before the mode switch.
    state.audio_tap_phase = 0;
    state.squelch_was_open = false;
    state.transcription_squelch_was_open = false;
    // Mode switch rebuilds the AF chain + CTCSS
    // detector + voice squelch — edge trackers
    // must match the new closed state.
    state.ctcss_was_sustained = false;
    // Voice squelch reset to closed in an active
    // mode; in Off mode it's still "open" so the
    // tracker should track whatever the AF chain
    // reports after the rebuild. Simpler to just
    // snapshot it here and let the next process
    // iteration emit an edge if anything changed.
    state.voice_squelch_was_open = state.radio.voice_squelch_open();
    let _ = dsp_tx.send(DspToUi::DemodModeChanged(mode));
}

/// Handler for `UiToDsp::SetBandwidth`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_bandwidth(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, bw: f64) {
    // INFO-level so silent-fail demod regressions can be diagnosed
    // by grepping the log alone. Pairs with `tune_to_target`'s
    // TUNE_REQUEST line on the UI side.
    tracing::info!(target: "set_bandwidth", requested_hz = bw, "DSP_APPLY_REQUEST");
    on_tune_change(state);
    // Update the VFO channel filter first; only persist on success.
    if let Some(vfo) = &mut state.vfo {
        match vfo.set_bandwidth(bw) {
            Ok(()) => state.bandwidth = bw,
            Err(e) => {
                tracing::warn!("VFO bandwidth update failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Bandwidth failed: {e}")));
            }
        }
    } else {
        state.bandwidth = bw;
    }
    // Also pass to the radio module (some demods use it internally).
    state.radio.set_bandwidth(bw);
    // Companion log to DSP_APPLY_REQUEST above. If `requested`
    // and `applied` differ, the VFO clamped the value to its
    // valid range — visible from a single line.
    tracing::info!(
        target: "set_bandwidth",
        requested_hz = bw,
        applied_hz = state.bandwidth,
        "DSP_APPLIED"
    );
    // Notify UI so widgets that initiate bandwidth changes
    // via a different path (VFO drag handles on the
    // spectrum) can reflect the new value in the Radio
    // panel's bandwidth spin row. The `bandwidth_row`'s
    // own `set_value` path guards against feedback loops
    // via a `suppress_notify` flag on the UI side.
    let _ = dsp_tx.send(DspToUi::BandwidthChanged(state.bandwidth));
}
