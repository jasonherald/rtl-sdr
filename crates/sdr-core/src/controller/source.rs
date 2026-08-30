//! Source lifecycle — open/close, RTL-TCP rebuild/reconnect, persisted
//! tuner settings replay, and frontend/VFO rebuild helpers.

use super::{
    AcarsHandlerOutcome, AudioSinkType, DEVICE_INDEX, DIRECT_SAMPLING_MAX, DIRECT_SAMPLING_MIN,
    DspState, DspToUi, IqFrontend, NetworkSinkStatus, RTL_TCP_STATE_POLL_INTERVAL, RtlSdrSource,
    RtlTcpConnectionState, RxVfo, Source, SourceType, acars_lock_rejects_geometry_change,
    apply_acars_geometry, apply_bias_tee_idle, cleanup, handle_set_acars_enabled,
    iq_recording_rejects_rate_change, mpsc, orbcomm_lock_rejects_geometry_change,
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
    if let Err(e) = source.set_converter_offset(state.converter_offset_hz) {
        tracing::warn!(
            error = %e,
            offset_hz = state.converter_offset_hz,
            "pre-start converter-offset dispatch failed"
        );
    }
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

/// Airspy counterpart of [`rtl_sdr_pre_start_settings`]: gain mode +
/// gain value must reach the source before `start()` so the open
/// applies the user's persisted choice instead of the first-time
/// default. The dB-scaled `tuner_gain_tenths_db` maps onto the
/// 0–21 linearity ladder inside the source (clamped) — a persisted
/// RTL-era value lands at the nearest ladder end once, until the
/// user touches the gain slider on the new device. Per issue #848;
/// per-device gain persistence is follow-up scope there.
pub(super) fn airspy_pre_start_settings(state: &DspState, source: &mut dyn Source) {
    if let Err(e) = source.set_converter_offset(state.converter_offset_hz) {
        tracing::warn!(
            error = %e,
            offset_hz = state.converter_offset_hz,
            "pre-start converter-offset dispatch failed"
        );
    }
    if let Err(e) = source.set_gain_mode(!state.tuner_agc_auto) {
        tracing::warn!(
            error = %e,
            agc_auto = state.tuner_agc_auto,
            "pre-start Airspy gain-mode dispatch failed"
        );
    }
    if let Err(e) = source.set_gain(state.tuner_gain_tenths_db) {
        tracing::warn!(
            error = %e,
            gain_tenths = state.tuner_gain_tenths_db,
            "pre-start Airspy gain dispatch failed"
        );
    }
}

/// Airspy counterpart of [`rtl_sdr_replay_persisted_settings`].
/// Only bias-T applies post-start (no PPM — 0.5 ppm TCXO; no direct
/// sampling / offset tuning / RTL AGC — RTL2832 concepts). Warn +
/// toast on failure so the UI switch can't silently lie about
/// hardware state (same contract as the RTL replay, #551).
pub(super) fn airspy_replay_persisted_settings(
    state: &DspState,
    source: &mut dyn Source,
    dsp_tx: &mpsc::Sender<DspToUi>,
) {
    if let Err(e) = source.set_bias_tee(state.bias_tee_enabled) {
        tracing::warn!(
            error = %e,
            enabled = state.bias_tee_enabled,
            "re-applying persisted bias-T on Airspy open failed"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!("Bias tee failed: {e}")));
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
        // First enumerated Airspy; serial-number selection is #848
        // follow-up scope. The source snapshots the firmware rate
        // table at start() and clamps a persisted RTL-era rate to
        // the nearest supported value instead of failing Play.
        SourceType::Airspy => {
            let mut source = sdr_source_airspy::AirspySource::new();
            source.set_device_serial(state.airspy_serial);
            Box::new(source)
        }
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
    if matches!(
        state.source_type,
        SourceType::RtlSdr | SourceType::RtlTcp | SourceType::Airspy
    ) {
        source.tune(state.center_freq).map_err(|e| e.to_string())?;
    }

    // Settings the driver programs during `start()` must reach the
    // source BEFORE it, not in the post-start replay below — see
    // `rtl_sdr_pre_start_settings` for why each one is there.
    if state.source_type == SourceType::RtlSdr {
        rtl_sdr_pre_start_settings(state, source.as_mut());
    }
    if state.source_type == SourceType::Airspy {
        airspy_pre_start_settings(state, source.as_mut());
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
    if state.source_type == SourceType::Airspy {
        airspy_replay_persisted_settings(state, source.as_mut(), dsp_tx);
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

/// Handler for `UiToDsp::Start`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_start(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    if state.running {
        tracing::warn!("start requested but already running");
        return;
    }
    tracing::info!("starting DSP pipeline");
    state.audio_frames_written = 0;
    state.iq_samples_read = 0;
    state.diag_log_at = std::time::Instant::now();
    match open_source(state, dsp_tx) {
        Ok(()) => {
            start_audio_sink(state, dsp_tx);
            state.running = true;
            tracing::info!("DSP pipeline started");

            if !reassert_acars_geometry_post_start(state, dsp_tx) {
                // Unrecoverable ACARS reassert failure — the source
                // was torn down; skip the Start success epilogue.
                return;
            }

            send_start_epilogue(state, dsp_tx);
        }
        Err(e) => {
            tracing::error!("failed to start source: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Start failed: {e}")));
            let _ = dsp_tx.send(DspToUi::SourceStopped);
        }
    }
}
/// Start the audio sink for [`handle_start`], discriminating the
/// error path by sink type so the network status row sees a real
/// `NetworkSinkStatus` event. Split out per the 50-NLOC gate
/// (#816 PR B).
fn start_audio_sink(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    // Start the audio sink -- if it fails, log but continue
    // so the spectrum display still works. Discriminate the
    // error path by sink type so the network status row in
    // the UI sees a real `NetworkSinkStatus::Error` event
    // instead of a generic toast.
    let start_result = state.audio_sink.start();
    // Re-arm or latch the write path based on
    // the start outcome. See the
    // `audio_sink_offline` docstring for the
    // full one-shot rationale — failed starts
    // must latch, otherwise the next DSP block
    // would re-fire the same terminal error
    // when `write_samples` hits the stopped
    // sink. Per CodeRabbit round 6 on PR #351.
    state.audio_sink_offline = start_result.is_err();
    let is_network = matches!(state.audio_sink_type, AudioSinkType::Network);
    if let Err(e) = start_result {
        tracing::warn!(
            sink_type = ?state.audio_sink_type,
            "audio sink failed to start (spectrum still works): {e}"
        );
        if is_network {
            let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Error {
                message: format!("{e}"),
            }));
        } else {
            let _ = dsp_tx.send(DspToUi::Error(format!("Audio output failed: {e}")));
        }
    } else if is_network {
        // Successful start of a network sink — this is the
        // moment the panel's status row should flip to
        // "Streaming to ...". Driving status from real
        // start/stop transitions (rather than the
        // sink-type swap) keeps the UI honest about what's
        // actually on the wire. Per CodeRabbit round 1 on
        // PR #351.
        let _ = dsp_tx.send(DspToUi::NetworkSinkStatus(NetworkSinkStatus::Active {
            endpoint: format!("{}:{}", state.network_sink_host, state.network_sink_port),
            protocol: state.network_sink_protocol,
        }));
    }
}

/// Post-Start ACARS geometry reassert for [`handle_start`] (epic
/// #474): `open_source` may have hardware-rounded the sample rate
/// and auto-selected decimation, both of which break the airband
/// lock's decim=1 contract. Returns `false` when an unrecoverable
/// double-failure tore the source down and the caller must skip the
/// Start epilogue. Split out per the 50-NLOC gate (#816 PR B).
fn reassert_acars_geometry_post_start(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
) -> bool {
    // ACARS bank coherence (epic #474). open_source
    // may have:
    //   - updated state.sample_rate to the hardware-
    //     rounded value, AND
    //   - auto-selected `frontend.set_decimation(>1)`
    //     based on the current demod IF (per the
    //     auto_decimation_ratio call inside the
    //     Start path).
    // The latter breaks the ACARS contract that the
    // tap reads post-IqFrontend IQ at SOURCE rate
    // (decim=1). Reassert the full airband geometry
    // via `apply_acars_geometry` so frontend decim,
    // configured_sample_rate, rebuild_frontend,
    // rebuild_vfo, and on_tune_change all snap back
    // into the locked state. Then drop the bank for
    // lazy-rebuild at the now-coherent live rate.
    // Per CR rounds 4+8 on PR #584.
    if state.acars_pre_lock.is_some() {
        // Use the active region's center, not
        // the US-6 default — for non-US regions
        // the post-Start reassert would otherwise
        // retune to the wrong source center and
        // desynchronize channel geometry. Issue
        // #581 / CR round 1 on PR #593.
        let acars_center = state.acars_region.center_hz();
        match apply_acars_geometry(
            state,
            dsp_tx,
            crate::acars_airband_lock::ACARS_SOURCE_RATE_HZ,
            acars_center,
            crate::acars_airband_lock::ACARS_FRONTEND_DECIM,
        ) {
            Ok(()) => {
                state.acars_bank = None;
                state.acars_init_failed = false;
                state.acars_stats_emitted_at = std::time::Instant::now();
                // Writer thread auto-opens JSONL / UDP
                // on the next message based on the
                // config lock; no per-Start reopen
                // needed. Issue #596.
                tracing::debug!(
                    sample_rate = state.sample_rate,
                    center_freq = state.center_freq,
                    "ACARS geometry reasserted post-Start; bank lazy-rebuild pending"
                );
            }
            Err(err) => {
                return acars_reassert_failed(state, dsp_tx, err);
            }
        }
    }
    true
}
/// Failure path of [`reassert_acars_geometry_post_start`]: best-effort
/// restore of the live graph to the pre-engage snapshot; on a double
/// failure, patch in-memory state, tear the source down, and return
/// `false` so the caller skips the Start epilogue (CR round 10 on
/// PR #584). Split out per the 50-NLOC gate (#816 PR B).
fn acars_reassert_failed(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    err: crate::acars_airband_lock::AcarsEnableError,
) -> bool {
    // Reassertion failed. The DSP graph
    // is in an indeterminate state — the
    // partial mutation inside
    // `apply_acars_geometry` may have
    // already pushed source rate or
    // frontend toward airband before
    // returning Err, so we can't trust
    // the live pipeline matches anything
    // coherent.
    //
    // Best-effort: try to push the live
    // graph back to the snapshot tuning
    // via a second apply_acars_geometry
    // call. If that succeeds, the live
    // graph + controller state are both
    // at the user's pre-engage values
    // and we can continue running with
    // ACARS off. If it ALSO fails, the
    // graph is unrecoverable: tear down
    // the source so the user gets a
    // clean re-Start path. CR round 10
    // on PR #584.
    tracing::error!("ACARS geometry reassert failed post-Start: {err}");
    let snapshot_clone = state.acars_pre_lock.clone();
    let live_restore = match &snapshot_clone {
        Some(snap) => {
            // Restore the pre-engage offset BEFORE
            // the geometry rebuild so `rebuild_vfo`
            // clamps it against the restored rate
            // and echoes the applied value once
            // (#699, CR round 3 on PR #787).
            state.vfo_offset = snap.vfo_offset_hz;
            apply_acars_geometry(
                state,
                dsp_tx,
                snap.source_rate_hz,
                snap.center_freq_hz,
                snap.frontend_decim,
            )
        }
        None => Ok(()),
    };

    if let Err(restore_err) = &live_restore {
        // Live restore failed too. Patch
        // in-memory state from snapshot
        // anyway so configured_sample_rate
        // reflects user intent for the
        // post-stop re-Start, even though
        // the current live graph won't
        // honor it. (apply_acars_geometry
        // may have left those fields
        // mid-update on its way to Err.)
        if let Some(snap) = &snapshot_clone {
            state.configured_sample_rate = snap.source_rate_hz;
            state.center_freq = snap.center_freq_hz;
            state.vfo_offset = snap.vfo_offset_hz;
        }
        tracing::error!("ACARS live-graph restore ALSO failed: {restore_err}");
    }

    state.acars_bank = None;
    state.acars_init_failed = false;
    state.acars_pre_lock = None;
    let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(err)));
    // Also send a definitive Ok(false)
    // so the UI (which preserves state
    // on Err per CR round 1) snaps the
    // toggle off — same pattern as
    // cleanup's forced-off ack (round 7).
    let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(false)));

    if live_restore.is_err() {
        // Tear down the source so the
        // user gets a clean re-Start.
        // pre_lock is None at this point,
        // so cleanup()'s top-of-function
        // ACARS-disengage guard skips
        // (no infinite recursion through
        // handle_set_acars_enabled).
        tracing::error!("tearing down source after unrecoverable ACARS reassert failure");
        cleanup(state, dsp_tx);
        state.running = false;
        let _ = dsp_tx.send(DspToUi::SourceStopped);
        // Signal the caller to skip the Start
        // success epilogue (DisplayBandwidth /
        // DeviceInfo / GainList) on a controller
        // that's no longer running. CR round 11
        // on PR #584.
        return false;
    }
    true
}

/// Start-success epilogue for [`handle_start`]: display bandwidth,
/// device name, and the gain table. Split out per the 50-NLOC gate
/// (#816 PR B).
fn send_start_epilogue(state: &DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    // Send display bandwidth (raw sample rate) so
    // the spectrum display shows the full tuner
    // bandwidth. The FFT is computed on the pre-
    // decimation stream (see
    // `crates/sdr-pipeline/src/iq_frontend.rs:156`),
    // so bins span `sample_rate()`, not
    // `effective_sample_rate()`.
    let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));

    send_started_device_info(state, dsp_tx);
}

/// Send the started source's display name, gain table, and
/// device-reported sample rates to the UI. The rate list lets the UI
/// swap its static per-device model for the firmware's actual table
/// (Airspy R2 and Mini report different lists). Per #848 phase 5.
pub(super) fn send_started_device_info(state: &DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    if let Some(source) = &state.source {
        let _ = dsp_tx.send(DspToUi::DeviceInfo(source.name().to_string()));
        send_gain_list(source.as_ref(), dsp_tx);
        let rates = source.sample_rates().to_vec();
        if !rates.is_empty() {
            let _ = dsp_tx.send(DspToUi::SampleRateList {
                rates,
                current_hz: state.frontend.sample_rate(),
            });
        }
    }
}

/// Map the source's gain table (tenths of dB) to dB and emit
/// `GainList` when non-empty. Shared by the Start epilogue and the
/// post-switch restart (CR on PR #842).
fn send_gain_list(source: &dyn Source, dsp_tx: &mpsc::Sender<DspToUi>) {
    let gains: Vec<f64> = source
        .gains()
        .iter()
        .map(|&g| f64::from(g) / 10.0) // tenths of dB → dB
        .collect();
    if !gains.is_empty() {
        let _ = dsp_tx.send(DspToUi::GainList(gains));
    }
}

/// Handler for `UiToDsp::Stop`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_stop(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    if !state.running {
        tracing::warn!("stop requested but not running");
        return;
    }
    tracing::info!("stopping DSP pipeline");
    // Disconnect transcription tap so the worker stops receiving audio.
    state.transcription_tx = None;
    // Same treatment for the generic audio tap — the DSP pipeline
    // is tearing down and any registered FFI consumer is about to
    // see a `Disconnected` on their next pull regardless.
    state.audio_tap_tx = None;
    // `cleanup` now emits `NetworkSinkStatus::Inactive` itself
    // when the active sink was Network — same path used by the
    // file-EOF, fatal-source-error, and source-type restart
    // sites so every real stop transition reports Inactive.
    cleanup(state, dsp_tx);
    state.running = false;
    let _ = dsp_tx.send(DspToUi::SourceStopped);
}

/// Handler for `UiToDsp::Tune`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_tune(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, freq: f64) {
    if acars_lock_rejects_geometry_change(state, dsp_tx, "Tune") {
        return;
    }
    // INFO-level so silent-fail demod regressions can be diagnosed
    // by grepping the log alone. Pairs with `tune_to_target`'s
    // TUNE_REQUEST line on the UI side.
    tracing::info!(target: "tune", requested_hz = freq, "DSP_APPLY_REQUEST");
    on_tune_change(state);
    state.center_freq = freq;
    // No explicit Orbcomm invalidation needed here (issue #865, CR
    // round 1) — `orbcomm_decode_tap` self-checks its tracked
    // geometry against the live `state.center_freq` every call and
    // rebuilds on a mismatch.
    // A centre-frequency tune is a fresh start for the VFO: the
    // UI already zeroes its overlay offset on every tune path
    // (`set_center_frequency`), but the engine kept demodulating
    // at `center + old_offset`. Reset here and echo so the
    // header / status bar / overlay all agree. Per #764.
    if state.vfo_offset != 0.0 {
        state.vfo_offset = 0.0;
        if let Some(vfo) = &mut state.vfo {
            vfo.set_offset(0.0);
        }
        let _ = dsp_tx.send(DspToUi::VfoOffsetChanged(0.0));
    }
    if let Some(source) = &mut state.source
        && let Err(e) = source.tune(freq)
    {
        tracing::warn!("tune failed: {e}");
        let _ = dsp_tx.send(DspToUi::Error(format!("Tune failed: {e}")));
    } else {
        tracing::info!(
            target: "tune",
            applied_hz = state.center_freq,
            "DSP_APPLIED"
        );
    }
}

/// Handler for `UiToDsp::SetSampleRate`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_sample_rate(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    rate: f64,
) {
    if acars_lock_rejects_geometry_change(state, dsp_tx, "SetSampleRate") {
        return;
    }
    // `apply_rate_to_frontend` below auto-selects decimation for the
    // new rate — that would silently walk it away from Orbcomm's
    // forced 1 while engaged. Issue #865, CR round 4.
    if orbcomm_lock_rejects_geometry_change(state, dsp_tx, "SetSampleRate") {
        return;
    }
    if iq_recording_rejects_rate_change(state, dsp_tx, "SetSampleRate") {
        return;
    }
    tracing::debug!(sample_rate = rate, "set sample rate");
    state.configured_sample_rate = rate;
    if let Some(source) = &mut state.source {
        if let Err(e) = source.set_sample_rate(rate) {
            tracing::warn!("set sample rate failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Sample rate failed: {e}")));
            return;
        }
        // Use the source's actual rate (may differ due to hardware rounding)
        state.sample_rate = source.sample_rate();
    } else {
        state.sample_rate = rate;
    }

    apply_rate_to_frontend(state, dsp_tx, rate);
}
/// Frontend follow-through for [`handle_set_sample_rate`]: auto-pick
/// the decimation ratio for the new rate, rebuild the frontend and
/// VFO, and echo the applied rates. Split out per the 50-NLOC gate
/// (#816 PR B).
fn apply_rate_to_frontend(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, rate: f64) {
    // Auto-select decimation ratio so the effective rate is close to
    // the demod IF rate. This prevents the VFO from having to process
    // all raw samples when the sample rate is much higher than needed.
    let if_rate = state.radio.demod_config().if_sample_rate;
    let auto_decim = auto_decimation_ratio(rate, if_rate);
    if auto_decim != state.frontend.decim_ratio() {
        tracing::info!(
            sample_rate = rate,
            auto_decim,
            effective = rate / f64::from(auto_decim),
            "auto-adjusting decimation for sample rate"
        );
        if let Err(e) = state.frontend.set_decimation(auto_decim) {
            tracing::warn!("auto-decimation failed: {e}");
        }
    }

    match rebuild_frontend(state) {
        Ok(()) => {
            if let Err(e) = rebuild_vfo_echoing(state, dsp_tx) {
                tracing::warn!("VFO rebuild on sample rate change failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("VFO rebuild failed: {e}")));
            }
            let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                state.frontend.effective_sample_rate(),
            ));
            let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));
        }
        Err(e) => {
            tracing::warn!("frontend rebuild failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Frontend rebuild: {e}")));
        }
    }
}

/// Handler for `UiToDsp::SetDecimation`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_decimation(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    ratio: u32,
) {
    if acars_lock_rejects_geometry_change(state, dsp_tx, "SetDecimation") {
        return;
    }
    // Issue #865, CR round 4 — this command sets decimation directly,
    // which is exactly what Orbcomm's engage forces to 1; reject it
    // outright while engaged rather than let it silently walk away.
    if orbcomm_lock_rejects_geometry_change(state, dsp_tx, "SetDecimation") {
        return;
    }
    tracing::debug!(ratio, "set decimation");
    if let Err(e) = state.frontend.set_decimation(ratio) {
        tracing::warn!("set decimation failed: {e}");
        let _ = dsp_tx.send(DspToUi::Error(format!("Decimation failed: {e}")));
    } else {
        // Rebuild VFO for the new effective sample rate.
        if let Err(e) = rebuild_vfo_echoing(state, dsp_tx) {
            tracing::warn!("VFO rebuild on decimation change failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("VFO rebuild failed: {e}")));
        }
        let _ = dsp_tx.send(DspToUi::SampleRateChanged(
            state.frontend.effective_sample_rate(),
        ));
        let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));
    }
}

/// Handler for `UiToDsp::SetFftSize`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_fft_size(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    size: usize,
) {
    tracing::debug!(fft_size = size, "set FFT size");
    match IqFrontend::new(
        state.frontend.sample_rate(),
        state.frontend.decim_ratio(),
        size,
        state.window_fn,
        state.dc_blocking,
    ) {
        Ok(mut new_frontend) => {
            apply_persisted_frontend_settings(state, &mut new_frontend);
            state.frontend = new_frontend;
            state.fft_buf = vec![0.0; size];
        }
        Err(e) => {
            tracing::warn!("set FFT size failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("FFT size failed: {e}")));
        }
    }
}

/// Handler for `UiToDsp::SetGain`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_gain(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, gain_db: f64) {
    tracing::debug!(gain_db, "set gain");
    #[allow(clippy::cast_possible_truncation)]
    // Source gain is in tenths of dB (e.g., 49.6 dB = 496)
    let gain_tenths = (gain_db * 10.0) as i32;
    // Persist FIRST so a dispatch with no live source
    // survives until `open_source` runs. Per #551.
    state.tuner_gain_tenths_db = gain_tenths;
    // Clear the indexed-gain cache so this dB value is
    // authoritative on the next reopen. The replay
    // helper applies dB then index, so a leftover
    // `Some(index)` from a prior `SetGainByIndex`
    // dispatch would otherwise overwrite the newer dB
    // value. Per CR round 2 on PR #553.
    state.tuner_gain_index = None;
    if let Some(source) = &mut state.source
        && let Err(e) = source.set_gain(gain_tenths)
    {
        tracing::warn!("set gain failed: {e}");
        let _ = dsp_tx.send(DspToUi::Error(format!("Set gain failed: {e}")));
    }
}

/// Handler for `UiToDsp::SetWindowFunction`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_window_function(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    window: sdr_pipeline::iq_frontend::FftWindow,
) {
    tracing::debug!(?window, "set window function");
    state.window_fn = window;
    match IqFrontend::new(
        state.frontend.sample_rate(),
        state.frontend.decim_ratio(),
        state.frontend.fft_size(),
        window,
        state.dc_blocking,
    ) {
        Ok(mut new_frontend) => {
            apply_persisted_frontend_settings(state, &mut new_frontend);
            state.fft_buf = vec![0.0; new_frontend.fft_size()];
            state.frontend = new_frontend;
        }
        Err(e) => {
            tracing::warn!("set window function failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Window function failed: {e}")));
        }
    }
}

/// Handler for `UiToDsp::SetVfoOffset`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_vfo_offset(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    offset: f64,
) {
    if acars_lock_rejects_geometry_change(state, dsp_tx, "SetVfoOffset") {
        return;
    }
    // Expanded tracing for the #337 click-to-tune-no-audio
    // investigation: the #337 hypotheses point at a
    // display-span vs. VFO-input-sample-rate mismatch
    // (decim > 1) and/or clicks landing outside the AA-
    // filter-safe subset, so surface BOTH rates + whether
    // the VFO chain exists so the next smoke-test trace
    // shows the offset's relationship to the filterable
    // range at a glance.
    let raw_rate = state.frontend.sample_rate();
    let effective_rate = state.frontend.effective_sample_rate();
    let vfo_exists = state.vfo.is_some();
    // The spectrum spans the RAW rate but the VFO mixes at the
    // post-decimation rate: an offset beyond ±effective/2
    // wraps (`hz_to_rads` aliases it) and the user hears a
    // different station than the readout claims — the #337
    // symptom. Clamp to the reachable span and echo the
    // clamped value so every readout agrees (#699).
    let reachable = vfo_reachable_offset_hz(effective_rate);
    let clamped = offset.clamp(-reachable, reachable);
    if (clamped - offset).abs() > f64::EPSILON {
        tracing::warn!(
            requested_hz = offset,
            clamped_hz = clamped,
            effective_sample_rate_hz = effective_rate,
            "VFO offset outside ±effective/2; clamped"
        );
    }
    let offset = clamped;
    tracing::debug!(
        offset_hz = offset,
        raw_sample_rate_hz = raw_rate,
        effective_sample_rate_hz = effective_rate,
        vfo_exists,
        "set VFO offset"
    );
    state.vfo_offset = offset;
    if let Some(vfo) = &mut state.vfo {
        vfo.set_offset(offset);
    }
    // Echo so UI paths that trigger this indirectly
    // (reset-to-defaults button, future scanner / scripting
    // hooks) reflect the new offset in their overlay /
    // frequency readout without optimistically guessing
    // locally. Matches the `BandwidthChanged` echo above.
    let _ = dsp_tx.send(DspToUi::VfoOffsetChanged(offset));
}

/// Handler for `UiToDsp::SetSourceType`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_source_type(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    source_type: SourceType,
) {
    tracing::info!(?source_type, "switching source type");
    let acars_outcome = auto_disable_acars_for_source_switch(state, dsp_tx, source_type);
    let was_running = state.running;
    if was_running {
        cleanup(state, dsp_tx);
        state.running = false;
    }
    // Honor TeardownNeeded from the auto-disable: if the
    // disengage hit double-failure AND we didn't already
    // tear down via the `was_running` branch above, do it
    // now. The `was_running` guard handles the common case
    // where ACARS is engaged on a live source — cleanup
    // already ran. The fallback is for an engaged-but-
    // somehow-not-running scenario (defensive, shouldn't
    // happen in practice). CR round 18.
    if matches!(acars_outcome, AcarsHandlerOutcome::TeardownNeeded) && !was_running {
        tracing::error!("ACARS auto-disable double-failure with !running; tearing down source");
        cleanup(state, dsp_tx);
        state.running = false;
        let _ = dsp_tx.send(DspToUi::SourceStopped);
    }
    state.source_type = source_type;
    // Force the rtl_tcp status row to reset when switching
    // away from RTL-TCP. Without this, a user mid-session
    // who switches to a different source would see the
    // stale "Connected — R820T" text linger until the next
    // poll tick (which won't fire if running=false). Only
    // emits on an actual edge.
    if source_type != SourceType::RtlTcp
        && !matches!(
            state.last_rtl_tcp_state,
            RtlTcpConnectionState::Disconnected
        )
    {
        state.last_rtl_tcp_state = RtlTcpConnectionState::Disconnected;
        let _ = dsp_tx.send(DspToUi::RtlTcpConnectionState(
            RtlTcpConnectionState::Disconnected,
        ));
    }
    // Restart with the new source type if was playing
    if was_running {
        restart_after_source_switch(state, dsp_tx);
    }
}
/// ACARS source-type gate for [`handle_set_source_type`] (epic
/// #474): synthesize a disengage on the live old source before
/// `cleanup()` drops the bank. Split out per the 50-NLOC gate
/// (#816 PR B).
fn auto_disable_acars_for_source_switch(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    source_type: SourceType,
) -> AcarsHandlerOutcome {
    // ACARS source-type gate (epic #474). MUST run BEFORE
    // cleanup() — cleanup() drops state.acars_bank, which
    // would make the `is_some()` check below trivially
    // false and skip the synthetic disengage entirely
    // (CR round 2 on PR #584). Running first means the
    // disengage operates on the live old source: it can
    // physically retune rate/center back to the snapshot
    // values, drop the bank, and emit the
    // AcarsEnabledChanged(Ok(false)) ack the UI is
    // waiting on. cleanup() then stops the source as
    // usual; the post-cleanup acars_bank=None becomes a
    // harmless re-set since handle_set_acars_enabled
    // already cleared it.
    // Use `acars_pre_lock.is_some()` (the canonical
    // "ACARS engaged" signal), NOT `acars_bank.is_some()`.
    // The Start path intentionally invalidates the bank
    // for the lazy-rebuild window — bank can be None
    // while ACARS is still engaged. CR round 5 on PR #584.
    // While an IQ recording is open on a live source, leave the
    // disengage to `cleanup()` below: it stops the recording first
    // and then performs the forced ACARS teardown, whereas the
    // user-path disengage would be refused by the recording mutex
    // (#695) and emit a misleading failure for a switch that is
    // about to succeed.
    let defer_to_cleanup = state.iq_writer.is_some() && state.running;
    // Identity-based, not capability-based: even a switch between two
    // ACARS-capable USB sources tears the lock down, because the
    // pre-lock snapshot belongs to the OLD hardware — restoring its
    // rate onto different hardware would silently clamp through the
    // new source's rate table instead of faithfully restoring the
    // user's state. Re-engaging on the new source takes one click
    // and snapshots correctly. Per review round 1 on PR #860.
    if state
        .acars_pre_lock
        .as_ref()
        .is_some_and(|snap| snap.source_type != source_type)
        && !defer_to_cleanup
    {
        tracing::info!(
            ?source_type,
            "ACARS auto-disabling: source type changing while engaged"
        );
        handle_set_acars_enabled(state, false, dsp_tx)
    } else {
        AcarsHandlerOutcome::Normal
    }
}

/// Post-swap restart for [`handle_set_source_type`]: reopen the
/// source, bring the audio sink back (same status semantics as
/// [`start_audio_sink`]), and refresh the UI's capability info.
/// Split out per the 50-NLOC gate (#816 PR B).
fn restart_after_source_switch(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    match open_source(state, dsp_tx) {
        Ok(()) => {
            // Clear the audio-sink offline latch on
            // a successful restart, same as the
            // other successful-start paths (engine
            // Start, SetAudioSinkType,
            // SetNetworkSinkConfig). Without this,
            // a prior-session terminal write
            // failure could leave the latch set
            // through a source-type swap and gate
            // writes off until the next explicit
            // Start command. Per `CodeRabbit`
            // round 3 on PR #351.
            // Mirror the network-specific lifecycle
            // events the other start paths emit
            // (engine Start, SetAudioSinkType,
            // SetNetworkSinkConfig). Without these,
            // a source-type swap could leave the
            // GTK network status row stuck on a
            // stale Active or Error from before the
            // swap. Per `CodeRabbit` round 5 on
            // PR #351.
            start_audio_sink(state, dsp_tx);
            state.running = true;
            // Refresh UI with new source capabilities
            if let Some(source) = &state.source {
                send_gain_list(source.as_ref(), dsp_tx);
            }
            let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                state.frontend.effective_sample_rate(),
            ));
            let _ = dsp_tx.send(DspToUi::DisplayBandwidth(state.frontend.sample_rate()));
        }
        Err(e) => {
            tracing::warn!("source switch failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Source switch failed: {e}")));
            let _ = dsp_tx.send(DspToUi::SourceStopped);
        }
    }
}

/// Handler for `UiToDsp::SetRtlTcpClientConfig`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_rtl_tcp_client_config(
    state: &mut DspState,
    requested_role: sdr_server_rtltcp::extension::Role,
    auth_key: Option<Vec<u8>>,
) {
    // Role-only updates log the role; auth-key updates
    // log the has/not-has state, not the bytes.
    tracing::debug!(
        ?requested_role,
        auth_key_set = auth_key.is_some(),
        "set rtl_tcp client config"
    );
    state.rtl_tcp_requested_role = requested_role;
    state.rtl_tcp_auth_key = auth_key;
    // Takes effect on the NEXT connect. An already-
    // running rtl_tcp session keeps its admitted role
    // until it disconnects — changing role mid-stream
    // would require the server to re-admit the client,
    // which the wire protocol doesn't support (the
    // role byte is part of the hello). Per issue #396.
}

/// Handler for `UiToDsp::SetFileLooping`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_file_looping(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    looping: bool,
) {
    // Store on the state so a source rebuild (e.g. after
    // a file-path change) picks up the latest setting,
    // and also apply to the live source so an already-
    // playing file starts / stops looping at its next
    // EOF. Non-file sources silently accept per the
    // trait default. Per issue #236.
    tracing::debug!(looping, "set file looping");
    state.file_looping = looping;
    if let Some(source) = &mut state.source
        && let Err(e) = source.set_looping(looping)
    {
        tracing::warn!("set file looping failed: {e}");
        let _ = dsp_tx.send(DspToUi::Error(format!("File looping failed: {e}")));
    }
}

/// Handler for `UiToDsp::SetConverterOffset` (#848 phase 4). Persist
/// to state first (so a dispatch with no live source is replayed on
/// the next open via the pre-start settings), then forward to the
/// live source, which retunes at the unchanged display frequency.
pub(super) fn handle_set_converter_offset(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    offset_hz: f64,
) {
    tracing::info!(offset_hz, "set converter offset");
    let previous_offset_hz = state.converter_offset_hz;
    state.converter_offset_hz = offset_hz;
    if let Some(source) = &mut state.source
        && let Err(e) = source.set_converter_offset(offset_hz)
    {
        // The source rolled back to its previous offset — mirror
        // that here, or the pre-start replay would retry the
        // rejected configuration on every later open. Per CR
        // round 4 on PR #851.
        state.converter_offset_hz = previous_offset_hz;
        tracing::warn!("set converter offset failed: {e}");
        let _ = dsp_tx.send(DspToUi::Error(format!("Converter offset failed: {e}")));
    }
}

/// Handler for `UiToDsp::SetAirspyDeviceSerial` (#848 phase 5).
/// Stored only — the source factory applies it at the next open; an
/// already-running stream is not re-opened.
pub(super) fn handle_set_airspy_device_serial(state: &mut DspState, serial: Option<u64>) {
    tracing::info!(?serial, "set airspy device serial");
    state.airspy_serial = serial;
}

/// Handler for `UiToDsp::RefreshAirspyDevices` (#848 phase 5):
/// enumerate connected devices and answer with the serial list.
/// Enumeration only reads USB descriptors, so it is safe while a
/// stream is running.
pub(super) fn handle_refresh_airspy_devices(dsp_tx: &mpsc::Sender<DspToUi>) {
    match sdr_source_airspy::list_device_serials() {
        Ok(serials) => {
            tracing::info!(count = serials.len(), "airspy device enumeration");
            let _ = dsp_tx.send(DspToUi::AirspyDeviceList(serials));
        }
        Err(e) => {
            tracing::warn!("airspy device enumeration failed: {e}");
            let _ = dsp_tx.send(DspToUi::AirspyDeviceList(Vec::new()));
        }
    }
}

/// Handler for `UiToDsp::SetBiasTee`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_bias_tee(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    enabled: bool,
) {
    tracing::debug!(enabled, "set bias tee");
    // Persist FIRST so a dispatch with no live source
    // (e.g. startup before the user hits Play) survives
    // until `open_source` runs. Per CR on PR #550.
    state.bias_tee_enabled = enabled;
    if let Some(source) = &mut state.source {
        // Live-stream path: dongle is open and held by the
        // running source.
        if let Err(e) = source.set_bias_tee(enabled) {
            tracing::warn!("set bias tee failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Bias tee failed: {e}")));
        }
    } else if state.source_type == SourceType::RtlSdr {
        // Idle path (#652): no live source, but the user has
        // selected RTL-SDR. Briefly open the dongle, set the
        // GPIO, and drop. Lets a user toggle bias-T between
        // sessions to power their SAWbird+ on/off without
        // having to start playback first. The RTL-SDR Blog v3
        // GPIO latches state across device close, so the
        // change persists until the next toggle (or until a
        // streaming session reapplies via
        // `rtl_sdr_replay_persisted_settings`).
        if let Err(e) = apply_bias_tee_idle(DEVICE_INDEX, enabled) {
            tracing::warn!("idle bias-T toggle failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!(
                "Bias tee toggle failed (device busy or unavailable): {e}"
            )));
        }
    }
    // For non-RTL-SDR source types (file / network), bias-T
    // doesn't apply — silent no-op consistent with
    // `RtlSdrSource::set_bias_tee`'s default trait fallback.
}

/// Handler for `UiToDsp::SetDirectSampling`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_direct_sampling(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    mode: i32,
) {
    tracing::debug!(mode, "set direct sampling");
    if (DIRECT_SAMPLING_MIN..=DIRECT_SAMPLING_MAX).contains(&mode) {
        // Persist FIRST so a dispatch with no live source
        // survives until `open_source` runs. Per #551.
        state.direct_sampling_mode = mode;
        if let Some(source) = &mut state.source
            && let Err(e) = source.set_direct_sampling(mode)
        {
            tracing::warn!("set direct sampling failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Direct sampling failed: {e}")));
        }
    } else {
        tracing::warn!(
            "set direct sampling rejected: mode {mode} out of range \
             ({DIRECT_SAMPLING_MIN}..={DIRECT_SAMPLING_MAX})"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!(
            "Direct sampling mode {mode} out of range \
             ({DIRECT_SAMPLING_MIN}..={DIRECT_SAMPLING_MAX})"
        )));
    }
}

/// Handler for `UiToDsp::SetGainByIndex`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_gain_by_index(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    index: u32,
) {
    tracing::debug!(index, "set gain by index");
    // Persist FIRST so a dispatch with no live source
    // survives until `open_source` runs. The bounds
    // check below depends on the live source's gain
    // table, so we can only validate when a source is
    // present — replay also bounds-checks against the
    // freshly-opened source. Per #551.
    state.tuner_gain_index = Some(index);
    if let Some(source) = &mut state.source {
        // Bounds-check the index. Two sources of truth for
        // the legal count:
        //
        //   1. `source.gains()` — populated for local
        //      RTL-SDR USB (the tuner's discrete gain
        //      table).
        //   2. The rtl_tcp `Connected` connection state's
        //      `gain_count` field — servers publish the
        //      count but not the values, and
        //      `RtlTcpSource::gains()` returns an empty
        //      slice.
        //
        // Prefer (1) when it's non-empty; fall back to
        // (2) for the rtl_tcp case. If neither is
        // available we dispatch the command unchecked —
        // the source may no-op (default trait impl) or
        // surface a wire-level error later. Per
        // `CodeRabbit` round 1 on PR #360.
        let max_count = {
            let gains_len = source.gains().len();
            if gains_len > 0 {
                Some(gains_len)
            } else {
                match source.rtl_tcp_connection_state() {
                    Some(sdr_types::RtlTcpConnectionState::Connected { gain_count, .. }) => {
                        Some(gain_count as usize)
                    }
                    _ => None,
                }
            }
        };
        if let Some(max) = max_count
            && (index as usize) >= max
        {
            tracing::warn!("set gain by index rejected: {index} >= {max}");
            let _ = dsp_tx.send(DspToUi::Error(format!(
                "Gain index {index} out of range (source has {max} gains)"
            )));
        } else if let Err(e) = source.set_gain_by_index(index) {
            tracing::warn!("set gain by index failed: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Set gain failed: {e}")));
        }
    }
}

/// Handler for `UiToDsp::DisconnectRtlTcp`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_disconnect_rtl_tcp(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    // Only meaningful while `RtlTcp` is the active source
    // type. For any other source we log-and-drop so
    // misrouted commands from buggy UI paths don't panic.
    if state.source_type != SourceType::RtlTcp {
        tracing::debug!(
            active = ?state.source_type,
            "DisconnectRtlTcp ignored — active source is not RtlTcp"
        );
        return;
    }
    // Same teardown as every other stop path: `cleanup()`
    // stops the source AND the audio sink, finalizes WAV
    // writers, emits `NetworkSinkStatus::Inactive`, disengages
    // ACARS and flushes the imaging decoders. Dropping the
    // source by hand skipped all of that, so the next Play
    // hit `AlreadyRunning` on the sink and latched audio
    // offline for the rest of the session (#693). `cleanup()`
    // leaves `state.source = None`, so
    // `rtl_tcp_connection_state` reports Disconnected on the
    // next poll and the UI row reflects reality.
    cleanup(state, dsp_tx);
    state.running = false;
    let _ = dsp_tx.send(DspToUi::SourceStopped);
}

/// Handler for `UiToDsp::RetryRtlTcpNow`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_retry_rtl_tcp_now(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    // "Retry now" REBUILDS the `RtlTcpSource` from the
    // latest `DspState` (role + auth_key) instead of just
    // stopping + starting the existing instance. Rebuild
    // is required because the role / auth-key config is
    // baked into `RtlTcpSource` at construction via
    // `with_config(...)`; a subsequent `start()` on the
    // same instance replays its original `ClientHello`,
    // which means a newly-entered key or flipped role
    // from the UI would never land on the wire until the
    // user forced a full source tear-down (Stop + Play,
    // source-type switch). After an `AuthRequired` /
    // `AuthFailed` / `ControllerBusy` denial those retry
    // semantics are explicitly user-driven, so the
    // rebuild is the correct behavior.
    //
    // The sticky-command replay cache (gain, AGC, PPM,
    // bias tee, direct sampling, etc.) is carried across
    // the rebuild via the Source-trait snapshot hooks so
    // the reconnect lands with the pre-retry device state.
    // Per `CodeRabbit` round 3 on PR #408.
    if state.source_type != SourceType::RtlTcp {
        tracing::debug!(
            active = ?state.source_type,
            "RetryRtlTcpNow ignored — active source is not RtlTcp"
        );
        return;
    }
    if state.source.is_none() {
        tracing::debug!("RetryRtlTcpNow ignored — no live source (was disconnected)");
        return;
    }
    rebuild_rtl_tcp_source(state, dsp_tx, /* request_takeover */ false);
}

/// Handler for `UiToDsp::RetryRtlTcpWithTakeover`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_retry_rtl_tcp_with_takeover(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
) {
    // One-shot Take-control reconnect per #396. Same
    // rebuild machinery as `RetryRtlTcpNow`, but with
    // `request_takeover = true` set on the rebuilt
    // config's `ClientHello`. The flag doesn't persist on
    // `DspState` — the next non-takeover retry or a
    // fresh `open_source` (Play after Stop, source-type
    // switch) rebuilds without it. Keeping takeover
    // "one-shot per action" matches the #393 spec:
    // takeover is an explicit user decision, not a
    // persistent preference.
    if state.source_type != SourceType::RtlTcp {
        tracing::debug!(
            active = ?state.source_type,
            "RetryRtlTcpWithTakeover ignored — active source is not RtlTcp"
        );
        return;
    }
    // Gate on a live source. After `DisconnectRtlTcp`
    // the source is gone (`state.source = None`) but
    // `state.source_type` remains `RtlTcp`, so a stale
    // "Take control" toast action could otherwise
    // recreate + start a fresh source here — breaking
    // the disconnect contract (reopen path after an
    // explicit disconnect is Play/Start, not a retry
    // command). Mirrors the `RetryRtlTcpNow` gate above.
    if state.source.is_none() {
        tracing::debug!("RetryRtlTcpWithTakeover ignored — no live source (was disconnected)");
        return;
    }
    rebuild_rtl_tcp_source(state, dsp_tx, /* request_takeover */ true);
}
