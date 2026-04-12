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

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig};

use crate::backend::{
    BackendConfig, BackendError, BackendHandle, ModelChoice, TranscriptionBackend,
    TranscriptionEvent,
};
use crate::sherpa_model::{self, SherpaModel};
use crate::{denoise, resampler};

/// Bounded channel capacity for audio buffers from DSP → backend.
const AUDIO_CHANNEL_CAPACITY: usize = 256;

/// Polling interval for the audio receive loop when checking for cancellation.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Endpoint detection rule defaults — match upstream sherpa-onnx examples.
const RULE1_MIN_TRAILING_SILENCE: f32 = 2.4;
const RULE2_MIN_TRAILING_SILENCE: f32 = 1.2;
const RULE3_MIN_UTTERANCE_LENGTH: f32 = 20.0;

/// Maximum time we wait for the host worker thread to report initialization
/// success or failure before giving up. Recognizer load is typically <1s.
const HOST_INIT_TIMEOUT: Duration = Duration::from_mins(1);

/// Process-wide singleton for the sherpa-onnx host. Stores either a ready
/// host or the error message from a failed initialization. Set exactly once
/// by [`init_sherpa_host`]; subsequent calls are no-ops.
static SHERPA_HOST: OnceLock<Result<SherpaHost, String>> = OnceLock::new();

/// Spawn the global sherpa-onnx host thread.
///
/// **MUST be called from `main()` BEFORE GTK is initialized** (before
/// `sdr_ui::run()`). The host's worker thread creates the
/// [`OnlineRecognizer`] once at startup, which initializes ONNX Runtime's
/// C++ runtime state. Doing this before GTK loads avoids a static-initializer
/// collision that causes `free(): invalid pointer` corruption inside
/// libstdc++ regex code on the first decode call.
///
/// Idempotent — safe to call multiple times; only the first call has effect.
/// If initialization fails (model files missing, ONNX error), the error is
/// stashed in the global slot and reported when the user actually tries to
/// start a Sherpa transcription session.
pub fn init_sherpa_host(model: SherpaModel) {
    let _ = SHERPA_HOST.set(SherpaHost::spawn(model).map_err(|e| e.to_string()));
}

/// Look up the global sherpa host. Returns `None` if `init_sherpa_host` was
/// never called.
fn global_sherpa_host() -> Option<&'static Result<SherpaHost, String>> {
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
    /// Spawn the host worker thread and block until the recognizer is
    /// either ready or initialization has failed.
    ///
    /// Returns `BackendError::ModelNotFound` if the model files for `model`
    /// are not present on disk, or `BackendError::Init(_)` if the
    /// recognizer creation fails.
    pub fn spawn(model: SherpaModel) -> Result<Self, BackendError> {
        if !sherpa_model::model_exists(model) {
            return Err(BackendError::ModelNotFound {
                path: sherpa_model::model_directory(model),
            });
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<HostCommand>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        std::thread::Builder::new()
            .name("sherpa-host".into())
            .spawn(move || {
                run_host_loop(model, &cmd_rx, init_tx);
            })?;

        match init_rx.recv_timeout(HOST_INIT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                state: Mutex::new(SherpaHostState { cmd_tx }),
            }),
            Ok(Err(msg)) => Err(BackendError::Init(msg)),
            Err(_) => Err(BackendError::Init(
                "sherpa host worker timed out during initialization".to_owned(),
            )),
        }
    }

    /// Send a `StartSession` command to the host. Returns an error if the
    /// host worker has died.
    fn start_session(&self, params: SessionParams) -> Result<(), BackendError> {
        let state = self.state.lock().expect("sherpa host mutex poisoned");
        state
            .cmd_tx
            .send(HostCommand::StartSession(params))
            .map_err(|_| BackendError::Init("sherpa host worker is no longer running".to_owned()))
    }
}

/// Worker thread entry point. Creates the [`OnlineRecognizer`] once and
/// signals success/failure on `init_tx`, then loops processing
/// `StartSession` commands until the command channel disconnects (which
/// happens at process exit when the global host is dropped).
fn run_host_loop(
    model: SherpaModel,
    cmd_rx: &mpsc::Receiver<HostCommand>,
    init_tx: mpsc::SyncSender<Result<(), String>>,
) {
    let recognizer_config = build_recognizer_config(model, "cpu");
    tracing::info!(?model, "creating sherpa-onnx recognizer (host init)");

    let Some(recognizer) = OnlineRecognizer::create(&recognizer_config) else {
        let msg = "OnlineRecognizer::create returned None — check model file paths".to_owned();
        tracing::error!(%msg);
        let _ = init_tx.send(Err(msg));
        return;
    };
    tracing::info!("sherpa-onnx recognizer created successfully");

    if init_tx.send(Ok(())).is_err() {
        tracing::warn!("sherpa host init channel closed before send — controller dropped");
        return;
    }
    drop(init_tx);

    tracing::info!("sherpa-host ready, waiting for sessions");
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

/// Build the `OnlineRecognizerConfig` for a Streaming Zipformer model.
fn build_recognizer_config(model: SherpaModel, provider: &str) -> OnlineRecognizerConfig {
    let (encoder, decoder, joiner, tokens) = sherpa_model::model_file_paths(model);

    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
    config.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
    config.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
    config.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
    config.model_config.provider = Some(provider.to_owned());
    config.model_config.num_threads = 1;
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

    let mut mono_buf: Vec<f32> = Vec::with_capacity(16_000);
    let mut last_partial = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa session cancelled");
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
                return;
            }
            resampler::downsample_stereo_to_mono_16k(&extra, &mut mono_buf);
        }

        if mono_buf.is_empty() {
            continue;
        }

        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        stream.accept_waveform(16_000_i32, &mono_buf);

        while recognizer.is_ready(&stream) {
            if cancel.load(Ordering::Relaxed) {
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
            if !current_text.is_empty() {
                let timestamp = wall_clock_timestamp();
                tracing::debug!(%timestamp, text = %current_text, "sherpa committed utterance");
                let _ = event_tx.send(TranscriptionEvent::Text {
                    timestamp,
                    text: current_text,
                });
            }
            recognizer.reset(&stream);
            last_partial.clear();
        }
    }

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
        let ModelChoice::Sherpa(_) = config.model else {
            return Err(BackendError::WrongModelKind);
        };

        let host = match global_sherpa_host() {
            Some(Ok(h)) => h,
            Some(Err(msg)) => {
                return Err(BackendError::Init(format!(
                    "sherpa host failed to initialize: {msg}"
                )));
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

/// Wall-clock "HH:MM:SS" string. Same implementation as
/// [`crate::backends::whisper`] but kept local to avoid a public re-export
/// of an internal helper.
fn wall_clock_timestamp() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };

    // SAFETY: gettimeofday writes into the provided buffer and is thread-safe.
    #[allow(unsafe_code)]
    let epoch = unsafe {
        libc::gettimeofday(&raw mut tv, std::ptr::null_mut());
        tv.tv_sec
    };

    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();

    // SAFETY: localtime_r is the reentrant variant; gmtime_r is the UTC fallback.
    #[allow(unsafe_code)]
    let tm = unsafe {
        let result = libc::localtime_r(&raw const epoch, tm.as_mut_ptr());
        let result = if result.is_null() {
            libc::gmtime_r(&raw const epoch, tm.as_mut_ptr())
        } else {
            result
        };
        if result.is_null() {
            return "00:00:00".to_owned();
        }
        tm.assume_init()
    };

    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
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

    #[test]
    #[allow(clippy::panic)]
    fn sherpa_backend_start_returns_init_error_when_host_not_initialized() {
        // The test process never calls init_sherpa_host, so the global
        // OnceLock is empty and start() should return BackendError::Init
        // with a clear message about the host not being initialized.
        //
        // Note: this test relies on no other test in this binary calling
        // init_sherpa_host. Cargo runs each integration test binary
        // separately but unit tests share a process. If this test ever
        // becomes flaky, mark it #[ignore] and run manually.
        let mut backend = SherpaBackend::new();
        let config = BackendConfig {
            model: ModelChoice::Sherpa(SherpaModel::StreamingZipformerEn),
            silence_threshold: 0.007,
            noise_gate_ratio: 3.0,
        };
        match backend.start(config) {
            Err(BackendError::Init(msg)) => {
                assert!(
                    msg.contains("not initialized") || msg.contains("failed to initialize"),
                    "expected init error mentioning initialization, got: {msg}"
                );
            }
            Err(e) => panic!("expected Init error, got: {e:?}"),
            Ok(_) => {
                panic!("expected Init error because init_sherpa_host was never called, got Ok")
            }
        }
    }

    #[test]
    #[allow(clippy::panic)]
    fn sherpa_host_spawn_returns_model_not_found_when_files_missing() {
        // SherpaHost::spawn checks for model files synchronously before
        // spawning the worker thread. In an environment without the
        // model bundle, this should return ModelNotFound. If the dev
        // happens to have the model installed, gracefully skip without
        // calling spawn() — spawning and then dropping a live host
        // would trigger ONNX Runtime cleanup that races with other tests.
        if sherpa_model::model_exists(SherpaModel::StreamingZipformerEn) {
            eprintln!("skipping test: streaming-zipformer-en model is present locally");
            return;
        }
        match SherpaHost::spawn(SherpaModel::StreamingZipformerEn) {
            Err(BackendError::ModelNotFound { path }) => {
                assert!(path.ends_with("sherpa/streaming-zipformer-en"));
            }
            Ok(_host) => {
                // model_exists() returned false above, so spawn() returning
                // Ok here would be a logic error in model_exists.
                panic!("model_exists returned false but spawn succeeded — inconsistent state");
            }
            Err(e) => panic!("expected ModelNotFound, got {e:?}"),
        }
    }
}
