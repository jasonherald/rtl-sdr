//! Audio/IQ recording helpers shared by the command handlers.

use super::{DspState, DspToUi, mpsc};

/// Is audio or IQ recording currently active?
///
/// Used by future scanner tasks; suppress the unused-function lint
/// so it survives until the first call site arrives.
#[allow(dead_code)]
pub(super) fn recording_active(state: &DspState) -> bool {
    state.audio_writer.is_some() || state.iq_writer.is_some()
}

/// Stop any active recording. Returns `true` if anything was
/// actually stopped (caller emits a mutex-stopped event only in
/// that case, avoiding spurious toasts when scanner enables
/// with nothing to stop).
pub(super) fn stop_any_recording(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) -> bool {
    let mut stopped = false;
    if state.audio_writer.take().is_some() {
        let _ = dsp_tx.send(DspToUi::AudioRecordingStopped);
        stopped = true;
    }
    if state.iq_writer.take().is_some() {
        let _ = dsp_tx.send(DspToUi::IqRecordingStopped);
        stopped = true;
    }
    stopped
}

/// User-facing message for a failed recording write. The WAV size cap
/// (#694) is an expected end-of-file condition, not a fault, so it gets
/// its own wording.
pub(super) fn recording_write_error_message(kind: &str, err: &std::io::Error) -> String {
    // A full filesystem also arrives as `StorageFull`; only the writer's
    // typed marker means the WAV structural limit.
    if crate::wav_writer::is_wav_limit(err) {
        format!("{kind} recording stopped: WAV 4 GiB limit reached")
    } else {
        format!("{kind} recording write failed")
    }
}

/// Returns `true` (after warn-logging and toasting) when an IQ
/// recording is open. The WAV header committed to the sample rate at
/// start, so a rate change mid-recording would silently desync header
/// and data (#695). Mirrors [`acars_lock_rejects_geometry_change`].
pub(super) fn iq_recording_rejects_rate_change(
    state: &DspState,
    dsp_tx: &mpsc::Sender<crate::messages::DspToUi>,
    cmd_label: &str,
) -> bool {
    if state.iq_writer.is_some() {
        tracing::warn!(
            cmd = cmd_label,
            "IQ recording in progress: ignoring {cmd_label} command"
        );
        let _ = dsp_tx.send(crate::messages::DspToUi::Error(format!(
            "{cmd_label} ignored: an IQ recording is in progress. \
             Stop the recording to change the sample rate."
        )));
        return true;
    }
    false
}

/// Mono-downmix the radio's pre-gate audio into `buf` by averaging
/// L+R — equivalent to taking either channel for FM-demodulated
/// audio once any stereo pilot is filtered out. Pre-gate audio
/// because the speaker path zeroes on a closed power / CTCSS /
/// voice squelch, and the imaging subcarriers have no speech
/// cadence, so the gated buffer would feed the decoders black
/// lines on every fade (#734). Shared by the APT and SSTV taps
/// per CR on PR #841.
pub(super) fn downmix_pre_gate_mono(
    radio: &sdr_radio::RadioModule,
    audio_count: usize,
    buf: &mut Vec<f32>,
) {
    // `extend` over a `map` iterator is exact-size, so `Vec`'s
    // internal reserve is precise — no manual `reserve` needed.
    buf.clear();
    buf.extend(
        radio.pre_gate_audio()[..audio_count]
            .iter()
            .map(|s| f32::midpoint(s.l, s.r)),
    );
}
