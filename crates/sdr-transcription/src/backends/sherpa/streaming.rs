//! Streaming session loop for Zipformer (and future Parakeet-TDT).
//!
//! Runs on the sherpa-host worker thread. Owns nothing — all state
//! lives in the caller-provided `OnlineRecognizer` reference and a
//! per-session `OnlineStream`.

use std::sync::atomic::Ordering;
use std::sync::mpsc;

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};

use crate::backend::TranscriptionEvent;
use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};
use crate::{denoise, resampler};

use super::host::{AUDIO_RECV_TIMEOUT, SHERPA_SAMPLE_RATE_HZ, SessionParams};

/// Endpoint detection rule defaults — match upstream sherpa-onnx examples.
const RULE1_MIN_TRAILING_SILENCE: f32 = 2.4;
const RULE2_MIN_TRAILING_SILENCE: f32 = 1.2;
const RULE3_MIN_UTTERANCE_LENGTH: f32 = 20.0;

/// Initial capacity for the per-session resampled-mono scratch buffer.
const SESSION_MONO_BUFFER_CAPACITY: usize = 16_000;

/// ONNX Runtime threads per recognizer. Sherpa is fast enough on CPU
/// that one thread is sufficient and avoids competing with the audio
/// pipeline.
const SHERPA_NUM_THREADS: i32 = 1;

/// Build the `OnlineRecognizerConfig` for a streaming transducer model.
///
/// Note: `BackendConfig::silence_threshold` is intentionally NOT honored here
/// because sherpa-onnx's `OnlineRecognizer` has native endpoint detection
/// (via `rule1`/`rule2`/`rule3_min_trailing_silence`) that handles silence
/// at the model level. Adding an RMS-based pre-gate would mask short pauses
/// inside utterances and confuse the streaming decoder. The Whisper backend
/// uses `silence_threshold` because Whisper has no built-in VAD.
pub(super) fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    // Irrefutable today — will become refutable when Moonshine variant lands (plan Task 6).
    #[allow(irrefutable_let_patterns)]
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("StreamingZipformerEn is always a Transducer")
    };

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
    config.model_config.provider = Some(provider.to_owned());
    config.model_config.num_threads = SHERPA_NUM_THREADS;
    config.enable_endpoint = true;
    config.decoding_method = Some("greedy_search".to_owned());
    config.rule1_min_trailing_silence = RULE1_MIN_TRAILING_SILENCE;
    config.rule2_min_trailing_silence = RULE2_MIN_TRAILING_SILENCE;
    config.rule3_min_utterance_length = RULE3_MIN_UTTERANCE_LENGTH;

    config
}

/// One transcription session. Creates a fresh stream from `recognizer`,
/// runs the feed loop until cancelled or the audio channel disconnects.
pub(super) fn run_session(recognizer: &OnlineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
    } = params;

    let stream = recognizer.create_stream();

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let mut mono_buf: Vec<f32> = Vec::with_capacity(SESSION_MONO_BUFFER_CAPACITY);
    let mut last_partial = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa session cancelled");
            finalize_session(recognizer, &stream, &last_partial, &event_tx);
            return;
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                finalize_session(recognizer, &stream, &last_partial, &event_tx);
                return;
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, &mono_buf);

        while recognizer.is_ready(&stream) {
            if cancel.load(Ordering::Relaxed) {
                finalize_session(recognizer, &stream, &last_partial, &event_tx);
                return;
            }
            recognizer.decode(&stream);
        }

        let current_text = if let Some(result) = recognizer.get_result(&stream) {
            let trimmed = result.text.trim().to_owned();
            if !trimmed.is_empty() && trimmed != last_partial {
                last_partial.clone_from(&trimmed);
                let _ = event_tx.send(TranscriptionEvent::Partial {
                    text: trimmed.clone(),
                });
            }
            trimmed
        } else {
            String::new()
        };

        if recognizer.is_endpoint(&stream) {
            let committed_text = if current_text.is_empty() {
                last_partial.clone()
            } else {
                current_text
            };
            if !committed_text.is_empty() {
                let timestamp = crate::util::wall_clock_timestamp();
                tracing::debug!(%timestamp, text = %committed_text, "sherpa committed utterance");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: committed_text,
                });
            }
            recognizer.reset(&stream);
            last_partial.clear();
        }
    }

    finalize_session(recognizer, &stream, &last_partial, &event_tx);
    tracing::info!("sherpa session ended (audio channel disconnected)");
}

/// Commit any in-flight partial hypothesis as a final `Text` event before
/// the session ends. Called from both the cancel and disconnect exit paths.
fn finalize_session(
    recognizer: &OnlineRecognizer,
    stream: &OnlineStream,
    last_partial: &str,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let final_text = recognizer
        .get_result(stream)
        .map(|r| r.text.trim().to_owned())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| last_partial.to_owned());

    if !final_text.is_empty() {
        let timestamp = crate::util::wall_clock_timestamp();
        tracing::debug!(%timestamp, text = %final_text, "sherpa finalizing on session end");
        let _ = event_tx.send(TranscriptionEvent::Text {
            timestamp,
            text: final_text,
        });
    }
}
