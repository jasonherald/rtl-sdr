//! Offline session loop for Moonshine (and future offline recognizers).
//!
//! Runs on the sherpa-host worker thread. Uses Silero VAD to detect
//! utterance boundaries in the incoming audio stream, then batch-decodes
//! each completed segment through the `OfflineRecognizer`.
//!
//! Unlike the streaming loop, this path emits NO `TranscriptionEvent::Partial`
//! events. Moonshine is offline — partials aren't meaningful. The UI hides
//! the Live/Final display-mode toggle when a Moonshine model is selected
//! (see `SherpaModel::supports_partials`).

use std::sync::atomic::Ordering;
use std::sync::mpsc;

use sherpa_onnx::{
    OfflineModelConfig, OfflineMoonshineModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineTransducerModelConfig,
};

use crate::backend::TranscriptionEvent;
use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};
use crate::vad::VoiceActivityDetector;
use crate::{denoise, resampler};

use super::host::{AUDIO_RECV_TIMEOUT, SHERPA_SAMPLE_RATE_HZ, SessionParams};
use super::silero_vad::SherpaSileroVad;

/// Initial capacity for the per-session resampled-mono scratch buffer.
const SESSION_MONO_BUFFER_CAPACITY: usize = 16_000;

/// ONNX Runtime threads per recognizer. Sherpa is fast enough on CPU
/// that one thread is sufficient and avoids competing with the audio
/// pipeline.
const SHERPA_NUM_THREADS: i32 = 1;

/// sherpa-onnx `model_type` field value that selects `NeMo`'s Token-and-Duration
/// Transducer decode loop. Without this exact string, sherpa-onnx falls back
/// to the generic transducer decode path which doesn't understand TDT's
/// joiner output shape, and `OfflineRecognizer::create` returns `None` at
/// runtime — silent failure mode.
///
/// Mirrors the upstream `rust-api-examples/examples/nemo_parakeet.rs` example.
const NEMO_TRANSDUCER_MODEL_TYPE: &str = "nemo_transducer";

/// Build the `OfflineRecognizerConfig` for a Moonshine v1 model.
///
/// k2-fsa's int8 Moonshine releases use the v1 layout with five files:
/// preprocessor (not quantized), encoder, uncached decoder, cached
/// decoder, and tokens. The v2 two-file layout (encoder plus merged
/// decoder) exists in `OfflineMoonshineModelConfig` but is not what
/// the releases actually ship.
pub(super) fn build_moonshine_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> OfflineRecognizerConfig {
    let ModelFilePaths::Moonshine {
        preprocessor,
        encoder,
        uncached_decoder,
        cached_decoder,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        unreachable!("offline::build_moonshine_recognizer_config called with non-Moonshine model")
    };

    let moonshine = OfflineMoonshineModelConfig {
        preprocessor: Some(preprocessor.to_string_lossy().into_owned()),
        encoder: Some(encoder.to_string_lossy().into_owned()),
        uncached_decoder: Some(uncached_decoder.to_string_lossy().into_owned()),
        cached_decoder: Some(cached_decoder.to_string_lossy().into_owned()),
        ..OfflineMoonshineModelConfig::default()
    };

    let model_config = OfflineModelConfig {
        moonshine,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        ..OfflineModelConfig::default()
    };

    OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    }
}

/// Build the `OfflineRecognizerConfig` for a `NeMo` Parakeet-TDT model.
///
/// Uses sherpa-onnx's offline transducer config (4 files: encoder,
/// decoder, joiner, tokens) with `model_type = "nemo_transducer"`.
/// The `model_type` field is required — without it, sherpa-onnx tries
/// to use the generic transducer decode loop which doesn't understand
/// `NeMo`'s TDT (Token-and-Duration Transducer) joiner output shape.
///
/// Mirrors the upstream `rust-api-examples/examples/nemo_parakeet.rs`
/// example.
pub(super) fn build_nemo_transducer_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> OfflineRecognizerConfig {
    let ModelFilePaths::Transducer {
        encoder,
        decoder,
        joiner,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        unreachable!(
            "offline::build_nemo_transducer_recognizer_config called with non-Transducer layout"
        )
    };

    let transducer = OfflineTransducerModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        decoder: Some(decoder.to_string_lossy().into_owned()),
        joiner: Some(joiner.to_string_lossy().into_owned()),
    };

    let model_config = OfflineModelConfig {
        transducer,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        // Required — tells sherpa-onnx to use NeMo's TDT decode loop
        // instead of the generic transducer path.
        model_type: Some(NEMO_TRANSDUCER_MODEL_TYPE.to_owned()),
        ..OfflineModelConfig::default()
    };

    OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    }
}

/// One offline transcription session. Feeds audio through the VAD,
/// batch-decodes each detected speech segment, and emits `Text` events.
/// Never emits `Partial`.
pub(super) fn run_session(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    params: SessionParams,
) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
    } = params;

    // Clear any residual state from a previous session.
    vad.reset();

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let mut mono_buf: Vec<f32> = Vec::with_capacity(SESSION_MONO_BUFFER_CAPACITY);

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa offline session cancelled");
            drain_vad_on_exit(recognizer, vad, &event_tx);
            return;
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Resample 48 kHz stereo → 16 kHz mono.
        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(recognizer, vad, &event_tx);
                return;
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        // Spectral denoise BEFORE VAD — RTL-SDR squelch tails confuse
        // Silero just as much as they confuse decoders.
        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        vad.accept(&mono_buf);

        while let Some(segment) = vad.pop_segment() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(recognizer, vad, &event_tx);
                return;
            }
            decode_segment(recognizer, &segment, &event_tx);
        }
    }

    // Audio channel disconnected — flush any in-flight segment.
    drain_vad_on_exit(recognizer, vad, &event_tx);
    tracing::info!("sherpa offline session ended (audio channel disconnected)");
}

/// Batch-decode a single speech segment and emit a `Text` event if
/// the recognizer produced any text.
fn decode_segment(
    recognizer: &OfflineRecognizer,
    segment: &[f32],
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let stream = recognizer.create_stream();
    stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, segment);
    recognizer.decode(&stream);
    let Some(result) = stream.get_result() else {
        return;
    };
    let text = result.text.trim().to_owned();
    if !text.is_empty() {
        let timestamp = crate::util::wall_clock_timestamp();
        tracing::debug!(%timestamp, %text, "moonshine committed utterance");
        let _ = event_tx.send(TranscriptionEvent::Text { timestamp, text });
    }
}

/// Flush the VAD on session exit and drain every remaining segment —
/// including any in-flight utterance Silero hadn't yet finalized.
///
/// Without the explicit `flush` call, a user stopping transcription
/// mid-speech would lose the last utterance because `pop_segment`
/// only returns segments that VAD already marked complete. `flush`
/// forces finalization so the final `while let` sees that segment.
///
/// Resets the VAD afterward so the next session starts clean.
fn drain_vad_on_exit(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    vad.flush();
    while let Some(segment) = vad.pop_segment() {
        decode_segment(recognizer, &segment, event_tx);
    }
    vad.reset();
}
