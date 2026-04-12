//! Live audio transcription.
//!
//! Provides [`TranscriptionEngine`] — a backend-agnostic façade over
//! [`backend::TranscriptionBackend`] implementations. The engine owns
//! one backend at a time and delegates lifecycle to it.
//!
//! Two backends are currently implemented: [`backends::whisper::WhisperBackend`]
//! (file-based, chunked inference via whisper-rs) and
//! [`backends::sherpa::SherpaBackend`] (true streaming via sherpa-onnx).
//!
//! The `whisper` and `sherpa` cargo features are mutually exclusive. Exactly
//! one must be enabled at build time (see the `compile_error` guards below).

#[cfg(all(feature = "whisper", feature = "sherpa"))]
compile_error!(
    "features `whisper` and `sherpa` are mutually exclusive. \
     Pick exactly one. For Whisper: `--features cuda` (or hipblas/vulkan/metal/etc) \
     or `--features whisper`. For Sherpa: `--no-default-features --features sherpa`."
);

#[cfg(not(any(feature = "whisper", feature = "sherpa")))]
compile_error!(
    "exactly one of the `whisper` or `sherpa` features must be enabled. \
     The default is `whisper`. For Sherpa: `--no-default-features --features sherpa`."
);

pub mod backend;
pub mod backends;
pub mod denoise;
pub mod resampler;

#[cfg(feature = "whisper")]
pub mod model;

#[cfg(feature = "sherpa")]
pub mod sherpa_model;

pub use backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};

#[cfg(feature = "whisper")]
pub use model::WhisperModel;

#[cfg(feature = "sherpa")]
pub use backends::sherpa::init_sherpa_host;

#[cfg(feature = "sherpa")]
pub use sherpa_model::SherpaModel;

use std::sync::mpsc;

#[cfg(feature = "whisper")]
use crate::backends::whisper::WhisperBackend;

#[cfg(feature = "sherpa")]
use crate::backends::sherpa::SherpaBackend;

/// Error type for engine-level operations.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("transcription is already running")]
    AlreadyRunning,
    #[error("transcription is not running")]
    NotRunning,
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Backend-agnostic live transcription engine.
///
/// Holds one [`TranscriptionBackend`] at a time. The public API matches
/// the pre-refactor `TranscriptionEngine` so existing call sites in
/// `sdr-ui` need no changes.
pub struct TranscriptionEngine {
    backend: Option<Box<dyn TranscriptionBackend>>,
    audio_tx: Option<mpsc::SyncSender<Vec<f32>>>,
}

impl Default for TranscriptionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptionEngine {
    pub fn new() -> Self {
        Self {
            backend: None,
            audio_tx: None,
        }
    }

    /// Start a transcription backend selected by `config.model`.
    ///
    /// Constructs the right backend (Whisper or Sherpa) for the chosen
    /// model and returns a receiver for [`TranscriptionEvent`].
    pub fn start(
        &mut self,
        config: BackendConfig,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        let backend: Box<dyn TranscriptionBackend> = match config.model {
            #[cfg(feature = "whisper")]
            ModelChoice::Whisper(_) => Box::new(WhisperBackend::new()),
            #[cfg(feature = "sherpa")]
            ModelChoice::Sherpa(_) => Box::new(SherpaBackend::new()),
        };
        self.start_with_backend(backend, config)
    }

    /// Start the engine with a caller-provided backend.
    ///
    /// Used internally by [`Self::start`] and by unit tests that want to
    /// inject a mock backend. Will become `pub` in PR 2 once the UI can
    /// pick a backend.
    pub(crate) fn start_with_backend(
        &mut self,
        mut backend: Box<dyn TranscriptionBackend>,
        config: BackendConfig,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        if self.backend.is_some() {
            return Err(TranscriptionError::AlreadyRunning);
        }

        let BackendHandle { audio_tx, event_rx } = backend.start(config)?;
        self.audio_tx = Some(audio_tx);
        self.backend = Some(backend);

        tracing::info!("transcription engine started");
        Ok(event_rx)
    }

    /// Stop the engine, blocking until the backend's worker has finished.
    ///
    /// May block for the duration of one inference pass. Use
    /// [`Self::shutdown_nonblocking`] from the UI thread or during app exit.
    pub fn stop(&mut self) {
        self.audio_tx.take();
        if let Some(mut backend) = self.backend.take() {
            backend.stop();
            tracing::info!("transcription engine stopped");
        }
    }

    /// Signal the backend to shut down without waiting.
    ///
    /// Drops the audio sender so the worker exits after its current
    /// inference completes; detaches the thread so the caller never blocks.
    pub fn shutdown_nonblocking(&mut self) {
        self.audio_tx.take();
        if let Some(mut backend) = self.backend.take() {
            backend.shutdown_nonblocking();
            tracing::info!("transcription engine stopped");
        }
    }

    /// Get a clone of the audio sender for feeding samples from the DSP thread.
    pub fn audio_sender(&self) -> Option<mpsc::SyncSender<Vec<f32>>> {
        self.audio_tx.clone()
    }

    /// True if the engine has an active backend.
    pub fn is_running(&self) -> bool {
        self.backend.is_some()
    }

    /// True if the active backend can emit partial hypotheses.
    /// Returns `false` if no backend is running.
    pub fn supports_partials(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.supports_partials())
    }
}

impl Drop for TranscriptionEngine {
    fn drop(&mut self) {
        self.shutdown_nonblocking();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::mock::MockBackend;
    use std::sync::atomic::Ordering;

    fn dummy_config() -> BackendConfig {
        #[cfg(feature = "whisper")]
        let model = ModelChoice::Whisper(WhisperModel::TinyEn);
        #[cfg(feature = "sherpa")]
        let model = ModelChoice::Sherpa(crate::SherpaModel::StreamingZipformerEn);
        BackendConfig {
            model,
            silence_threshold: 0.007,
            noise_gate_ratio: 3.0,
        }
    }

    #[test]
    fn engine_new_is_not_running() {
        let engine = TranscriptionEngine::new();
        assert!(!engine.is_running());
        assert!(engine.audio_sender().is_none());
        assert!(!engine.supports_partials());
    }

    #[test]
    fn engine_starts_with_mock_backend() {
        let mut engine = TranscriptionEngine::new();
        let backend = Box::new(MockBackend::new());
        let state = backend.state();

        let _event_rx = engine
            .start_with_backend(backend, dummy_config())
            .expect("start should succeed");

        assert!(engine.is_running());
        assert!(engine.audio_sender().is_some());
        assert_eq!(state.start_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn engine_double_start_returns_already_running() {
        let mut engine = TranscriptionEngine::new();
        let backend1 = Box::new(MockBackend::new());
        engine
            .start_with_backend(backend1, dummy_config())
            .expect("first start ok");

        let backend2 = Box::new(MockBackend::new());
        let err = engine
            .start_with_backend(backend2, dummy_config())
            .expect_err("second start should fail");
        assert!(matches!(err, TranscriptionError::AlreadyRunning));
    }

    #[test]
    fn engine_shutdown_clears_state() {
        let mut engine = TranscriptionEngine::new();
        let backend = Box::new(MockBackend::new());
        let state = backend.state();

        engine
            .start_with_backend(backend, dummy_config())
            .expect("start ok");

        engine.shutdown_nonblocking();

        assert!(!engine.is_running());
        assert!(engine.audio_sender().is_none());
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn engine_supports_partials_reflects_backend() {
        let mut engine = TranscriptionEngine::new();
        let backend = Box::new(MockBackend::new().with_supports_partials(true));

        engine
            .start_with_backend(backend, dummy_config())
            .expect("start ok");

        assert!(engine.supports_partials());
    }

    #[test]
    fn engine_drop_runs_shutdown() {
        let backend = Box::new(MockBackend::new());
        let state = backend.state();
        {
            let mut engine = TranscriptionEngine::new();
            engine
                .start_with_backend(backend, dummy_config())
                .expect("start ok");
        }
        // Engine dropped here.
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn engine_stop_clears_state() {
        let mut engine = TranscriptionEngine::new();
        let backend = Box::new(MockBackend::new());
        let state = backend.state();

        engine
            .start_with_backend(backend, dummy_config())
            .expect("start ok");

        engine.stop();

        assert!(!engine.is_running());
        assert!(engine.audio_sender().is_none());
        // Mock inherits the default stop() impl which delegates to
        // shutdown_nonblocking, so the shutdown counter should fire.
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 1);
    }
}
