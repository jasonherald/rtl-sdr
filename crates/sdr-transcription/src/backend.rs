//! Backend abstraction for the transcription engine.
//!
//! `TranscriptionBackend` is the trait every ASR implementation must satisfy.
//! The engine owns one backend at a time and delegates lifecycle to it.
//! This file defines the trait, the handle returned by `start`, the config
//! passed in, and the event type emitted to consumers.

use std::path::PathBuf;
use std::sync::mpsc;

#[cfg(feature = "whisper")]
use crate::model::WhisperModel;

/// Minimum allowed value for [`BackendConfig::vad_threshold`].
pub const VAD_THRESHOLD_MIN: f32 = 0.10;

/// Maximum allowed value for [`BackendConfig::vad_threshold`].
pub const VAD_THRESHOLD_MAX: f32 = 0.90;

/// Default value for [`BackendConfig::vad_threshold`]. Matches
/// sherpa-onnx's upstream Silero VAD default and works well on clean
/// broadcast audio (WFM talk radio). Drop to ~0.25-0.30 for noisy
/// scanner/NFM sources.
pub const VAD_THRESHOLD_DEFAULT: f32 = 0.50;

/// Frames sent from the DSP controller into a transcription backend.
///
/// Carries both raw audio samples and segmentation-boundary hints. The
/// boundary variants are emitted by `sdr-core::controller` only when the
/// current demod mode is NFM — backends never need to gate on mode
/// themselves.
///
/// Backends that don't care about squelch-based segmentation (Whisper,
/// streaming Zipformer, offline sherpa in `SegmentationMode::Vad`)
/// pattern-match on `Samples` and drop the other variants.
#[derive(Debug, Clone)]
pub enum TranscriptionInput {
    /// Interleaved-stereo f32 PCM at 48 kHz. Always emitted, gap-free.
    Samples(Vec<f32>),

    /// Radio squelch just opened. Edge event, emitted exactly once per
    /// close→open transition. NFM demod only.
    SquelchOpened,

    /// Radio squelch just closed. Edge event, emitted exactly once per
    /// open→close transition. NFM demod only.
    SquelchClosed,
}

/// Which segmentation engine drives utterance boundaries for an offline
/// sherpa transcription session.
///
/// Mutex: exactly one is active per session. Streaming Zipformer always
/// uses `Vad` (its own endpoint detection handles the rest).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SegmentationMode {
    /// Silero VAD drives segmentation. Default for backward compatibility
    /// and the only valid mode for streaming Zipformer.
    #[default]
    Vad,

    /// Auto Break: the radio's squelch gate drives segmentation. Valid
    /// only for offline sherpa models on NFM demod. See the Auto Break
    /// state machine in `backends/sherpa/offline.rs`.
    AutoBreak,
}

/// Configuration handed to a backend at `start` time.
///
/// `model` selects which ASR model the backend should load. Additional
/// fields are preprocessing parameters shared across all backends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackendConfig {
    pub model: ModelChoice,
    pub silence_threshold: f32,
    pub noise_gate_ratio: f32,
    /// Silero VAD speech detection threshold (offline models only).
    /// Clamp to `VAD_THRESHOLD_MIN..=VAD_THRESHOLD_MAX`.
    /// Default `VAD_THRESHOLD_DEFAULT`. Lower catches quieter audio
    /// (NFM/scanner); higher is stricter (talk radio). Ignored by
    /// Whisper (no Silero VAD) and ignored when
    /// `segmentation_mode == SegmentationMode::AutoBreak`.
    pub vad_threshold: f32,
    /// How utterance boundaries are detected in an offline sherpa
    /// session. See `SegmentationMode` for valid values. Streaming
    /// Zipformer rejects `AutoBreak` at session start.
    pub segmentation_mode: SegmentationMode,
}

/// User-facing model selection.
///
/// The variant determines which backend the engine instantiates internally.
/// At any given build, exactly one variant exists — the `whisper` and
/// `sherpa` features are mutually exclusive (see `lib.rs` `compile_error`
/// guards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelChoice {
    #[cfg(feature = "whisper")]
    Whisper(WhisperModel),
    #[cfg(feature = "sherpa")]
    Sherpa(crate::sherpa_model::SherpaModel),
}

/// Events emitted by a backend during its lifecycle.
///
/// Variant names are stable — UI consumers match on them by name.
#[derive(Debug, Clone)]
pub enum TranscriptionEvent {
    /// Model download in progress (0..=100).
    Downloading { progress_pct: u8 },
    /// Model loaded and ready for inference.
    Ready,
    /// Incremental hypothesis from a streaming backend. May be replaced
    /// by another `Partial` before being committed as a `Text`. Backends
    /// that return `false` from `supports_partials()` never emit this.
    Partial { text: String },
    /// Transcribed text from one inference pass (or one committed
    /// streaming utterance).
    Text {
        /// Wall-clock timestamp in "HH:MM:SS" format.
        timestamp: String,
        /// Transcribed text (trimmed, non-empty).
        text: String,
    },
    /// Fatal error — backend will exit after sending this.
    Error(String),
}

/// Returned by [`TranscriptionBackend::start`]. Carries the channels the
/// engine wires through to its caller.
pub struct BackendHandle {
    /// Push audio frames + squelch edge events into the backend. See
    /// [`TranscriptionInput`] for the wire format.
    pub audio_tx: mpsc::SyncSender<TranscriptionInput>,
    /// Receive transcription events from the backend.
    pub event_rx: mpsc::Receiver<TranscriptionEvent>,
}

/// Errors a backend can return from `start`.
///
/// Mirrors `crate::TranscriptionError` so the engine can convert
/// transparently. Kept separate so backends don't depend on the engine.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("failed to spawn worker thread: {0}")]
    Spawn(#[from] std::io::Error),
    #[error(
        "model files not found at {path}; download the bundle and place its contents in this directory"
    )]
    ModelNotFound { path: PathBuf },
    // Retained for when whisper+sherpa are re-unified. In single-feature
    // builds the cfg-gated match arms below are never both present, so
    // this variant is never constructed — silence the lint.
    #[allow(dead_code)]
    #[error("backend received the wrong model kind in BackendConfig — engine bug")]
    WrongModelKind,
    #[error("backend initialization failed: {0}")]
    Init(String),
}

/// Trait every transcription backend must implement.
///
/// Backends own their own worker threads. The engine just holds a
/// `Box<dyn TranscriptionBackend>` and delegates lifecycle calls.
pub trait TranscriptionBackend: Send {
    /// Human-readable backend name (used for tracing/logging).
    fn name(&self) -> &'static str;

    /// True if this backend can emit incremental partial hypotheses.
    /// Whisper returns `false`; streaming backends return `true`.
    /// Used by the UI to enable/disable the "live captions" toggle.
    fn supports_partials(&self) -> bool;

    /// Spawn worker thread(s) and return channels for audio in / events out.
    ///
    /// Must emit [`TranscriptionEvent::Ready`] once the model is loaded.
    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError>;

    /// Stop the backend, waiting for any in-flight inference to finish.
    ///
    /// Default impl delegates to [`Self::shutdown_nonblocking`]; backends
    /// that own a worker thread should override this to join it. May block
    /// for the duration of one inference pass — do not call from the UI
    /// thread or during app exit. Use [`Self::shutdown_nonblocking`] there.
    fn stop(&mut self) {
        self.shutdown_nonblocking();
    }

    /// Signal the backend to stop without waiting for it to finish.
    ///
    /// The backend should set its cancellation flag and detach (not join)
    /// any worker threads so the caller never blocks. The backend does NOT
    /// own the [`BackendHandle::audio_tx`] returned from `start` — the
    /// caller (`TranscriptionEngine::shutdown_nonblocking`) drops that
    /// sender separately, which is what eventually causes the worker's
    /// `recv` to see `Disconnected` if the cancel flag hasn't already
    /// short-circuited the loop.
    fn shutdown_nonblocking(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcription_input_variants_construct() {
        assert!(matches!(
            TranscriptionInput::Samples(vec![0.0_f32; 16]),
            TranscriptionInput::Samples(_)
        ));
        assert!(matches!(
            TranscriptionInput::SquelchOpened,
            TranscriptionInput::SquelchOpened
        ));
        assert!(matches!(
            TranscriptionInput::SquelchClosed,
            TranscriptionInput::SquelchClosed
        ));
    }

    #[test]
    fn segmentation_mode_default_is_vad() {
        assert_eq!(SegmentationMode::default(), SegmentationMode::Vad);
    }
}
