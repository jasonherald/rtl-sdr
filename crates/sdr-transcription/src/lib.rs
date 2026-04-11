//! Live audio transcription via Whisper.
//!
//! Provides `TranscriptionEngine` that runs a background worker thread for
//! speech-to-text. Audio samples are fed from the DSP thread via a bounded
//! channel; transcription results are returned via an event channel.

pub mod model;
pub mod resampler;
pub mod worker;

pub use worker::TranscriptionEvent;

use std::sync::mpsc;

/// Bounded channel capacity for audio buffers from DSP → transcription.
const AUDIO_CHANNEL_CAPACITY: usize = 10;

/// Error type for transcription operations.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("transcription is already running")]
    AlreadyRunning,
    #[error("transcription is not running")]
    NotRunning,
}

/// Live audio transcription engine.
pub struct TranscriptionEngine {
    audio_tx: Option<mpsc::SyncSender<Vec<f32>>>,
    worker_thread: Option<std::thread::JoinHandle<()>>,
}

impl Default for TranscriptionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptionEngine {
    pub fn new() -> Self {
        Self {
            audio_tx: None,
            worker_thread: None,
        }
    }

    /// Start the transcription worker thread.
    /// Returns a receiver for `TranscriptionEvent`.
    pub fn start(&mut self) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        if self.worker_thread.is_some() {
            return Err(TranscriptionError::AlreadyRunning);
        }

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("transcription-worker".into())
            .spawn(move || {
                worker::run_worker(&audio_rx, &event_tx);
            })
            .expect("failed to spawn transcription worker thread");

        self.audio_tx = Some(audio_tx);
        self.worker_thread = Some(handle);

        tracing::info!("transcription engine started");
        Ok(event_rx)
    }

    /// Stop the transcription worker.
    pub fn stop(&mut self) {
        self.audio_tx.take();
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
        tracing::info!("transcription engine stopped");
    }

    /// Get a clone of the audio sender for feeding samples from the DSP thread.
    pub fn audio_sender(&self) -> Option<mpsc::SyncSender<Vec<f32>>> {
        self.audio_tx.clone()
    }

    /// Check if the engine is currently running.
    pub fn is_running(&self) -> bool {
        self.worker_thread.is_some()
    }
}

impl Drop for TranscriptionEngine {
    fn drop(&mut self) {
        self.stop();
    }
}
