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

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, TranscriptionBackend,
};

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

    fn start(&mut self, _config: BackendConfig) -> Result<BackendHandle, BackendError> {
        // Skeleton — real implementation lands in Task 7.
        Err(BackendError::Init(
            "SherpaBackend is not yet implemented (Task 7)".to_owned(),
        ))
    }

    fn stop(&mut self) {
        self.shutdown_nonblocking();
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.worker.take(); // detach
        tracing::info!("sherpa backend shutdown (non-blocking)");
    }
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
}
