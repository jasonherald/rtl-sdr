//! Sherpa-onnx backend — streaming-native ASR via the official k2-fsa
//! `sherpa-onnx` Rust crate.
//!
//! ## Architecture
//!
//! The recognizer is created ONCE per process by [`SherpaHost`], a
//! long-lived worker thread spawned from `main()` BEFORE GTK is loaded.
//! This is a workaround for a C++ static-initializer collision between
//! sherpa-onnx's bundled ONNX Runtime and GTK4's transitive C++ deps —
//! creating the recognizer after GTK init causes `free(): invalid pointer`
//! inside `std::regex` constructors called by ONNX Runtime's
//! `ParseSemVerVersion`.
//!
//! [`SherpaBackend`] is a thin facade that asks the global host for a
//! new session. The host creates a fresh [`OnlineStream`] from the
//! existing recognizer and runs the audio feed loop until the session
//! is cancelled or the audio channel disconnects.
//!
//! Both [`OnlineRecognizer`] and [`OnlineStream`] are `!Send`. They
//! live entirely on the host worker thread; the host wraps its command
//! sender in a Mutex so it can be stored in a process-wide `OnceLock`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};
use crate::init_event::InitEvent;
use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};
use crate::{denoise, resampler};

/// Bounded channel capacity for audio buffers from DSP → backend.
const AUDIO_CHANNEL_CAPACITY: usize = 256;

/// Polling interval for the audio receive loop when checking for cancellation.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Endpoint detection rule defaults — match upstream sherpa-onnx examples.
const RULE1_MIN_TRAILING_SILENCE: f32 = 2.4;
const RULE2_MIN_TRAILING_SILENCE: f32 = 1.2;
const RULE3_MIN_UTTERANCE_LENGTH: f32 = 20.0;

/// Sample rate sherpa-onnx expects from `accept_waveform`.
const SHERPA_SAMPLE_RATE_HZ: i32 = 16_000;
/// Initial capacity for the per-session resampled-mono scratch buffer.
const SESSION_MONO_BUFFER_CAPACITY: usize = 16_000;
/// ONNX Runtime threads per recognizer. Sherpa is fast enough on CPU
/// that one thread is sufficient and avoids competing with the audio
/// pipeline.
const SHERPA_NUM_THREADS: i32 = 1;

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
///
/// The previous synchronous variant returned `Result<(), String>`; the
/// event channel replaces that. Failures route through
/// `InitEvent::Failed` AND through `SHERPA_HOST.get() -> Some(Err(_))`,
/// so the existing `SherpaBackend::start` error path is unchanged.
pub fn init_sherpa_host(model: SherpaModel) -> std::sync::mpsc::Receiver<InitEvent> {
    SherpaHost::spawn(model)
}

/// Look up the global sherpa host. Returns `None` if `init_sherpa_host` was
/// never called.
fn global_sherpa_host() -> Option<&'static Result<SherpaHost, Arc<BackendError>>> {
    SHERPA_HOST.get()
}

/// Parameters handed to the host worker for one transcription session.
struct SessionParams {
    cancel: Arc<AtomicBool>,
    audio_rx: mpsc::Receiver<Vec<f32>>,
    event_tx: mpsc::Sender<TranscriptionEvent>,
    noise_gate_ratio: f32,
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
pub struct SherpaHost {
    state: Mutex<SherpaHostState>,
}

impl SherpaHost {
    /// Spawn the host worker thread and return immediately.
    ///
    /// Returns a `Receiver<InitEvent>` that streams progress events as
    /// the worker downloads (if needed) + creates the recognizer. The
    /// caller is responsible for draining the receiver until it sees
    /// `InitEvent::Ready` or `InitEvent::Failed` — the worker populates
    /// the global `SHERPA_HOST` `OnceLock` itself before emitting the
    /// final event.
    ///
    /// The signature is intentionally non-`Result` because failures
    /// surface through the event channel as `InitEvent::Failed`. This
    /// keeps the synchronous path (no immediate Result) consistent
    /// with the async event-driven model.
    pub fn spawn(model: SherpaModel) -> std::sync::mpsc::Receiver<InitEvent> {
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

    /// Send a `StartSession` command to the host. Returns an error if the
    /// host worker has died.
    fn start_session(&self, params: SessionParams) -> Result<(), BackendError> {
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
///
/// Phase 1: download the model bundle if it's missing locally
/// Phase 2: create the `OnlineRecognizer`
/// Phase 3: store the `SherpaHost` in `SHERPA_HOST` and emit Ready
/// Phase 4: process `StartSession` commands forever
///
/// Failures during phases 1 or 2 store an error in `SHERPA_HOST` and
/// emit `InitEvent::Failed` before returning early.
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

        // Forwarder thread translates u8 progress percents into
        // InitEvent::DownloadProgress messages.
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

        // Drop the sender so the forwarder thread exits when it drains.
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

        // Now fire the Extracting event BEFORE actually extracting, so
        // the splash label updates while extraction is happening (~1-2
        // seconds for the 256 MB archive).
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
    let recognizer_config = build_recognizer_config(model, "cpu");
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
        // Someone else already set it — this worker is unreachable.
        // Don't emit Ready because the caller expects Ready to mean
        // "SHERPA_HOST now points at a live host constructed by this
        // worker"; the previous set is whatever the caller gets when
        // they look up the global.
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
                run_session(&recognizer, params);
                tracing::info!("sherpa-host: session ended");
            }
        }
    }
    tracing::info!("sherpa-host worker exiting");
}

/// Helper to store an initialization failure in the global `OnceLock`.
/// The error gets wrapped in `Arc` to satisfy the
/// `OnceLock<Result<..., Arc<BackendError>>>` type.
fn store_init_failure(err: BackendError) {
    let _ = SHERPA_HOST.set(Err(std::sync::Arc::new(err)));
}

/// Build the `OnlineRecognizerConfig` for a Streaming Zipformer model.
///
/// Note: `BackendConfig::silence_threshold` is intentionally NOT honored here
/// because sherpa-onnx's `OnlineRecognizer` has native endpoint detection
/// (via `rule1`/`rule2`/`rule3_min_trailing_silence`) that handles silence
/// at the model level. Adding an RMS-based pre-gate would mask short pauses
/// inside utterances and confuse the streaming decoder. The Whisper backend
/// uses `silence_threshold` because Whisper has no built-in VAD.
fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    // Irrefutable today — will become refutable when Moonshine variant lands (plan Task 6).
    #[allow(irrefutable_let_patterns)]
    let ModelFilePaths::Transducer { encoder, decoder, joiner, tokens } =
        sherpa_model::model_file_paths(model)
    else {
        unreachable!("StreamingZipformerEn is always a Transducer")
    };

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
    config.model_config.provider = Some(provider.to_owned());
    config.model_config.num_threads = SHERPA_NUM_THREADS;
    config.enable_endpoint = true;
    config.decoding_method = Some("greedy_search".to_owned());
    config.rule1_min_trailing_silence = RULE1_MIN_TRAILING_SILENCE;
    config.rule2_min_trailing_silence = RULE2_MIN_TRAILING_SILENCE;
    config.rule3_min_utterance_length = RULE3_MIN_UTTERANCE_LENGTH;

    config
}

/// One transcription session. Creates a fresh stream from `recognizer`,
/// runs the feed loop until cancelled or the audio channel disconnects.
fn run_session(recognizer: &OnlineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
    } = params;

    let stream = recognizer.create_stream();

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let mut mono_buf: Vec<f32> = Vec::with_capacity(SESSION_MONO_BUFFER_CAPACITY);
    let mut last_partial = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa session cancelled");
            finalize_session(recognizer, &stream, &last_partial, &event_tx);
            return;
        }

        let interleaved = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Resample 48 kHz stereo → 16 kHz mono into the scratch buffer.
        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        // Drain any additional queued buffers into the same scratch
        // (same pattern as WhisperBackend) so we don't fall behind.
        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                finalize_session(recognizer, &stream, &last_partial, &event_tx);
                return;
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, &mono_buf);

        while recognizer.is_ready(&stream) {
            if cancel.load(Ordering::Relaxed) {
                finalize_session(recognizer, &stream, &last_partial, &event_tx);
                return;
            }
            recognizer.decode(&stream);
        }

        // Pull the current hypothesis. Emit a Partial event if it changed
        // since the last one. Capture the trimmed text into a local that's
        // reused below for the committed Text event when the endpoint fires.
        let current_text = if let Some(result) = recognizer.get_result(&stream) {
            let trimmed = result.text.trim().to_owned();
            if !trimmed.is_empty() && trimmed != last_partial {
                last_partial.clone_from(&trimmed);
                let _ = event_tx.send(TranscriptionEvent::Partial {
                    text: trimmed.clone(),
                });
            }
            trimmed
        } else {
            // get_result can return None on a serde failure inside the C
            // layer. We must NOT skip the endpoint check below in that
            // case — otherwise the stream can get stuck in endpoint state.
            String::new()
        };

        // Endpoint check is independent of get_result and must always run
        // so reset() fires when the recognizer says the utterance is over.
        if recognizer.is_endpoint(&stream) {
            // If get_result returned empty, fall back to last_partial so
            // we don't drop the utterance. Mirrors finalize_session.
            let committed_text = if current_text.is_empty() {
                last_partial.clone()
            } else {
                current_text
            };
            if !committed_text.is_empty() {
                let timestamp = crate::util::wall_clock_timestamp();
                tracing::debug!(%timestamp, text = %committed_text, "sherpa committed utterance");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: committed_text,
                });
            }
            recognizer.reset(&stream);
            last_partial.clear();
        }
    }

    // Audio channel disconnected — commit any in-flight hypothesis as Text
    // before exiting so the last spoken phrase isn't lost.
    finalize_session(recognizer, &stream, &last_partial, &event_tx);
    tracing::info!("sherpa session ended (audio channel disconnected)");
}

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
                // Reconstruct a fresh BackendError so callers (and the UI)
                // see the original variant. ModelNotFound is the most
                // important case to preserve — it tells the user exactly
                // where to download the model bundle.
                return Err(match &**stored {
                    BackendError::ModelNotFound { path } => {
                        BackendError::ModelNotFound { path: path.clone() }
                    }
                    BackendError::Init(msg) => {
                        BackendError::Init(format!("sherpa host failed to initialize: {msg}"))
                    }
                    BackendError::Spawn(io_err) => {
                        // io::Error isn't Clone; flatten to a string and
                        // wrap in Init. This is a rare path (worker thread
                        // spawn failure during init) so the loss of fidelity
                        // is acceptable.
                        BackendError::Init(format!(
                            "sherpa host worker thread spawn failed: {io_err}"
                        ))
                    }
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
        // The host detects the cancel flag on its next recv_timeout
        // (every 100 ms) and ends the session naturally. We don't need
        // to join anything — the worker thread is shared and lives forever.
        self.cancel.store(true, Ordering::Relaxed);
        tracing::info!("sherpa backend stopped");
    }

    fn shutdown_nonblocking(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        tracing::info!("sherpa backend shutdown (non-blocking)");
    }
}

/// Commit any in-flight partial hypothesis as a final `Text` event before
/// the session ends. Called from both the cancel and disconnect exit paths.
///
/// We pull `get_result` one more time so we capture any text the recognizer
/// produced after the last partial event but before the loop exited. If
/// that's empty we fall back to `last_partial` which holds whatever was
/// most recently emitted.
fn finalize_session(
    recognizer: &OnlineRecognizer,
    stream: &OnlineStream,
    last_partial: &str,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let final_text = recognizer
        .get_result(stream)
        .map(|r| r.text.trim().to_owned())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| last_partial.to_owned());

    if !final_text.is_empty() {
        let timestamp = crate::util::wall_clock_timestamp();
        tracing::debug!(%timestamp, text = %final_text, "sherpa finalizing on session end");
        let _ = event_tx.send(TranscriptionEvent::Text {
            timestamp,
            text: final_text,
        });
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

    /// **Manual-only test.** Kicks off the real worker — which, if the
    /// model files are absent, proceeds past the first `DownloadStart`
    /// event into the full 256 MB download + extract path. That makes
    /// the test network-dependent, machine-state-dependent, and has
    /// cross-test leakage concerns (the worker populates `SHERPA_HOST`
    /// `OnceLock` for the rest of the process lifetime, affecting
    /// subsequent unit tests in the same binary).
    ///
    /// Marked `#[ignore]` so it doesn't run on `cargo test`. Run
    /// manually with `cargo test ... -- --ignored` to exercise the
    /// `DownloadStart` path explicitly.
    ///
    /// Proper hermetic coverage (injectable model paths, mocked spawn,
    /// etc.) is tracked as a follow-up refactor that would require
    /// threading a base-dir parameter through `sherpa_models_dir()`
    /// and friends — out of scope for PR #254.
    #[test]
    #[ignore = "spawns the real download worker; run manually with --ignored"]
    fn sherpa_host_spawn_emits_download_start_when_files_missing() {
        if sherpa_model::model_exists(SherpaModel::StreamingZipformerEn) {
            eprintln!("skipping test: streaming-zipformer-en model is present locally");
            return;
        }
        let event_rx = SherpaHost::spawn(SherpaModel::StreamingZipformerEn);
        let first_event = event_rx
            .recv()
            .expect("worker should send at least one event");
        assert!(
            matches!(first_event, InitEvent::DownloadStart { .. }),
            "expected DownloadStart when model is missing, got {first_event:?}"
        );
        // Drop the receiver — the worker will silently discard further events.
        drop(event_rx);
    }
}
