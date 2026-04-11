//! Live audio transcription via Whisper.
//!
//! Provides `TranscriptionEngine` that runs a background worker thread for
//! speech-to-text. Audio samples are fed from the DSP thread via a bounded
//! channel; transcription results are returned via an event channel.

pub mod denoise;
pub mod model;
pub mod resampler;
pub mod worker;

pub use model::WhisperModel;
pub use worker::TranscriptionEvent;

use std::sync::mpsc;

/// Bounded channel capacity for audio buffers from DSP → transcription.
/// Each buffer is ~1024-4096 stereo samples (~20-80ms). At 48 kHz with
/// 5-second inference chunks, we need ~250 buffers to avoid drops during
/// a single inference pass. 512 gives comfortable headroom.
const AUDIO_CHANNEL_CAPACITY: usize = 512;

/// Error type for transcription operations.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("transcription is already running")]
    AlreadyRunning,
    #[error("transcription is not running")]
    NotRunning,
    #[error("failed to spawn worker thread: {0}")]
    Spawn(#[from] std::io::Error),
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

    /// Start the transcription worker thread with the given model.
    /// Returns a receiver for `TranscriptionEvent`.
    ///
    /// # Arguments
    /// * `whisper_model` — which Whisper model to load
    /// * `silence_threshold` — RMS below which a chunk is skipped
    /// * `noise_gate_ratio` — spectral gate multiplier over noise floor
    pub fn start(
        &mut self,
        whisper_model: WhisperModel,
        silence_threshold: f32,
        noise_gate_ratio: f32,
    ) -> Result<mpsc::Receiver<TranscriptionEvent>, TranscriptionError> {
        if self.worker_thread.is_some() {
            return Err(TranscriptionError::AlreadyRunning);
        }

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("transcription-worker".into())
            .spawn(move || {
                worker::run_worker(
                    &audio_rx,
                    &event_tx,
                    whisper_model,
                    silence_threshold,
                    noise_gate_ratio,
                );
            })?;

        self.audio_tx = Some(audio_tx);
        self.worker_thread = Some(handle);

        tracing::info!("transcription engine started");
        Ok(event_rx)
    }

    /// Stop the transcription worker, waiting for it to finish.
    ///
    /// This may block if Whisper inference is in progress. Use
    /// [`shutdown_nonblocking`] during app exit to avoid freezing the UI.
    pub fn stop(&mut self) {
        self.audio_tx.take();
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
        tracing::info!("transcription engine stopped");
    }

    /// Signal the worker to stop without waiting for it to finish.
    ///
    /// Drops the audio sender so the worker exits after its current
    /// inference completes. The thread is detached — the process can
    /// exit without joining it.
    pub fn shutdown_nonblocking(&mut self) {
        self.audio_tx.take();
        self.worker_thread.take(); // detach — don't join
        tracing::info!("transcription engine shutdown (non-blocking)");
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
        self.shutdown_nonblocking();
    }
}
