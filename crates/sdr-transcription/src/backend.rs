//! Backend abstraction for the transcription engine.
//!
//! `TranscriptionBackend` is the trait every ASR implementation must satisfy.
//! The engine owns one backend at a time and delegates lifecycle to it.
//! This file defines the trait, the handle returned by `start`, the config
//! passed in, and the event type emitted to consumers.

use std::path::PathBuf;
use std::sync::mpsc;

use crate::model::WhisperModel;

/// Configuration handed to a backend at `start` time.
///
/// `model` selects which ASR model the backend should load. Additional
/// fields are preprocessing parameters shared across all backends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackendConfig {
    pub model: ModelChoice,
    pub silence_threshold: f32,
    pub noise_gate_ratio: f32,
}

/// User-facing model selection.
///
/// The variant determines which backend the engine instantiates internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelChoice {
    Whisper(WhisperModel),
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
    /// Push 48 kHz interleaved stereo f32 samples into the backend.
    pub audio_tx: mpsc::SyncSender<Vec<f32>>,
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
