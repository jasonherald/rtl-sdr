//! HF radiofax (WEFAX) decode tap — bridges the post-demod USB audio
//! path into the [`WefaxDecoder`] and forwards decoded scan lines,
//! decoder-state transitions, and completed charts to the UI.
//! Mirrors [`super::sstv::sstv_decode_tap`]'s shape as closely as the
//! two decoders' APIs allow. Issue #877.

use super::{DspState, DspToUi, WefaxDecoder, WefaxLine, mpsc};
use sdr_dsp::wefax::PIXELS_PER_LINE;

/// Max scan lines a single [`WefaxDecoder::process`] call can emit
/// into `wefax_lines_buf`. WEFAX runs at 120 lines/minute (2 lines
/// per second); an audio block is far shorter than half a second in
/// practice, so lines-per-call is almost always 0 or 1. Generous
/// headroom, allocated once — mirrors the sizing philosophy behind
/// `sdr_dsp::apt::READY_QUEUE_CAP`.
const WEFAX_LINES_BUF_CAP: usize = 8;

/// Cadence, in decoded lines, of the throttled progress log. At 120 lpm
/// this fires roughly once per 50 s so a live reception is visible in the
/// logs without flooding at the per-line rate.
const WEFAX_PROGRESS_LOG_INTERVAL_LINES: u32 = 100;

/// HF radiofax decode tap. Lazy-inits the [`WefaxDecoder`] at the
/// `RadioModule`'s current audio sample rate, downmixes the post-
/// `radio.process` stereo audio block to mono, feeds the decoder,
/// and dispatches produced lines / state changes / completed charts
/// through the DSP→UI channel.
///
/// Runs whenever the active demod is `DemodMode::Wefax`, independent
/// of whether a `WefaxImageHandle` is wired: the decoder keeps
/// running (preserving phasing/imaging state) even with no live
/// viewer open, and only the handle-dependent writes are skipped.
/// Per issue #877.
pub(super) fn wefax_decode_tap(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    audio_count: usize,
) {
    if state.wefax_decoder.is_none() && !init_wefax_decoder(state) {
        return;
    }
    let Some(decoder) = state.wefax_decoder.as_mut() else {
        return;
    };

    // Mono downmix, pre-gate for the same reason as the APT/SSTV
    // taps (#734): the fax subcarrier has no speech cadence, so the
    // gated (squelch-zeroed) buffer would feed the decoder black
    // lines on every fade.
    super::audio::downmix_pre_gate_mono(&state.radio, audio_count, &mut state.wefax_mono_buf);

    let produced = match decoder.process(&state.wefax_mono_buf, &mut state.wefax_lines_buf) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("WEFAX decode failed: {e}");
            return;
        }
    };

    emit_lines(state, dsp_tx, produced);
    emit_state_if_changed(state, dsp_tx);
    emit_complete_if_ready(state, dsp_tx);
}

/// Lazy-init `state.wefax_decoder`. Caches a failing rate in
/// `wefax_init_failed_at_rate` so the audio-block hot loop doesn't
/// retry (and warn-log) construction on every block. Mirrors
/// `apt_decode_tap` / `sstv_decode_tap`'s init guard.
fn init_wefax_decoder(state: &mut DspState) -> bool {
    // The decoder is initialised at the RadioModule's *audio* output rate
    // (48 kHz), which is exactly the rate `wefax_decode_tap` feeds it via
    // `downmix_pre_gate_mono` (the post-`radio.process` audio block). Line
    // geometry is therefore self-consistent. The WEFAX demod's 24 kHz IF
    // rate is internal to the demod and is resampled up to the audio rate
    // before the tap ever sees it — the two never touch, so 48 kHz here is
    // correct, not a mismatch.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rate_hz = state.radio.audio_sample_rate() as u32;
    if state.wefax_init_failed_at_rate == Some(rate_hz) {
        return false;
    }
    match WefaxDecoder::new(rate_hz) {
        Ok(decoder) => {
            tracing::info!("WEFAX decoder initialised at {rate_hz} Hz");
            state.wefax_decoder = Some(decoder);
            state.wefax_init_failed_at_rate = None;
            state
                .wefax_lines_buf
                .resize(WEFAX_LINES_BUF_CAP, WefaxLine::default());
            true
        }
        Err(e) => {
            tracing::warn!("WEFAX decoder init failed at {rate_hz} Hz: {e}");
            state.wefax_init_failed_at_rate = Some(rate_hz);
            false
        }
    }
}

/// Write the `produced` newly-decoded lines into the shared image
/// handle (if wired) and notify the UI of each. `mem::take` lifts
/// each line out of the pre-allocated buffer by swapping in
/// `WefaxLine::default()`, avoiding a ~1.8 KB clone per line —
/// mirrors `apt_decode_tap`'s emission loop.
fn emit_lines(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, produced: usize) {
    for slot in state.wefax_lines_buf.iter_mut().take(produced) {
        let line = std::mem::take(slot);
        if let Some(handle) = state.wefax_image.as_ref() {
            #[allow(clippy::cast_possible_truncation)]
            handle.write_line(line.line_index, PIXELS_PER_LINE as u32, &line.pixels);
        }
        // Throttled progress log so a live reception is visible in the
        // logs without flooding at the per-line rate.
        if line.line_index % WEFAX_PROGRESS_LOG_INTERVAL_LINES == 0 {
            tracing::debug!(line = line.line_index, "WEFAX line decoded");
        }
        let _ = dsp_tx.send(DspToUi::WefaxLineDecoded(line.line_index));
    }
}

/// Send `DspToUi::WefaxState` when the decoder's phase changed since
/// the last check. Edge-triggered so the channel isn't flooded at
/// the audio-block rate.
fn emit_state_if_changed(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    let Some(decoder) = state.wefax_decoder.as_ref() else {
        return;
    };
    let current = decoder.state();
    if state.wefax_last_state != Some(current) {
        // Log the phase transition (Idle → Phasing → Imaging → Stopped).
        // The `WefaxState` message only reaches the live viewer; this log
        // is the sole record for headless / log-based debugging of a live
        // reception.
        tracing::debug!(from = ?state.wefax_last_state, to = ?current, "WEFAX decoder state changed");
        state.wefax_last_state = Some(current);
        let _ = dsp_tx.send(DspToUi::WefaxState(current));
    }
}

/// Drain the decoder's one-shot "chart complete" flag and, if a
/// shared image handle is wired, hand the finished chart to the UI.
/// The flag is always drained (even with no handle wired) so a
/// handle attached mid-chart later doesn't see a stale completion.
fn emit_complete_if_ready(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    let Some(decoder) = state.wefax_decoder.as_mut() else {
        return;
    };
    if !decoder.take_chart_complete() {
        return;
    }
    let Some(handle) = state.wefax_image.as_ref() else {
        return;
    };
    let Some(completed) = handle.take_completed() else {
        return;
    };
    let (width, height) = (completed.width, completed.height);
    tracing::info!(width, height, "WEFAX chart complete");
    let _ = dsp_tx.send(DspToUi::WefaxImageComplete {
        width,
        height,
        pixels: completed.pixels,
    });
}
