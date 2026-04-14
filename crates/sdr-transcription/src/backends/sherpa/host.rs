//! Sherpa-onnx host worker thread — owns the recognizer for the
//! entire process lifetime.
//!
//! Spawned from `main()` before GTK init via [`init_sherpa_host`].
//! The worker populates the process-wide `SHERPA_HOST` `OnceLock`
//! then sits on a command channel waiting for session requests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use sherpa_onnx::{OfflineRecognizer, OnlineRecognizer};

use crate::backend::{BackendError, TranscriptionEvent, TranscriptionInput};
use crate::init_event::InitEvent;
use crate::sherpa_model::{self, SherpaModel};

use super::silero_vad::SherpaSileroVad;

/// Bounded channel capacity for audio buffers from DSP → backend.
pub(super) const AUDIO_CHANNEL_CAPACITY: usize = 256;

/// Polling interval for the audio receive loop when checking for cancellation.
pub(super) const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Sample rate sherpa-onnx expects from `accept_waveform`.
pub(super) const SHERPA_SAMPLE_RATE_HZ: i32 = 16_000;

/// Absolute-difference tolerance for VAD threshold rebuild comparison.
/// If the requested threshold differs from the currently-held VAD's
/// threshold by more than this, the worker rebuilds Silero at session
/// start. Keeps the rebuild policy in sync with the UI's slider step
/// (0.05) — anything smaller than a slider step is just float drift.
const VAD_THRESHOLD_REBUILD_EPSILON: f32 = 0.01;

/// Process-wide singleton for the sherpa-onnx host. Stores either a ready
/// host or the error message from a failed initialization. Set exactly once
/// by the first successful worker thread.
static SHERPA_HOST: OnceLock<Result<SherpaHost, Arc<BackendError>>> = OnceLock::new();

/// Flag that atomically ensures the worker thread is spawned at most once.
/// Paired with `SHERPA_HOST` to give [`init_sherpa_host`] true idempotency:
/// a second call after init is in flight won't race to spawn another worker.
static INIT_STARTED: AtomicBool = AtomicBool::new(false);

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
///
/// Idempotent: if called more than once, the second and subsequent calls
/// return a pre-filled channel reflecting the current host state
/// (`Ready`, `Failed`, or "already in progress") without spawning another
/// worker.
pub fn init_sherpa_host(model: SherpaModel) -> mpsc::Receiver<InitEvent> {
    // Atomic compare-and-swap: first caller transitions false → true and
    // proceeds to spawn. Later callers see the flag already set and fall
    // through to the pre-filled channel path.
    if INIT_STARTED.swap(true, Ordering::AcqRel) {
        let (event_tx, event_rx) = mpsc::channel::<InitEvent>();
        match SHERPA_HOST.get() {
            Some(Ok(_)) => {
                tracing::debug!("init_sherpa_host called after host is already initialized");
                let _ = event_tx.send(InitEvent::Ready);
            }
            Some(Err(err)) => {
                let _ = event_tx.send(InitEvent::Failed {
                    message: format!("sherpa host was previously initialized with an error: {err}"),
                });
            }
            None => {
                // Init already in flight on another caller; return a stub
                // receiver so this caller doesn't block forever. Callers
                // should serialize their init on the first returned channel.
                let _ = event_tx.send(InitEvent::Failed {
                    message: "sherpa host initialization is already in progress".to_owned(),
                });
            }
        }
        return event_rx;
    }

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
    pub audio_rx: mpsc::Receiver<TranscriptionInput>,
    pub event_tx: mpsc::Sender<TranscriptionEvent>,
    pub noise_gate_ratio: f32,
    /// Silero VAD threshold requested for this session (offline VAD mode only).
    /// The worker rebuilds the VAD if this differs from the currently-held
    /// VAD's threshold. Ignored when `segmentation_mode == AutoBreak`.
    pub vad_threshold: f32,
    /// Which segmentation engine drives utterance boundaries. Validated
    /// against the model kind at the top of the session runner — streaming
    /// online models reject `AutoBreak`, and the offline session loop
    /// dispatches VAD vs Auto Break on this field.
    pub segmentation_mode: crate::backend::SegmentationMode,
    /// Auto Break timing parameters, read by `run_session_auto_break`.
    /// Ignored in VAD mode and by the streaming path.
    pub auto_break_thresholds: AutoBreakThresholds,
}

/// Auto Break timing parameters consumed by the state machine in
/// `backends/sherpa/offline.rs`. Bundled into a `Copy` struct so the
/// machine can own its configuration via one field instead of three.
///
/// The `_ms` suffix on every field is a deliberate unit marker, not
/// redundant naming — dropping it would make the struct easier to
/// misread as "these are sample counts" or "these are seconds" when
/// the callers mix raw u32 values with the rest of the timing math.
/// Clippy's `struct_field_names` lint would normally flag the shared
/// postfix; we allow it locally to keep the unit clarity.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)]
pub(super) struct AutoBreakThresholds {
    pub min_open_ms: u32,
    pub tail_ms: u32,
    pub min_segment_ms: u32,
}

impl AutoBreakThresholds {
    /// Default values matching the hardcoded constants from PR 8.
    /// Currently only used by unit tests in `offline.rs`; the
    /// production path always receives thresholds from
    /// `BackendConfig` via `SherpaBackend::start`, so the
    /// production code never calls this.
    #[cfg(test)]
    pub(super) const fn defaults() -> Self {
        Self {
            min_open_ms: crate::backend::AUTO_BREAK_MIN_OPEN_MS_DEFAULT,
            tail_ms: crate::backend::AUTO_BREAK_TAIL_MS_DEFAULT,
            min_segment_ms: crate::backend::AUTO_BREAK_MIN_SEGMENT_MS_DEFAULT,
        }
    }
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
    ///
    /// On thread-spawn failure (a rare OS-level error), routes the failure
    /// through the normal `InitEvent::Failed` + `SHERPA_HOST` error path
    /// instead of panicking — library crates are not allowed to abort.
    pub(super) fn spawn(model: SherpaModel) -> mpsc::Receiver<InitEvent> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HostCommand>();
        let (event_tx, event_rx) = mpsc::channel::<InitEvent>();

        // Keep a clone for the failure branch — the happy-path closure
        // moves the original `event_tx` into the worker thread.
        let event_tx_for_failure = event_tx.clone();

        let spawn_result = std::thread::Builder::new()
            .name("sherpa-host".into())
            .spawn(move || {
                run_host_loop(model, &cmd_rx, cmd_tx, event_tx);
            });

        if let Err(io_err) = spawn_result {
            let msg = format!("failed to spawn sherpa-host worker thread: {io_err}");
            tracing::error!(%msg);
            store_init_failure(BackendError::Spawn(io_err));
            let _ = event_tx_for_failure.send(InitEvent::Failed { message: msg });
        }

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
        // Both offline kinds share init_offline — only the recognizer
        // config builder differs, and that branching happens inside
        // init_offline based on model.kind() again.
        ModelKind::OfflineMoonshine | ModelKind::OfflineNemoTransducer => {
            match init_offline(model, &event_tx) {
                Ok(state) => state,
                Err(()) => return,
            }
        }
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
                        // Check if the user's requested VAD threshold differs
                        // from the currently-held VAD's threshold. If so,
                        // rebuild the VAD before starting the session. The
                        // rebuild is ~50-100ms of ONNX model load — only
                        // happens when the slider value actually changed.
                        //
                        // Skip the rebuild entirely when Auto Break mode is
                        // selected — the Silero VAD is unused in that path
                        // (the squelch gate drives segmentation), so paying
                        // the rebuild cost is pure waste. The rebuild still
                        // runs on the next Vad-mode session if the threshold
                        // has drifted.
                        if params.segmentation_mode == crate::backend::SegmentationMode::Vad {
                            let requested = params.vad_threshold;
                            // Treat differences smaller than the slider step
                            // as "same" to avoid pointless rebuilds from
                            // float drift.
                            if (vad.current_threshold() - requested).abs()
                                > VAD_THRESHOLD_REBUILD_EPSILON
                            {
                                tracing::info!(
                                    old = vad.current_threshold(),
                                    new = requested,
                                    "rebuilding Silero VAD with new threshold"
                                );
                                let vad_path = sherpa_model::silero_vad_path();
                                match super::silero_vad::SherpaSileroVad::new(&vad_path, requested)
                                {
                                    Ok(new_vad) => {
                                        *vad = new_vad;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            "VAD rebuild failed; reusing existing VAD"
                                        );
                                    }
                                }
                            }
                        }
                        super::offline::run_session(recognizer, vad, params);
                    }
                }
                tracing::info!("sherpa-host: session ended");
            }
            HostCommand::ReloadRecognizer {
                model: new_model,
                event_tx,
            } => {
                handle_reload_recognizer(new_model, &event_tx, &mut recognizer_state);
            }
        }
    }
    tracing::info!("sherpa-host worker exiting");
}

/// Handle a `HostCommand::ReloadRecognizer` by building the new
/// recognizer WITHOUT dropping the current one first, then swapping
/// only if the new recognizer is ready.
///
/// If the new recognizer fails to build (transient download error,
/// missing files, `model_type` mismatch, etc.) the existing
/// `*recognizer_state` is left untouched so `StartSession` continues
/// to find the previous working recognizer.
///
/// Previous implementation dropped the current state up front which
/// stranded the host on any init failure — every subsequent session
/// until the next manual reload was rejected with "no recognizer
/// loaded". The brief double-RAM footprint during the transition
/// (~2x the model size; worst case ~1.2 GB for a Parakeet-to-Parakeet
/// swap) is tolerable on personal-use hardware.
fn handle_reload_recognizer(
    new_model: SherpaModel,
    event_tx: &mpsc::Sender<InitEvent>,
    recognizer_state: &mut Option<RecognizerState>,
) {
    tracing::info!(?new_model, "sherpa-host: reloading recognizer");

    let new_state = match new_model.kind() {
        crate::sherpa_model::ModelKind::OnlineTransducer => init_online(new_model, event_tx),
        crate::sherpa_model::ModelKind::OfflineMoonshine
        | crate::sherpa_model::ModelKind::OfflineNemoTransducer => {
            init_offline(new_model, event_tx)
        }
    };

    match new_state {
        Ok(state) => {
            tracing::info!(
                ?new_model,
                "sherpa-host: reload complete, replacing old recognizer"
            );
            // Assignment drops the old recognizer at end of statement.
            // Both recognizers exist in memory for exactly one
            // expression evaluation.
            *recognizer_state = Some(state);
            let _ = event_tx.send(InitEvent::Ready);
        }
        Err(()) => {
            // init_online/init_offline already emitted
            // InitEvent::Failed through event_tx. The old
            // `recognizer_state` is left untouched so StartSession
            // continues to find the previous recognizer. No
            // host-stranding.
            tracing::warn!(
                ?new_model,
                "sherpa-host: reload failed, keeping previous recognizer"
            );
        }
    }
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
    let recognizer_config =
        super::streaming::build_recognizer_config(model, super::SHERPA_PROVIDER);
    tracing::info!(
        ?model,
        provider = super::SHERPA_PROVIDER,
        "creating sherpa-onnx OnlineRecognizer"
    );

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

/// Phase 1-2 for any offline model (`OfflineMoonshine` or
/// `OfflineNemoTransducer`): download the Silero VAD if missing,
/// download the model bundle if missing, then build the right
/// `OfflineRecognizerConfig` for the model's kind and create the
/// `OfflineRecognizer` + `SherpaSileroVad`.
///
/// The recognizer config builder is selected via `model.kind()` so
/// callers don't need to know which offline family they're using.
/// Returns `Err(())` on any failure — the error has already been
/// stored in `SHERPA_HOST` and emitted as `InitEvent::Failed`.
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
    // Both offline kinds use OfflineRecognizer but with different config
    // builders. Branch here so init_offline's Phase 1-2 (download VAD +
    // bundle) stays generic across all offline models.
    //
    // The `OnlineTransducer` arm should be unreachable in practice —
    // every caller of `init_offline` (the two places in `run_host_loop`
    // that dispatch on `model.kind()`) only routes `OfflineMoonshine`
    // and `OfflineNemoTransducer` here. Still, library crates forbid
    // `panic!` / `unreachable!` so we route through the normal init
    // failure path instead of aborting the process.
    let recognizer_config = match model.kind() {
        crate::sherpa_model::ModelKind::OfflineMoonshine => {
            super::offline::build_moonshine_recognizer_config(model, super::SHERPA_PROVIDER)
        }
        crate::sherpa_model::ModelKind::OfflineNemoTransducer => {
            super::offline::build_nemo_transducer_recognizer_config(model, super::SHERPA_PROVIDER)
        }
        crate::sherpa_model::ModelKind::OnlineTransducer => {
            let msg = format!(
                "init_offline called with online model {} — engine routing bug",
                model.label()
            );
            tracing::error!(%msg);
            store_init_failure(BackendError::Init(msg.clone()));
            let _ = event_tx.send(InitEvent::Failed { message: msg });
            return Err(());
        }
    };
    tracing::info!(
        ?model,
        provider = super::SHERPA_PROVIDER,
        "creating sherpa-onnx OfflineRecognizer"
    );

    let Some(recognizer) = OfflineRecognizer::create(&recognizer_config) else {
        let msg = format!(
            "OfflineRecognizer::create returned None for {} — check model files and model_type",
            model.label()
        );
        tracing::error!(%msg);
        store_init_failure(BackendError::Init(msg.clone()));
        let _ = event_tx.send(InitEvent::Failed { message: msg });
        return Err(());
    };
    tracing::info!("OfflineRecognizer created successfully");

    // --- Build SherpaSileroVad ---
    // Use the default threshold at startup (Option A): if the user has a
    // persisted non-default threshold, it will cause a ~50-100ms VAD
    // rebuild on the first session start (in the StartSession handler below).
    let vad_path = sherpa_model::silero_vad_path();
    let vad = match SherpaSileroVad::new(&vad_path, super::silero_vad::SILERO_THRESHOLD) {
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
