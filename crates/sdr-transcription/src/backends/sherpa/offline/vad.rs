//! VAD-driven offline session — the session I/O thread that owns a
//! per-session Silero VAD, plus the exit-drain that flushes the last
//! in-flight utterance. Split out of `offline.rs` per the file-size
//! pass (issue #820).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use sherpa_onnx::OfflineRecognizer;

use crate::backend::{TranscriptionEvent, TranscriptionInput};
use crate::sherpa_model;
use crate::vad::VoiceActivityDetector;
use crate::{denoise, resampler};

use super::super::host::{AUDIO_RECV_TIMEOUT, SessionParams};
use super::super::silero_vad::SherpaSileroVad;
use super::decode::{DecodeRequest, decoder_service_loop};

/// Initial capacity for the per-session resampled-mono scratch buffer.
const SESSION_MONO_BUFFER_CAPACITY: usize = 16_000;

/// VAD-driven offline session. Spawns a session I/O thread that builds
/// its own Silero and runs the VAD state machine; the current (host)
/// thread runs [`decoder_service_loop`] and performs the actual
/// `OfflineRecognizer::decode` calls for each segment the I/O thread
/// forwards.
///
/// (`pub(super)`: dispatched from the root `run_session`.)
///
/// Silero is built on the I/O thread (not the host thread) because
/// `SherpaSileroVad` is `!Send`, so owning it per-thread is simpler
/// than smuggling an `&mut` across the thread boundary. The ~50 ms
/// construction cost per session start is imperceptible next to the
/// model's own init time.
pub(super) fn run_session_vad(recognizer: &OfflineRecognizer, params: SessionParams) {
    let SessionParams {
        cancel,
        audio_rx,
        event_tx,
        noise_gate_ratio,
        vad_threshold,
        segmentation_mode: _,
        auto_break_thresholds: _,
        audio_enhancement,
    } = params;

    let (decode_tx, decode_rx) = mpsc::channel::<DecodeRequest>();

    // Spawn the session I/O thread. Owns the audio channel, builds its
    // own Silero VAD, and forwards ready-to-decode segments to the
    // decoder via `decode_tx`.
    let cancel_io = Arc::clone(&cancel);
    let event_tx_io = event_tx.clone();
    // Worker spawn + host-thread decoder service + join live in the
    // shared `run_with_session_io_worker` (Codacy clone detection on
    // PR #891); only the loop body differs per flavor.
    super::decode::run_with_session_io_worker(
        recognizer,
        &decode_rx,
        &event_tx,
        &cancel,
        move || {
            session_io_loop_vad(SessionIoVadParams {
                cancel: cancel_io,
                audio_rx,
                event_tx: event_tx_io,
                decode_tx,
                noise_gate_ratio,
                vad_threshold,
                audio_enhancement,
            });
        },
        "sherpa offline session ended",
    );
}

/// Parameters for the VAD-mode session I/O thread.
struct SessionIoVadParams {
    cancel: Arc<AtomicBool>,
    audio_rx: mpsc::Receiver<TranscriptionInput>,
    event_tx: mpsc::Sender<TranscriptionEvent>,
    decode_tx: mpsc::Sender<DecodeRequest>,
    noise_gate_ratio: f32,
    vad_threshold: f32,
    audio_enhancement: denoise::AudioEnhancement,
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
        audio_enhancement,
    } = params;

    let Some(mut vad) = init_session_vad(vad_threshold, &event_tx) else {
        return;
    };

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
        let outcome = process_vad_input(VadIntake {
            input,
            vad: &mut vad,
            mono_buf: &mut mono_buf,
            audio_rx: &audio_rx,
            cancel: &cancel,
            decode_tx: &decode_tx,
            noise_gate_ratio,
            audio_enhancement,
        });
        if outcome.is_break() {
            return;
        }
    }

    drain_vad_on_exit(&mut vad, &decode_tx);
    tracing::info!("sherpa offline session I/O thread ended (audio channel disconnected)");
}

/// Borrowed working set for one [`process_vad_input`] call — a named
/// bundle per the 8-parameter gate (#820; `SpectrumShared` /
/// `ClientSetupDeps` precedent).
struct VadIntake<'a> {
    input: TranscriptionInput,
    vad: &'a mut SherpaSileroVad,
    mono_buf: &'a mut Vec<f32>,
    audio_rx: &'a mpsc::Receiver<TranscriptionInput>,
    cancel: &'a Arc<AtomicBool>,
    decode_tx: &'a mpsc::Sender<DecodeRequest>,
    noise_gate_ratio: f32,
    audio_enhancement: denoise::AudioEnhancement,
}

/// Process one received input: resample + coalesce queued frames,
/// denoise, feed Silero, and forward finalized segments. `Break` =
/// the session is over (cancel fired — exit drain already run — or
/// the decoder hung up). Split out of [`session_io_loop_vad`] per
/// the 50-NLOC gate (#820; the loop was a pre-existing over-gate
/// function moved by the split).
fn process_vad_input(intake: VadIntake<'_>) -> std::ops::ControlFlow<()> {
    let VadIntake {
        input,
        vad,
        mono_buf,
        audio_rx,
        cancel,
        decode_tx,
        noise_gate_ratio,
        audio_enhancement,
    } = intake;
    let interleaved = match input {
        TranscriptionInput::Samples(s) => s,
        TranscriptionInput::SquelchOpened | TranscriptionInput::SquelchClosed => {
            return std::ops::ControlFlow::Continue(());
        }
    };

    mono_buf.clear();
    resampler::downsample_stereo_to_mono_16k(&interleaved, mono_buf);
    if coalesce_extra_frames(audio_rx, cancel, mono_buf).is_break() {
        drain_vad_on_exit(vad, decode_tx);
        return std::ops::ControlFlow::Break(());
    }

    if mono_buf.is_empty() {
        return std::ops::ControlFlow::Continue(());
    }

    denoise::apply(mono_buf, audio_enhancement, noise_gate_ratio);

    vad.accept(mono_buf);

    forward_ready_segments(vad, cancel, decode_tx)
}

/// Build the per-session Silero VAD and emit the `Ready` event.
/// `None` = construction failed (already logged + surfaced as an
/// `Error` event) or the event channel is gone — the caller returns
/// either way. Split out of [`session_io_loop_vad`] per the 50-NLOC
/// gate (#820; the loop was a pre-existing over-gate function moved
/// by the split).
fn init_session_vad(
    vad_threshold: f32,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) -> Option<SherpaSileroVad> {
    // Build Silero on this thread. `SherpaSileroVad` holds an onnxruntime
    // session handle that is !Send by default, so we construct it here
    // rather than passing it in from the host thread.
    let vad_path = sherpa_model::silero_vad_path();
    let vad = match SherpaSileroVad::new(&vad_path, vad_threshold) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("Silero VAD creation failed on session start: {e}");
            tracing::error!(%msg);
            let _ = event_tx.send(TranscriptionEvent::Error(msg));
            return None;
        }
    };

    if event_tx.send(TranscriptionEvent::Ready).is_err() {
        return None;
    }
    Some(vad)
}

/// Drain whatever else is already queued on the audio channel into
/// `mono_buf` so one VAD `accept` call sees the coalesced audio.
/// `Break` = cancel fired mid-drain; the caller runs the exit drain
/// and returns. Split out per the 50-NLOC gate (#820).
fn coalesce_extra_frames(
    audio_rx: &mpsc::Receiver<TranscriptionInput>,
    cancel: &Arc<AtomicBool>,
    mono_buf: &mut Vec<f32>,
) -> std::ops::ControlFlow<()> {
    while let Ok(extra) = audio_rx.try_recv() {
        if cancel.load(Ordering::Relaxed) {
            return std::ops::ControlFlow::Break(());
        }
        if let TranscriptionInput::Samples(s) = extra {
            resampler::downsample_stereo_to_mono_16k(&s, mono_buf);
        }
    }
    std::ops::ControlFlow::Continue(())
}

/// Forward every segment Silero has finalized to the decoder.
/// `Break` = the session is over (cancel fired — the VAD exit drain
/// has already run — or the decoder hung up). Split out per the
/// 50-NLOC gate (#820).
fn forward_ready_segments(
    vad: &mut SherpaSileroVad,
    cancel: &Arc<AtomicBool>,
    decode_tx: &mpsc::Sender<DecodeRequest>,
) -> std::ops::ControlFlow<()> {
    while let Some(segment) = vad.pop_segment() {
        if cancel.load(Ordering::Relaxed) {
            drain_vad_on_exit(vad, decode_tx);
            return std::ops::ControlFlow::Break(());
        }
        // Send non-blocking-wise: the decode channel is unbounded so
        // this never blocks — worst case the decoder falls behind
        // and memory grows, but the real-world queue depth is tiny
        // (one in-flight decode + a handful queued).
        if decode_tx.send(DecodeRequest { mono: segment }).is_err() {
            // Host thread exited early — nothing more to do.
            return std::ops::ControlFlow::Break(());
        }
    }
    std::ops::ControlFlow::Continue(())
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
