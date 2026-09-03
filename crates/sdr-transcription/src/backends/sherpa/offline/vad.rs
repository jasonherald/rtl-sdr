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
/// (`pub(super)`: dispatched from the root `run_session`.)\n/// Silero is built on the I/O thread (not the host thread) because
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
                audio_enhancement,
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

        denoise::apply(&mut mono_buf, audio_enhancement, noise_gate_ratio);

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
