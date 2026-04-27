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
use crate::backends::earshot_vad::EarshotVad;
use crate::vad::VoiceActivityDetector;
use crate::{denoise, model, resampler};

/// Bounded channel capacity for audio buffers from DSP → backend.
/// Each buffer is ~1024-4096 stereo samples (~20-80 ms). At
/// 48 kHz with 5-second inference chunks, we need ~250 buffers
/// to avoid drops during a single inference pass; whisper
/// inference can extend to several seconds on slower CPUs and
/// CUDA-busy systems, and the previous `512` ceiling filled
/// during long utterances and surfaced as a flood of
/// `transcription channel full; retrying squelch edge next
/// block` warns. `2048` gives enough headroom that a normal
/// worker pause doesn't spam the log even when the warn is
/// throttled. Per FYI-flood reported during PR for issues
/// #538 / #539.
const AUDIO_CHANNEL_CAPACITY: usize = 2048;

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
        let audio_enhancement = config.audio_enhancement;

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
                    audio_enhancement,
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
/// * `audio_enhancement` — which denoise strategy to apply per segment
fn run_worker(
    audio_rx: &mpsc::Receiver<TranscriptionInput>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
    model: model::WhisperModel,
    silence_threshold: f32,
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
) {
    if let Err(e) = run_worker_inner(
        audio_rx,
        event_tx,
        cancel,
        model,
        silence_threshold,
        noise_gate_ratio,
        audio_enhancement,
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
    audio_enhancement: denoise::AudioEnhancement,
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

    // --- VAD + audio loop ---
    //
    // Pre-#259: fixed 5-second chunking + broadband RMS silence gate.
    // The fixed chunking frequently cut utterances in half (typical NFM
    // transmission is 1–4 s, so two back-to-back transmissions land in
    // different chunks or a long one gets split), and the RMS gate
    // false-triggered on squelch tails. Both showed up in committed
    // text as noisy mid-word splits and transcripts of dead air.
    //
    // Post-#259: EarshotVad (pure-Rust VAD, no ONNX runtime) runs a
    // 16-ms-frame state machine over the incoming audio. Each popped
    // segment is a complete utterance bounded by silence, so Whisper
    // only ever sees whole transmissions and never decodes dead air.
    //
    // `silence_threshold` is still accepted in BackendConfig so the
    // UI slider doesn't break, but it's no longer read here — the VAD
    // subsumes its purpose. A follow-up PR can retire the slider.
    let _ = silence_threshold; // preserved for API compatibility
    let mut scratch: Vec<f32> = Vec::with_capacity(CHUNK_SAMPLES * 2);
    let mut vad = EarshotVad::new();

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

        scratch.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut scratch);

        // Drain any additional queued buffers to minimize frame drops
        // during long inference passes. Check cancel between drains.
        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("transcription cancelled, worker exiting");
                return Ok(());
            }
            if let TranscriptionInput::Samples(s) = extra {
                resampler::downsample_stereo_to_mono_16k(&s, &mut scratch);
            }
        }

        if scratch.is_empty() {
            continue;
        }

        // Feed the accumulated samples through the VAD. EarshotVad
        // handles partial-frame stitching internally so we can send
        // any length.
        vad.accept(&scratch);

        // Decode every segment the VAD has completed since the last
        // iteration. Each segment is a bounded utterance; Whisper no
        // longer sees arbitrary 5-second chunks of mixed speech and
        // silence.
        while let Some(mut segment) = vad.pop_segment() {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("transcription cancelled, worker exiting");
                return Ok(());
            }
            decode_and_emit_segment(
                &mut state,
                &mut segment,
                event_tx,
                noise_gate_ratio,
                audio_enhancement,
            );
        }
    }

    // Channel disconnected — force any in-flight segment out of the
    // VAD so the last utterance isn't silently dropped.
    vad.flush();
    while let Some(mut segment) = vad.pop_segment() {
        decode_and_emit_segment(
            &mut state,
            &mut segment,
            event_tx,
            noise_gate_ratio,
            audio_enhancement,
        );
    }

    tracing::info!("audio channel closed, worker exiting");
    Ok(())
}

/// Denoise a VAD-completed speech segment, run Whisper inference,
/// filter hallucinations, and emit the result on `event_tx` as a
/// `TranscriptionEvent::Text`. Extracted so the main audio loop and
/// the on-exit flush path share one implementation — they need
/// identical behavior but differ in what they do on error (the main
/// loop continues, the flush path returns either way).
fn decode_and_emit_segment(
    state: &mut whisper_rs::WhisperState,
    segment: &mut [f32],
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
) {
    // Audio enhancement dispatcher (#281) — routes to
    // `enhance_speech` (default voice-band), `spectral_denoise`
    // (broadband), or no-op (Off) based on the user-selected
    // mode threaded through from `BackendConfig`. Runs per
    // segment rather than per chunk now that segmentation lives
    // upstream.
    denoise::apply(segment, audio_enhancement, noise_gate_ratio);

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_context(true);

    if let Err(e) = state.full(params, segment) {
        tracing::warn!("whisper inference failed: {e}");
        return;
    }

    let n_segments = state.full_n_segments();
    let mut combined = String::new();

    for i in 0..n_segments {
        if let Some(seg) = state.get_segment(i)
            && let Ok(text) = seg.to_str()
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
        tracing::debug!(%timestamp, %combined, "transcribed segment");
        let _ = event_tx.send(TranscriptionEvent::Text {
            timestamp,
            text: combined,
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
