//! Sherpa-onnx-backed Silero VAD wrapper implementing the
//! feature-agnostic [`VoiceActivityDetector`] trait.
//!
//! The underlying `sherpa_onnx::VoiceActivityDetector` is a queue-based
//! detector: you feed audio via `accept_waveform`, it buffers internally
//! and queues completed speech segments, and you pull them via
//! `front`/`pop` until `is_empty` returns true.
//!
//! This adapter flattens that queue-based API into the trait's
//! `accept` + `pop_segment` pattern so callers can write a simple
//! `while let Some(segment) = vad.pop_segment() { decode(segment) }`
//! loop.

use std::path::Path;

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector as SherpaVad};

use crate::backend::BackendError;
use crate::vad::VoiceActivityDetector;

use super::host::SHERPA_SAMPLE_RATE_HZ;

// Silero VAD default hyperparameters. These match the sherpa-onnx
// upstream `moonshine_v2.rs` example and are appropriate for radio
// audio (short bursts, occasional long silences).
const SILERO_MIN_SILENCE_DURATION: f32 = 0.25;
const SILERO_MIN_SPEECH_DURATION: f32 = 0.25;
const SILERO_MAX_SPEECH_DURATION: f32 = 20.0;
const SILERO_WINDOW_SIZE: i32 = 512;

/// Internal buffer size for the detector, in seconds of audio.
/// 30 seconds is well above `SILERO_MAX_SPEECH_DURATION`, giving
/// the detector plenty of headroom even on the longest permitted
/// utterance.
const VAD_BUFFER_SIZE_SECONDS: f32 = 30.0;

/// Sherpa-onnx-backed Silero VAD.
pub struct SherpaSileroVad {
    inner: SherpaVad,
    /// The threshold this VAD was created with. Used by the host worker to
    /// detect when a new session requests a different threshold so it can
    /// rebuild the VAD before starting.
    current_threshold: f32,
}

impl SherpaSileroVad {
    /// Create a new Silero VAD using the ONNX file at `model_path` and
    /// the given `threshold` (0.10..=0.90).
    ///
    /// Use `crate::backend::VAD_THRESHOLD_DEFAULT` as `threshold` for
    /// the default value. The file is typically installed by
    /// [`crate::sherpa_model::download_silero_vad`].
    pub fn new(model_path: &Path, threshold: f32) -> Result<Self, BackendError> {
        let silero_config = SileroVadModelConfig {
            model: Some(model_path.to_string_lossy().into_owned()),
            threshold,
            min_silence_duration: SILERO_MIN_SILENCE_DURATION,
            min_speech_duration: SILERO_MIN_SPEECH_DURATION,
            max_speech_duration: SILERO_MAX_SPEECH_DURATION,
            window_size: SILERO_WINDOW_SIZE,
        };

        // Silero VAD is ~2 MB and per-chunk inference is trivial; always
        // run it on CPU regardless of `SHERPA_PROVIDER`. Sending every
        // 32 ms window across the PCIe bus to the GPU would cost more
        // than the compute itself, and it avoids cross-provider
        // onnxruntime state that we don't otherwise need.
        let vad_config = VadModelConfig {
            silero_vad: silero_config,
            sample_rate: SHERPA_SAMPLE_RATE_HZ,
            num_threads: 1,
            provider: Some("cpu".to_owned()),
            debug: false,
            ..Default::default()
        };

        let inner = SherpaVad::create(&vad_config, VAD_BUFFER_SIZE_SECONDS).ok_or_else(|| {
            BackendError::Init(format!(
                "Silero VAD creation failed — check model at {}",
                model_path.display()
            ))
        })?;

        Ok(Self {
            inner,
            current_threshold: threshold,
        })
    }

    /// The threshold this VAD was built with.
    pub fn current_threshold(&self) -> f32 {
        self.current_threshold
    }
}

impl VoiceActivityDetector for SherpaSileroVad {
    fn accept(&mut self, samples: &[f32]) {
        self.inner.accept_waveform(samples);
    }

    fn pop_segment(&mut self) -> Option<Vec<f32>> {
        // front() returns None when the queue is empty, so a single
        // call handles both the is_empty check and the borrow. Clone
        // the samples so the returned segment is owned, then pop to
        // advance the queue.
        let segment = self.inner.front()?;
        let samples = segment.samples().to_vec();
        self.inner.pop();
        Some(samples)
    }

    fn flush(&mut self) {
        // Forces Silero to finalize whatever partial utterance it was
        // still evaluating so the next `pop_segment` can return it.
        self.inner.flush();
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}
