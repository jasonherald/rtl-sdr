//! Source lifecycle — open/close, RTL-TCP rebuild/reconnect, persisted
//! tuner settings replay, and frontend/VFO rebuild helpers.

use super::{
    DEVICE_INDEX, DspState, DspToUi, IqFrontend, RTL_TCP_STATE_POLL_INTERVAL, RtlSdrSource,
    RtlTcpConnectionState, RxVfo, Source, SourceType, mpsc,
};

/// Poll the active source's projected `rtl_tcp_connection_state()`
/// and emit `DspToUi::RtlTcpConnectionState` on edge (state changed
/// since last emit). Throttled via `state.rtl_tcp_poll_at`.
///
/// Non-`RtlTcp` sources return `None` from the trait method — we
/// map that to `Disconnected` so the UI can track the absence
/// uniformly (source-type change → status row collapses without a
/// separate teardown signal).
pub(super) fn poll_rtl_tcp_connection_state(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    let now = std::time::Instant::now();
    if now < state.rtl_tcp_poll_at {
        return;
    }
    state.rtl_tcp_poll_at = now + RTL_TCP_STATE_POLL_INTERVAL;

    let current = state
        .source
        .as_ref()
        .and_then(|s| s.rtl_tcp_connection_state())
        .unwrap_or(RtlTcpConnectionState::Disconnected);

    // `RtlTcpConnectionState` derives PartialEq; Retrying variants
    // with a different `retry_in` compare unequal, so the poll
    // emits twice a second during the backoff wait. That's what we
    // want — the UI renders a live countdown without the status
    // text going stale between attempt-counter bumps.
    if state.last_rtl_tcp_state != current {
        state.last_rtl_tcp_state = current.clone();
        let _ = dsp_tx.send(DspToUi::RtlTcpConnectionState(current));
    }
}

/// Reset the per-tune engine state that MUST NOT carry over a
/// frequency / demod / bandwidth change:
///
/// 1. The controller-side squelch-edge trackers
///    (`state.squelch_was_open` for the scanner +
///    `state.transcription_squelch_was_open` for the offline
///    transcription tap) — so a fresh `SquelchEdge::Open` at
///    the new channel isn't suppressed by the previous
///    channel's trailing open state. Originally added for the
///    scanner retune path (PR #372 round 3); the same risk
///    applies to every manual tune / mode / bandwidth change.
///
/// 2. Auto-squelch noise-floor tracking
///    (`state.radio.rearm_auto_squelch`) — the floor estimate
///    settles over seconds; carrying it from one band to
///    another leaves the threshold pinned to the wrong value,
///    so the new channel hard-opens (old floor was louder) or
///    stays hard-closed (old floor was quieter). No-op when
///    auto-squelch is disabled. Per issue #374.
///
/// Call from every UI-origin retune site (`UiToDsp::Tune`,
/// `SetDemodMode`, `SetBandwidth`) and the scanner retune
/// path. Cheap — three field writes plus an `if`-guarded reset.
pub(super) fn on_tune_change(state: &mut DspState) {
    state.squelch_was_open = false;
    state.transcription_squelch_was_open = false;
    state.radio.rearm_auto_squelch();
}

/// Destroy the current `RtlTcpSource` and construct a fresh one
/// with the latest role / `auth_key` config from `DspState`, then
/// start it. Used by both `RetryRtlTcpNow` (ordinary manual
/// retry after an `AuthRequired` / `AuthFailed` / `ControllerBusy`
/// denial) and `RetryRtlTcpWithTakeover` (the #393 "Take
/// control" one-shot).
///
/// **Why rebuild instead of stop+start the existing source:** the
/// `ClientHello` is built at `with_config(...)` time from the
/// `RtlTcpConfig` passed to the constructor. Calling `start()` on
/// the same instance replays its original hello — a newly entered
/// auth key or a flipped role would never land on the wire until
/// a full source tear-down (Stop + Play, source-type switch).
/// Rebuilding picks up the current `state.rtl_tcp_requested_role`
/// and `state.rtl_tcp_auth_key` for the next hello, which is the
/// behavior the UI expects after any denial arm.
///
/// **Sticky-command cache:** the previous source's replay
/// snapshot (gain, AGC, PPM, bias tee, direct sampling, etc.) is
/// captured via `Source::rtl_tcp_sticky_snapshot()` and restored
/// onto the new instance BEFORE `start()` so the reconnect's
/// `replay_sticky_commands` emits the same setters the old
/// session had. Without this, a takeover / auth-retry rebuild
/// would reset device state to defaults (gain = 0, AGC off, PPM
/// = 0, ...) and the user would lose their tuning setup.
///
/// `request_takeover` is the one bit of per-call config not read
/// from `DspState` — we keep takeover as an explicit one-shot
/// parameter so the next non-takeover retry or `open_source` call
/// cleanly starts without the flag.
///
/// Caller must have already ensured `state.source_type ==
/// RtlTcp` and `state.source.is_some()`. Per `CodeRabbit` round
/// 3 on PR #408.
pub(super) fn rebuild_rtl_tcp_source(
    state: &mut DspState,
    dsp_tx: &std::sync::mpsc::Sender<DspToUi>,
    request_takeover: bool,
) {
    let error_prefix = if request_takeover {
        "Take control failed"
    } else {
        "Retry failed"
    };
    // Snapshot the replay cache + drop the old source. The
    // Source-trait hook returns `None` for non-RtlTcp sources —
    // can't happen here given the caller's `source_type` gate,
    // but `unwrap_or_default()` keeps this defensive.
    let sticky_snapshot = state
        .source
        .as_ref()
        .and_then(|s| s.rtl_tcp_sticky_snapshot())
        .unwrap_or_default();
    if let Some(mut source) = state.source.take()
        && let Err(e) = source.stop()
    {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp stop before rebuild failed"
        );
    }
    // Build the fresh config from the latest DspState.
    // `Default` covers timeouts + compression; role and auth
    // come from state, and `request_takeover` is the caller's
    // one-shot choice.
    let rtl_tcp_config = sdr_source_network::rtl_tcp::RtlTcpConfig {
        requested_role: state.rtl_tcp_requested_role,
        auth_key: state.rtl_tcp_auth_key.clone(),
        request_takeover,
        ..Default::default()
    };
    let mut source: Box<dyn Source> = Box::new(sdr_source_network::RtlTcpSource::with_config(
        &state.network_host,
        state.network_port,
        rtl_tcp_config,
    ));
    // Restore sticky cache BEFORE `start()` so the manager
    // thread's `replay_sticky_commands` call on the freshly-
    // opened stream already sees the pre-rebuild values.
    source.rtl_tcp_restore_sticky_snapshot(&sticky_snapshot);
    // Reapply sample rate + tune on the new instance — these
    // are derived from `DspState`, not the snapshot, because
    // they can change between the snapshot and the restart
    // (e.g., a user sample-rate switch while the old source
    // was in `AuthRequired`). Both calls also update the
    // sticky cache on the new source, which is fine — any
    // subsequent reconnect replays the fresher value.
    reapply_rtl_tcp_rate_and_tune(state, source.as_mut(), request_takeover);
    if let Err(e) = source.start() {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp rebuild start failed"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!("{error_prefix}: {e}")));
        return;
    }
    state.source = Some(source);
}

/// Warn-and-continue reapplication of the `DspState`-derived sample
/// rate and center frequency on a freshly rebuilt `rtl_tcp` source.
/// Split out of [`rebuild_rtl_tcp_source`] per CR on PR #841.
fn reapply_rtl_tcp_rate_and_tune(
    state: &DspState,
    source: &mut dyn Source,
    request_takeover: bool,
) {
    if let Err(e) = source.set_sample_rate(state.configured_sample_rate) {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp rebuild set_sample_rate failed"
        );
    }
    if let Err(e) = source.tune(state.center_freq) {
        tracing::warn!(
            error = %e,
            request_takeover,
            "rtl_tcp rebuild tune failed"
        );
    }
}

/// Dispatch the persisted RTL-SDR settings that the driver programs
/// during `start()` — they have to be on the source BEFORE that call.
/// With no device open yet the source only records each value and
/// replays it at open time, so every call here is best-effort and
/// warn-logged.
///
/// - **Direct sampling**: `start()` performs the first
///   `set_center_freq`, and an HF frequency is only tunable once the
///   RTL2832 is in direct-sampling mode (the R820T can't go below
///   ~24 MHz). Dispatching it afterwards meant `start()` failed on the
///   tune and the replay was never reached.
/// - **RTL AGC, tuner gain mode, tuner gain**: `start()` programs the
///   tuner from its remembered values, falling back to first-time
///   defaults (manual, 29.7 dB) when it has none. Replaying these only
///   after `start()` forced those defaults onto the air for every Play
///   and every satellite auto-record restart — a saturated burst for a
///   0 dB + LNA chain, forced manual gain for AGC users (#703).
///
/// Order: direct sampling → RTL AGC → gain mode → gain, matching the
/// driver's own open-time programming order.
pub(super) fn rtl_sdr_pre_start_settings(state: &DspState, source: &mut dyn Source) {
    if let Err(e) = source.set_direct_sampling(state.direct_sampling_mode) {
        tracing::warn!(
            error = %e,
            mode = state.direct_sampling_mode,
            "pre-start direct-sampling dispatch failed"
        );
    }
    if let Err(e) = source.set_rtl_agc(state.rtl_agc_enabled) {
        tracing::warn!(
            error = %e,
            enabled = state.rtl_agc_enabled,
            "pre-start RTL AGC dispatch failed"
        );
    }
    // Gain mode before the manual value so a switch into manual mode
    // lands at the persisted gain instead of the dongle's reset default.
    if let Err(e) = source.set_gain_mode(!state.tuner_agc_auto) {
        tracing::warn!(
            error = %e,
            agc_auto = state.tuner_agc_auto,
            "pre-start tuner AGC mode dispatch failed"
        );
    }
    // librtlsdr ignores the manual gain while AGC is on, so always
    // dispatch — explicit-OFF AGC + persisted gain takes effect;
    // AGC-on + persisted gain is a harmless write.
    if let Err(e) = source.set_gain(state.tuner_gain_tenths_db) {
        tracing::warn!(
            error = %e,
            gain_tenths = state.tuner_gain_tenths_db,
            "pre-start tuner gain dispatch failed"
        );
    }
}

/// Re-apply the persisted RTL-SDR settings tracked on
/// [`DspState`] to a freshly-opened source. Each setting is
/// best-effort — we warn-log and toast on failure but never
/// abort the source open, since these are non-critical
/// configuration knobs and the source is otherwise streaming.
///
/// Called from [`open_source`] after `source.start()` succeeds,
/// gated on `SourceType::RtlSdr` because every replayed setting
/// is RTL-SDR-specific (no-op on Network/File/RtlTcp). The
/// source argument is `&mut dyn Source` so this runs against
/// the freshly-built `Box<dyn Source>` before it's stored on
/// `state.source`.
///
/// Replay order matches the rationale in the call site comment:
/// PPM (clock baseline) → offset-tuning → discrete gain index →
/// bias-T last (LNA power should follow the established signal
/// path, not lead it). Direct sampling, RTL AGC, tuner gain mode
/// and gain are NOT replayed here: they are dispatched by
/// [`rtl_sdr_pre_start_settings`] ahead of `start()`, which
/// programs them itself (#703).
///
/// Per issue #551.
pub(super) fn rtl_sdr_replay_persisted_settings(
    state: &DspState,
    source: &mut dyn Source,
    dsp_tx: &mpsc::Sender<DspToUi>,
) {
    if let Err(e) = source.set_ppm_correction(state.ppm_correction) {
        tracing::warn!(
            error = %e,
            ppm = state.ppm_correction,
            "re-applying persisted PPM correction on source open failed"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!("PPM correction failed: {e}")));
    }

    // Only replay an *enabled* offset-tuning state. The
    // librtlsdr R820T-family branch returns `InvalidParameter`
    // for every `set_offset_tuning` call regardless of the
    // value — even setting it to `false`, which is already the
    // driver default. Replaying `false` on every source open
    // therefore generated a spurious "Offset tuning failed"
    // toast for the vast majority of users (R820T2 / R828D
    // dongles) at every Play / source-type switch / source-
    // restart, even when they never enabled the toggle.
    // Replaying `true` still goes through — that's the actual
    // hardware-state restore path that matters for E4000
    // tuners. The user-driven toggle (the `SetOffsetTuning`
    // handler at line ~1693) keeps surfacing errors so an
    // explicit user action gets explicit feedback. Per issue
    // #564.
    if state.offset_tuning_enabled
        && let Err(e) = source.set_offset_tuning(true)
    {
        tracing::warn!(
            error = %e,
            "re-applying persisted offset-tuning on source open failed"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!("Offset tuning failed: {e}")));
    }

    // Discrete gain index — only when the FFI/scanner side has
    // explicitly chosen one. Bounds-check against the freshly-
    // opened source's gain table: the index can be stale (set
    // via FFI before any source was open, or persisted from a
    // prior session against a different dongle), and replaying
    // it unchecked produces a recurring startup toast on every
    // open until the user overwrites it. Mirrors the live
    // `UiToDsp::SetGainByIndex` handler's pre-check. Skip when
    // `gains().len() == 0` — the rtl_tcp path can't populate it
    // synchronously, but the replay helper is gated to
    // `SourceType::RtlSdr` upstream, where the local USB driver
    // does fill the table at open time. Per CR round 1 on
    // PR #553.
    replay_tuner_gain_index(state, source, dsp_tx);

    if let Err(e) = source.set_bias_tee(state.bias_tee_enabled) {
        tracing::warn!(
            error = %e,
            enabled = state.bias_tee_enabled,
            "re-applying persisted bias-T on source open failed"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!(
            "Bias tee {} failed: {e}",
            if state.bias_tee_enabled { "on" } else { "off" }
        )));
    }
}

/// Replay a persisted discrete tuner-gain index, bounds-checked
/// against the freshly-opened source's gain table — the index can be
/// stale (persisted from a prior session against a different dongle),
/// and replaying it unchecked produces a recurring startup toast on
/// every open. Mirrors the live `UiToDsp::SetGainByIndex` handler's
/// pre-check; skips when the table is empty (`rtl_tcp` can't populate
/// it synchronously). Per CR round 1 on PR #553; split out of
/// [`rtl_sdr_replay_persisted_settings`] per CR on PR #841.
fn replay_tuner_gain_index(
    state: &DspState,
    source: &mut dyn Source,
    dsp_tx: &mpsc::Sender<DspToUi>,
) {
    if let Some(index) = state.tuner_gain_index {
        let gains_len = source.gains().len();
        if gains_len > 0 && (index as usize) >= gains_len {
            tracing::warn!(
                index,
                gains_len,
                "re-applying persisted tuner gain index rejected: out of range"
            );
            let _ = dsp_tx.send(DspToUi::Error(format!(
                "Gain index {index} out of range (source has {gains_len} gains)"
            )));
        } else if let Err(e) = source.set_gain_by_index(index) {
            tracing::warn!(
                error = %e,
                index,
                "re-applying persisted tuner gain index on source open failed"
            );
            let _ = dsp_tx.send(DspToUi::Error(format!("Set gain failed: {e}")));
        }
    }
}

/// Construct the right [`Source`] implementation for the current
/// `state.source_type`, applying construction-time config (file
/// looping, `rtl_tcp` role/auth). Pure factory — rate/tune/start and
/// the persisted-settings replay stay in [`open_source`]. Split out
/// per CR on PR #841.
fn create_source_instance(state: &DspState) -> Box<dyn Source> {
    match state.source_type {
        SourceType::RtlSdr => Box::new(RtlSdrSource::new(DEVICE_INDEX)),
        SourceType::Network => Box::new(sdr_source_network::NetworkSource::new(
            &state.network_host,
            state.network_port,
            state.network_protocol,
        )),
        SourceType::File => {
            // Apply the persisted loop flag to the freshly-
            // constructed source so a replay after a path
            // change honors the latest setting — without
            // this, switching files would reset looping to
            // the constructor default. Per issue #236.
            let mut fs = sdr_source_file::FileSource::new(&state.file_path);
            fs.set_looping(state.file_looping);
            Box::new(fs)
        }
        // rtl_tcp client: connects to a remote `rtl_tcp`-compatible
        // server, handshakes the 12-byte RTL0 header, and routes
        // future tune / gain / PPM messages through the 5-byte
        // command channel. Reuses the `network_host` + `network_port`
        // config fields for address, but also threads the #396
        // `requested_role` + `auth_key` fields from state into an
        // `RtlTcpConfig` so the hello carries the user's choices.
        //
        // **Capability signals, in narrow scope:**
        // - `codecs=3` in the mDNS TXT record says "this server
        //   parses `ClientHello`, so a hello won't be mis-framed as
        //   two 5-byte commands." That makes `compression`,
        //   `request_takeover`, and `requested_role` opt-ins wire-
        //   safe on this server. It does **NOT** prove the server
        //   supports auth — auth uses the v2 protocol path, which
        //   `codecs=3` doesn't speak to.
        // - Auth capability is a separate signal: the #394 servers
        //   advertise `auth_required` (present in the mDNS TXT
        //   record and persisted on `FavoriteEntry`) AND accept
        //   hellos with `PROTOCOL_VERSION_V2`. Eager-auth hellos
        //   therefore require v2 — `required_protocol_version(flags)`
        //   in `sdr-source-network` picks the minimum viable version
        //   from the flag set, returning v2 when `FLAG_HAS_AUTH` is
        //   set. Sending an auth-bearing hello to a `codecs=3`-only
        //   server that doesn't understand v2 will bounce at the
        //   server's version gate.
        //
        // The source panel's discovery gating is responsible for
        // refusing role / compression / takeover opt-ins against
        // legacy-only (non-`codecs=3`) servers, and for only
        // offering the auth field on servers that advertise
        // `auth_required`. Per #396 / `CodeRabbit` round 2 on
        // PR #408.
        SourceType::RtlTcp => {
            let rtl_tcp_config = sdr_source_network::rtl_tcp::RtlTcpConfig {
                requested_role: state.rtl_tcp_requested_role,
                auth_key: state.rtl_tcp_auth_key.clone(),
                ..Default::default()
            };
            Box::new(sdr_source_network::RtlTcpSource::with_config(
                &state.network_host,
                state.network_port,
                rtl_tcp_config,
            ))
        }
    }
}

/// Open the active IQ source and configure it for streaming.
pub(super) fn open_source(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
) -> Result<(), String> {
    let mut source: Box<dyn Source> = create_source_instance(state);

    if let Err(e) = source.set_sample_rate(state.configured_sample_rate) {
        if state.source_type == SourceType::File {
            tracing::warn!("file source sample rate mismatch: {e}");
        } else {
            return Err(e.to_string());
        }
    }

    // Tune is a meaningful operation for both the local RTL-SDR and
    // any remote (RtlTcp) — both need the initial center frequency.
    // Network raw-IQ and File sources ignore it.
    if matches!(state.source_type, SourceType::RtlSdr | SourceType::RtlTcp) {
        source.tune(state.center_freq).map_err(|e| e.to_string())?;
    }

    // Settings the driver programs during `start()` must reach the
    // source BEFORE it, not in the post-start replay below — see
    // `rtl_sdr_pre_start_settings` for why each one is there.
    if state.source_type == SourceType::RtlSdr {
        rtl_sdr_pre_start_settings(state, source.as_mut());
    }

    source.start().map_err(|e| e.to_string())?;

    // Re-apply persisted RTL-SDR settings to the freshly-opened
    // source. Each `UiToDsp::SetX` handler writes to state up-
    // front (so dispatches with no live source aren't lost) and
    // forwards to the live source if any. Without this replay,
    // first-play after restart would land with each setting at
    // the dongle's power-on default rather than the user's
    // persisted choice.
    //
    // All settings always apply (not gated on non-default
    // values) so explicit OFF / 0 / disabled-mode also wins
    // over any stale state from a prior app. No-op for non-RTL-
    // SDR sources, so we gate on `SourceType::RtlSdr`. Warn-
    // and-continue on each failure plus a non-fatal toast so
    // the user isn't left with a UI switch that silently lies
    // about hardware state.
    //
    // Replay order: PPM first (clock baseline that affects the
    // tuner's sample-rate / freq accuracy), then path-shaping
    // toggles (direct sampling, offset tuning, RTL-AGC), then
    // tuner gain mode + value, then bias-T (LNA power last so
    // earlier failures don't leave the LNA powered without the
    // intended signal path).
    //
    // Per #551 (and CR round 2 on PR #550 for the bias-T
    // template).
    if state.source_type == SourceType::RtlSdr {
        rtl_sdr_replay_persisted_settings(state, source.as_mut(), dsp_tx);
    }

    // Sync sample rate from the source (file sources have fixed rates).
    state.sample_rate = source.sample_rate();

    // Auto-adjust decimation for the source's actual sample rate.
    let if_rate = state.radio.demod_config().if_sample_rate;
    let auto_decim = auto_decimation_ratio(state.sample_rate, if_rate);
    if auto_decim != state.frontend.decim_ratio() {
        tracing::info!(auto_decim, "auto-adjusting decimation for source rate");
        let _ = state.frontend.set_decimation(auto_decim);
    }

    // Rebuild frontend and VFO before committing the source to state.
    // If either fails, stop the source to avoid a leaked running source.
    if let Err(e) = rebuild_frontend(state).and_then(|()| rebuild_vfo_echoing(state, dsp_tx)) {
        let _ = source.stop();
        return Err(e);
    }
    state.source = Some(source);

    tracing::info!(
        sample_rate = state.sample_rate,
        center_freq = state.center_freq,
        "source opened"
    );
    Ok(())
}

/// Rebuild the IQ frontend with the current sample rate, preserving user settings.
pub(super) fn rebuild_frontend(state: &mut DspState) -> Result<(), String> {
    let mut new_frontend = IqFrontend::new(
        state.sample_rate,
        state.frontend.decim_ratio(),
        state.frontend.fft_size(),
        state.window_fn,
        state.dc_blocking,
    )
    .map_err(|e| format!("frontend rebuild: {e}"))?;

    apply_persisted_frontend_settings(state, &mut new_frontend);
    state.frontend = new_frontend;
    Ok(())
}

/// Copy every user setting that lives outside `IqFrontend::new`'s
/// constructor arguments onto a freshly built frontend. The single place
/// to extend when a new persisted frontend flag is added — `SetFftSize`,
/// `SetWindowFunction` and `rebuild_frontend` all replace the frontend and
/// previously each carried its own (drifting) copy of this list; the
/// IQ-correction flag was missing from two of them. Per `CodeRabbit`
/// round 1 on PR #781 / #692.
pub(super) fn apply_persisted_frontend_settings(state: &DspState, frontend: &mut IqFrontend) {
    frontend.set_invert_iq(state.invert_iq);
    frontend.set_iq_correction(state.iq_correction);
    frontend.set_fft_rate(state.fft_rate);
    frontend.set_fft_enabled(state.fft_enabled);
}

/// Build or rebuild the `RxVfo` from the current frontend and demod configuration.
///
/// Also tells `RadioModule` that its input is now at the demod IF rate (since the
/// VFO handles resampling from the frontend effective rate to the IF rate).
/// Rebuild the VFO for the current frontend rate, demod IF rate,
/// bandwidth and offset. Transactional: the new VFO is built and the
/// radio input rate applied before `state.vfo_offset` / `state.vfo`
/// are committed, so a failure leaves the previous VFO and offset in
/// place. Returns `Ok(true)` when the retained offset had to be clamped
/// to the new reachable span (#699) — callers with a UI channel should
/// echo `VfoOffsetChanged` in that case; see [`rebuild_vfo_echoing`].
pub(super) fn rebuild_vfo(state: &mut DspState) -> Result<bool, String> {
    let effective_rate = state.frontend.effective_sample_rate();
    let reachable = vfo_reachable_offset_hz(effective_rate);
    let applied_offset = state.vfo_offset.clamp(-reachable, reachable);
    let offset_clamped = (applied_offset - state.vfo_offset).abs() > f64::EPSILON;
    let demod_cfg = state.radio.demod_config();
    let if_rate = demod_cfg.if_sample_rate;

    let vfo = RxVfo::new(effective_rate, if_rate, state.bandwidth, applied_offset)
        .map_err(|e| format!("RxVfo build: {e}"))?;

    // Tell RadioModule it receives samples at the demod IF rate — no internal
    // resampling needed since the VFO already handled it.
    state
        .radio
        .set_input_sample_rate(if_rate)
        .map_err(|e| format!("radio input rate: {e}"))?;

    // Both succeeded — commit.
    if offset_clamped {
        tracing::warn!(
            previous_hz = state.vfo_offset,
            clamped_hz = applied_offset,
            reachable_hz = reachable,
            "VFO offset outside the new ±effective/2 span; clamped"
        );
    }
    state.vfo_offset = applied_offset;
    state.vfo = Some(vfo);

    tracing::debug!(
        frontend_rate = effective_rate,
        if_rate,
        bandwidth = state.bandwidth,
        offset = state.vfo_offset,
        "RxVfo rebuilt"
    );
    Ok(offset_clamped)
}

/// [`rebuild_vfo`] plus the `VfoOffsetChanged` echo when the retained
/// offset was clamped, so the UI overlay / frequency readout follow the
/// engine after a decimation, sample-rate or demod-mode transition.
pub(super) fn rebuild_vfo_echoing(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
) -> Result<(), String> {
    if rebuild_vfo(state)? {
        let _ = dsp_tx.send(DspToUi::VfoOffsetChanged(state.vfo_offset));
    }
    Ok(())
}

/// Compute the optimal power-of-2 decimation ratio to bring the sample rate
/// close to the demod IF rate. The effective rate will be >= `if_rate` (never
/// below, since undersampling causes aliasing).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn auto_decimation_ratio(sample_rate: f64, if_rate: f64) -> u32 {
    if sample_rate <= if_rate {
        return 1;
    }
    // Largest power-of-2 that keeps effective rate >= if_rate
    let ratio = (sample_rate / if_rate).floor() as u32;
    if ratio < 2 {
        return 1;
    }
    // Round down to nearest power of 2
    let pow2 = 1_u32 << ratio.ilog2();
    pow2.clamp(1, sdr_dsp::decim_taps::MAX_RATIO)
}

/// Largest VFO offset the post-decimation chain can reach without
/// aliasing: half the effective sample rate (#699).
pub(super) fn vfo_reachable_offset_hz(effective_sample_rate_hz: f64) -> f64 {
    effective_sample_rate_hz / 2.0
}
