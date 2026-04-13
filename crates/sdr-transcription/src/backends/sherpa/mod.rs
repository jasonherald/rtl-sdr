//! Sherpa-onnx backend — streaming-native ASR via the official k2-fsa
//! `sherpa-onnx` Rust crate.
//!
//! ## Architecture
//!
//! The recognizer is created ONCE per process by [`host::SherpaHost`], a
//! long-lived worker thread spawned from `main()` BEFORE GTK is loaded.
//! This is a workaround for a C++ static-initializer collision between
//! sherpa-onnx's bundled ONNX Runtime and GTK4's transitive C++ deps —
//! creating the recognizer after GTK init causes `free(): invalid pointer`
//! inside `std::regex` constructors called by ONNX Runtime's
//! `ParseSemVerVersion`.
//!
//! [`SherpaBackend`] is a thin facade that asks the global host for a
//! new session. The host creates a fresh stream from the existing
//! recognizer and runs the audio feed loop until the session is
//! cancelled or the audio channel disconnects.
//!
//! ## Submodules
//!
//! - [`host`] owns the `SHERPA_HOST` `OnceLock`, the worker thread,
//!   and the init flow that downloads + creates the recognizer.
//! - [`streaming`] contains the `OnlineRecognizer` session loop used
//!   by Zipformer (and future transducer models like Parakeet).

mod host;
mod offline;
mod silero_vad;
mod streaming;

pub use host::init_sherpa_host;
pub use silero_vad::SherpaSileroVad;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
};
use host::{AUDIO_CHANNEL_CAPACITY, SessionParams, global_sherpa_host};

/// `TranscriptionBackend` implementation backed by the global sherpa host.
///
/// `SherpaBackend` is stateless apart from a per-session cancellation flag.
/// All actual recognizer state lives on the long-lived host worker thread
/// spawned by [`init_sherpa_host`].
pub struct SherpaBackend {
    cancel: Arc<AtomicBool>,
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

    fn start(&mut self, config: BackendConfig) -> Result<BackendHandle, BackendError> {
        match config.model {
            ModelChoice::Sherpa(_) => {}
            #[cfg(feature = "whisper")]
            ModelChoice::Whisper(_) => return Err(BackendError::WrongModelKind),
        }

        let host = match global_sherpa_host() {
            Some(Ok(h)) => h,
            Some(Err(stored)) => {
                return Err(match &**stored {
                    BackendError::ModelNotFound { path } => {
                        BackendError::ModelNotFound { path: path.clone() }
                    }
                    BackendError::Init(msg) => {
                        BackendError::Init(format!("sherpa host failed to initialize: {msg}"))
                    }
                    BackendError::Spawn(io_err) => BackendError::Init(format!(
                        "sherpa host worker thread spawn failed: {io_err}"
                    )),
                    BackendError::WrongModelKind => BackendError::WrongModelKind,
                });
            }
            None => {
                return Err(BackendError::Init(
                    "sherpa host not initialized — main() must call \
                     sdr_transcription::init_sherpa_host before sdr_ui::run"
                        .to_owned(),
                ));
            }
        };

        self.cancel.store(false, Ordering::Relaxed);

        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel();

        host.start_session(SessionParams {
            cancel: Arc::clone(&self.cancel),
            audio_rx,
            event_tx,
            noise_gate_ratio: config.noise_gate_ratio,
        })?;

        tracing::info!("sherpa backend session requested");

        Ok(BackendHandle { audio_tx, event_rx })
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        tracing::info!("sherpa backend stopped");
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        tracing::info!("sherpa backend shutdown (non-blocking)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InitEvent;
    use crate::sherpa_model::{self, SherpaModel};

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

    /// **Manual-only test.** Kicks off the real worker — which, if the
    /// model files are absent, proceeds past the first `DownloadStart`
    /// event into the full 256 MB download + extract path. See the
    /// note on issues #250 and #255 about the hermetic testing follow-up.
    #[test]
    #[ignore = "spawns the real download worker; run manually with --ignored"]
    fn sherpa_host_spawn_emits_download_start_when_files_missing() {
        if sherpa_model::model_exists(SherpaModel::StreamingZipformerEn) {
            eprintln!("skipping test: streaming-zipformer-en model is present locally");
            return;
        }
        let event_rx = host::SherpaHost::spawn(SherpaModel::StreamingZipformerEn);
        let first_event = event_rx
            .recv()
            .expect("worker should send at least one event");
        assert!(
            matches!(first_event, InitEvent::DownloadStart { .. }),
            "expected DownloadStart when model is missing, got {first_event:?}"
        );
        drop(event_rx);
    }
}
