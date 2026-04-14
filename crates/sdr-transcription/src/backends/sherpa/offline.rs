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

use std::sync::atomic::Ordering;
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

/// Squelch openings shorter than this are treated as noise spikes and
/// produce no segment. Chosen to exclude sub-syllable blips while still
/// catching short single-word transmissions ("copy").
const AUTO_BREAK_MIN_OPEN_MS: u32 = 100;

/// Continue buffering audio for this long after the squelch closes, so
/// the last syllable isn't chopped by a tight squelch-close timing.
/// Covers typical `PowerSquelch` fall time plus ~100 ms of spoken tail.
const AUTO_BREAK_TAIL_MS: u32 = 200;

/// Segments shorter than this are discarded instead of decoded.
/// Moonshine and Parakeet both hallucinate on sub-word fragments, so
/// dropping them is an accuracy improvement, not a loss.
const AUTO_BREAK_MIN_SEGMENT_MS: u32 = 400;

/// Safety cap: if squelch stays open longer than this, flush anyway.
/// Protects against pathological stuck-open situations (bad auto-squelch,
/// carrier jam, band opening) that would otherwise cause unbounded
/// memory growth in the segment buffer.
const AUTO_BREAK_MAX_SEGMENT_MS: u32 = 30_000;

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
pub(super) fn run_session(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    params: SessionParams,
) {
    match params.segmentation_mode {
        crate::backend::SegmentationMode::Vad => {
            run_session_vad(recognizer, vad, params);
        }
        crate::backend::SegmentationMode::AutoBreak => {
            run_session_auto_break(recognizer, params);
        }
    }
}

/// VAD-driven offline session (unchanged from the pre-Auto-Break behavior).
/// Feeds audio through the VAD, batch-decodes each detected speech segment,
/// and emits `Text` events. Never emits `Partial`.
fn run_session_vad(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    params: SessionParams,
) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold: _,
        segmentation_mode: _,
    } = params;

    // Clear any residual state from a previous session.
    vad.reset();

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let mut mono_buf: Vec<f32> = Vec::with_capacity(SESSION_MONO_BUFFER_CAPACITY);

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa offline session cancelled");
            drain_vad_on_exit(recognizer, vad, &event_tx);
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

        // Resample 48 kHz stereo → 16 kHz mono.
        mono_buf.clear();
        resampler::downsample_stereo_to_mono_16k(&interleaved, &mut mono_buf);

        while let Ok(extra) = audio_rx.try_recv() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(recognizer, vad, &event_tx);
                return;
            }
            if let TranscriptionInput::Samples(s) = extra {
                resampler::downsample_stereo_to_mono_16k(&s, &mut mono_buf);
            }
        }

        if mono_buf.is_empty() {
            continue;
        }

        // Spectral denoise BEFORE VAD — RTL-SDR squelch tails confuse
        // Silero just as much as they confuse decoders.
        denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);

        vad.accept(&mono_buf);

        while let Some(segment) = vad.pop_segment() {
            if cancel.load(Ordering::Relaxed) {
                drain_vad_on_exit(recognizer, vad, &event_tx);
                return;
            }
            decode_segment(recognizer, &segment, &event_tx);
        }
    }

    // Audio channel disconnected — flush any in-flight segment.
    drain_vad_on_exit(recognizer, vad, &event_tx);
    tracing::info!("sherpa offline session ended (audio channel disconnected)");
}

/// Batch-decode a single speech segment and emit a `Text` event if
/// the recognizer produced any text.
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
}

impl AutoBreakMachine {
    fn new() -> Self {
        Self {
            state: AutoBreakState::Idle,
            buffer: Vec::new(),
            closed_len_samples: 0,
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
        let decision = if duration < AUTO_BREAK_MIN_OPEN_MS {
            FlushDecision::DiscardPhantom
        } else if duration < AUTO_BREAK_MIN_SEGMENT_MS {
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

/// Auto Break offline session. Drives an `AutoBreakMachine` from the
/// transcription input channel and a hold-off timer implemented via
/// `recv_timeout`. Buffers stereo 48 kHz interleaved samples during
/// `Recording` / `HoldingOff` states; on flush, resamples to 16 kHz mono,
/// applies the spectral denoiser, and decodes through the recognizer.
fn run_session_auto_break(recognizer: &OfflineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold: _,
        segmentation_mode: _,
    } = params;

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return;
    }

    let tail_duration = std::time::Duration::from_millis(u64::from(AUTO_BREAK_TAIL_MS));
    let mut machine = AutoBreakMachine::new();
    // When Some, we're in HoldingOff and waiting until this instant before flushing.
    let mut pending_flush_deadline: Option<std::time::Instant> = None;

    loop {
        if cancel.load(Ordering::Relaxed) {
            tracing::info!("sherpa Auto Break session cancelled");
            drain_auto_break_on_exit(recognizer, &mut machine, noise_gate_ratio, &event_tx);
            return;
        }

        // Choose recv timeout: if we're holding off, use the remaining
        // tail duration so we wake up on time to flush. Otherwise use
        // the standard audio polling interval so we can check `cancel`.
        let timeout = match pending_flush_deadline {
            Some(deadline) => deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_else(|| std::time::Duration::from_millis(0)),
            None => AUDIO_RECV_TIMEOUT,
        };

        match audio_rx.recv_timeout(timeout) {
            Ok(crate::backend::TranscriptionInput::Samples(samples)) => {
                machine.on_samples(&samples);
                // Max-segment safety check: if buffer has grown past the
                // cap, force-flush regardless of squelch state.
                //
                // Resume in `Recording`, NOT `Idle`. The controller only
                // emits `SquelchOpened` on state transitions, so if the
                // carrier stays up past the 30 s cap and we transitioned
                // to Idle, the remainder of that same transmission would
                // be silently dropped until the next close→open edge.
                // Staying in Recording splits a long transmission into
                // 30 s chunks rather than truncating it.
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
                    flush_auto_break_segment(recognizer, &stereo_buf, noise_gate_ratio, &event_tx);
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
                // Check if the tail deadline has expired.
                if let Some(deadline) = pending_flush_deadline
                    && std::time::Instant::now() >= deadline
                {
                    match machine.on_tail_timeout() {
                        Some(FlushDecision::Decode) => {
                            let stereo_buf = machine.take_buffer();
                            flush_auto_break_segment(
                                recognizer,
                                &stereo_buf,
                                noise_gate_ratio,
                                &event_tx,
                            );
                        }
                        Some(FlushDecision::DiscardPhantom) => {
                            tracing::debug!("Auto Break: discarded phantom open");
                        }
                        Some(FlushDecision::DiscardShort) => {
                            tracing::debug!("Auto Break: discarded sub-min segment");
                        }
                        None => {
                            // Not in HoldingOff anymore — nothing to do.
                        }
                    }
                    pending_flush_deadline = None;
                }
                // Otherwise just loop back and recv again.
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("sherpa Auto Break session ended (channel disconnected)");
                drain_auto_break_on_exit(recognizer, &mut machine, noise_gate_ratio, &event_tx);
                return;
            }
        }
    }
}

/// Resample + denoise + decode a completed Auto Break segment, emit
/// a `Text` event if the recognizer produced non-empty output.
fn flush_auto_break_segment(
    recognizer: &OfflineRecognizer,
    stereo_buf: &[f32],
    noise_gate_ratio: f32,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    if stereo_buf.is_empty() {
        return;
    }
    let mut mono_buf: Vec<f32> =
        Vec::with_capacity(stereo_buf.len() / STEREO_48K_TO_MONO_16K_CAPACITY_DIVISOR);
    resampler::downsample_stereo_to_mono_16k(stereo_buf, &mut mono_buf);
    denoise::spectral_denoise(&mut mono_buf, noise_gate_ratio);
    decode_segment(recognizer, &mono_buf, event_tx);
}

/// Finalize an Auto Break session on cancellation or channel disconnect.
///
/// Mirrors the `drain_vad_on_exit` semantics from the VAD path: if the
/// user stops transcription mid-transmission (including the hard stop
/// triggered by a demod mode change), whatever is in the buffer is
/// either decoded as a legitimate final utterance or discarded as a
/// sub-threshold fragment, applying the same length-gate rules the tail
/// timeout path uses. Without this, the final utterance was silently
/// thrown away whenever the session ended during `Recording` or
/// `HoldingOff`.
fn drain_auto_break_on_exit(
    recognizer: &OfflineRecognizer,
    machine: &mut AutoBreakMachine,
    noise_gate_ratio: f32,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    if matches!(machine.state(), AutoBreakState::Idle) {
        // Nothing captured — nothing to drain.
        return;
    }

    // Use `transmission_duration_ms` here so the pre-close snapshot
    // governs the discard decision when we're caught in HoldingOff —
    // same reasoning as `on_tail_timeout`. In Recording state the
    // snapshot is unused and this falls back to the full buffer length.
    let duration = machine.transmission_duration_ms();
    if duration < AUTO_BREAK_MIN_OPEN_MS {
        tracing::debug!(
            ms = duration,
            "Auto Break: discarded phantom open on session exit"
        );
    } else if duration < AUTO_BREAK_MIN_SEGMENT_MS {
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
        flush_auto_break_segment(recognizer, &stereo_buf, noise_gate_ratio, event_tx);
    }
    // Session is ending — go back to Idle, not Recording, so a fresh
    // session starts from a clean state.
    machine.reset_after_force_flush(AutoBreakState::Idle);
}

/// Flush the VAD on session exit and drain every remaining segment —
/// including any in-flight utterance Silero hadn't yet finalized.
///
/// Without the explicit `flush` call, a user stopping transcription
/// mid-speech would lose the last utterance because `pop_segment`
/// only returns segments that VAD already marked complete. `flush`
/// forces finalization so the final `while let` sees that segment.
///
/// Resets the VAD afterward so the next session starts clean.
fn drain_vad_on_exit(
    recognizer: &OfflineRecognizer,
    vad: &mut SherpaSileroVad,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    vad.flush();
    while let Some(segment) = vad.pop_segment() {
        decode_segment(recognizer, &segment, event_tx);
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

    #[test]
    fn clean_utterance_produces_one_decode() {
        let mut machine = AutoBreakMachine::new();
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
        let mut machine = AutoBreakMachine::new();
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
        let mut machine = AutoBreakMachine::new();
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
        let mut machine = AutoBreakMachine::new();
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
        let mut machine = AutoBreakMachine::new();

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
        let mut machine = AutoBreakMachine::new();
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
        let mut machine = AutoBreakMachine::new();
        let mut counts = AutoBreakFlushCounts::default();

        machine.on_squelch_opened();
        machine.on_samples(&samples_for_ms(50)); // 50 ms open (phantom)
        machine.on_squelch_closed();
        machine.on_samples(&samples_for_ms(200)); // 200 ms tail

        assert_eq!(machine.transmission_duration_ms(), 50);
        assert!(machine.buffer_duration_ms() >= AUTO_BREAK_MIN_OPEN_MS);

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
        let mut machine = AutoBreakMachine::new();
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
