//! Transcription tap helpers.

use super::{DspToUi, mpsc, stop_any_recording};

use super::DspState;

/// Stop the transcription tap. Returns `true` if it was active.
pub(super) fn stop_transcription(state: &mut DspState) -> bool {
    if state.transcription_tx.take().is_some() {
        // Mirror the reset from the explicit DisableTranscription
        // handler — next EnableTranscription starts fresh.
        // Scanner tracker stays intact (the two are independent
        // since the scanner ↔ transcription mutex was removed).
        state.transcription_squelch_was_open = false;
        true
    } else {
        false
    }
}

/// Handler for `UiToDsp::EnableTranscription`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_enable_transcription(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    tx: std::sync::mpsc::SyncSender<sdr_transcription::TranscriptionInput>,
) {
    // Scanner ↔ transcription mutex was REMOVED — the two
    // are designed to coexist (issue #517 emits per-channel
    // markers in the transcript log when the scanner hops).
    // Recording ↔ transcription mutex still applies because
    // a running WAV writer + concurrent transcription tap
    // produced inconsistent audio flow in earlier rounds.
    // Recording ↔ transcription leg of the mutex. Both
    // `stop_any_recording` sends cover the UI (it emits
    // `AudioRecordingStopped` / `IqRecordingStopped`), so
    // the recording buttons flip off automatically.
    stop_any_recording(state, dsp_tx);
    // Reset the TRANSCRIPTION-side squelch edge tracker when a
    // new tap is wired up. Without this, a previous session
    // that ended with squelch open leaves the tracker `true`,
    // so the first chunk of the new session sees `now_open ==
    // was_open` and no SquelchOpened edge is emitted — the
    // offline Auto Break state machine would stay in Idle and
    // drop the entire current transmission until the next
    // open/close cycle. The SCANNER tracker
    // (`squelch_was_open`) is intentionally NOT reset here:
    // doing so used to fire a spurious `ScannerEvent::SquelchEdge`
    // on the next block once the mutex was removed (the two
    // are now designed to coexist). Per CodeRabbit round 1 on
    // PR #558.
    state.transcription_squelch_was_open = false;
    state.transcription_tx = Some(tx);
    tracing::info!("transcription audio tap enabled");
}
