//! The shared decode service — the request type the session I/O
//! threads hand across, the host-thread service loop that drains
//! them through `OfflineRecognizer::decode`, and the stop-time
//! notification. Used by BOTH segmentation modes (VAD and Auto
//! Break). Split out of `offline.rs` per the file-size pass
//! (issue #820).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use sherpa_onnx::OfflineRecognizer;

use crate::backend::TranscriptionEvent;

use super::super::host::{AUDIO_RECV_TIMEOUT, SHERPA_SAMPLE_RATE_HZ};

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
/// (`pub(super)`: called by both session modules.) Returns nothing — results are pushed directly to `event_tx` as
/// `TranscriptionEvent::Text`.
pub(super) fn decoder_service_loop(
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
pub(super) fn emit_stop_notification(
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

/// Batch-decode a single speech segment and emit a `Text` event if
/// the recognizer produced any text. Called by [`decoder_service_loop`]
/// on the sherpa-host thread — never on the session I/O thread.
///
/// Diagnostic tracing added for issue #281 (Moonshine produces no text
/// while Parakeet produces correct text on the same audio). When run
/// with `RUST_LOG=sdr_transcription=debug`, every decode call emits a
/// `decode_segment` log event showing:
///
/// - `segment_len` — samples at 16 kHz mono, confirms the I/O thread is
///   actually forwarding segments to the decoder
/// - `got_result` — whether `stream.get_result()` returned `Some`
///   (recognizer produced an output object at all)
/// - `raw_text_len` — character count of the recognizer's text BEFORE
///   trim, so we can distinguish between "sherpa returned nothing" and
///   "sherpa returned whitespace-only output that trim wipes"
/// - `trimmed_text_len` — post-trim length, the one that actually drives
///   the `TranscriptionEvent::Text` emission
///
/// Once the root cause is nailed down the tracing will be downgraded or
/// removed — this is an investigation scaffold, not a permanent
/// observability layer.
fn decode_segment(
    recognizer: &OfflineRecognizer,
    segment: &[f32],
    event_tx: &mpsc::Sender<TranscriptionEvent>,
) {
    let segment_len = segment.len();
    let stream = recognizer.create_stream();
    stream.accept_waveform(SHERPA_SAMPLE_RATE_HZ, segment);
    recognizer.decode(&stream);

    let result = stream.get_result();
    let got_result = result.is_some();
    let Some(result) = result else {
        tracing::debug!(
            segment_len,
            got_result = false,
            "decode_segment: recognizer returned None"
        );
        return;
    };

    let raw_text = result.text;
    let raw_text_len = raw_text.len();
    let text = raw_text.trim().to_owned();
    let trimmed_text_len = text.len();

    tracing::debug!(
        segment_len,
        got_result,
        raw_text_len,
        trimmed_text_len,
        "decode_segment: recognizer returned result"
    );

    if !text.is_empty() {
        let timestamp = crate::util::wall_clock_timestamp();
        // Log metadata only — the raw transcript stays inside the
        // recognizer process boundary until the UI renders it. This
        // matches the established privacy contract for partials and
        // finals across the crate: we count characters for observability
        // but never write user speech to a log file that could be
        // collected by a `RUST_LOG=debug` run.
        tracing::debug!(
            %timestamp,
            text_chars = text.chars().count(),
            "offline recognizer committed utterance"
        );
        let _ = event_tx.send(TranscriptionEvent::Text { timestamp, text });
    }
}
