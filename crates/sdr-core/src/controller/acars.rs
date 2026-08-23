//! ACARS decode tap, geometry/engage handling, and the `SetAcars*`
//! command helpers.

use super::{
    ACARS_NETWORK_DEFAULT_ADDR, AcarsOutputs, DspState, DspToUi, mpsc, on_tune_change,
    rebuild_frontend, rebuild_vfo_echoing,
};

/// ACARS decode tap. Mirrors `lrpt_decode_tap`'s shape: takes
/// the bank slot, init-failed flag, current geometry, IQ
/// buffer, and `dsp_tx` as separate parameters so the call
/// site can hold a live borrow of `state.processed_buf`.
///
/// Lazy-init: on the first call with `bank.is_none()` and
/// `*init_failed == false`, builds the `ChannelBank` from
/// `(source_rate_hz, center_hz, channels)`. If construction
/// fails, sets `*init_failed = true` and skips subsequent
/// calls until source-stop clears the flag (matching the
/// LRPT pattern).
///
/// Per-block: feeds `iq` through `bank.process(...)` and
/// forwards each decoded `AcarsMessage` to `dsp_tx`. The
/// caller is responsible for periodic `AcarsChannelStats`
/// emission (handled in `process_iq_block` via the
/// throttle in `state.acars_stats_emitted_at`).
///
/// Visibility: private — same as the analogous
/// `apt_decode_tap` and `lrpt_decode_tap` siblings. Inline
/// `#[cfg(test)] mod tests` blocks at the bottom of this file
/// exercise it directly. End-to-end pipeline integration
/// (engage → ack → disengage) is covered by the `Engine`-API
/// tests in `tests/acars_pipeline_integration.rs`.
#[allow(clippy::too_many_arguments)]
pub(super) fn acars_decode_tap(
    bank: &mut Option<sdr_acars::ChannelBank>,
    init_failed: &mut bool,
    source_rate_hz: f64,
    center_hz: f64,
    channels: &[f64],
    iq: &[sdr_types::Complex],
    dsp_tx: &std::sync::mpsc::Sender<crate::messages::DspToUi>,
    outputs: &AcarsOutputs,
) {
    // Compile-time guard: `bytemuck::cast_slice::<Complex, Complex32>`
    // below is sound because both types are `repr(C) { re: f32, im: f32 }`
    // with `bytemuck::Pod`. If a future refactor changes either
    // layout, this assertion fails to compile and surfaces the
    // drift at the cast site rather than as a runtime panic.
    const _: () = assert!(
        std::mem::size_of::<sdr_types::Complex>() == std::mem::size_of::<num_complex::Complex32>(),
        "sdr_types::Complex and num_complex::Complex32 must have identical size \
         for the bytemuck zero-copy cast in acars_decode_tap"
    );
    const _: () = assert!(
        std::mem::align_of::<sdr_types::Complex>()
            == std::mem::align_of::<num_complex::Complex32>(),
        "sdr_types::Complex and num_complex::Complex32 must have identical \
         alignment for the bytemuck zero-copy cast in acars_decode_tap"
    );

    if *init_failed {
        return;
    }
    if bank.is_none() {
        match sdr_acars::ChannelBank::new(source_rate_hz, center_hz, channels) {
            Ok(b) => {
                tracing::info!(
                    "ACARS bank initialised: source_rate={source_rate_hz} \
                     center={center_hz} n_channels={}",
                    channels.len()
                );
                *bank = Some(b);
            }
            Err(e) => {
                tracing::warn!("ACARS bank init failed: {e}");
                *init_failed = true;
                return;
            }
        }
    }
    let Some(bank) = bank.as_mut() else { return };
    // `sdr_types::Complex` and `num_complex::Complex32` share the
    // same `repr(C)` layout (`{f32, f32}`); both implement
    // `bytemuck::Pod`, so the cast is zero-copy and safe. The
    // `const _: () = assert!(...)` guards above pin that contract
    // at compile time.
    let iq_c32: &[num_complex::Complex32] = bytemuck::cast_slice(iq);
    bank.process(iq_c32, |msg| {
        // Hand off to the writer thread via the bounded
        // channel. Drop-on-full is handled by `try_send`
        // (rate-limited warn lives there). The writer thread
        // owns JsonlWriter::write + UdpFeeder::send and reads
        // station_id / paths from the shared config lock.
        // Issue #596.
        outputs.try_send(msg.clone());
        // Forward to the UI viewer regardless of writer state.
        // Boxed because AcarsMessage is large enough that
        // unboxed would inflate the DspToUi enum's footprint.
        let _ = dsp_tx.send(crate::messages::DspToUi::AcarsMessage(Box::new(msg)));
    });
}

/// Apply a `(source_rate, center, frontend_decim)` triple to
/// the controller's DSP graph in the same order `UiToDsp::SetSampleRate`
/// uses: update `configured_sample_rate` (so re-opens see the locked
/// rate), retune the live source (rate then center), force frontend
/// decimation, then rebuild the frontend + VFO. Returns `Err` on the
/// first failure WITHOUT rolling back — the caller is responsible for
/// invoking `apply_acars_geometry` again with the prior values.
///
/// Used by `handle_set_acars_enabled` for both engage (apply airband)
/// and disengage (restore snapshot). The symmetric structure keeps
/// the two paths in lockstep — anything that needs to mutate on engage
/// gets restored on disengage automatically.
pub(super) fn apply_acars_geometry(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    target_source_rate_hz: f64,
    target_center_hz: f64,
    target_frontend_decim: u32,
) -> Result<(), crate::acars_airband_lock::AcarsEnableError> {
    use crate::acars_airband_lock::AcarsEnableError;

    state.configured_sample_rate = target_source_rate_hz;
    state.center_freq = target_center_hz;

    if let Some(source) = state.source.as_mut() {
        source
            .set_sample_rate(target_source_rate_hz)
            .map_err(|e| AcarsEnableError::SourceRetuneFailed(e.to_string()))?;
        // Read back actual hardware rate (may differ from
        // requested due to PLL rounding on RTL-SDR).
        state.sample_rate = source.sample_rate();
        source
            .tune(target_center_hz)
            .map_err(|e| AcarsEnableError::SourceRetuneFailed(e.to_string()))?;
    } else {
        state.sample_rate = target_source_rate_hz;
    }

    state
        .frontend
        .set_decimation(target_frontend_decim)
        .map_err(|e| AcarsEnableError::FrontendDecimFailed(e.to_string()))?;

    rebuild_frontend(state).map_err(AcarsEnableError::FrontendRebuildFailed)?;
    // Echo a clamped offset like every other rate transition (#699).
    rebuild_vfo_echoing(state, dsp_tx).map_err(AcarsEnableError::VfoRebuildFailed)?;

    // Reset tune-dependent state. ACARS engage/disengage IS a
    // retune (forced to airband or restored to snapshot), so it
    // must clear `squelch_was_open`, `transcription_squelch_was_open`,
    // and the auto-squelch floor — same contract as
    // `UiToDsp::Tune` / `SetDemodMode` / `SetBandwidth` /
    // scanner retune. Without this, the first squelch-edge on
    // the new channel can be suppressed and the restored
    // channel inherits the wrong floor. CR round 6 on PR #584.
    on_tune_change(state);
    Ok(())
}

/// Result from `handle_set_acars_enabled` and friends, telling
/// the caller whether the helper ran into a double-failure
/// requiring the caller to tear down the source. Without this,
/// helpers calling `cleanup()` themselves would either:
///   - cascade duplicate `SourceStopped` emissions when the
///     outer caller (e.g. `cleanup` itself, or the `Stop` arm)
///     also wants to tear down, or
///   - infinite-recurse when called from inside `cleanup()`
///     (`cleanup` → `handle_set_acars_enabled` double-fail →
///     `cleanup` again).
///
/// New contract: helpers do their own ACARS-state cleanup
/// (clear `bank` / `init_failed` / `pre_lock`, restore snapshot
/// fields, emit `AcarsEnabledChanged` acks) but DO NOT call
/// `cleanup()` or emit `SourceStopped` themselves. Callers see
/// `TeardownNeeded` and act per their own context —
/// `Stop` / `SetAcarsEnabled` callers run the teardown;
/// `cleanup` ignores the signal since it's already tearing
/// down. CR round 18 on PR #584.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AcarsHandlerOutcome {
    /// Normal path. Caller proceeds as usual.
    Normal,
    /// ACARS lifecycle hit unrecoverable double-failure. Helper
    /// has already cleared ACARS session state and emitted
    /// `AcarsEnabledChanged(Err)` + `Ok(false)` so the UI knows.
    /// Caller should tear down the source (`cleanup` +
    /// `state.running = false` + `SourceStopped`) UNLESS the
    /// caller is itself `cleanup()` (which is already handling
    /// teardown).
    TeardownNeeded,
}

pub(super) fn acars_lock_rejects_geometry_change(
    state: &DspState,
    dsp_tx: &mpsc::Sender<crate::messages::DspToUi>,
    cmd_label: &str,
) -> bool {
    if state.acars_pre_lock.is_some() {
        tracing::warn!(
            cmd = cmd_label,
            "ACARS airband-lock active: ignoring {cmd_label} command"
        );
        let _ = dsp_tx.send(crate::messages::DspToUi::Error(format!(
            "{cmd_label} ignored: ACARS airband lock is active. \
             Disable ACARS to change tuning."
        )));
        return true;
    }
    false
}

/// Shared failure-handling for both engage failure points
/// (geometry-apply Err, `ChannelBank::new` Err). Mirrors the
/// post-Start reassert-failure structure: best-effort rollback
/// to snapshot tuning; if rollback also fails, restore in-memory
/// state from snapshot and clear partial ACARS session state.
/// Always emits `AcarsEnabledChanged(Err(orig_err))`.
///
/// Returns `TeardownNeeded` when the live rollback also failed
/// AND a source is active — caller tears the source down per
/// the [`AcarsHandlerOutcome`] contract. CR rounds 12 + 18 on
/// PR #584.
pub(super) fn handle_acars_engage_failure(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<crate::messages::DspToUi>,
    snapshot: &crate::acars_airband_lock::PreLockSnapshot,
    orig_err: crate::acars_airband_lock::AcarsEnableError,
) -> AcarsHandlerOutcome {
    use crate::messages::DspToUi;

    // Attempt rollback to snapshot tuning. Restore the offset first so
    // the rebuild clamps + echoes it against the restored rate (#699).
    state.vfo_offset = snapshot.vfo_offset_hz;
    let rollback = apply_acars_geometry(
        state,
        dsp_tx,
        snapshot.source_rate_hz,
        snapshot.center_freq_hz,
        snapshot.frontend_decim,
    );

    if let Err(ref rollback_err) = rollback {
        tracing::error!("ACARS engage rollback ALSO failed: {rollback_err}");
        // Patch in-memory state from snapshot so a future Start
        // reopens at the user's intended rate, even though the
        // current live graph (if any) is in an indeterminate
        // state.
        state.configured_sample_rate = snapshot.source_rate_hz;
        state.center_freq = snapshot.center_freq_hz;
        state.vfo_offset = snapshot.vfo_offset_hz;
    }

    // Defensive: clear any partial ACARS session state. The
    // engage path doesn't write `acars_pre_lock` / `acars_bank`
    // until ChannelBank::new succeeds, but this guards against
    // future refactors that might.
    state.acars_bank = None;
    state.acars_init_failed = false;
    state.acars_pre_lock = None;

    let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(orig_err)));

    // Signal teardown to caller if the live rollback failed AND
    // a source is active. Caller decides whether to actually
    // tear down (cleanup() already would, so it ignores this
    // signal; SetAcarsEnabled / SetSourceType arms act on it).
    if rollback.is_err() && state.source.is_some() {
        tracing::error!("ACARS engage double-failure: caller should tear down source");
        AcarsHandlerOutcome::TeardownNeeded
    } else {
        AcarsHandlerOutcome::Normal
    }
}

/// Resolve a JSONL path string. Empty ⇒ default
/// `~/sdr-recordings/acars.jsonl`. A leading `~/` (or just
/// `~`) is expanded to the user's home directory so the
/// documented default — which is what users will copy from
/// the placeholder text in the entry row — round-trips
/// without targeting a literal `~` directory. CR round 4 on
/// PR #595.
pub(super) fn resolve_jsonl_path(path: &str) -> std::path::PathBuf {
    let home = || dirs_next::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    if path.is_empty() {
        return home().join("sdr-recordings").join("acars.jsonl");
    }
    if path == "~" {
        return home();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home().join(rest);
    }
    std::path::PathBuf::from(path)
}

/// Acquire the ACARS writer config write lock, recovering from
/// poisoning rather than panicking. A poisoned lock means the
/// writer thread previously panicked while holding the write
/// guard — recoverable from the controller's POV (we just take
/// the inner guard and continue). Per the no-panic rule for
/// library crates + CR round 1 on PR #598.
pub(super) fn acars_config_write(
    cfg: &std::sync::RwLock<crate::acars_output::AcarsWriterConfig>,
) -> std::sync::RwLockWriteGuard<'_, crate::acars_output::AcarsWriterConfig> {
    cfg.write().unwrap_or_else(|poisoned| {
        tracing::warn!("acars writer config lock was poisoned; recovering");
        poisoned.into_inner()
    })
}

pub(super) fn handle_set_acars_jsonl_enabled(
    state: &mut DspState,
    _dsp_tx: &mpsc::Sender<DspToUi>,
    enabled: bool,
) {
    {
        let mut cfg = acars_config_write(&state.acars_outputs.config);
        if enabled {
            // Restore the user's last-chosen path (preserved
            // across disable/enable cycles via
            // `acars_last_user_jsonl_path`). Falls back to the
            // default if the user hasn't picked a path yet.
            // CR round 2 on PR #598.
            cfg.jsonl_path = Some(
                state
                    .acars_last_user_jsonl_path
                    .clone()
                    .unwrap_or_else(|| resolve_jsonl_path("")),
            );
        } else {
            // Disable: clear `cfg.jsonl_path` so the writer
            // stops, but keep `acars_last_user_jsonl_path` so
            // re-enable restores. CR round 2 on PR #598.
            cfg.jsonl_path = None;
        }
    }
    // Wake the writer so the disable/enable takes effect even
    // if no decoded message follows. CR round 1 on PR #598.
    state.acars_outputs.notify_config_changed();
}

pub(super) fn handle_set_acars_jsonl_path(
    state: &mut DspState,
    _dsp_tx: &mpsc::Sender<DspToUi>,
    path: &str,
) {
    let trimmed = path.trim();
    let new_value = if trimmed.is_empty() {
        // Empty apply: don't implicitly disable. If the sink
        // is currently enabled, normalize to the default path
        // (matching the user-intent "use the default"). If
        // the sink is currently disabled, just clear the
        // remembered path and stay disabled. CR round 2 on
        // PR #598.
        let currently_enabled = acars_config_write(&state.acars_outputs.config)
            .jsonl_path
            .is_some();
        if currently_enabled {
            Some(resolve_jsonl_path(""))
        } else {
            None
        }
    } else {
        Some(resolve_jsonl_path(trimmed))
    };
    state.acars_last_user_jsonl_path.clone_from(&new_value);
    // Only push to the writer's config if the sink is currently
    // enabled (Some) — otherwise we'd accidentally turn it on.
    {
        let mut cfg = acars_config_write(&state.acars_outputs.config);
        if cfg.jsonl_path.is_some() {
            cfg.jsonl_path = new_value;
        }
    }
    state.acars_outputs.notify_config_changed();
}

pub(super) fn handle_set_acars_network_enabled(
    state: &mut DspState,
    _dsp_tx: &mpsc::Sender<DspToUi>,
    enabled: bool,
) {
    {
        let mut cfg = acars_config_write(&state.acars_outputs.config);
        if enabled {
            // Same pattern as JSONL: restore the user's last-
            // chosen address, fall back to the airframes.io
            // default. CR round 2 on PR #598.
            cfg.network_addr = Some(
                state
                    .acars_last_user_network_addr
                    .clone()
                    .unwrap_or_else(|| ACARS_NETWORK_DEFAULT_ADDR.to_string()),
            );
        } else {
            cfg.network_addr = None;
        }
    }
    state.acars_outputs.notify_config_changed();
}

pub(super) fn handle_set_acars_network_addr(
    state: &mut DspState,
    _dsp_tx: &mpsc::Sender<DspToUi>,
    addr: &str,
) {
    let trimmed = addr.trim();
    let new_value = if trimmed.is_empty() {
        // Same empty-apply semantics as JSONL: only normalize
        // to default if currently enabled; else stay
        // disabled. CR round 2 on PR #598.
        let currently_enabled = acars_config_write(&state.acars_outputs.config)
            .network_addr
            .is_some();
        if currently_enabled {
            Some(ACARS_NETWORK_DEFAULT_ADDR.to_string())
        } else {
            None
        }
    } else {
        Some(trimmed.to_string())
    };
    state.acars_last_user_network_addr.clone_from(&new_value);
    {
        let mut cfg = acars_config_write(&state.acars_outputs.config);
        if cfg.network_addr.is_some() {
            cfg.network_addr = new_value;
        }
    }
    state.acars_outputs.notify_config_changed();
}

pub(super) fn handle_set_acars_station_id(state: &mut DspState, station_id: &str) {
    // Trim and bound at the DSP boundary so non-UI callers
    // (config replay, future FFI, integration tests) can't
    // leak whitespace-only or oversized IDs into emitted JSON.
    // 8-char cap matches acarsdec's `idstation` field width.
    // CR round 3 on PR #595.
    let trimmed = station_id.trim();
    acars_config_write(&state.acars_outputs.config).station_id = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(8).collect())
    };
    state.acars_outputs.notify_config_changed();
}

/// Handler for `UiToDsp::SetAcarsEnabled`. Engages or
/// releases the airband lock, instantiates / drops the
/// `ChannelBank`, rebuilds the DSP graph, and emits an ack
/// via `DspToUi`.
///
/// Returns `TeardownNeeded` when the lifecycle hits an
/// unrecoverable double-failure that requires source teardown
/// per the [`AcarsHandlerOutcome`] contract. Caller MUST honor
/// the return — see the enum docs for the contract.
#[allow(clippy::too_many_lines)]
pub(super) fn handle_set_acars_enabled(
    state: &mut DspState,
    enable: bool,
    dsp_tx: &std::sync::mpsc::Sender<crate::messages::DspToUi>,
) -> AcarsHandlerOutcome {
    use crate::acars_airband_lock::{AcarsEnableError, CurrentSourceState, disengage, engage};
    use crate::messages::DspToUi;

    if enable {
        if state.acars_pre_lock.is_some() {
            // Idempotent: already engaged. Re-ack with current
            // state. Use `acars_pre_lock.is_some()` rather than
            // `acars_bank.is_some()` because the Start path
            // intentionally invalidates the bank during the
            // lazy-rebuild window. Without this, a second
            // SetAcarsEnabled(true) in that window would fall
            // through, re-snapshot the already-locked
            // (airband) geometry as if it were the user's prior
            // config, and a later disengage would restore to
            // the ACARS lock instead of the user's pre-engage
            // settings. CR round 5 on PR #584.
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(true)));
            return AcarsHandlerOutcome::Normal;
        }

        // Refuse while the scanner is running. Scanner mutates
        // source rate / center / decimation directly via
        // `apply_scanner_commands`, bypassing the UiToDsp
        // dispatcher and therefore the airband-lock guards on
        // those commands. The user has to stop the scanner
        // first — same UI-explainable contract as the source-
        // type gate. CR round 16 on PR #584.
        if state.scanner.is_enabled() {
            tracing::warn!("ACARS engage rejected: scanner is running");
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(
                AcarsEnableError::ScannerActive,
            )));
            return AcarsHandlerOutcome::Normal;
        }

        // Refuse while an IQ recording is open: `apply_acars_geometry`
        // forces the airband source rate, and the recording's WAV
        // header already committed to the current one (#695).
        if state.iq_writer.is_some() {
            tracing::warn!("ACARS engage rejected: IQ recording in progress");
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(
                AcarsEnableError::IqRecordingActive,
            )));
            return AcarsHandlerOutcome::Normal;
        }

        // Snapshot the user's PRIOR config. `configured_sample_rate`
        // (not `sample_rate`) is the right field to capture: it's
        // the rate the user explicitly set, before any hardware-
        // rounding from `source.sample_rate()`. Restoring this on
        // disengage means a subsequent re-open uses the user's
        // intended rate.
        let current = CurrentSourceState {
            source_rate_hz: state.configured_sample_rate,
            center_freq_hz: state.center_freq,
            vfo_offset_hz: state.vfo_offset,
            source_type: state.source_type,
            frontend_decim: state.frontend.decim_ratio(),
        };
        let plan = match engage(&current, &state.acars_region) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("ACARS engage rejected: {e}");
                let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(e)));
                return AcarsHandlerOutcome::Normal;
            }
        };

        // Apply target geometry. On any failure, delegate to
        // `handle_acars_engage_failure` which rolls back to the
        // snapshot, and if THAT rollback also fails, restores
        // in-memory state + signals teardown to the caller so
        // the live graph and controller state can't end up
        // diverged.
        if let Err(err) = apply_acars_geometry(
            state,
            dsp_tx,
            plan.target_source_rate_hz,
            plan.target_center_hz,
            plan.target_frontend_decim,
        ) {
            tracing::warn!("ACARS engage geometry-apply failed: {err}");
            return handle_acars_engage_failure(state, dsp_tx, &plan.snapshot, err);
        }

        // Geometry locked. Pre-build the ChannelBank now (rather
        // than on first IQ block) so init failure surfaces in
        // the engage ack rather than as a quiet `init_failed=true`
        // state the UI never finds out about.
        //
        // Use POST-APPLY values (`state.sample_rate` /
        // `state.center_freq`) — `apply_acars_geometry` already
        // read back `source.sample_rate()` for the live-source
        // case, so this picks up any hardware-rounding the device
        // applied. CR round 4 on PR #584. The Start handler
        // additionally invalidates the bank when source comes
        // online for the enable-while-stopped/startup-replay
        // path; the lazy-init in `acars_decode_tap` rebuilds it
        // at the actual streaming rate.
        match sdr_acars::ChannelBank::new(
            state.sample_rate,
            state.center_freq,
            state.acars_region.channels(),
        ) {
            Ok(bank) => {
                state.acars_bank = Some(bank);
                state.acars_init_failed = false;
                state.acars_pre_lock = Some(plan.snapshot);
                state.acars_stats_emitted_at = std::time::Instant::now();
                // Writer thread auto-opens JSONL / UDP on the
                // next message if path/addr are set in the
                // config. Disengage clears the config, so a
                // re-engage requires the user to re-enable
                // outputs (phase-2 follow-up to preserve
                // intent across disengage). Issue #596.
                tracing::info!("ACARS engaged: airband lock active");
                let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(true)));
                AcarsHandlerOutcome::Normal
            }
            Err(e) => {
                let err = AcarsEnableError::ChannelBankInit(e.to_string());
                tracing::warn!("ACARS bank init failed: {err}");
                handle_acars_engage_failure(state, dsp_tx, &plan.snapshot, err)
            }
        }
    } else {
        // Refuse a user-initiated disengage while an IQ recording is
        // open: the restore puts the source back at the pre-lock rate
        // and the recording's WAV header committed to the airband
        // one (#695). `cleanup()` finalizes recordings before it
        // disengages, so the forced teardown path is unaffected.
        if state.acars_pre_lock.is_some() && state.iq_writer.is_some() {
            tracing::warn!("ACARS disengage rejected: IQ recording in progress");
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(
                AcarsEnableError::IqRecordingActive,
            )));
            return AcarsHandlerOutcome::Normal;
        }
        // Disengage. Idempotent: silently OK if already off.
        let Some(snapshot) = state.acars_pre_lock.take() else {
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(false)));
            return AcarsHandlerOutcome::Normal;
        };
        let restore = disengage(&snapshot);
        // Restore the pre-engage offset BEFORE the geometry rebuild so
        // `rebuild_vfo` clamps it against the restored rate and echoes
        // the applied value once (#699, CR round 3 on PR #787).
        state.vfo_offset = restore.target_vfo_offset_hz;

        // Try the restore FIRST, BEFORE tearing down the bank.
        // If `apply_acars_geometry` fails mid-flight, the
        // controller may already be partially mutated toward
        // the snapshot geometry — re-applying the engaged
        // (airband) geometry as a best-effort rollback keeps
        // the still-live bank usable, and we emit Err so the
        // UI knows the disengage didn't take. CR round 3 on
        // PR #584: don't drop the bank until the restore
        // path actually succeeds.
        if let Err(err) = apply_acars_geometry(
            state,
            dsp_tx,
            restore.target_source_rate_hz,
            restore.target_center_hz,
            restore.target_frontend_decim,
        ) {
            tracing::error!("ACARS disengage restore failed: {err}");
            // Best-effort: re-engage the airband geometry so the
            // still-live bank can keep decoding at the locked
            // rate. If THIS also fails the system is half-
            // broken — both the snapshot's rate/center AND
            // airband's failed to apply, so the live graph is
            // in indeterminate state. Signal teardown to the
            // caller via TeardownNeeded; helper no longer owns
            // the cleanup() call to avoid recursion when invoked
            // from inside cleanup() itself. CR rounds 14 + 18.
            // Use the active region's center, not the US-6
            // default — best-effort re-lock for a non-US session
            // would otherwise pull the source back to the wrong
            // band. Issue #581 / CR round 1 on PR #593.
            let acars_center = state.acars_region.center_hz();
            let relock = apply_acars_geometry(
                state,
                dsp_tx,
                crate::acars_airband_lock::ACARS_SOURCE_RATE_HZ,
                acars_center,
                crate::acars_airband_lock::ACARS_FRONTEND_DECIM,
            );
            if let Err(relock_err) = &relock {
                tracing::error!("ACARS disengage best-effort re-lock ALSO failed: {relock_err}");
                // Patch in-memory state from snapshot so the
                // user's intended tuning persists across the
                // teardown for next Start.
                state.configured_sample_rate = snapshot.source_rate_hz;
                state.center_freq = snapshot.center_freq_hz;
                state.vfo_offset = snapshot.vfo_offset_hz;
                // Force-clear ACARS session, ack Err + Ok(false)
                // so UI snaps the toggle off. Caller will tear
                // down the source per the TeardownNeeded contract.
                state.acars_bank = None;
                state.acars_init_failed = false;
                state.acars_pre_lock = None;
                let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(err)));
                let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(false)));
                return if state.source.is_some() {
                    AcarsHandlerOutcome::TeardownNeeded
                } else {
                    AcarsHandlerOutcome::Normal
                };
            }
            // Re-lock succeeded. Bank stays alive at airband
            // rate, snapshot stays for retry, UI sees Err so
            // it knows the disengage didn't take.
            state.acars_pre_lock = Some(snapshot);
            let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Err(err)));
            return AcarsHandlerOutcome::Normal;
        }

        // Restore succeeded — NOW it's safe to tear down the
        // bank. Any in-flight per-block tap calls before this
        // point would still see the bank as Some and process
        // normally; after the assignment, subsequent tap calls
        // see None and short-circuit.
        state.acars_bank = None;
        state.acars_init_failed = false;
        tracing::info!("ACARS disengaged: source restored to snapshot");
        let _ = dsp_tx.send(DspToUi::AcarsEnabledChanged(Ok(false)));
        AcarsHandlerOutcome::Normal
    }
}
