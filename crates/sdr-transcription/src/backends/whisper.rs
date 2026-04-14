//! Whisper backend — `whisper-rs` powered transcription.
//!
//! Implements [`TranscriptionBackend`] for the [`crate::model::WhisperModel`]
//! family. Receives interleaved stereo f32 audio at 48 kHz, resamples to
//! 16 kHz mono, accumulates 5-second chunks, and runs Whisper inference on
//! non-silent chunks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent, TranscriptionInput,
};
use crate::{denoise, model, resampler};

/// Bounded channel capacity for audio buffers from DSP → backend.
/// Each buffer is ~1024-4096 stereo samples (~20-80 ms). At 48 kHz with
/// 5-second inference chunks, we need ~250 buffers to avoid drops during
/// a single inference pass. 512 gives comfortable headroom.
const AUDIO_CHANNEL_CAPACITY: usize = 512;

/// Seconds of audio per transcription chunk.
const CHUNK_SECONDS: usize = 5;

/// Number of 16 kHz mono samples per chunk (16000 * 5 = 80000).
const CHUNK_SAMPLES: usize = 16_000 * CHUNK_SECONDS;

/// Polling interval for the audio receive loop when checking for cancellation.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// `TranscriptionBackend` implementation backed by `whisper-rs`.
pub struct WhisperBackend {
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for WhisperBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WhisperBackend {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl TranscriptionBackend for WhisperBackend {
    fn name(&self) -> &'static str {
        "whisper"
    }

    fn supports_partials(&self) -> bool {
        false
    }

    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError> {
        // The cfg-gated Sherpa arm exists only when both features are
        // enabled (which compile_error prevents). In whisper-only builds
        // this match has one arm — allow clippy's infallible_destructuring_match
        // warning rather than hiding the intent of the guard.
        #[allow(clippy::infallible_destructuring_match)]
        let whisper_model = match config.model {
            ModelChoice::Whisper(m) => m,
            #[cfg(feature = "sherpa")]
            ModelChoice::Sherpa(_) => return Err(BackendError::WrongModelKind),
        };

        self.cancel.store(false, Ordering::Relaxed);

        let (audio_tx, audio_rx) = mpsc::sync_channel::<TranscriptionInput>(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        let cancel = Arc::clone(&self.cancel);
        let silence_threshold = config.silence_threshold;
        let noise_gate_ratio = config.noise_gate_ratio;

        let handle = std::thread::Builder::new()
            .name("whisper-worker".into())
            .spawn(move || {
                run_worker(
                    &audio_rx,
                    &event_tx,
                    &cancel,
                    whisper_model,
                    silence_threshold,
                    noise_gate_ratio,
                );
            })?;

        self.worker = Some(handle);
        tracing::info!("whisper backend started");

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        tracing::info!("whisper backend stopped");
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.worker.take(); // detach — don't join
        tracing::info!("whisper backend shutdown (non-blocking)");
    }
}

/// Main worker loop. Blocks the calling thread and exits when `audio_rx` is
/// closed (all senders dropped) or the cancellation token is set.
/// Should be spawned on a dedicated thread.
///
/// # Arguments
/// * `audio_rx` — receives interleaved stereo f32 audio at 48 kHz
/// * `event_tx` — sends transcription events to the UI/consumer
/// * `cancel` — cancellation token; when set to `true`, the worker exits promptly
/// * `model` — which Whisper model to load
/// * `silence_threshold` — RMS below which a chunk is skipped
/// * `noise_gate_ratio` — spectral gate multiplier over noise floor
fn run_worker(
    audio_rx: &mpsc::Receiver<TranscriptionInput>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: model::WhisperModel,
    silence_threshold: f32,
    noise_gate_ratio: f32,
) {
    if let Err(e) = run_worker_inner(
        audio_rx,
        event_tx,
        cancel,
        model,
        silence_threshold,
        noise_gate_ratio,
    ) {
        let _ = event_tx.send(TranscriptionEvent::Error(e));
    }
}

/// Inner implementation that returns errors as strings so the outer function
/// can forward them as `TranscriptionEvent::Error`.
#[allow(clippy::too_many_lines)]
fn run_worker_inner(
    audio_rx: &mpsc::Receiver<TranscriptionInput>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: model::WhisperModel,
    silence_threshold: f32,
    noise_gate_ratio: f32,
) -> Result<(), String> {
    // --- Model download / load ---
    let model_path = if model::model_exists(model) {
        tracing::info!(?model, "whisper model already present");
        model::model_path(model)
    } else {
        tracing::info!("whisper model not found, downloading");
        let (progress_tx, progress_rx) = mpsc::channel::<u8>();
        let event_tx_dl = event_tx.clone();

        let progress_thread = std::thread::Builder::new()
            .name("whisper-dl-progress".into())
            .spawn(move || {
                while let Ok(pct) = progress_rx.recv() {
                    let _ = event_tx_dl.send(TranscriptionEvent::Downloading { progress_pct: pct });
                }
            })
            .map_err(|e| format!("failed to spawn progress thread: {e}"))?;

        let path = model::download_model(model, &progress_tx)
            .map_err(|e| format!("model download failed: {e}"))?;

        drop(progress_tx);
        let _ = progress_thread.join();

        path
    };

    tracing::info!(?model_path, "loading Whisper model");
    let ctx = WhisperContext::new_with_params(&model_path, WhisperContextParameters::default())
        .map_err(|e| {
            format!(
                "Failed to load model: {e}. If using a GPU, try a smaller model — \
                 the selected model may exceed available VRAM."
            )
        })?;

    let mut state = ctx
        .create_state()
        .map_err(|e| format!("failed to create whisper state: {e}"))?;

    tracing::info!("whisper model loaded, ready for inference");
    event_tx
        .send(TranscriptionEvent::Ready)
        .map_err(|_| "event channel closed before Ready".to_owned())?;

    // --- Audio loop ---
    let mut mono_buf: Vec<f32> = Vec::with_capacity(CHUNK_SAMPLES * 2);

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("transcription cancelled, worker exiting");
            return Ok(());
        }

        let input = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(data) => data,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let interleaved = match input {
            TranscriptionInput::Samples(s) => s,
            TranscriptionInput::SquelchOpened | TranscriptionInput::SquelchClosed => continue,
        };

        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        // Drain any additional queued buffers to minimize frame drops
        // during long inference passes. Check cancel between drains.
        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("transcription cancelled, worker exiting");
                return Ok(());
            }
            if let TranscriptionInput::Samples(s) = extra {
                resampler::downsample_stereo_to_mono_16k(&s, &mut mono_buf);
            }
        }

        while mono_buf.len() >= CHUNK_SAMPLES {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("transcription cancelled, worker exiting");
                return Ok(());
            }

            let mut chunk: Vec<f32> = mono_buf.drain(..CHUNK_SAMPLES).collect();

            // Voice-band shaped spectral gate (#274) — remove broadband
            // static, rumble, PL tones, and above-voice hiss before
            // Whisper sees the audio.
            denoise::enhance_speech(&mut chunk, noise_gate_ratio);

            let rms = compute_rms(&chunk);
            if rms < silence_threshold {
                tracing::debug!(rms, "chunk below silence threshold, skipping");
                continue;
            }

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some("en"));
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_no_context(true);

            if let Err(e) = state.full(params, &chunk) {
                tracing::warn!("whisper inference failed: {e}");
                continue;
            }

            let n_segments = state.full_n_segments();
            let mut combined = String::new();

            for i in 0..n_segments {
                if let Some(segment) = state.get_segment(i)
                    && let Ok(text) = segment.to_str()
                {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if !combined.is_empty() {
                            combined.push(' ');
                        }
                        combined.push_str(trimmed);
                    }
                }
            }

            if !combined.is_empty() && !is_hallucination(&combined) {
                let timestamp = crate::util::wall_clock_timestamp();
                tracing::debug!(%timestamp, %combined, "transcribed chunk");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: combined,
                });
            }
        }
    }

    tracing::info!("audio channel closed, worker exiting");
    Ok(())
}

/// Common hallucination phrases Whisper produces on silence/noise.
const HALLUCINATIONS: &[&str] = &[
    "thank you",
    "thanks for watching",
    "subscribe",
    "like and subscribe",
    "see you next time",
    "bye",
    "you",
    "the end",
];

/// Check if Whisper output is a known hallucination pattern.
///
/// Whisper tends to produce these when fed non-speech audio (radio static,
/// tones, data bursts). We filter them out to keep the transcript clean.
fn is_hallucination(text: &str) -> bool {
    let lower = text.to_lowercase();

    if (lower.starts_with('[') && lower.ends_with(']'))
        || (lower.starts_with('(') && lower.ends_with(')'))
    {
        return true;
    }

    HALLUCINATIONS
        .iter()
        .any(|h| lower.trim().eq_ignore_ascii_case(h))
}

/// Compute the root-mean-square of a sample buffer.
pub(crate) fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum_sq / samples.len() as f32;
    mean.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero() {
        let silence = vec![0.0_f32; 1024];
        let rms = compute_rms(&silence);
        assert!((rms - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_ones_is_one() {
        let ones = vec![1.0_f32; 1024];
        let rms = compute_rms(&ones);
        assert!((rms - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rms_of_empty_is_zero() {
        let rms = compute_rms(&[]);
        assert!((rms - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn whisper_backend_does_not_support_partials() {
        let backend = WhisperBackend::new();
        assert!(!backend.supports_partials());
    }

    #[test]
    fn whisper_backend_name_is_stable() {
        let backend = WhisperBackend::new();
        assert_eq!(backend.name(), "whisper");
    }
}
