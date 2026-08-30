//! Scanner engine bridge — applies `sdr_scanner` command outputs to the
//! DSP state and reports the active channel to the UI.

use super::{
    DspState, DspToUi, ScannerMutexReason, auto_decimation_ratio, mpsc, on_tune_change,
    rebuild_vfo_echoing, stop_any_recording,
};

/// Apply scanner-emitted commands to the DSP state.
pub(super) fn apply_scanner_commands(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    commands: Vec<sdr_scanner::ScannerCommand>,
) {
    for cmd in commands {
        match cmd {
            sdr_scanner::ScannerCommand::Retune {
                freq_hz,
                demod_mode,
                bandwidth,
                ctcss,
                voice_squelch,
            } => {
                handle_scanner_retune(
                    state,
                    dsp_tx,
                    freq_hz,
                    demod_mode,
                    bandwidth,
                    ctcss,
                    voice_squelch,
                );
            }
            sdr_scanner::ScannerCommand::MuteAudio(muted) => {
                state.scanner_muted = muted;
            }
            sdr_scanner::ScannerCommand::ActiveChannelChanged(key) => {
                emit_scanner_active_channel(state, dsp_tx, key);
            }
            sdr_scanner::ScannerCommand::StateChanged(scanner_state) => {
                let _ = dsp_tx.send(DspToUi::ScannerStateChanged(scanner_state));
            }
            sdr_scanner::ScannerCommand::EmptyRotation => {
                let _ = dsp_tx.send(DspToUi::ScannerEmptyRotation);
            }
        }
    }
}

/// Apply one scanner `Retune` hop: center frequency, demod mode +
/// VFO rebuild, bandwidth, CTCSS, and voice squelch — mirroring the
/// manual `Tune` / `SetDemodMode` / `SetBandwidth` handlers so hops
/// leave the same persisted state behind. Split out of
/// [`apply_scanner_commands`] per CR on PR #841.
fn handle_scanner_retune(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    freq_hz: u64,
    demod_mode: sdr_types::DemodMode,
    bandwidth: f64,
    ctcss: Option<sdr_radio::af_chain::CtcssMode>,
    voice_squelch: Option<sdr_dsp::voice_squelch::VoiceSquelchMode>,
) {
    // Mirror the manual `Tune` / `SetDemodMode` /
    // `SetBandwidth` handlers so scanner hops end up
    // with the same persisted state + VFO rebuild
    // behavior. Omissions would leave `state.center_freq`
    // / `state.bandwidth` / the RxVfo config stale —
    // a subsequent `open_source()` restart would tune
    // back to whatever the user manually picked before
    // scanner started, and the IF filter width could
    // stay locked to the previous channel's mode.
    //
    // Deliberately NOT emitting the corresponding
    // `DspToUi::SampleRateChanged` / `DisplayBandwidth`
    // / `DemodModeChanged` / `BandwidthChanged` events
    // the manual handlers send — those are UI-sync
    // signals for user-initiated changes. Scanner
    // retunes carry their own `ScannerActiveChannelChanged`
    // payload with freq/mode/bandwidth/name that the
    // UI handler fans out to the same widgets; emitting
    // both paths would double-drive the sync.

    // Reset the squelch edge tracker AND re-arm the
    // auto-squelch noise-floor estimate for the new
    // channel. See `on_tune_change` for the full
    // rationale — both are critical: without the
    // edge reset a fresh `SquelchEdge::Open` would
    // be suppressed by a trailing-open state from
    // the previous channel (scanner invariant
    // `persistent_open_during_settle_goes_directly_to_listening`
    // relies on this); without the auto-squelch
    // re-arm the scanner inherits the previous
    // band's noise floor, which is the same bug
    // issue #374 describes for manual tunes.
    on_tune_change(state);

    // 1. Center frequency (mirrors `UiToDsp::Tune`).
    #[allow(clippy::cast_precision_loss)]
    let freq_f64 = freq_hz as f64;
    state.center_freq = freq_f64;
    if let Some(source) = state.source.as_mut()
        && let Err(e) = source.tune(freq_f64)
    {
        tracing::warn!(?e, "scanner retune: source.tune failed");
    }

    retune_demod_mode(state, dsp_tx, demod_mode, bandwidth);

    // 3. Bandwidth (mirrors `UiToDsp::SetBandwidth`).
    // Applied to the VFO channel filter first; only
    // persist on success. For same-mode retunes the
    // VFO already exists; for mode-change retunes the
    // rebuild above already used `state.bandwidth`
    // so the two paths converge.
    if let Some(vfo) = &mut state.vfo {
        match vfo.set_bandwidth(bandwidth) {
            Ok(()) => state.bandwidth = bandwidth,
            Err(e) => {
                tracing::warn!(?e, "scanner retune: VFO bandwidth update failed");
            }
        }
    } else {
        state.bandwidth = bandwidth;
    }
    state.radio.set_bandwidth(bandwidth);

    // 4. CTCSS is per-channel: force-Off when the new
    // channel doesn't carry a tone, otherwise a stale
    // tone gate would silence the new channel.
    let ctcss_mode = ctcss.unwrap_or(sdr_radio::af_chain::CtcssMode::Off);
    if let Err(e) = state.radio.set_ctcss_mode(ctcss_mode) {
        tracing::warn!(?e, "scanner retune: set_ctcss_mode failed");
    }
    // 5. Voice squelch is device-level — preserve
    // current setting when the channel doesn't
    // override it.
    if let Some(m) = voice_squelch
        && let Err(e) = state.radio.set_voice_squelch_mode(m)
    {
        tracing::warn!(?e, "scanner retune: set_voice_squelch_mode failed");
    }
}
/// Demod-mode half of [`handle_scanner_retune`]: on a mode change,
/// swap the demodulator, reset the generic audio tap, re-derive
/// decimation for the new IF rate, and rebuild the VFO. No-op for
/// same-mode hops. Split out per CR on PR #841.
fn retune_demod_mode(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    demod_mode: sdr_types::DemodMode,
    bandwidth: f64,
) {
    // 2. Demod mode + VFO rebuild on change (mirrors
    // `UiToDsp::SetDemodMode`). The scanner doesn't
    // emit retune commands redundantly — each Retune
    // marks a new channel — but the target mode may
    // equal the current mode (rotation pass on same-
    // mode channels), so guard to avoid gratuitous
    // rebuilds.
    let old_mode = state.radio.current_mode();
    if old_mode != demod_mode {
        if let Err(e) = state.radio.set_mode(demod_mode) {
            tracing::warn!(?e, "scanner retune: set_mode failed");
        } else {
            // Generic audio tap: same hard-boundary
            // treatment the `UiToDsp::SetDemodMode` path
            // applies. Scanner retunes deliberately
            // suppress `DemodModeChanged` to the UI
            // (per-hop chatter would be noise), which
            // means FFI tap consumers never see the
            // normal restart signal — so without this
            // reset, one audio stream would span mixed
            // demod outputs with stale 3:1 decimation
            // phase state. Mirrors the treatment at
            // L652-657 for the user-driven mode switch.
            state.audio_tap_tx = None;
            state.audio_tap_phase = 0;

            // Auto-adjust decimation for the new
            // demod's IF rate.
            let if_rate = state.radio.demod_config().if_sample_rate;
            let auto_decim = auto_decimation_ratio(state.sample_rate, if_rate);
            if auto_decim != state.frontend.decim_ratio()
                && let Err(e) = state.frontend.set_decimation(auto_decim)
            {
                tracing::warn!(?e, "scanner retune: auto-decimation failed");
            }
            // Rebuild the VFO for the new demod's IF
            // rate + bandwidth. Bandwidth is set
            // below; rebuild picks it up via
            // `state.bandwidth`.
            state.bandwidth = bandwidth;
            if let Err(e) = rebuild_vfo_echoing(state, dsp_tx) {
                tracing::warn!(?e, "scanner retune: VFO rebuild failed");
            }
        }
    }
}

/// Build the `ScannerActiveChannelChanged` payload by looking
/// up the full channel info for the given key in the cached
/// channel list.
///
/// If `key` is `Some(k)` but `k` isn't in `scanner_channels`
/// (a race between `UpdateScannerChannels` and
/// `ActiveChannelChanged`), we degrade to the idle-shape payload
/// (`key = None`, zeroed fields) rather than sending a non-None
/// key with zeroed freq/bandwidth/name — the UI can't tell
/// those apart from a valid zero-frequency channel, and the
/// resulting display would be incoherent (key says "active
/// channel X" but fields say "no channel"). A warning log
/// surfaces the cache miss so this stays diagnosable if it ever
/// fires in practice.
#[allow(
    clippy::needless_pass_by_value,
    reason = "owned key is passed from ScannerCommand::ActiveChannelChanged \
              and this helper decides whether it lands in the outgoing DspToUi \
              payload (cache hit) or gets logged + dropped (cache miss); \
              taking a reference would force callers to clone unnecessarily \
              on the common-case hit path"
)]
pub(super) fn emit_scanner_active_channel(
    state: &DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    key: Option<sdr_scanner::ChannelKey>,
) {
    let channel = key
        .as_ref()
        .and_then(|k| state.scanner_channels.iter().find(|c| c.key == *k).cloned());
    if key.is_some() && channel.is_none() {
        tracing::warn!(
            ?key,
            "scanner active-channel key not found in cached ScannerChannels — \
             degrading to idle payload; likely an UpdateScannerChannels race"
        );
    }
    let msg = match channel {
        Some(c) => DspToUi::ScannerActiveChannelChanged {
            freq_hz: c.key.frequency_hz,
            demod_mode: c.demod_mode,
            bandwidth: c.bandwidth,
            name: c.key.name.clone(),
            // CTCSS + voice-squelch mirror the channel record
            // verbatim (including `None`). The UI decides how to
            // interpret `None` — CTCSS forces the row to Off to
            // match the scanner's engine-side behavior;
            // voice-squelch leaves the row alone, also matching
            // the scanner's "no override → preserve" contract.
            ctcss: c.ctcss,
            voice_squelch: c.voice_squelch,
            key: Some(c.key),
        },
        None => DspToUi::ScannerActiveChannelChanged {
            freq_hz: 0,
            demod_mode: sdr_types::DemodMode::Nfm,
            bandwidth: 0.0,
            name: String::new(),
            ctcss: None,
            voice_squelch: None,
            key: None,
        },
    };
    let _ = dsp_tx.send(msg);
}

/// Whether the scanner should treat the squelch as "carrier present".
///
/// `IfChain::squelch_open()` reports `true` whenever the squelch is not
/// gating at all (manual off and auto off), because nothing is muting.
/// Handing that to the scanner as an `Open` edge latched it in
/// `Listening` on the first channel forever (#755): only a squelch that is
/// actually gating can signal activity.
pub(super) fn scanner_carrier_present(gating: bool, open: bool) -> bool {
    gating && open
}

/// Handler for `UiToDsp::SetScannerEnabled`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_scanner_enabled(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    enabled: bool,
) {
    // Reject scanner enable while ACARS is engaged. The
    // reverse direction (refusing ACARS engage while
    // scanner is running) was added in CR round 16; this
    // closes the symmetric hole. Without it, enabling
    // scanner mid-engagement would retune the source via
    // apply_scanner_commands and violate the airband-lock
    // invariants the round 14-15 UiToDsp guards protect.
    // CR round 17 on PR #584.
    if enabled && state.acars_pre_lock.is_some() {
        tracing::warn!("scanner enable rejected: ACARS airband lock is active");
        let _ = dsp_tx.send(DspToUi::Error(
            "Scanner enable ignored: ACARS airband lock is active. \
             Disable ACARS first."
                .to_string(),
        ));
        return;
    }
    // Symmetric to Orbcomm's own scanner-running refusal in
    // `orbcomm::engage_orbcomm` — the scanner mutates frontend
    // decimation directly (this handler, on demod-mode hops), which
    // would fight the decim=1 Orbcomm forces while enabled. Issue
    // #865, CR round 3 (smoke-test fix).
    if enabled && state.orbcomm_enabled {
        tracing::warn!("scanner enable rejected: Orbcomm decode is active");
        let _ = dsp_tx.send(DspToUi::Error(
            "Scanner enable ignored: Orbcomm decode is active. Disable Orbcomm first.".to_string(),
        ));
        return;
    }
    if enabled && stop_any_recording(state, dsp_tx) {
        let _ = dsp_tx.send(DspToUi::ScannerMutexStopped(
            ScannerMutexReason::RecordingStoppedForScanner,
        ));
    }
    // Without a gating squelch there is no carrier detection,
    // so the scanner can only hop on dwell timeouts and never
    // stop on activity. Tell the user instead of looking
    // broken. Per #755.
    if enabled && !state.radio.if_chain().squelch_active() {
        let _ = dsp_tx.send(DspToUi::Error(
            "Scanner: enable manual or auto squelch so it can detect activity; \
             without it the scanner will only cycle through channels."
                .to_string(),
        ));
    }
    // Scanner ↔ transcription mutex was REMOVED — the two
    // are designed to coexist (issue #517).
    let cmds = state
        .scanner
        .handle_event(sdr_scanner::ScannerEvent::SetEnabled(enabled));
    apply_scanner_commands(state, dsp_tx, cmds);
}
