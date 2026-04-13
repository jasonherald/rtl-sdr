//! Sherpa-onnx host worker thread — owns the recognizer for the
//! entire process lifetime.
//!
//! Spawned from `main()` before GTK init via [`init_sherpa_host`].
//! The worker populates the process-wide `SHERPA_HOST` `OnceLock`
//! then sits on a command channel waiting for session requests.

use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use sherpa_onnx::{OfflineRecognizer, OnlineRecognizer};

use crate::backend::{BackendError, TranscriptionEvent};
use crate::init_event::InitEvent;
use crate::sherpa_model::{self, SherpaModel};

use super::silero_vad::SherpaSileroVad;

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

/// Reload the sherpa-onnx host with a different model.
///
/// Returns a `Receiver<InitEvent>` that streams progress events the same
/// way `init_sherpa_host` does. The caller should drain it until it produces
/// either `InitEvent::Ready` or `InitEvent::Failed`.
///
/// Prerequisite: [`init_sherpa_host`] must have been called successfully
/// earlier in the process. Returns an error `InitEvent::Failed` via the
/// returned channel if the host was never initialized.
pub fn reload_sherpa_host(new_model: SherpaModel) -> mpsc::Receiver<InitEvent> {
    let (event_tx, event_rx) = mpsc::channel::<InitEvent>();

    let Some(stored) = SHERPA_HOST.get() else {
        let _ = event_tx.send(InitEvent::Failed {
            message: "sherpa host not initialized — cannot reload".to_owned(),
        });
        return event_rx;
    };

    let host = match stored {
        Ok(h) => h,
        Err(e) => {
            let _ = event_tx.send(InitEvent::Failed {
                message: format!("sherpa host is in a failed state: {e}"),
            });
            return event_rx;
        }
    };

    if let Err(e) = host.reload(new_model, event_tx.clone()) {
        let _ = event_tx.send(InitEvent::Failed {
            message: format!("failed to send reload command: {e}"),
        });
    }

    event_rx
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
    ReloadRecognizer {
        model: SherpaModel,
        event_tx: mpsc::Sender<InitEvent>,
    },
}

/// Which recognizer (and optional VAD) the host worker owns.
///
/// Set once in `run_host_loop` based on `SherpaModel::kind()`. The
/// command loop pattern-matches on this to dispatch to the right
/// session runner.
pub(super) enum RecognizerState {
    Online(OnlineRecognizer),
    Offline {
        recognizer: OfflineRecognizer,
        vad: SherpaSileroVad,
    },
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

    /// Request the host to drop its current recognizer and build a new one
    /// for `new_model`. Returns an error if the worker thread has died.
    pub(super) fn reload(
        &self,
        new_model: SherpaModel,
        event_tx: mpsc::Sender<InitEvent>,
    ) -> Result<(), BackendError> {
        let state = self
            .state
            .lock()
            .map_err(|_| BackendError::Init("sherpa host mutex poisoned".to_owned()))?;
        state
            .cmd_tx
            .send(HostCommand::ReloadRecognizer {
                model: new_model,
                event_tx,
            })
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
    use crate::sherpa_model::ModelKind;

    let recognizer_state = match model.kind() {
        ModelKind::OnlineTransducer => match init_online(model, &event_tx) {
            Ok(state) => state,
            Err(()) => return, // init_online already published Failed and stored the error
        },
        ModelKind::OfflineMoonshine => match init_offline(model, &event_tx) {
            Ok(state) => state,
            Err(()) => return,
        },
    };

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
    let mut recognizer_state = Some(recognizer_state);
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            HostCommand::StartSession(params) => {
                let Some(state) = recognizer_state.as_mut() else {
                    tracing::warn!("sherpa-host: StartSession rejected, no recognizer loaded");
                    let _ = params.event_tx.send(TranscriptionEvent::Error(
                        "no recognizer loaded — a previous model reload failed".to_owned(),
                    ));
                    continue;
                };
                tracing::info!("sherpa-host: starting session");
                match state {
                    RecognizerState::Online(recognizer) => {
                        super::streaming::run_session(recognizer, params);
                    }
                    RecognizerState::Offline { recognizer, vad } => {
                        super::offline::run_session(recognizer, vad, params);
                    }
                }
                tracing::info!("sherpa-host: session ended");
            }
            HostCommand::ReloadRecognizer {
                model: new_model,
                event_tx,
            } => {
                tracing::info!(?new_model, "sherpa-host: reloading recognizer");
                // Drop the old recognizer (and VAD if offline) BEFORE building
                // the new one — we can't hold two at once because they share
                // the ONNX Runtime singleton and memory budget.
                recognizer_state = None;

                let new_state = match new_model.kind() {
                    crate::sherpa_model::ModelKind::OnlineTransducer => {
                        init_online(new_model, &event_tx)
                    }
                    crate::sherpa_model::ModelKind::OfflineMoonshine => {
                        init_offline(new_model, &event_tx)
                    }
                };

                match new_state {
                    Ok(state) => {
                        tracing::info!(?new_model, "sherpa-host: reload complete");
                        recognizer_state = Some(state);
                        let _ = event_tx.send(InitEvent::Ready);
                    }
                    Err(()) => {
                        // init_online/init_offline already emitted Failed through
                        // event_tx AND stored the error in SHERPA_HOST (via
                        // store_init_failure). Leave recognizer_state as None so
                        // the next StartSession returns the error above.
                        tracing::warn!(?new_model, "sherpa-host: reload failed");
                    }
                }
            }
        }
    }
    tracing::info!("sherpa-host worker exiting");
}

/// Phase 1-2 for the `OnlineTransducer` path: download the bundle if
/// needed, then create the `OnlineRecognizer`. Returns `Err(())` on
/// any failure — the error has already been stored in `SHERPA_HOST` and
/// emitted as `InitEvent::Failed`.
fn init_online(
    model: SherpaModel,
    event_tx: &mpsc::Sender<InitEvent>,
) -> Result<RecognizerState, ()> {
    if !sherpa_model::model_exists(model)
        && !download_and_extract_bundle(model, event_tx, model.label())
    {
        return Err(());
    }

    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = super::streaming::build_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx OnlineRecognizer");

    let Some(recognizer) = OnlineRecognizer::create(&recognizer_config) else {
        let msg = "OnlineRecognizer::create returned None — check model file paths".to_owned();
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return Err(());
    };
    tracing::info!("OnlineRecognizer created successfully");
    Ok(RecognizerState::Online(recognizer))
}

/// Phase 1-2 for the `OfflineMoonshine` path: download the Silero VAD
/// model if missing, download the Moonshine bundle if missing, then
/// create the `OfflineRecognizer` + `SherpaSileroVad`. Returns `Err(())`
/// on any failure — the error has already been stored in `SHERPA_HOST`
/// and emitted as `InitEvent::Failed`.
fn init_offline(
    model: SherpaModel,
    event_tx: &mpsc::Sender<InitEvent>,
) -> Result<RecognizerState, ()> {
    // --- Silero VAD ---
    if !sherpa_model::silero_vad_exists() {
        tracing::info!("silero VAD not found locally, downloading");
        if !download_silero_vad_with_progress(event_tx) {
            return Err(());
        }
    }

    // --- Moonshine model bundle ---
    if !sherpa_model::model_exists(model)
        && !download_and_extract_bundle(model, event_tx, model.label())
    {
        return Err(());
    }

    // --- Build OfflineRecognizer ---
    let _ = event_tx.send(InitEvent::CreatingRecognizer);
    let recognizer_config = super::offline::build_moonshine_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx OfflineRecognizer (Moonshine)");

    let Some(recognizer) = OfflineRecognizer::create(&recognizer_config) else {
        let msg =
            "OfflineRecognizer::create returned None — check Moonshine model files".to_owned();
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return Err(());
    };
    tracing::info!("OfflineRecognizer created successfully");

    // --- Build SherpaSileroVad ---
    let vad_path = sherpa_model::silero_vad_path();
    let vad = match SherpaSileroVad::new(&vad_path) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("Silero VAD creation failed: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return Err(());
        }
    };
    tracing::info!("SherpaSileroVad created successfully");

    Ok(RecognizerState::Offline { recognizer, vad })
}

/// Helper to store an initialization failure in the global `OnceLock`.
fn store_init_failure(err: BackendError) {
    let _ = SHERPA_HOST.set(Err(std::sync::Arc::new(err)));
}

/// Download + extract a sherpa model bundle. Returns `false` on any
/// failure (error already stored + `InitEvent::Failed` emitted).
fn download_and_extract_bundle(
    model: SherpaModel,
    event_tx: &mpsc::Sender<InitEvent>,
    component: &'static str,
) -> bool {
    tracing::info!(?model, "sherpa model bundle not found locally, downloading");
    let _ = event_tx.send(InitEvent::DownloadStart { component });

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
            return false;
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
            return false;
        }
    };

    tracing::info!("sherpa archive download complete, extracting");
    let _ = event_tx.send(InitEvent::Extracting { component });

    if let Err(e) = sherpa_model::extract_sherpa_archive(model, &archive_path) {
        let msg = format!("sherpa model extraction failed: {e}");
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return false;
    }

    tracing::info!("sherpa model bundle installed");
    true
}

/// Download the Silero VAD ONNX file. Returns `false` on any failure
/// (error already stored + `InitEvent::Failed` emitted).
fn download_silero_vad_with_progress(event_tx: &mpsc::Sender<InitEvent>) -> bool {
    const VAD_COMPONENT: &str = "Silero VAD";
    let _ = event_tx.send(InitEvent::DownloadStart {
        component: VAD_COMPONENT,
    });

    let (dl_tx, dl_rx) = mpsc::channel::<u8>();
    let event_tx_dl = event_tx.clone();

    let dl_forwarder = match std::thread::Builder::new()
        .name("sherpa-vad-dl-progress".into())
        .spawn(move || {
            while let Ok(pct) = dl_rx.recv() {
                let _ = event_tx_dl.send(InitEvent::DownloadProgress { pct });
            }
        }) {
        Ok(handle) => handle,
        Err(e) => {
            let msg = format!("failed to spawn sherpa-vad-dl-progress thread: {e}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return false;
        }
    };

    let result = sherpa_model::download_silero_vad(&dl_tx);
    drop(dl_tx);
    let _ = dl_forwarder.join();

    if let Err(e) = result {
        let msg = format!("silero VAD download failed: {e}");
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return false;
    }

    tracing::info!("silero VAD download complete");
    true
}
