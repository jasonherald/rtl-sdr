//! Mock backend for unit-testing the transcription engine.
//!
//! Records lifecycle calls and lets tests push events into the channel
//! the engine hands out to its consumer. Construct via `MockBackend::new`,
//! optionally configure `supports_partials` for testing UI gating logic.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, TranscriptionBackend, TranscriptionEvent,
};

/// Records what the engine did to a backend. Cloneable handle so tests
/// can inspect state after the engine drops the backend.
#[derive(Clone, Default)]
pub struct MockState {
    pub start_count: Arc<AtomicUsize>,
    pub shutdown_count: Arc<AtomicUsize>,
    pub last_event_tx: Arc<Mutex<Option<mpsc::Sender<TranscriptionEvent>>>>,
    pub supports_partials_value: Arc<AtomicBool>,
}

pub struct MockBackend {
    state: MockState,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            state: MockState::default(),
        }
    }

    /// Get a clone of the state for inspection in tests.
    pub fn state(&self) -> MockState {
        self.state.clone()
    }

    /// Configure what `supports_partials` returns.
    #[must_use]
    pub fn with_supports_partials(self, value: bool) -> Self {
        self.state
            .supports_partials_value
            .store(value, Ordering::Relaxed);
        self
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptionBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn supports_partials(&self) -> bool {
        self.state.supports_partials_value.load(Ordering::Relaxed)
    }

    fn start(&mut self, _config: BackendConfig) -> Result<BackendHandle, BackendError> {
        self.state.start_count.fetch_add(1, Ordering::Relaxed);

        let (audio_tx, _audio_rx) = mpsc::sync_channel(8);
        let (event_tx, event_rx) = mpsc::channel();

        // Stash the event_tx so tests can push events through it.
        *self
            .state
            .last_event_tx
            .lock()
            .expect("mock state poisoned") = Some(event_tx);

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn shutdown_nonblocking(&mut self) {
        self.state.shutdown_count.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ModelChoice;
    use crate::model::WhisperModel;

    fn dummy_config() -> BackendConfig {
        BackendConfig {
            model: ModelChoice::Whisper(WhisperModel::TinyEn),
            silence_threshold: 0.007,
            noise_gate_ratio: 3.0,
        }
    }

    #[test]
    fn mock_records_start_and_shutdown() {
        let mut backend = MockBackend::new();
        let state = backend.state();

        assert_eq!(state.start_count.load(Ordering::Relaxed), 0);
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 0);

        let _handle = backend.start(dummy_config()).expect("start should succeed");
        assert_eq!(state.start_count.load(Ordering::Relaxed), 1);

        backend.shutdown_nonblocking();
        assert_eq!(state.shutdown_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn mock_partials_default_false() {
        let backend = MockBackend::new();
        assert!(!backend.supports_partials());
    }

    #[test]
    fn mock_partials_can_be_configured() {
        let backend = MockBackend::new().with_supports_partials(true);
        assert!(backend.supports_partials());
    }

    #[test]
    fn mock_can_push_events_through_handle() {
        let mut backend = MockBackend::new();
        let state = backend.state();

        let handle = backend.start(dummy_config()).expect("start should succeed");

        // Test pushes an event through the stashed sender; the handle's
        // receiver should see it.
        let tx = state
            .last_event_tx
            .lock()
            .expect("mock state poisoned")
            .clone()
            .expect("event_tx should be stashed after start");
        tx.send(TranscriptionEvent::Ready).expect("send Ready");

        let received = handle.event_rx.recv().expect("recv Ready");
        assert!(matches!(received, TranscriptionEvent::Ready));
    }
}
