//! NOAA APT decode tap — bridges the post-VFO audio path into the APT
//! decoder and forwards finished scan lines to the UI.

use super::{AptDecoder, AptLine, DspState, DspToUi, READY_QUEUE_CAP, mpsc};

/// NOAA APT decode tap. Lazy-initialises the decoder at the
/// `RadioModule`'s current audio sample rate, downmixes the post-
/// `radio.process` stereo audio block to mono, runs the decoder,
/// and emits any newly-produced lines through the DSP→UI channel
/// as `DspToUi::AptLine`.
///
/// Per epic #468 / ticket #482. Caller must ensure
/// `audio_count > 0` and the active demod is NFM.
pub(super) fn apt_decode_tap(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    audio_count: usize,
) {
    // Lazy-init. Audio rate comes from `RadioModule::audio_sample_rate`
    // (typically 48 kHz, well above the decoder's 10.6 kHz floor).
    if state.apt_decoder.is_none() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate_hz = state.radio.audio_sample_rate() as u32;
        // Guard against retry-spamming the warn log on a rate the
        // decoder will reject. If we've already tried this exact
        // rate and it failed, silently bail until either the rate
        // changes (next-block check) or `cleanup` clears the cache.
        if state.apt_init_failed_at_rate == Some(rate_hz) {
            return;
        }
        match AptDecoder::new(rate_hz) {
            Ok(decoder) => {
                tracing::info!("APT decoder initialised at {rate_hz} Hz");
                state.apt_decoder = Some(decoder);
                state.apt_init_failed_at_rate = None;
                // Size the output slice to the decoder's documented
                // per-call emission cap so a single `process` call
                // can never need to flush.
                state
                    .apt_lines_buf
                    .resize(READY_QUEUE_CAP, AptLine::default());
            }
            Err(e) => {
                tracing::warn!("APT decoder init failed at {rate_hz} Hz: {e}");
                state.apt_init_failed_at_rate = Some(rate_hz);
                return;
            }
        }
    }
    let Some(decoder) = state.apt_decoder.as_mut() else {
        return;
    };

    // Mono downmix. APT is mono by spec — averaging L+R is
    // equivalent to taking either channel for FM-demodulated
    // audio (both channels carry the same baseband signal once
    // any stereo pilot is filtered out by the channel filter).
    // `extend` over a `map` iterator is exact-size, so `Vec`'s
    // internal reserve is precise — no manual `reserve` needed.
    // Pre-gate audio: the speaker path zeroes on a closed power /
    // CTCSS / voice squelch, and the APT 2400 Hz subcarrier has no
    // speech cadence, so the gated buffer would feed the decoder
    // black lines on every fade (#734).
    state.apt_mono_buf.clear();
    state.apt_mono_buf.extend(
        state.radio.pre_gate_audio()[..audio_count]
            .iter()
            .map(|s| f32::midpoint(s.l, s.r)),
    );

    match decoder.process(&state.apt_mono_buf, &mut state.apt_lines_buf) {
        Ok(produced) => {
            // `mem::take` lifts each emitted line out by swapping in
            // `AptLine::default()` — moves ownership without the
            // ~2 KB clone. The next `process` call overwrites the
            // (now-default) slot regardless, so leaving an empty
            // line behind is harmless.
            for slot in state.apt_lines_buf.iter_mut().take(produced) {
                let line = std::mem::take(slot);
                let _ = dsp_tx.send(DspToUi::AptLine(Box::new(line)));
            }
        }
        Err(e) => {
            tracing::warn!("APT decode failed: {e}");
        }
    }
}
