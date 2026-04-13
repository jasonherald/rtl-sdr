//! Sherpa-onnx host worker thread — owns the recognizer for the
//! entire process lifetime.
//!
//! Spawned from `main()` before GTK init via [`init_sherpa_host`].
//! The worker populates the process-wide `SHERPA_HOST` `OnceLock`
//! then sits on a command channel waiting for session requests.

use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use sherpa_onnx::OnlineRecognizer;

use crate::backend::{BackendError, TranscriptionEvent};
use crate::init_event::InitEvent;
use crate::sherpa_model::{self, SherpaModel};

use super::streaming;

/// Bounded channel capacity for audio buffers from DSP → backend.
pub(super) const AUDIO_CHANNEL_CAPACITY: usize = 256;

/// Polling interval for the audio receive loop when checking for cancellation.
pub(super) const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Sample rate sherpa-onnx expects from `accept_waveform`.
pub(super) const SHERPA_SAMPLE_RATE_HZ: i32 = 16_000;

/// Process-wide singleton for the sherpa-onnx host. Stores either a ready
/// host or the error message from a failed initialization. Set exactly once
/// by [`init_sherpa_host`]; subsequent calls are no-ops.
static SHERPA_HOST: OnceLock<Result<SherpaHost, Arc<BackendError>>> = OnceLock::new();

/// Spawn the global sherpa-onnx host thread and return a channel that
/// streams initialization progress events.
///
/// **MUST be called from `main()` BEFORE GTK is initialized** (before
/// `sdr_ui::run()`). The returned `Receiver<InitEvent>` MUST be drained
/// by the caller until it produces either `InitEvent::Ready` or
/// `InitEvent::Failed` — the worker populates the global `SHERPA_HOST`
/// `OnceLock` itself before emitting the final event, but `main()` needs
/// to block until that's done so the recognizer creation completes
/// before GTK loads.
pub fn init_sherpa_host(model: SherpaModel) -> mpsc::Receiver<InitEvent> {
    SherpaHost::spawn(model)
}

/// Look up the global sherpa host. Returns `None` if `init_sherpa_host` was
/// never called.
pub(super) fn global_sherpa_host() -> Option<&'static Result<SherpaHost, Arc<BackendError>>> {
    SHERPA_HOST.get()
}

/// Parameters handed to the host worker for one transcription session.
pub(super) struct SessionParams {
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub audio_rx: mpsc::Receiver<Vec<f32>>,
    pub event_tx: mpsc::Sender<TranscriptionEvent>,
    pub noise_gate_ratio: f32,
}

/// Commands sent to the host worker thread.
enum HostCommand {
    StartSession(SessionParams),
}

/// Internal state of a sherpa host. Wrapped in a `Mutex` inside `SherpaHost`
/// because `mpsc::Sender` is `!Sync` and we need `SherpaHost: Sync` for
/// `OnceLock` storage.
struct SherpaHostState {
    cmd_tx: mpsc::Sender<HostCommand>,
}

/// Long-lived host for sherpa-onnx transcription. Owns one worker thread
/// that holds the [`OnlineRecognizer`] for the entire process lifetime.
pub(super) struct SherpaHost {
    state: Mutex<SherpaHostState>,
}

impl SherpaHost {
    /// Spawn the host worker thread and return immediately.
    pub(super) fn spawn(model: SherpaModel) -> mpsc::Receiver<InitEvent> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HostCommand>();
        let (event_tx, event_rx) = mpsc::channel::<InitEvent>();

        std::thread::Builder::new()
            .name("sherpa-host".into())
            .spawn(move || {
                run_host_loop(model, &cmd_rx, cmd_tx, event_tx);
            })
            .expect("failed to spawn sherpa-host worker thread");

        event_rx
    }

    /// Send a `StartSession` command to the host.
    pub(super) fn start_session(&self, params: SessionParams) -> Result<(), BackendError> {
        let state = self
            .state
            .lock()
            .map_err(|_| BackendError::Init("sherpa host mutex poisoned".to_owned()))?;
        state
            .cmd_tx
            .send(HostCommand::StartSession(params))
            .map_err(|_| BackendError::Init("sherpa host worker is no longer running".to_owned()))
    }
}

/// Worker thread entry point. Owns the recognizer for the entire
/// process lifetime and handles both initialization and command
/// processing.
fn run_host_loop(
    model: SherpaModel,
    cmd_rx: &mpsc::Receiver<HostCommand>,
    cmd_tx: mpsc::Sender<HostCommand>,
    event_tx: mpsc::Sender<InitEvent>,
) {
    // --- Phase 1: download if needed ---
    if !sherpa_model::model_exists(model) {
        tracing::info!(
            ?model,
            "sherpa model not found locally, downloading bundle (~256 MB)"
        );
        let _ = event_tx.send(InitEvent::DownloadStart {
            component: model.label(),
        });

        let (dl_tx, dl_rx) = mpsc::channel::<u8>();
        let event_tx_dl = event_tx.clone();

        let dl_forwarder = match std::thread::Builder::new()
            .name("sherpa-dl-progress".into())
            .spawn(move || {
                while let Ok(pct) = dl_rx.recv() {
                    let _ = event_tx_dl.send(InitEvent::DownloadProgress { pct });
                }
            }) {
            Ok(handle) => handle,
            Err(e) => {
                let msg = format!("failed to spawn sherpa-dl-progress thread: {e}");
                tracing::error!(%msg);
                store_init_failure(BackendError::Init(msg.clone()));
                let _ = event_tx.send(InitEvent::Failed { message: msg });
                return;
            }
        };

        let archive_result = sherpa_model::download_sherpa_archive(model, &dl_tx);
        drop(dl_tx);
        let _ = dl_forwarder.join();

        let archive_path = match archive_result {
            Ok(path) => path,
            Err(e) => {
                let msg = format!("sherpa model download failed: {e}");
                tracing::error!(%msg);
                store_init_failure(BackendError::Init(msg.clone()));
                let _ = event_tx.send(InitEvent::Failed { message: msg });
                return;
            }
        };

        tracing::info!("sherpa archive download complete, extracting");
        let _ = event_tx.send(InitEvent::Extracting {
            component: model.label(),
        });

        if let Err(e) = sherpa_model::extract_sherpa_archive(model, &archive_path) {
            let msg = format!("sherpa model extraction failed: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return;
        }

        tracing::info!("sherpa model installed, proceeding to recognizer init");
    }

    // --- Phase 2: create the recognizer ---
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = streaming::build_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx recognizer (host init)");

    let Some(recognizer) = OnlineRecognizer::create(&recognizer_config) else {
        let msg = "OnlineRecognizer::create returned None — check model file paths".to_owned();
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return;
    };
    tracing::info!("sherpa-onnx recognizer created successfully");

    // --- Phase 3: build SherpaHost and store in SHERPA_HOST ---
    let host = SherpaHost {
        state: Mutex::new(SherpaHostState { cmd_tx }),
    };
    if SHERPA_HOST.set(Ok(host)).is_err() {
        let msg = "sherpa host OnceLock was already set; this worker is unreachable".to_owned();
        tracing::error!(%msg);
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return;
    }
    tracing::info!("sherpa-host ready, signaling Ready event");
    let _ = event_tx.send(InitEvent::Ready);
    drop(event_tx);

    // --- Phase 4: command loop ---
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            HostCommand::StartSession(params) => {
                tracing::info!("sherpa-host: starting session");
                streaming::run_session(&recognizer, params);
                tracing::info!("sherpa-host: session ended");
            }
        }
    }
    tracing::info!("sherpa-host worker exiting");
}

/// Helper to store an initialization failure in the global `OnceLock`.
fn store_init_failure(err: BackendError) {
    let _ = SHERPA_HOST.set(Err(std::sync::Arc::new(err)));
}
