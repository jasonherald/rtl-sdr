//! Auto Break offline session — the pure squelch-edge-driven
//! segmentation state machine, its timing constants, and the session
//! I/O loop that drives it. Split out of `offline.rs` per the
//! file-size pass (issue #820). Items the sibling `auto_break_tests`
//! module (declared in `offline.rs`) exercises are `pub(super)`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use sherpa_onnx::OfflineRecognizer;

use crate::backend::{TranscriptionEvent, TranscriptionInput};
use crate::{denoise, resampler};

use super::super::host::{AUDIO_RECV_TIMEOUT, SessionParams};
use super::decode::{DecodeRequest, decoder_service_loop};

/// Safety cap: if squelch stays open longer than this, flush anyway.
/// Protects against pathological stuck-open situations (bad auto-squelch,
/// carrier jam, band opening) that would otherwise cause unbounded
/// memory growth in the segment buffer.
///
/// NOTE: unlike the other Auto Break constants, this one is NOT user
/// tunable. It's a hard OOM safety guard, not a segmentation preference,
/// and exposing it would invite users to disable the protection.
pub(super) const AUTO_BREAK_MAX_SEGMENT_MS: u32 = 30_000;

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
pub(super) const TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ: u64 = 48_000;

/// Channel count of incoming `TranscriptionInput::Samples` frames
/// (interleaved stereo = 2 f32 values per audio frame).
pub(super) const TRANSCRIPTION_INPUT_CHANNELS: usize = 2;

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

/// The three possible outcomes of a `HoldingOff` tail-timer expiration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlushDecision {
    /// Buffer is a valid utterance — decode and emit.
    Decode,
    /// Buffer is too short to decode reliably (sub-word fragment).
    DiscardShort,
    /// Buffer is too short to even be a real transmission (phantom open).
    DiscardPhantom,
}

/// Internal state of the Auto Break segmentation machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AutoBreakState {
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
pub(super) struct AutoBreakMachine {
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
    thresholds: super::super::host::AutoBreakThresholds,
}

impl AutoBreakMachine {
    pub(super) fn new(thresholds: super::super::host::AutoBreakThresholds) -> Self {
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
    pub(super) fn buffer_duration_ms(&self) -> u32 {
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
    pub(super) fn transmission_duration_ms(&self) -> u32 {
        let samples = match self.state {
            AutoBreakState::Idle => 0,
            AutoBreakState::Recording => self.buffer.len(),
            AutoBreakState::HoldingOff => self.closed_len_samples,
        };
        let frames = samples / TRANSCRIPTION_INPUT_CHANNELS;
        ((frames as u64 * 1000) / TRANSCRIPTION_INPUT_SAMPLE_RATE_HZ) as u32
    }

    pub(super) fn on_samples(&mut self, samples: &[f32]) {
        if matches!(
            self.state,
            AutoBreakState::Recording | AutoBreakState::HoldingOff
        ) {
            self.buffer.extend_from_slice(samples);
        }
        // Idle: discard
    }

    pub(super) fn on_squelch_opened(&mut self) {
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

    pub(super) fn on_squelch_closed(&mut self) {
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
    pub(super) fn on_tail_timeout(&mut self) -> Option<FlushDecision> {
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
    pub(super) fn take_buffer(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.buffer)
    }

    /// Return current state (used by the session loop to decide
    /// whether to trigger the max-segment safety flush).
    pub(super) fn state(&self) -> AutoBreakState {
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
    pub(super) fn reset_after_force_flush(&mut self, next_state: AutoBreakState) {
        self.buffer.clear();
        self.closed_len_samples = 0;
        self.state = next_state;
    }
}

/// Auto Break offline session. Spawns a session I/O thread that runs
/// the `AutoBreakMachine` against the audio channel; the current (host)
/// thread drains a decode-request channel and runs
/// `OfflineRecognizer::decode` on each flushed segment.
pub(super) fn run_session_auto_break(recognizer: &OfflineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold: _,
        segmentation_mode: _,
        auto_break_thresholds,
        audio_enhancement,
    } = params;

    let (decode_tx, decode_rx) = mpsc::channel::<DecodeRequest>();

    let cancel_io = Arc::clone(&cancel);
    let event_tx_io = event_tx.clone();
    // Shared spawn/decoder/join plumbing — see
    // `decode::run_with_session_io_worker` (Codacy clone detection
    // on PR #891).
    super::decode::run_with_session_io_worker(
        recognizer,
        &decode_rx,
        &event_tx,
        &cancel,
        move || {
            session_io_loop_auto_break(SessionIoAutoBreakParams {
                cancel: cancel_io,
                audio_rx,
                event_tx: event_tx_io,
                decode_tx,
                noise_gate_ratio,
                auto_break_thresholds,
                audio_enhancement,
            });
        },
        "sherpa Auto Break session ended",
    );
}

/// Parameters for the Auto-Break-mode session I/O thread.
pub(super) struct SessionIoAutoBreakParams {
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) audio_rx: mpsc::Receiver<TranscriptionInput>,
    pub(super) event_tx: mpsc::Sender<TranscriptionEvent>,
    pub(super) decode_tx: mpsc::Sender<DecodeRequest>,
    pub(super) noise_gate_ratio: f32,
    pub(super) auto_break_thresholds: super::super::host::AutoBreakThresholds,
    pub(super) audio_enhancement: denoise::AudioEnhancement,
}

/// Session I/O loop for Auto Break segmentation. Runs on the spawned
/// I/O thread — drives an `AutoBreakMachine` from the audio channel and
/// forwards flushed segments to `decode_tx` (resampled + denoised
/// before the send so the decoder thread stays zero-prep).
pub(super) fn session_io_loop_auto_break(params: SessionIoAutoBreakParams) {
    let SessionIoAutoBreakParams {
        cancel,
        audio_rx,
        event_tx,
        decode_tx,
        noise_gate_ratio,
        auto_break_thresholds,
        audio_enhancement,
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
            drain_auto_break_on_exit(
                &mut machine,
                noise_gate_ratio,
                audio_enhancement,
                &decode_tx,
            );
            return;
        }

        let timeout = recv_deadline_timeout(pending_flush_deadline);

        let event = audio_rx.recv_timeout(timeout);
        if handle_channel_event(
            event,
            &mut machine,
            tail_duration,
            noise_gate_ratio,
            audio_enhancement,
            &decode_tx,
            &mut pending_flush_deadline,
        )
        .is_break()
        {
            return;
        }
    }
}

/// The `recv_timeout` budget for the next loop tick: time left
/// until the pending tail-flush deadline, or the standard audio
/// poll interval when no flush is pending. Split out of
/// [`session_io_loop_auto_break`] per the 50-NLOC gate (#820).
fn recv_deadline_timeout(
    pending_flush_deadline: Option<std::time::Instant>,
) -> std::time::Duration {
    match pending_flush_deadline {
        Some(deadline) => deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_else(|| std::time::Duration::from_millis(0)),
        None => AUDIO_RECV_TIMEOUT,
    }
}

/// One channel event (or timeout) of the Auto Break session loop:
/// samples feed the machine (with the max-segment cap), squelch
/// edges drive the state transitions and the tail-timer deadline,
/// a timeout past the deadline flushes, and a disconnect drains and
/// ends the session. `Break` = session over (drain already run
/// where the pre-carve inline arms ran it; a decoder hang-up
/// mid-dispatch breaks without draining, exactly as before). Split
/// out of [`session_io_loop_auto_break`] per the 50-NLOC gate
/// (#820; the loop was a pre-existing over-gate function moved by
/// the split — its `too_many_lines` allow is retired with the
/// carve).
fn handle_channel_event(
    event: Result<TranscriptionInput, mpsc::RecvTimeoutError>,
    machine: &mut AutoBreakMachine,
    tail_duration: std::time::Duration,
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
    decode_tx: &mpsc::Sender<DecodeRequest>,
    pending_flush_deadline: &mut Option<std::time::Instant>,
) -> std::ops::ControlFlow<()> {
    match event {
        Ok(TranscriptionInput::Samples(samples)) => handle_samples_arm(
            machine,
            &samples,
            noise_gate_ratio,
            audio_enhancement,
            decode_tx,
            pending_flush_deadline,
        ),
        Ok(TranscriptionInput::SquelchOpened) => {
            machine.on_squelch_opened();
            *pending_flush_deadline = None;
            std::ops::ControlFlow::Continue(())
        }
        Ok(TranscriptionInput::SquelchClosed) => {
            machine.on_squelch_closed();
            *pending_flush_deadline = Some(std::time::Instant::now() + tail_duration);
            std::ops::ControlFlow::Continue(())
        }
        Err(mpsc::RecvTimeoutError::Timeout) => handle_recv_timeout(
            machine,
            noise_gate_ratio,
            audio_enhancement,
            decode_tx,
            pending_flush_deadline,
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            tracing::info!("sherpa Auto Break I/O thread ended (channel disconnected)");
            drain_auto_break_on_exit(machine, noise_gate_ratio, audio_enhancement, decode_tx);
            std::ops::ControlFlow::Break(())
        }
    }
}

/// `Samples` arm of the Auto Break session loop: feed the machine,
/// then apply the max-segment safety cap — resume in `Recording`,
/// not `Idle`, so a long transmission is split rather than
/// truncated (see [`AutoBreakMachine::reset_after_force_flush`]).
/// Clears `pending_flush_deadline` after a forced flush, exactly as
/// the pre-carve inline arm did. `Break` = the decoder hung up.
/// Split out of [`session_io_loop_auto_break`] per the 50-NLOC gate
/// (#820; the loop was a pre-existing over-gate function moved by
/// the split — its `too_many_lines` allow is retired with the
/// carve).
pub(super) fn handle_samples_arm(
    machine: &mut AutoBreakMachine,
    samples: &[f32],
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
    decode_tx: &mpsc::Sender<DecodeRequest>,
    pending_flush_deadline: &mut Option<std::time::Instant>,
) -> std::ops::ControlFlow<()> {
    machine.on_samples(samples);
    if !matches!(machine.state(), AutoBreakState::Idle)
        && machine.buffer_duration_ms() >= AUTO_BREAK_MAX_SEGMENT_MS
    {
        tracing::warn!(
            ms = machine.buffer_duration_ms(),
            cap = AUTO_BREAK_MAX_SEGMENT_MS,
            "Auto Break buffer exceeded max segment cap — forcing flush (check squelch configuration)"
        );
        let stereo_buf = machine.take_buffer();
        // Resume in the state we were in: `Recording` splits a long
        // open transmission (split-not-truncate, as before), while
        // `HoldingOff` keeps the already-observed close edge so the
        // pending tail flush still finalizes the transmission.
        // Hard-coding `Recording` here lost that edge — the machine
        // stayed `Recording` with the squelch closed, buffering dead
        // air and dispatching a silence segment every cap interval
        // (`CodeRabbit` round 1 on PR #891; pre-existing, moved
        // verbatim by the split).
        let resume_state = machine.state();
        machine.reset_after_force_flush(resume_state);
        if dispatch_auto_break_segment(&stereo_buf, noise_gate_ratio, audio_enhancement, decode_tx)
            .is_err()
        {
            return std::ops::ControlFlow::Break(());
        }
        // The tail deadline only exists in `HoldingOff` — keep it
        // there so `on_tail_timeout` still runs for this
        // transmission; clear it on the `Recording` split path as
        // before.
        if matches!(resume_state, AutoBreakState::Recording) {
            *pending_flush_deadline = None;
        }
    }
    std::ops::ControlFlow::Continue(())
}

/// `Timeout` arm of [`handle_channel_event`]: when the tail-flush
/// deadline has passed, run the flush decision and clear the
/// deadline — byte-equivalent to the pre-carve inline arm. `Break`
/// = the decoder hung up. Split out per the 50-NLOC gate (#820).
fn handle_recv_timeout(
    machine: &mut AutoBreakMachine,
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
    decode_tx: &mpsc::Sender<DecodeRequest>,
    pending_flush_deadline: &mut Option<std::time::Instant>,
) -> std::ops::ControlFlow<()> {
    if let Some(deadline) = *pending_flush_deadline
        && std::time::Instant::now() >= deadline
    {
        if handle_tail_expiry(machine, noise_gate_ratio, audio_enhancement, decode_tx).is_break() {
            return std::ops::ControlFlow::Break(());
        }
        *pending_flush_deadline = None;
    }
    std::ops::ControlFlow::Continue(())
}

/// Tail-timer expiry: ask the machine for its flush decision and
/// dispatch or discard accordingly. The caller clears the deadline
/// afterwards regardless of decision (matching the pre-carve inline
/// arm). `Break` = the decoder hung up. Split out per the 50-NLOC
/// gate (#820).
fn handle_tail_expiry(
    machine: &mut AutoBreakMachine,
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
    decode_tx: &mpsc::Sender<DecodeRequest>,
) -> std::ops::ControlFlow<()> {
    match machine.on_tail_timeout() {
        Some(FlushDecision::Decode) => {
            let stereo_buf = machine.take_buffer();
            if dispatch_auto_break_segment(
                &stereo_buf,
                noise_gate_ratio,
                audio_enhancement,
                decode_tx,
            )
            .is_err()
            {
                return std::ops::ControlFlow::Break(());
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
    std::ops::ControlFlow::Continue(())
}

/// Resample + denoise a completed Auto Break segment and hand it to
/// the decoder via `decode_tx`. Returns `Err(())` if the decoder
/// channel has hung up (host thread exited early) so the I/O loop
/// can stop cleanly instead of spinning on dead sends.
fn dispatch_auto_break_segment(
    stereo_buf: &[f32],
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
    decode_tx: &mpsc::Sender<DecodeRequest>,
) -> Result<(), ()> {
    if stereo_buf.is_empty() {
        return Ok(());
    }
    let mut mono_buf: Vec<f32> =
        Vec::with_capacity(stereo_buf.len() / STEREO_48K_TO_MONO_16K_CAPACITY_DIVISOR);
    resampler::downsample_stereo_to_mono_16k(stereo_buf, &mut mono_buf);
    denoise::apply(&mut mono_buf, audio_enhancement, noise_gate_ratio);
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
    audio_enhancement: denoise::AudioEnhancement,
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
        let _ = dispatch_auto_break_segment(
            &stereo_buf,
            noise_gate_ratio,
            audio_enhancement,
            decode_tx,
        );
    }
    machine.reset_after_force_flush(AutoBreakState::Idle);
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct AutoBreakFlushCounts {
    pub(super) decodes_flushed: u32,
    pub(super) discarded_short: u32,
    pub(super) discarded_phantom: u32,
}

#[cfg(test)]
impl AutoBreakFlushCounts {
    pub(super) fn record(&mut self, decision: FlushDecision) {
        match decision {
            FlushDecision::Decode => self.decodes_flushed += 1,
            FlushDecision::DiscardShort => self.discarded_short += 1,
            FlushDecision::DiscardPhantom => self.discarded_phantom += 1,
        }
    }
}
