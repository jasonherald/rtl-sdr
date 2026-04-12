//! Sherpa-onnx backend — streaming-native ASR via the official k2-fsa
//! `sherpa-onnx` Rust crate.
//!
//! Implements [`TranscriptionBackend`] using `OnlineRecognizer` for true
//! frame-by-frame streaming with endpoint detection. Partial hypotheses
//! are emitted as `TranscriptionEvent::Partial`; committed utterances
//! after silence detection are emitted as `TranscriptionEvent::Text`.
//!
//! `OnlineRecognizer` and `OnlineStream` are `!Send` (they wrap raw
//! pointers into the C library), so all recognizer interaction happens
//! on the worker thread. The `SherpaBackend` struct itself only holds
//! the cancellation token and the worker join handle, mirroring
//! `WhisperBackend`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};
use crate::sherpa_model::{self, SherpaModel};
use crate::{denoise, resampler};

/// Bounded channel capacity for audio buffers from DSP → backend.
/// Sherpa accepts much smaller chunks than Whisper (per-frame, not
/// 5-second windows), so we don't need the same headroom `WhisperBackend`
/// does. 256 still gives plenty of buffer.
const AUDIO_CHANNEL_CAPACITY: usize = 256;

/// Polling interval for the audio receive loop when checking for cancellation.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Endpoint detection rule defaults — match upstream sherpa-onnx examples.
const RULE1_MIN_TRAILING_SILENCE: f32 = 2.4;
const RULE2_MIN_TRAILING_SILENCE: f32 = 1.2;
const RULE3_MIN_UTTERANCE_LENGTH: f32 = 20.0;

/// `TranscriptionBackend` implementation backed by `sherpa-onnx`.
pub struct SherpaBackend {
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for SherpaBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SherpaBackend {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl TranscriptionBackend for SherpaBackend {
    fn name(&self) -> &'static str {
        "sherpa"
    }

    fn supports_partials(&self) -> bool {
        true
    }

    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError> {
        let ModelChoice::Sherpa(sherpa_model) = config.model else {
            return Err(BackendError::WrongModelKind);
        };

        // Verify the model files exist before spawning the worker, so the
        // user gets a useful error immediately rather than after the
        // recognizer fails inside the thread.
        if !sherpa_model::model_exists(sherpa_model) {
            return Err(BackendError::ModelNotFound {
                path: sherpa_model::model_directory(sherpa_model),
            });
        }

        self.cancel.store(false, Ordering::Relaxed);

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        let cancel = Arc::clone(&self.cancel);
        // silence_threshold is unused: sherpa's endpoint detection
        // handles silence natively.
        let noise_gate_ratio = config.noise_gate_ratio;

        let handle = std::thread::Builder::new()
            .name("sherpa-worker".into())
            .spawn(move || {
                run_worker(
                    &audio_rx,
                    &event_tx,
                    &cancel,
                    sherpa_model,
                    noise_gate_ratio,
                );
            })?;

        self.worker = Some(handle);
        tracing::info!("sherpa backend started");

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        tracing::info!("sherpa backend stopped");
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.worker.take(); // detach — don't join
        tracing::info!("sherpa backend shutdown (non-blocking)");
    }
}

/// Build the `OnlineRecognizerConfig` for a Streaming Zipformer model.
///
/// The `provider` parameter selects the ONNX execution provider — `"cpu"`
/// is the default; `"cuda"` enables GPU acceleration if libsherpa was
/// built with CUDA support. PR 2 hardcodes `"cpu"`; CUDA validation is
/// the manual smoke test in Task 10.
fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    let (encoder, decoder, joiner, tokens) = sherpa_model::model_file_paths(model);

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
    config.model_config.provider = Some(provider.to_owned());
    config.model_config.num_threads = 1;
    config.enable_endpoint = true;
    config.decoding_method = Some("greedy_search".to_owned());
    config.rule1_min_trailing_silence = RULE1_MIN_TRAILING_SILENCE;
    config.rule2_min_trailing_silence = RULE2_MIN_TRAILING_SILENCE;
    config.rule3_min_utterance_length = RULE3_MIN_UTTERANCE_LENGTH;

    config
}

/// Worker thread entry point. Owns the recognizer and stream for the
/// entire transcription session.
fn run_worker(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: SherpaModel,
    noise_gate_ratio: f32,
) {
    if let Err(e) = run_worker_inner(audio_rx, event_tx, cancel, model, noise_gate_ratio) {
        let _ = event_tx.send(TranscriptionEvent::Error(e));
    }
}

#[allow(clippy::too_many_lines)]
fn run_worker_inner(
    audio_rx: &mpsc::Receiver<Vec<f32>>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: SherpaModel,
    noise_gate_ratio: f32,
) -> Result<(), String> {
    // For PR 2 we hardcode CPU provider. CUDA support is validated in
    // the smoke test step; if CUDA works, a follow-up PR wires it
    // through the UI as a separate setting.
    let provider = "cpu";
    let recognizer_config = build_recognizer_config(model, provider);

    tracing::info!(?model, provider, "loading sherpa-onnx model");
    let recognizer = OnlineRecognizer::create(&recognizer_config).ok_or_else(|| {
        "OnlineRecognizer::create returned None — check model file paths".to_owned()
    })?;

    let stream = recognizer.create_stream();

    tracing::info!("sherpa-onnx model loaded, ready for inference");
    event_tx
        .send(TranscriptionEvent::Ready)
        .map_err(|_| "event channel closed before Ready".to_owned())?;

    // Sherpa expects 16 kHz mono f32 samples. We accept the same
    // 48 kHz interleaved stereo from the DSP thread that Whisper does,
    // run it through the spectral denoiser, downsample to 16k mono, and
    // feed sherpa one chunk at a time.
    let mut mono_buf: Vec<f32> = Vec::with_capacity(16_000);
    let mut last_partial = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa transcription cancelled, worker exiting");
            return Ok(());
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(data) => data,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Resample 48k stereo → 16k mono into the scratch buffer.
        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        // Drain any additional queued buffers into the same scratch
        // (same pattern as WhisperBackend) so we don't fall behind.
        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("sherpa transcription cancelled, worker exiting");
                return Ok(());
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        // Spectral noise gate (same preprocessor as Whisper).
        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        // Feed the chunk to sherpa.
        stream.accept_waveform(16_000_i32, &mono_buf);

        // Decode as much as the recognizer is ready for.
        while recognizer.is_ready(&stream) {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("sherpa transcription cancelled mid-decode, exiting");
                return Ok(());
            }
            recognizer.decode(&stream);
        }

        // Pull the current hypothesis. Emit a Partial event if it
        // changed since the last one (avoid flooding the UI thread).
        // We capture the trimmed text so it can be reused for the
        // committed Text event below if the stream reaches an endpoint.
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
            // get_result can return None on a serde failure inside the C
            // layer. We must NOT skip the endpoint check below in that
            // case — otherwise the stream can get stuck in endpoint state
            // and silently stop transcribing.
            String::new()
        };

        // Endpoint check is independent of get_result and must always
        // run so reset() fires when the recognizer says the utterance
        // is over.
        if recognizer.is_endpoint(&stream) {
            if !current_text.is_empty() {
                let timestamp = wall_clock_timestamp();
                tracing::debug!(%timestamp, text = %current_text, "sherpa committed utterance");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: current_text,
                });
            }
            recognizer.reset(&stream);
            last_partial.clear();
        }
    }

    tracing::info!("sherpa audio channel closed, worker exiting");
    Ok(())
}

/// Wall-clock "HH:MM:SS" string. Same implementation as
/// [`crate::backends::whisper`] but kept local to avoid a public
/// re-export of an internal helper.
fn wall_clock_timestamp() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };

    // SAFETY: gettimeofday writes into the provided buffer and is thread-safe.
    #[allow(unsafe_code)]
    let epoch = unsafe {
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        tv.tv_sec
    };

    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();

    // SAFETY: localtime_r is the reentrant variant; gmtime_r is the UTC fallback.
    #[allow(unsafe_code)]
    let tm = unsafe {
        let result = libc::localtime_r(&raw const epoch, tm.as_mut_ptr());
        let result = if result.is_null() {
            libc::gmtime_r(&raw const epoch, tm.as_mut_ptr())
        } else {
            result
        };
        if result.is_null() {
            return "00:00:00".to_owned();
        }
        tm.assume_init()
    };

    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sherpa_backend_supports_partials() {
        let backend = SherpaBackend::new();
        assert!(backend.supports_partials());
    }

    #[test]
    fn sherpa_backend_name_is_stable() {
        let backend = SherpaBackend::new();
        assert_eq!(backend.name(), "sherpa");
    }

    #[test]
    #[allow(clippy::panic)]
    fn sherpa_backend_start_returns_model_not_found_when_files_missing() {
        // The test environment has no sherpa model files in
        // ~/.local/share/sdr-rs/models/sherpa/, so start() should
        // synchronously return ModelNotFound before spawning the worker.
        // This exercises the synchronous error path without needing a
        // real model bundle.
        //
        // Note: this test would fail if the developer running it has
        // actually downloaded the Streaming Zipformer model. That's
        // acceptable — CI runs in a clean environment.
        let mut backend = SherpaBackend::new();
        let config = BackendConfig {
            model: ModelChoice::Sherpa(SherpaModel::StreamingZipformerEn),
            silence_threshold: 0.007,
            noise_gate_ratio: 3.0,
        };
        let result = backend.start(config);
        match result {
            Err(BackendError::ModelNotFound { path }) => {
                assert!(path.ends_with("sherpa/streaming-zipformer-en"));
            }
            Ok(_) => {
                // If the developer happens to have the model downloaded
                // locally, skip this test rather than fail. Tearing down
                // the running backend is fine because it's just a thread.
                backend.shutdown_nonblocking();
                eprintln!(
                    "skipping test: streaming-zipformer-en model is present locally"
                );
            }
            Err(e) => panic!("expected ModelNotFound, got {e:?}"),
        }
    }
}
