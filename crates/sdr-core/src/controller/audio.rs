//! Audio/IQ recording helpers shared by the command handlers.

use super::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AudioSinkSlot, AudioSinkType, DspState, DspToUi,
    IQ_CHANNELS, NetworkSinkStatus, ScannerMutexReason, WavWriter, apply_scanner_commands, mpsc,
    stop_transcription,
};
use sdr_types::Protocol;

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

/// Handler for `UiToDsp::SetAudioSinkType`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_audio_sink_type(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    new_type: AudioSinkType,
) {
    tracing::info!(?new_type, "set audio sink type");
    if state.audio_sink_type == new_type {
        return;
    }
    // Snapshot the previous type so the post-swap
    // status logic can emit the correct "transitioning
    // away from network" event even when the
    // replacement local sink fails to start. Per
    // CodeRabbit round 2 on PR #351.
    let prev_type = state.audio_sink_type;
    // Stop the current sink so it releases its underlying
    // resource (audio device handle / socket) before we
    // construct the replacement.
    if let Err(e) = state.audio_sink.stop() {
        tracing::warn!("audio sink stop during type swap failed: {e}");
    }
    // Build the new sink.
    state.audio_sink = match new_type {
        AudioSinkType::Local => AudioSinkSlot::local_default(),
        AudioSinkType::Network => AudioSinkSlot::network(
            &state.network_sink_host,
            state.network_sink_port,
            state.network_sink_protocol,
        ),
    };
    state.audio_sink_type = new_type;
    // Re-apply the persisted local-device pick so the
    // post-swap Local sink routes to the user's last
    // choice instead of the system default. No-op for
    // Network.
    if matches!(new_type, AudioSinkType::Local)
        && let Err(e) = state.audio_sink.set_target(&state.audio_device_uid)
    {
        tracing::warn!("post-swap set_target failed: {e}");
    }
    start_swapped_sink(state, dsp_tx, new_type, prev_type);
}
/// Lifecycle half of [`handle_set_audio_sink_type`]: bring the
/// replacement sink online when the engine is running and emit the
/// real start/stop status transitions (per `CodeRabbit` rounds 1/2/6
/// on PR #351). Split out per the 50-NLOC gate (#816 PR B).
fn start_swapped_sink(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    new_type: AudioSinkType,
    prev_type: AudioSinkType,
) {
    // Bring the new sink online if the engine is already
    // running. Otherwise it'll start on the next Start
    // command — and we emit `Inactive` rather than
    // `Active` because the sink isn't really on the wire
    // yet. Per CodeRabbit round 1 on PR #351, status
    // events must reflect REAL lifecycle, not just the
    // user's selected type.
    if state.running {
        match state.audio_sink.start() {
            Ok(()) => {
                // Successful start clears the offline
                // latch so the audio write path resumes.
                state.audio_sink_offline = false;
                if matches!(new_type, AudioSinkType::Network) {
                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
                        endpoint: format!(
                            "{}:{}",
                            state.network_sink_host, state.network_sink_port
                        ),
                        protocol: state.network_sink_protocol,
                    }));
                } else {
                    // Switched away from network → that
                    // sink is no longer streaming. Emit
                    // Inactive so the panel's status row
                    // clears.
                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
                }
            }
            Err(e) => {
                // Latch so the next DSP block doesn't re-fire
                // the same terminal error against a stopped
                // sink. Per CodeRabbit round 6 on PR #351.
                state.audio_sink_offline = true;
                tracing::warn!("audio sink start after type swap failed: {e}");
                if matches!(new_type, AudioSinkType::Network) {
                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
                        message: format!("{e}"),
                    }));
                } else {
                    let _ = dsp_tx.send(DspToUi::Error(format!("Audio sink failed to start: {e}")));
                    // Even on failure, the network sink
                    // is gone — emit Inactive so the
                    // panel's status row clears its
                    // "Active" state. Per CodeRabbit
                    // round 2 on PR #351.
                    if matches!(prev_type, AudioSinkType::Network) {
                        let _ =
                            dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
                    }
                }
            }
        }
    } else {
        // Engine not running — nothing is on the wire.
        // Always emit Inactive so the panel doesn't
        // misreport a not-yet-bound sink as Active. The
        // matching Active will fire from the Start
        // handler if/when the user starts the engine.
        let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
    }
}

/// Handler for `UiToDsp::SetNetworkSinkConfig`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_network_sink_config(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    hostname: &str,
    port: u16,
    protocol: Protocol,
) {
    tracing::info!(%hostname, port, ?protocol, "set network sink config");
    // Persist on state so a future SetAudioSinkType swap
    // picks the new values up.
    hostname.clone_into(&mut state.network_sink_host);
    state.network_sink_port = port;
    state.network_sink_protocol = protocol;
    // If the network sink is currently selected, rebuild
    // it inline so the new endpoint takes effect now.
    // Status events fire only on the real start
    // outcome (Active on success, Error on failure,
    // Inactive when the engine isn't running yet) — per
    // CodeRabbit round 1 on PR #351.
    if matches!(state.audio_sink_type, AudioSinkType::Network) {
        if let Err(e) = state.audio_sink.stop() {
            tracing::warn!("network sink stop during reconfig failed: {e}");
        }
        state.audio_sink = AudioSinkSlot::network(hostname, port, protocol);
        if state.running {
            match state.audio_sink.start() {
                Ok(()) => {
                    state.audio_sink_offline = false;
                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
                        endpoint: format!("{hostname}:{port}"),
                        protocol,
                    }));
                }
                Err(e) => {
                    // Latch — per CodeRabbit round 6 on PR #351.
                    state.audio_sink_offline = true;
                    tracing::warn!("network sink restart after reconfig failed: {e}");
                    let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
                        message: format!("{e}"),
                    }));
                }
            }
        } else {
            // Engine not running — sink rebuilt but not
            // bound. Status stays Inactive.
            let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Inactive));
        }
    }
}

/// Handler for `UiToDsp::StartAudioRecording`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_start_audio_recording(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    path: std::path::PathBuf,
) {
    tracing::info!(?path, "start audio recording");
    // Open the writer FIRST. If it fails we want to leave
    // the scanner untouched — sending `ScannerMutexStopped`
    // before knowing the recording actually started would
    // visibly kill the scanner in the UI and misleadingly
    // tell the user recording started.
    match WavWriter::new(&path, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS) {
        Ok(writer) => {
            // Recording committed — now apply the mutex.
            // Scanner, per-hit recording, and transcription
            // are mutually exclusive in Phase 1.
            stop_scanner_for_recording(state, dsp_tx);
            // Recording ↔ transcription leg: stop any active
            // transcription tap so the two don't run concurrently.
            // `stop_transcription` is silent (no DspToUi event) —
            // the transcription lifecycle has no feedback channel
            // today, matching the existing DisableTranscription
            // path. UI-switch resync is a known follow-up.
            stop_transcription(state);
            state.audio_writer = Some(writer);
            let _ = dsp_tx.send(DspToUi::AudioRecordingStarted(path));
        }
        Err(e) => {
            tracing::warn!("failed to start audio recording: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Audio record failed: {e}")));
        }
    }
}

/// Handler for `UiToDsp::StartIqRecording`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_start_iq_recording(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    path: std::path::PathBuf,
) {
    tracing::info!(?path, "start IQ recording");
    // The header bakes in `state.sample_rate`, which is only
    // authoritative while a source is open (it is read back
    // from the hardware in `open_source`). With no source
    // there is no IQ to record anyway, and a writer opened
    // now would get whatever stale rate the last session
    // left behind (#695).
    if state.source.is_none() {
        tracing::warn!("IQ recording rejected: no source is running");
        let _ = dsp_tx.send(DspToUi::Error(
            "IQ record failed: press Play before recording IQ".to_string(),
        ));
        return;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let iq_rate = state.sample_rate as u32;
    // Open-first, apply-mutex-on-success — same rationale
    // as `StartAudioRecording` above.
    match WavWriter::new(&path, iq_rate, IQ_CHANNELS) {
        Ok(writer) => {
            stop_scanner_for_recording(state, dsp_tx);
            // Recording ↔ transcription mutex — see
            // StartAudioRecording for rationale.
            stop_transcription(state);
            state.iq_writer = Some(writer);
            let _ = dsp_tx.send(DspToUi::IqRecordingStarted(path));
        }
        Err(e) => {
            tracing::warn!("failed to start IQ recording: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("IQ record failed: {e}")));
        }
    }
}

/// Handler for `UiToDsp::EnableAudioTap`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_enable_audio_tap(
    state: &mut DspState,
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
) {
    // Generic audio tap — post-demod, pre-volume, resampled to
    // 16 kHz mono and dropped into `tx`. Distinct from the
    // transcription tap above so embedders receive
    // recognizer-ready samples without pulling in the
    // sdr-transcription dep.
    state.audio_tap_tx = Some(tx);
    // Reset the decimation phase so a new tap session starts
    // at clean 3:1 alignment — otherwise a stale phase from
    // a previous session (disabled, then re-enabled) would
    // desynchronize the 16 kHz timebase until the phase
    // wraps.
    state.audio_tap_phase = 0;
    tracing::info!("audio tap enabled");
}

/// Scanner ↔ recording mutex: stop a running scanner before a
/// recording starts and tell the UI why. Shared by the audio and IQ
/// recording paths (CR on PR #842).
fn stop_scanner_for_recording(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    if state.scanner.is_enabled() {
        let cmds = state
            .scanner
            .handle_event(sdr_scanner::ScannerEvent::SetEnabled(false));
        apply_scanner_commands(state, dsp_tx, cmds);
        let _ = dsp_tx.send(DspToUi::ScannerMutexStopped(
            ScannerMutexReason::ScannerStoppedForRecording,
        ));
    }
}

/// Handler for `UiToDsp::DisableAudioTap`, delegated from
/// `handle_command` (CR on PR #842).
pub(super) fn handle_disable_audio_tap(state: &mut DspState) {
    state.audio_tap_tx = None;
    tracing::info!("audio tap disabled");
}
