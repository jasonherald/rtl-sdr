//! Offline session loop for Moonshine (and future offline recognizers).
//!
//! Runs on the sherpa-host worker thread. Uses Silero VAD to detect
//! utterance boundaries in the incoming audio stream, then batch-decodes
//! each completed segment through the `OfflineRecognizer`.
//!
//! Unlike the streaming loop, this path emits NO `TranscriptionEvent::Partial`
//! events. Moonshine is offline — partials aren't meaningful. The UI hides
//! the Live/Final display-mode toggle when a Moonshine model is selected
//! (see `SherpaModel::supports_partials`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use sherpa_onnx::{
    OfflineModelConfig, OfflineMoonshineModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineTransducerModelConfig,
};

use crate::backend::{TranscriptionEvent, TranscriptionInput};
use crate::sherpa_model::{self, ModelFilePaths, SherpaModel};
use crate::vad::VoiceActivityDetector;
use crate::{denoise, resampler};

use super::host::{AUDIO_RECV_TIMEOUT, SHERPA_SAMPLE_RATE_HZ, SessionParams};
use super::silero_vad::SherpaSileroVad;

/// One segment handed from the session I/O thread to the decoder worker
/// (the sherpa-host thread). Already resampled to 16 kHz mono and
/// denoised — the host thread only has to feed it to the recognizer.
///
/// The I/O thread does all the audio prep so the decoder worker stays
/// cold-path-free: a `DecodeRequest` is the minimum data needed to
/// produce a transcription event.
pub(super) struct DecodeRequest {
    pub mono: Vec<f32>,
}

/// Decoder service loop — runs on the sherpa-host thread alongside a
/// spawned session I/O thread. Owns the `&OfflineRecognizer` reference
/// (never crosses threads) and drains `decode_rx` until the I/O thread
/// drops its sender (clean session end) or `cancel` fires.
///
/// On cancellation, the loop counts any remaining requests in the
/// channel, drops them, and emits a single `Text` event noting the
/// stop time and discard count so the user sees why the transcript
/// ended mid-flight.
///
/// Returns nothing — results are pushed directly to `event_tx` as
/// `TranscriptionEvent::Text`.
fn decoder_service_loop(
    recognizer: &OfflineRecognizer,
    decode_rx: &mpsc::Receiver<DecodeRequest>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    cancel: &Arc<AtomicBool>,
) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            emit_stop_notification(decode_rx, event_tx);
            return;
        }
        // Block on the next request but wake periodically to check
        // cancel — AUDIO_RECV_TIMEOUT is short enough for a
        // responsive stop without burning CPU.
        match decode_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(request) => {
                if cancel.load(Ordering::Relaxed) {
                    emit_stop_notification(decode_rx, event_tx);
                    return;
                }
                decode_segment(recognizer, &request.mono, event_tx);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Drain any queued `DecodeRequest`s from `decode_rx` without decoding
/// them and emit a single `Text` event describing the stop. Called
/// from [`decoder_service_loop`] when the user cancels mid-session.
///
/// If the queue had pending segments, the transcript shows how many
/// were discarded so the operator understands why audio between the
/// last committed utterance and the stop time is missing — useful
/// when reviewing a recording later.
fn emit_stop_notification(
    decode_rx: &mpsc::Receiver<DecodeRequest>,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let mut dropped: usize = 0;
    while decode_rx.try_recv().is_ok() {
        dropped += 1;
    }
    let timestamp = crate::util::wall_clock_timestamp();
    let text = if dropped == 0 {
        "[transcription stopped]".to_owned()
    } else {
        format!("[transcription stopped — {dropped} pending segment(s) discarded]")
    };
    tracing::info!(%timestamp, dropped, "sherpa offline session stop notification");
    let _ = event_tx.send(TranscriptionEvent::Text { timestamp, text });
}

/// Initial capacity for the per-session resampled-mono scratch buffer.
const SESSION_MONO_BUFFER_CAPACITY: usize = 16_000;

/// ONNX Runtime threads per recognizer. Sherpa is fast enough on CPU
/// that one thread is sufficient and avoids competing with the audio
/// pipeline.
const SHERPA_NUM_THREADS: i32 = 1;

/// sherpa-onnx `model_type` field value that selects `NeMo`'s Token-and-Duration
/// Transducer decode loop. Without this exact string, sherpa-onnx falls back
/// to the generic transducer decode path which doesn't understand TDT's
/// joiner output shape, and `OfflineRecognizer::create` returns `None` at
/// runtime — silent failure mode.
///
/// Mirrors the upstream `rust-api-examples/examples/nemo_parakeet.rs` example.
const NEMO_TRANSDUCER_MODEL_TYPE: &str = "nemo_transducer";

/// Safety cap: if squelch stays open longer than this, flush anyway.
/// Protects against pathological stuck-open situations (bad auto-squelch,
/// carrier jam, band opening) that would otherwise cause unbounded
/// memory growth in the segment buffer.
///
/// NOTE: unlike the other Auto Break constants, this one is NOT user
/// tunable. It's a hard OOM safety guard, not a segmentation preference,
/// and exposing it would invite users to disable the protection.
const AUTO_BREAK_MAX_SEGMENT_MS: u32 = 30_000;

// The previously-hardcoded `AUTO_BREAK_MIN_OPEN_MS`, `AUTO_BREAK_TAIL_MS`,
// and `AUTO_BREAK_MIN_SEGMENT_MS` constants were moved to per-session
// values on `SessionParams::auto_break_thresholds` (issue #272). Defaults
// live as `pub const AUTO_BREAK_*_MS_DEFAULT` in `crate::backend`; the UI
// reads the user-tuned values from config and passes them through
// `BackendConfig`.

/// Sample rate of incoming `TranscriptionInput::Samples` frames. The DSP
/// controller emits interleaved stereo f32 at 48 kHz (see
/// `sdr-core::controller::process_iq_block`). Extracted as a named
/// constant so the Auto Break buffer-duration math stays in sync if the
/// wire format ever changes.
const TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ: u64 = 48_000;

/// Channel count of incoming `TranscriptionInput::Samples` frames
/// (interleaved stereo = 2 f32 values per audio frame).
const TRANSCRIPTION_INPUT_CHANNELS: usize = 2;

/// Target sample rate of the mono buffer handed to the recognizer, as a
/// `usize` for capacity math. `SHERPA_SAMPLE_RATE_HZ` in `host.rs` is an
/// `i32` that sherpa-onnx wants for its `accept_waveform` API; this
/// mirror lives here in usize form so the capacity divisor below stays
/// pure integer math without casts.
const RECOGNIZER_SAMPLE_RATE_HZ_USIZE: usize = 16_000;

/// Mono-16k capacity heuristic for converting a stereo-48k buffer. The
/// target size is `len / CHANNELS / (48_000 / 16_000)` = `len / 6`, used
/// as the `Vec::with_capacity` hint when resampling Auto Break segments.
/// All integer math in usize to keep clippy's
/// `cast_possible_truncation` quiet on 32-bit targets.
const STEREO_48K_TO_MONO_16K_CAPACITY_DIVISOR: usize =
    TRANSCRIPTION_INPUT_CHANNELS * (48_000 / RECOGNIZER_SAMPLE_RATE_HZ_USIZE);

/// Build the `OfflineRecognizerConfig` for a Moonshine v1 model.
///
/// k2-fsa's int8 Moonshine releases use the v1 layout with five files:
/// preprocessor (not quantized), encoder, uncached decoder, cached
/// decoder, and tokens. The v2 two-file layout (encoder plus merged
/// decoder) exists in `OfflineMoonshineModelConfig` but is not what
/// the releases actually ship.
pub(super) fn build_moonshine_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> OfflineRecognizerConfig {
    let ModelFilePaths::Moonshine {
        preprocessor,
        encoder,
        uncached_decoder,
        cached_decoder,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        unreachable!("offline::build_moonshine_recognizer_config called with non-Moonshine model")
    };

    let moonshine = OfflineMoonshineModelConfig {
        preprocessor: Some(preprocessor.to_string_lossy().into_owned()),
        encoder: Some(encoder.to_string_lossy().into_owned()),
        uncached_decoder: Some(uncached_decoder.to_string_lossy().into_owned()),
        cached_decoder: Some(cached_decoder.to_string_lossy().into_owned()),
        ..OfflineMoonshineModelConfig::default()
    };

    let model_config = OfflineModelConfig {
        moonshine,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        ..OfflineModelConfig::default()
    };

    OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    }
}

/// Build the `OfflineRecognizerConfig` for a `NeMo` Parakeet-TDT model.
///
/// Uses sherpa-onnx's offline transducer config (4 files: encoder,
/// decoder, joiner, tokens) with `model_type = "nemo_transducer"`.
/// The `model_type` field is required — without it, sherpa-onnx tries
/// to use the generic transducer decode loop which doesn't understand
/// `NeMo`'s TDT (Token-and-Duration Transducer) joiner output shape.
///
/// Mirrors the upstream `rust-api-examples/examples/nemo_parakeet.rs`
/// example.
pub(super) fn build_nemo_transducer_recognizer_config(
    model: SherpaModel,
    provider: &str,
) -> OfflineRecognizerConfig {
    // `ModelFilePaths::Transducer` also matches `StreamingZipformerEn`
    // (same 4-file layout), so the destructuring alone wouldn't catch
    // a caller that passed the online Zipformer variant by mistake.
    // Guard on kind at the boundary so misuse fails loudly here
    // rather than silently building a NeMo config around Zipformer
    // files at runtime.
    debug_assert_eq!(
        model.kind(),
        crate::sherpa_model::ModelKind::OfflineNemoTransducer,
        "build_nemo_transducer_recognizer_config called with non-OfflineNemoTransducer model"
    );

    let ModelFilePaths::Transducer {
        encoder,
        decoder,
        joiner,
        tokens,
    } = sherpa_model::model_file_paths(model)
    else {
        unreachable!(
            "offline::build_nemo_transducer_recognizer_config called with non-Transducer layout"
        )
    };

    let transducer = OfflineTransducerModelConfig {
        encoder: Some(encoder.to_string_lossy().into_owned()),
        decoder: Some(decoder.to_string_lossy().into_owned()),
        joiner: Some(joiner.to_string_lossy().into_owned()),
    };

    let model_config = OfflineModelConfig {
        transducer,
        tokens: Some(tokens.to_string_lossy().into_owned()),
        provider: Some(provider.to_owned()),
        num_threads: SHERPA_NUM_THREADS,
        // Required — tells sherpa-onnx to use NeMo's TDT decode loop
        // instead of the generic transducer path.
        model_type: Some(NEMO_TRANSDUCER_MODEL_TYPE.to_owned()),
        ..OfflineModelConfig::default()
    };

    OfflineRecognizerConfig {
        model_config,
        ..OfflineRecognizerConfig::default()
    }
}

/// One offline transcription session. Dispatches to the VAD or Auto Break
/// implementation based on `params.segmentation_mode`.
///
/// Runs on the sherpa-host worker thread. The session spawns a second
/// "session I/O" thread that owns the audio channel, the state machine,
/// and (for VAD mode) a freshly-constructed Silero. The host thread then
/// drains a decode-request channel and runs `OfflineRecognizer::decode`
/// on each segment the I/O thread forwards. This decouples inference
/// latency from audio intake so a slow decode never backpressures the
/// DSP → transcription channel (issue #275).
pub(super) fn run_session(recognizer: &OfflineRecognizer, params: SessionParams) {
    match params.segmentation_mode {
        crate::backend::SegmentationMode::Vad => {
            run_session_vad(recognizer, params);
        }
        crate::backend::SegmentationMode::AutoBreak => {
            run_session_auto_break(recognizer, params);
        }
    }
}

/// VAD-driven offline session. Spawns a session I/O thread that builds
/// its own Silero and runs the VAD state machine; the current (host)
/// thread runs [`decoder_service_loop`] and performs the actual
/// `OfflineRecognizer::decode` calls for each segment the I/O thread
/// forwards.
///
/// Silero is built on the I/O thread (not the host thread) because
/// `SherpaSileroVad` is `!Send`, so owning it per-thread is simpler
/// than smuggling an `&mut` across the thread boundary. The ~50 ms
/// construction cost per session start is imperceptible next to the
/// model's own init time.
fn run_session_vad(recognizer: &OfflineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold,
        segmentation_mode: _,
        auto_break_thresholds: _,
    } = params;

    let (decode_tx, decode_rx) = mpsc::channel::<DecodeRequest>();

    // Spawn the session I/O thread. Owns the audio channel, builds its
    // own Silero VAD, and forwards ready-to-decode segments to the
    // decoder via `decode_tx`.
    let cancel_io = Arc::clone(&cancel);
    let event_tx_io = event_tx.clone();
    let io_thread = std::thread::Builder::new()
        .name("sherpa-session-io".into())
        .spawn(move || {
            session_io_loop_vad(SessionIoVadParams {
                cancel: cancel_io,
                audio_rx,
                event_tx: event_tx_io,
                decode_tx,
                noise_gate_ratio,
                vad_threshold,
            });
        });
    let io_thread = match io_thread {
        Ok(handle) => handle,
        Err(e) => {
            let msg = format!("failed to spawn sherpa session I/O thread: {e}");
            tracing::error!(%msg);
            let _ = event_tx.send(TranscriptionEvent::Error(msg));
            return;
        }
    };

    // Host thread: drain decode_rx and run `recognizer.decode` for each.
    // Returns when the I/O thread drops `decode_tx` (audio channel
    // disconnected or user cancelled).
    decoder_service_loop(recognizer, &decode_rx, &event_tx, &cancel);

    // The I/O thread is exiting or has exited. Join to avoid leaving a
    // detached worker behind; log on join failure but don't propagate
    // further since the session is ending anyway.
    if let Err(e) = io_thread.join() {
        tracing::warn!("sherpa session I/O thread panicked during join: {e:?}");
    }
    tracing::info!("sherpa offline session ended");
}

/// Parameters for the VAD-mode session I/O thread.
struct SessionIoVadParams {
    cancel: Arc<AtomicBool>,
    audio_rx: mpsc::Receiver<TranscriptionInput>,
    event_tx: mpsc::Sender<TranscriptionEvent>,
    decode_tx: mpsc::Sender<DecodeRequest>,
    noise_gate_ratio: f32,
    vad_threshold: f32,
}

/// Session I/O loop for VAD segmentation. Runs on the spawned I/O
/// thread — owns the Silero VAD and drains the audio channel, pushing
/// each completed segment onto `decode_tx` for the host-thread
/// decoder service.
fn session_io_loop_vad(params: SessionIoVadParams) {
    let SessionIoVadParams {
        cancel,
        audio_rx,
        event_tx,
        decode_tx,
        noise_gate_ratio,
        vad_threshold,
    } = params;

    // Build Silero on this thread. `SherpaSileroVad` holds an onnxruntime
    // session handle that is !Send by default, so we construct it here
    // rather than passing it in from the host thread.
    let vad_path = sherpa_model::silero_vad_path();
    let mut vad = match SherpaSileroVad::new(&vad_path, vad_threshold) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("Silero VAD creation failed on session start: {e}");
            tracing::error!(%msg);
            let _ = event_tx.send(TranscriptionEvent::Error(msg));
            return;
        }
    };

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let mut mono_buf: Vec<f32> = Vec::with_capacity(SESSION_MONO_BUFFER_CAPACITY);

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa offline session I/O thread cancelled");
            drain_vad_on_exit(&mut vad, &decode_tx);
            return;
        }

        let input = match audio_rx.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let interleaved = match input {
            TranscriptionInput::Samples(s) => s,
            TranscriptionInput::SquelchOpened | TranscriptionInput::SquelchClosed => continue,
        };

        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(&mut vad, &decode_tx);
                return;
            }
            if let TranscriptionInput::Samples(s) = extra {
                resampler::downsample_stereo_to_mono_16k(&s, &mut mono_buf);
            }
        }

        if mono_buf.is_empty() {
            continue;
        }

        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        vad.accept(&mono_buf);

        while let Some(segment) = vad.pop_segment() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(&mut vad, &decode_tx);
                return;
            }
            // Send non-blocking-wise: the decode channel is unbounded so
            // this never blocks — worst case the decoder falls behind
            // and memory grows, but the real-world queue depth is tiny
            // (one in-flight decode + a handful queued).
            if decode_tx.send(DecodeRequest { mono: segment }).is_err() {
                // Host thread exited early — nothing more to do.
                return;
            }
        }
    }

    drain_vad_on_exit(&mut vad, &decode_tx);
    tracing::info!("sherpa offline session I/O thread ended (audio channel disconnected)");
}

/// Batch-decode a single speech segment and emit a `Text` event if
/// the recognizer produced any text. Called by [`decoder_service_loop`]
/// on the sherpa-host thread — never on the session I/O thread.
fn decode_segment(
    recognizer: &OfflineRecognizer,
    segment: &[f32],
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let stream = recognizer.create_stream();
    stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, segment);
    recognizer.decode(&stream);
    let Some(result) = stream.get_result() else {
        return;
    };
    let text = result.text.trim().to_owned();
    if !text.is_empty() {
        let timestamp = crate::util::wall_clock_timestamp();
        tracing::debug!(%timestamp, %text, "moonshine committed utterance");
        let _ = event_tx.send(TranscriptionEvent::Text { timestamp, text });
    }
}

/// The three possible outcomes of a `HoldingOff` tail-timer expiration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushDecision {
    /// Buffer is a valid utterance — decode and emit.
    Decode,
    /// Buffer is too short to decode reliably (sub-word fragment).
    DiscardShort,
    /// Buffer is too short to even be a real transmission (phantom open).
    DiscardPhantom,
}

/// Internal state of the Auto Break segmentation machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoBreakState {
    /// No transmission in progress. Samples are discarded.
    Idle,
    /// Squelch is open, buffering the active transmission.
    Recording,
    /// Squelch recently closed; still buffering trailing audio until
    /// the tail timer expires, at which point we flush.
    HoldingOff,
}

/// Pure state machine for Auto Break segmentation. Holds no I/O handles
/// so it can be unit-tested. The real session loop owns one of these
/// and drives it from the `TranscriptionInput` channel + a `recv_timeout`
/// tail timer — on flush, the loop calls `take_buffer` and hands the
/// audio off to `decode_segment`.
struct AutoBreakMachine {
    state: AutoBreakState,
    /// Accumulated stereo interleaved f32 samples at 48 kHz.
    buffer: Vec<f32>,
    /// Snapshot of `buffer.len()` at the instant the squelch transitioned
    /// from `Recording` → `HoldingOff`. Used by `on_tail_timeout` to
    /// evaluate the phantom/short/decode thresholds against the *actual*
    /// transmission length, NOT the tail-extended buffer — otherwise a
    /// 200–399 ms transmission would cross the 400 ms `MIN_SEGMENT`
    /// threshold once the fixed 200 ms tail is included, and a sub-100 ms
    /// phantom open would cross `MIN_OPEN_MS`. Semantic meaning only
    /// applies during `HoldingOff`; reset to 0 in every other state
    /// transition.
    closed_len_samples: usize,
    /// Per-session timing parameters read from `SessionParams`.
    /// Previously hardcoded as module constants in PR 8; now threaded
    /// through from `BackendConfig` so the UI can tune them per-session
    /// (see issue #272).
    thresholds: super::host::AutoBreakThresholds,
}

impl AutoBreakMachine {
    fn new(thresholds: super::host::AutoBreakThresholds) -> Self {
        Self {
            state: AutoBreakState::Idle,
            buffer: Vec::new(),
            closed_len_samples: 0,
            thresholds,
        }
    }

    /// Raw buffer duration in ms, assuming the wire format is
    /// `TRANSCRIPTION_INPUT_CHANNELS`-interleaved f32 at
    /// `TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ`.
    ///
    /// Used by the max-segment safety cap check in the session loop
    /// (which cares about actual buffered memory, not semantic
    /// transmission length). For the phantom/short/decode decisions in
    /// `on_tail_timeout` and the drain-on-exit helper, use
    /// [`Self::transmission_duration_ms`] instead.
    #[allow(clippy::cast_possible_truncation)]
    fn buffer_duration_ms(&self) -> u32 {
        let frames = self.buffer.len() / TRANSCRIPTION_INPUT_CHANNELS;
        ((frames as u64 * 1000) / TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ) as u32
    }

    /// Semantic "how long was the transmission" in ms — the length of
    /// the audio the recognizer SHOULD see as one utterance, ignoring
    /// the tail-capture window that's applied after the squelch closes.
    ///
    ///   - `Idle`: 0 (no transmission)
    ///   - `Recording`: full buffer (the close event hasn't fired yet
    ///     so the snapshot doesn't exist; the current buffer IS the
    ///     transmission length so far)
    ///   - `HoldingOff`: pre-close snapshot (`closed_len_samples`) so
    ///     the 200 ms tail doesn't inflate the count past a threshold
    #[allow(clippy::cast_possible_truncation)]
    fn transmission_duration_ms(&self) -> u32 {
        let samples = match self.state {
            AutoBreakState::Idle => 0,
            AutoBreakState::Recording => self.buffer.len(),
            AutoBreakState::HoldingOff => self.closed_len_samples,
        };
        let frames = samples / TRANSCRIPTION_INPUT_CHANNELS;
        ((frames as u64 * 1000) / TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ) as u32
    }

    fn on_samples(&mut self, samples: &[f32]) {
        if matches!(
            self.state,
            AutoBreakState::Recording | AutoBreakState::HoldingOff
        ) {
            self.buffer.extend_from_slice(samples);
        }
        // Idle: discard
    }

    fn on_squelch_opened(&mut self) {
        match self.state {
            AutoBreakState::Idle => {
                self.buffer.clear();
                self.closed_len_samples = 0;
                self.state = AutoBreakState::Recording;
            }
            AutoBreakState::HoldingOff => {
                // Hysteresis blip — cancel deferred flush, stay with
                // the same buffer. Clear the snapshot so the NEXT
                // close event captures the full "has been continuously
                // open since this blip" length rather than inheriting
                // a stale value from the previous close.
                self.closed_len_samples = 0;
                self.state = AutoBreakState::Recording;
            }
            AutoBreakState::Recording => {
                // Redundant; ignore.
            }
        }
    }

    fn on_squelch_closed(&mut self) {
        if matches!(self.state, AutoBreakState::Recording) {
            // Snapshot buffer length at the moment the squelch closed.
            // `on_tail_timeout` uses this to evaluate discard
            // thresholds against the actual transmission length, not
            // the tail-extended buffer length.
            self.closed_len_samples = self.buffer.len();
            self.state = AutoBreakState::HoldingOff;
        }
    }

    /// Called when the tail timer expires while in `HoldingOff`. Returns
    /// the flush decision based on the *pre-close* transmission length,
    /// and resets to `Idle`. Returns `None` if called outside
    /// `HoldingOff` (no-op).
    fn on_tail_timeout(&mut self) -> Option<FlushDecision> {
        if !matches!(self.state, AutoBreakState::HoldingOff) {
            return None;
        }
        // Evaluate against the snapshot, not the current (tail-extended)
        // buffer. See the `closed_len_samples` docstring for why.
        let duration = self.transmission_duration_ms();
        let decision = if duration < self.thresholds.min_open_ms {
            FlushDecision::DiscardPhantom
        } else if duration < self.thresholds.min_segment_ms {
            FlushDecision::DiscardShort
        } else {
            FlushDecision::Decode
        };
        // Note: the caller is responsible for taking the buffer for
        // decoding AFTER this call, if the decision is Decode. The
        // caller gets the FULL (tail-extended) buffer even though the
        // decision was made against the pre-close snapshot — that's
        // deliberate: the 200 ms tail is captured audio we want the
        // recognizer to see, we just don't want it counted toward the
        // length gate.
        self.state = AutoBreakState::Idle;
        self.closed_len_samples = 0;
        if !matches!(decision, FlushDecision::Decode) {
            self.buffer.clear();
        }
        Some(decision)
    }

    /// Take ownership of the current buffer, leaving the machine's
    /// internal buffer empty. Used by the session loop to hand audio
    /// to the recognizer on flush.
    fn take_buffer(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.buffer)
    }

    /// Return current state (used by the session loop to decide
    /// whether to trigger the max-segment safety flush).
    fn state(&self) -> AutoBreakState {
        self.state
    }

    /// Clear the buffer and transition to `next_state` after a forced
    /// flush. Used by the max-segment safety cap path: the session loop
    /// takes the buffer via `take_buffer`, calls this to resume in the
    /// appropriate state, and then hands the taken buffer to the
    /// recognizer.
    ///
    /// **The caller chooses the next state deliberately**:
    ///
    ///   - Pass `AutoBreakState::Recording` from the max-segment safety
    ///     cap in the session loop's `Samples` handler — the squelch is
    ///     still open, the transmission is continuing, and we want the
    ///     30 s cap to SPLIT the transmission rather than truncate it.
    ///     Passing `Idle` here would strand the remainder of the
    ///     transmission until the next close→open edge, silently
    ///     dropping everything after the 30 s mark.
    ///   - Pass `AutoBreakState::Idle` from shutdown/drain paths where
    ///     the session is ending.
    fn reset_after_force_flush(&mut self, next_state: AutoBreakState) {
        self.buffer.clear();
        self.closed_len_samples = 0;
        self.state = next_state;
    }
}

/// Auto Break offline session. Spawns a session I/O thread that runs
/// the `AutoBreakMachine` against the audio channel; the current (host)
/// thread drains a decode-request channel and runs
/// `OfflineRecognizer::decode` on each flushed segment.
fn run_session_auto_break(recognizer: &OfflineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold: _,
        segmentation_mode: _,
        auto_break_thresholds,
    } = params;

    let (decode_tx, decode_rx) = mpsc::channel::<DecodeRequest>();

    let cancel_io = Arc::clone(&cancel);
    let event_tx_io = event_tx.clone();
    let io_thread = std::thread::Builder::new()
        .name("sherpa-session-io".into())
        .spawn(move || {
            session_io_loop_auto_break(SessionIoAutoBreakParams {
                cancel: cancel_io,
                audio_rx,
                event_tx: event_tx_io,
                decode_tx,
                noise_gate_ratio,
                auto_break_thresholds,
            });
        });
    let io_thread = match io_thread {
        Ok(handle) => handle,
        Err(e) => {
            let msg = format!("failed to spawn sherpa session I/O thread: {e}");
            tracing::error!(%msg);
            let _ = event_tx.send(TranscriptionEvent::Error(msg));
            return;
        }
    };

    decoder_service_loop(recognizer, &decode_rx, &event_tx, &cancel);

    if let Err(e) = io_thread.join() {
        tracing::warn!("sherpa session I/O thread panicked during join: {e:?}");
    }
    tracing::info!("sherpa Auto Break session ended");
}

/// Parameters for the Auto-Break-mode session I/O thread.
struct SessionIoAutoBreakParams {
    cancel: Arc<AtomicBool>,
    audio_rx: mpsc::Receiver<TranscriptionInput>,
    event_tx: mpsc::Sender<TranscriptionEvent>,
    decode_tx: mpsc::Sender<DecodeRequest>,
    noise_gate_ratio: f32,
    auto_break_thresholds: super::host::AutoBreakThresholds,
}

/// Session I/O loop for Auto Break segmentation. Runs on the spawned
/// I/O thread — drives an `AutoBreakMachine` from the audio channel and
/// forwards flushed segments to `decode_tx` (resampled + denoised
/// before the send so the decoder thread stays zero-prep).
fn session_io_loop_auto_break(params: SessionIoAutoBreakParams) {
    let SessionIoAutoBreakParams {
        cancel,
        audio_rx,
        event_tx,
        decode_tx,
        noise_gate_ratio,
        auto_break_thresholds,
    } = params;

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let tail_duration = std::time::Duration::from_millis(u64::from(auto_break_thresholds.tail_ms));
    let mut machine = AutoBreakMachine::new(auto_break_thresholds);
    let mut pending_flush_deadline: Option<std::time::Instant> = None;

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa Auto Break I/O thread cancelled");
            drain_auto_break_on_exit(&mut machine, noise_gate_ratio, &decode_tx);
            return;
        }

        let timeout = match pending_flush_deadline {
            Some(deadline) => deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_else(|| std::time::Duration::from_millis(0)),
            None => AUDIO_RECV_TIMEOUT,
        };

        match audio_rx.recv_timeout(timeout) {
            Ok(crate::backend::TranscriptionInput::Samples(samples)) => {
                machine.on_samples(&samples);
                // Max-segment safety check: see pre-refactor comment
                // above — resume in Recording, not Idle, so a long
                // transmission is split rather than truncated.
                if !matches!(machine.state(), AutoBreakState::Idle)
                    && machine.buffer_duration_ms() >= AUTO_BREAK_MAX_SEGMENT_MS
                {
                    tracing::warn!(
                        ms = machine.buffer_duration_ms(),
                        cap = AUTO_BREAK_MAX_SEGMENT_MS,
                        "Auto Break buffer exceeded max segment cap — forcing flush (check squelch configuration)"
                    );
                    let stereo_buf = machine.take_buffer();
                    machine.reset_after_force_flush(AutoBreakState::Recording);
                    if dispatch_auto_break_segment(&stereo_buf, noise_gate_ratio, &decode_tx)
                        .is_err()
                    {
                        return;
                    }
                    pending_flush_deadline = None;
                }
            }
            Ok(crate::backend::TranscriptionInput::SquelchOpened) => {
                machine.on_squelch_opened();
                pending_flush_deadline = None;
            }
            Ok(crate::backend::TranscriptionInput::SquelchClosed) => {
                machine.on_squelch_closed();
                pending_flush_deadline = Some(std::time::Instant::now() + tail_duration);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(deadline) = pending_flush_deadline
                    && std::time::Instant::now() >= deadline
                {
                    match machine.on_tail_timeout() {
                        Some(FlushDecision::Decode) => {
                            let stereo_buf = machine.take_buffer();
                            if dispatch_auto_break_segment(
                                &stereo_buf,
                                noise_gate_ratio,
                                &decode_tx,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                        Some(FlushDecision::DiscardPhantom) => {
                            tracing::debug!("Auto Break: discarded phantom open");
                        }
                        Some(FlushDecision::DiscardShort) => {
                            tracing::debug!("Auto Break: discarded sub-min segment");
                        }
                        None => {}
                    }
                    pending_flush_deadline = None;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("sherpa Auto Break I/O thread ended (channel disconnected)");
                drain_auto_break_on_exit(&mut machine, noise_gate_ratio, &decode_tx);
                return;
            }
        }
    }
}

/// Resample + denoise a completed Auto Break segment and hand it to
/// the decoder via `decode_tx`. Returns `Err(())` if the decoder
/// channel has hung up (host thread exited early) so the I/O loop
/// can stop cleanly instead of spinning on dead sends.
fn dispatch_auto_break_segment(
    stereo_buf: &[f32],
    noise_gate_ratio: f32,
    decode_tx: &mpsc::Sender<DecodeRequest>,
) -> Result<(), ()> {
    if stereo_buf.is_empty() {
        return Ok(());
    }
    let mut mono_buf: Vec<f32> =
        Vec::with_capacity(stereo_buf.len() / STEREO_48K_TO_MONO_16K_CAPACITY_DIVISOR);
    resampler::downsample_stereo_to_mono_16k(stereo_buf, &mut mono_buf);
    denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);
    decode_tx
        .send(DecodeRequest { mono: mono_buf })
        .map_err(|_| ())
}

/// Finalize an Auto Break session on cancellation or channel disconnect.
///
/// Mirrors the `drain_vad_on_exit` semantics from the VAD path: if the
/// user stops transcription mid-transmission (including the hard stop
/// triggered by a demod mode change), whatever is in the buffer is
/// either forwarded as a legitimate final utterance or discarded as a
/// sub-threshold fragment, applying the same length-gate rules the
/// tail timeout path uses. Without this, the final utterance was
/// silently thrown away whenever the session ended during `Recording`
/// or `HoldingOff`.
///
/// Runs on the session I/O thread — forwards via `decode_tx` for the
/// host thread to decode.
fn drain_auto_break_on_exit(
    machine: &mut AutoBreakMachine,
    noise_gate_ratio: f32,
    decode_tx: &mpsc::Sender<DecodeRequest>,
) {
    if matches!(machine.state(), AutoBreakState::Idle) {
        return;
    }

    let duration = machine.transmission_duration_ms();
    if duration < machine.thresholds.min_open_ms {
        tracing::debug!(
            ms = duration,
            "Auto Break: discarded phantom open on session exit"
        );
    } else if duration < machine.thresholds.min_segment_ms {
        tracing::debug!(
            ms = duration,
            "Auto Break: discarded sub-min segment on session exit"
        );
    } else {
        tracing::info!(
            ms = duration,
            "Auto Break: flushing in-flight segment on session exit"
        );
        let stereo_buf = machine.take_buffer();
        let _ = dispatch_auto_break_segment(&stereo_buf, noise_gate_ratio, decode_tx);
    }
    machine.reset_after_force_flush(AutoBreakState::Idle);
}

/// Flush the VAD on session exit and forward every remaining segment —
/// including any in-flight utterance Silero hadn't yet finalized.
///
/// Without the explicit `flush` call, a user stopping transcription
/// mid-speech would lose the last utterance because `pop_segment`
/// only returns segments that VAD already marked complete. `flush`
/// forces finalization so the final `while let` sees that segment.
///
/// Runs on the session I/O thread — forwards via `decode_tx` for the
/// host thread to decode.
fn drain_vad_on_exit(vad: &mut SherpaSileroVad, decode_tx: &mpsc::Sender<DecodeRequest>) {
    vad.flush();
    while let Some(segment) = vad.pop_segment() {
        if decode_tx.send(DecodeRequest { mono: segment }).is_err() {
            return;
        }
    }
    vad.reset();
}

#[cfg(test)]
#[derive(Debug, Default)]
struct AutoBreakFlushCounts {
    decodes_flushed: u32,
    discarded_short: u32,
    discarded_phantom: u32,
}

#[cfg(test)]
impl AutoBreakFlushCounts {
    fn record(&mut self, decision: FlushDecision) {
        match decision {
            FlushDecision::Decode => self.decodes_flushed += 1,
            FlushDecision::DiscardShort => self.discarded_short += 1,
            FlushDecision::DiscardPhantom => self.discarded_phantom += 1,
        }
    }
}

#[cfg(test)]
mod auto_break_tests {
    use super::*;

    // Build a test audio chunk corresponding to `ms` of stereo
    // interleaved silence at the wire format rate. Uses the same
    // constants the production buffer-duration math uses so the two
    // stay in sync if the wire format ever changes.
    fn samples_for_ms(ms: u32) -> Vec<f32> {
        let frames_per_ms = (TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ / 1000) as usize;
        let frames = frames_per_ms * (ms as usize);
        vec![0.5_f32; frames * TRANSCRIPTION_INPUT_CHANNELS]
    }

    // Default-threshold machine so the PR 8 test expectations around
    // the 100/200/400 ms thresholds still hold after the constants
    // moved to per-session fields in issue #272.
    fn default_machine() -> AutoBreakMachine {
        AutoBreakMachine::new(super::super::host::AutoBreakThresholds::defaults())
    }

    // Default-threshold alias so tests can phrase the intent as
    // "buffer should be ≥ MIN_OPEN" without reaching for the
    // session-level `crate::backend::AUTO_BREAK_*` re-exports each
    // time. Matches `AutoBreakThresholds::defaults().min_open_ms`.
    const TEST_MIN_OPEN_MS: u32 = crate::backend::AUTO_BREAK_MIN_OPEN_MS_DEFAULT;

    #[test]
    fn clean_utterance_produces_one_decode() {
        let mut machine = default_machine();
        let mut counts = AutoBreakFlushCounts::default();

        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(1_000));
        machine.on_squelch_closed();
        if let Some(decision) = machine.on_tail_timeout() {
            counts.record(decision);
        }

        assert_eq!(counts.decodes_flushed, 1);
        assert_eq!(counts.discarded_short, 0);
        assert_eq!(counts.discarded_phantom, 0);
    }

    #[test]
    fn hysteresis_blip_single_utterance() {
        let mut machine = default_machine();
        let mut counts = AutoBreakFlushCounts::default();

        // Open, record, close, re-open before tail timeout, record more, close, timeout.
        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(500));
        machine.on_squelch_closed();
        // Hysteresis blip: squelch re-opens before tail fires.
        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(500));
        machine.on_squelch_closed();
        if let Some(decision) = machine.on_tail_timeout() {
            counts.record(decision);
        }

        // One decode, not two — the blip should be absorbed into a single utterance.
        assert_eq!(counts.decodes_flushed, 1);
    }

    #[test]
    fn phantom_open_below_min_open_ms_discarded() {
        let mut machine = default_machine();
        let mut counts = AutoBreakFlushCounts::default();

        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(50)); // < MIN_OPEN_MS (100)
        machine.on_squelch_closed();
        if let Some(decision) = machine.on_tail_timeout() {
            counts.record(decision);
        }

        assert_eq!(counts.decodes_flushed, 0);
        assert_eq!(counts.discarded_phantom, 1);
    }

    #[test]
    fn sub_min_segment_discarded() {
        let mut machine = default_machine();
        let mut counts = AutoBreakFlushCounts::default();

        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(300)); // > MIN_OPEN (100) but < MIN_SEGMENT (400)
        machine.on_squelch_closed();
        if let Some(decision) = machine.on_tail_timeout() {
            counts.record(decision);
        }

        assert_eq!(counts.decodes_flushed, 0);
        assert_eq!(counts.discarded_short, 1);
    }

    #[test]
    fn max_segment_safety_cap_triggers_flush() {
        let mut machine = default_machine();

        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(31_000)); // > MAX_SEGMENT_MS (30_000)

        // At this point the machine should have buffer_duration_ms >= MAX,
        // so the external loop (or a check inside the state machine) should
        // treat it as a force-flush condition. We verify via the public API:
        assert!(machine.buffer_duration_ms() >= AUTO_BREAK_MAX_SEGMENT_MS);
        // The state machine itself doesn't flush on sample receipt — the
        // session loop driver checks the buffer duration after each
        // `on_samples` call and force-flushes. Test the take_buffer path.
        let buf = machine.take_buffer();
        assert!(
            !buf.is_empty(),
            "take_buffer should return the captured samples"
        );
    }

    #[test]
    fn tail_extension_does_not_inflate_discard_decision() {
        // Regression: a 300 ms transmission + 200 ms of tail-capture
        // samples (simulating the ~AUTO_BREAK_TAIL_MS of audio the
        // session loop buffers between SquelchClosed and the tail
        // timer expiration) pushes the raw buffer duration to 500 ms,
        // which would cross the 400 ms MIN_SEGMENT threshold. The
        // decision MUST still be DiscardShort because the actual
        // transmission was only 300 ms.
        let mut machine = default_machine();
        let mut counts = AutoBreakFlushCounts::default();

        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(300)); // 300 ms of open transmission
        machine.on_squelch_closed(); // Snapshot should fire here
        machine.on_samples(&samples_for_ms(200)); // 200 ms tail during HoldingOff

        // Raw buffer is now 500 ms, BUT transmission_duration_ms
        // should be the pre-close snapshot of 300 ms.
        assert_eq!(
            machine.buffer_duration_ms(),
            500,
            "raw buffer includes the tail-capture audio"
        );
        assert_eq!(
            machine.transmission_duration_ms(),
            300,
            "transmission duration reflects only the pre-close samples"
        );

        if let Some(decision) = machine.on_tail_timeout() {
            counts.record(decision);
        }

        assert_eq!(
            counts.decodes_flushed, 0,
            "sub-min transmission must NOT be decoded even though tail-extended buffer crossed the threshold"
        );
        assert_eq!(counts.discarded_short, 1);
    }

    #[test]
    fn phantom_open_with_tail_does_not_cross_min_open_threshold() {
        // Matching regression for the phantom-open lower bound. A 50 ms
        // open + 200 ms tail = 250 ms raw buffer, which would cross
        // MIN_OPEN_MS (100). The decision MUST still be DiscardPhantom.
        let mut machine = default_machine();
        let mut counts = AutoBreakFlushCounts::default();

        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(50)); // 50 ms open (phantom)
        machine.on_squelch_closed();
        machine.on_samples(&samples_for_ms(200)); // 200 ms tail

        assert_eq!(machine.transmission_duration_ms(), 50);
        assert!(machine.buffer_duration_ms() >= TEST_MIN_OPEN_MS);

        if let Some(decision) = machine.on_tail_timeout() {
            counts.record(decision);
        }

        assert_eq!(counts.decodes_flushed, 0);
        assert_eq!(
            counts.discarded_phantom, 1,
            "phantom open must still be discarded even with tail-inflated buffer"
        );
    }

    #[test]
    fn max_segment_safety_flush_resumes_recording_not_idle() {
        // Regression: after the 30 s safety cap fires on a carrier that
        // stays open, the state machine MUST resume in Recording (so
        // subsequent samples continue to buffer and the next cap splits
        // the transmission) rather than Idle (which would silently drop
        // all samples until the next close→open edge that never comes).
        let mut machine = default_machine();
        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(31_000)); // trigger safety cap

        // Simulate the session loop's force-flush path.
        let _ = machine.take_buffer();
        machine.reset_after_force_flush(AutoBreakState::Recording);

        assert_eq!(
            machine.state(),
            AutoBreakState::Recording,
            "safety cap must resume in Recording to split a long transmission"
        );
        assert_eq!(
            machine.buffer_duration_ms(),
            0,
            "buffer must be empty after reset"
        );

        // And subsequent samples should still be captured (proving the
        // resume state is effective, not just nominal).
        machine.on_samples(&samples_for_ms(1_000));
        assert_eq!(machine.buffer_duration_ms(), 1_000);
    }
}
