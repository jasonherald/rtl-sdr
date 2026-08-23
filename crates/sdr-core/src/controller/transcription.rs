//! Transcription tap helpers.

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
