//! Orbcomm decode tap and the `SetOrbcomm*` command helper.

use super::{DspState, DspToUi, mpsc, rebuild_frontend, rebuild_vfo_echoing};

/// Orbcomm channel-bank stats emission throttle. Same cadence as
/// `crate::acars_airband_lock::ACARS_STATS_EMIT_INTERVAL_MS` — kept
/// as a local constant rather than reused directly because that one
/// lives in the ACARS-specific airband-lock module and Orbcomm has
/// no engage/lock machinery to pull it in alongside.
pub(super) const ORBCOMM_STATS_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(1_000);

/// Leading text of the user-visible `DspToUi::Error` raised when
/// `ChannelBank::new` fails. A constant so the tap and the test that
/// pins "exactly once" agree on what to look for.
pub(super) const ORBCOMM_INIT_ERROR_PREFIX: &str = "Orbcomm decoder could not start";

/// Orbcomm decode tap. Mirrors `acars_decode_tap`'s lazy-init/latch
/// shape (`controller/acars.rs::acars_decode_tap`), minus the
/// output-writer plumbing ACARS has (Orbcomm has no JSONL/UDP sinks
/// yet) and minus a `bytemuck` cast — `sdr_orbcomm::ChannelBank::process`
/// takes `sdr_types::Complex` directly, so there is no layout gap to
/// bridge here.
///
/// # Self-checking geometry (issue #865, CR round 1)
///
/// Unlike ACARS, Orbcomm has no airband-lock engage/disengage
/// machinery gating every geometry-mutating command — a fixed set of
/// call sites can't be enumerated and clear-guarded exhaustively. So
/// the tap self-checks instead: `geometry` tracks the
/// `(source_rate_hz, center_hz)` pair the current `bank` /
/// `init_failed` state was last attempted at — success OR failure.
/// On every call, if the caller's `(source_rate_hz, center_hz)` no
/// longer bit-matches `*geometry`, the tap drops the stale bank AND
/// clears the failure latch (a geometry change may make a
/// previously-failing attempt succeed, or a previously-succeeding
/// one now decode the wrong channels' NCO mix) before proceeding.
/// This is a safety net for `handle_tune` specifically — a retune
/// changes `center_hz` but never `frontend.decim_ratio()`, and is
/// deliberately left un-rejected (see [`orbcomm_lock_rejects_geometry_change`])
/// precisely because this self-check is the mechanism designed to
/// absorb it without losing decoded packets. It is NOT what keeps
/// decimation-mutating commands or the scanner from corrupting the
/// tap's geometry — those are rejected outright while engaged
/// (`orbcomm_lock_rejects_geometry_change`, CR round 4, and
/// `handle_set_orbcomm_enabled` / `handle_set_scanner_enabled`'s
/// mutual exclusion, CR round 3) rather than self-corrected after the
/// fact, since a shrunk span can't be "corrected" — the packets in
/// flight during the narrow window are simply lost.
///
/// Lazy-init: once geometry is confirmed current, if `bank.is_none()`
/// and `*init_failed == false`, builds the `ChannelBank` from
/// `(source_rate_hz, center_hz, &ORBCOMM_CHANNELS_HZ)`, recording the
/// attempted geometry regardless of the outcome. If construction
/// fails, sends one user-visible [`DspToUi::Error`] (prefixed
/// [`ORBCOMM_INIT_ERROR_PREFIX`], carrying the `OrbcommError` display
/// text) so the toggle can't sit on with a dead strip behind it, sets
/// `*init_failed = true`, and skips subsequent calls at the *same*
/// geometry — so the message is emitted exactly once per failing
/// geometry, not per block. A later geometry change clears the latch
/// and retries per the self-check above.
///
/// Per-block: clears `events_scratch` (the caller-owned reuse
/// buffer — avoids a per-block allocation), feeds `iq` through
/// `bank.process(...)`, and forwards each event to `dsp_tx` boxed
/// as `DspToUi::OrbcommEvent`.
#[allow(clippy::too_many_arguments)]
pub(super) fn orbcomm_decode_tap(
    bank: &mut Option<sdr_orbcomm::ChannelBank>,
    init_failed: &mut bool,
    geometry: &mut Option<(f64, f64)>,
    source_rate_hz: f64,
    center_hz: f64,
    iq: &[sdr_types::Complex],
    events_scratch: &mut Vec<sdr_orbcomm::OrbcommEvent>,
    dsp_tx: &mpsc::Sender<DspToUi>,
) {
    if !ensure_orbcomm_bank(
        bank,
        init_failed,
        geometry,
        source_rate_hz,
        center_hz,
        dsp_tx,
    ) {
        return;
    }
    let Some(bank) = bank.as_mut() else { return };
    events_scratch.clear();
    bank.process(iq, events_scratch);
    for event in events_scratch.drain(..) {
        let _ = dsp_tx.send(DspToUi::OrbcommEvent(Box::new(event)));
    }
}

/// Geometry self-check + lazy bank construction, split out of
/// [`orbcomm_decode_tap`] so the per-block decode path reads as the
/// three steps it is. Returns `true` when `bank` is ready to process
/// this block, `false` when the caller must skip it (latched failure,
/// or a construction attempt that just failed).
///
/// See [`orbcomm_decode_tap`]'s doc comment for the full rationale of
/// both the self-check and the one-error-per-failing-geometry latch;
/// this function is only their mechanical half.
fn ensure_orbcomm_bank(
    bank: &mut Option<sdr_orbcomm::ChannelBank>,
    init_failed: &mut bool,
    geometry: &mut Option<(f64, f64)>,
    source_rate_hz: f64,
    center_hz: f64,
    dsp_tx: &mpsc::Sender<DspToUi>,
) -> bool {
    let stale = geometry.is_some_and(|(g_rate, g_center)| {
        g_rate.to_bits() != source_rate_hz.to_bits() || g_center.to_bits() != center_hz.to_bits()
    });
    if stale {
        tracing::info!(
            source_rate_hz,
            center_hz,
            "Orbcomm geometry changed underneath the tap (retune / scanner hop / rate change); \
             dropping the stale bank and rebuilding"
        );
        *bank = None;
        *init_failed = false;
    }

    if *init_failed {
        return false;
    }
    if bank.is_some() {
        return true;
    }

    // Record the attempt's geometry BEFORE the construction call
    // so a subsequent mismatch check is correct regardless of
    // whether this attempt succeeds or fails.
    *geometry = Some((source_rate_hz, center_hz));
    match sdr_orbcomm::ChannelBank::new(
        source_rate_hz,
        center_hz,
        &sdr_orbcomm::ORBCOMM_CHANNELS_HZ,
    ) {
        Ok(b) => {
            tracing::info!(
                "Orbcomm bank initialised: source_rate={source_rate_hz} \
                 center={center_hz} n_channels={}",
                sdr_orbcomm::ORBCOMM_CHANNELS_HZ.len()
            );
            *bank = Some(b);
            true
        }
        Err(e) => {
            tracing::warn!("Orbcomm bank init failed: {e}");
            // Surface it once, not per block: the caller's toggle is
            // still ON with a dead strip behind it, and the latch set
            // below is what keeps this to a single message per geometry
            // (design spec, "Error handling"). `DspToUi::Error` is the
            // codebase's one-shot pipeline-error idiom — see
            // `controller/scanner.rs` and `controller/audio.rs`.
            let _ = dsp_tx.send(DspToUi::Error(format!("{ORBCOMM_INIT_ERROR_PREFIX}: {e}")));
            *init_failed = true;
            false
        }
    }
}

/// Orbcomm-engaged rejection of decim-affecting commands (issue #865,
/// CR round 4 — the defect the round-3 fix closed reopens via a
/// different trigger without this). Mirrors
/// `acars_lock_rejects_geometry_change` exactly — same shape, same
/// one-shot `DspToUi::Error` wording style — keyed on
/// `orbcomm_pre_decim.is_some()` (the canonical "Orbcomm has forced
/// decim=1" signal, mirroring `acars_pre_lock`) instead of
/// `acars_pre_lock`.
///
/// # Which commands, and why not all of ACARS's list
///
/// ACARS guards `Tune` / `SetDemodMode` / `SetSampleRate` /
/// `SetDecimation` / `SetVfoOffset` because its airband lock forces
/// the FULL geometry (source rate, center, decim) and any of those
/// five commands could disturb some part of it. Orbcomm only forces
/// decimation, so only the commands that mutate or auto-adjust
/// `frontend.decim_ratio()` need rejecting here:
///
/// - `SetDemodMode` (`handle_set_demod_mode`) — auto-adjusts
///   decimation for the new mode's IF rate.
/// - `SetSampleRate` (`handle_set_sample_rate` →
///   `apply_rate_to_frontend`) — auto-selects decimation for the new
///   rate.
/// - `SetDecimation` (`handle_set_decimation`) — sets it directly.
///
/// `Tune` and `SetVfoOffset` are deliberately NOT guarded: neither
/// touches decimation, and `orbcomm_decode_tap`'s geometry self-check
/// (CR round 1, see its doc comment above) is exactly the mechanism
/// designed to let a retune keep working while Orbcomm is engaged —
/// rejecting `Tune` here would take away working functionality ACARS
/// only gives up because its OWN lock (not Orbcomm's) needs the
/// center frozen.
pub(super) fn orbcomm_lock_rejects_geometry_change(
    state: &DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    cmd_label: &str,
) -> bool {
    if state.orbcomm_pre_decim.is_some() {
        tracing::warn!(
            cmd = cmd_label,
            "Orbcomm decim lock active: ignoring {cmd_label} command"
        );
        let _ = dsp_tx.send(DspToUi::Error(format!(
            "{cmd_label} ignored: Orbcomm decode is active. \
             Disable Orbcomm to change decimation."
        )));
        return true;
    }
    false
}

/// Handler for `UiToDsp::SetOrbcommEnabled`. Dispatches to
/// [`engage_orbcomm`] / [`disengage_orbcomm`] — split the way
/// `handle_set_acars_enabled` splits into `engage_acars` /
/// `disengage_acars`, since engage now has a real (if narrow) failure
/// surface: forcing frontend decimation to 1 can be refused or fail.
///
/// Also the routing point [`super::cleanup`] uses to force the
/// toggle off on source stop (issue #865, CR round 2) — `orbcomm_enabled`
/// is the actual on/off state (unlike `acars_region`, a config
/// preference that legitimately persists across a stop), so leaving
/// it set across `cleanup()` would strand the UI toggle latched on
/// with no live tap behind it, and (CR round 3) would leave the
/// frontend stuck at the forced decim=1 with nothing to restore it.
pub(super) fn handle_set_orbcomm_enabled(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    enable: bool,
) {
    if enable {
        engage_orbcomm(state, dsp_tx);
    } else {
        disengage_orbcomm(state, dsp_tx);
    }
}

/// Engage half of [`handle_set_orbcomm_enabled`] (issue #865, CR
/// round 3 — smoke-test fix). Mirrors the ACARS-engage subset that
/// actually applies to Orbcomm: force frontend decimation to 1 while
/// enabled, refuse while the scanner is running.
///
/// # Why decimation must be forced
///
/// `orbcomm_decode_tap` reads the post-frontend-decimation buffer at
/// `frontend.effective_sample_rate()`. A narrow-mode decimation (NFM,
/// WFM, ...) both shrinks the tapped span below any Orbcomm channel's
/// bandwidth (surfaced as `"no orbcomm channel inside the source"`)
/// and can land the effective rate on a poorly-conditioned /
/// non-integer value `plan_resampling` can't plan (surfaced as a
/// resampler tap-count init error). ACARS avoids both by forcing
/// frontend decim=1 for the duration of its airband-lock engagement
/// (`ACARS_FRONTEND_DECIM`, `apply_acars_geometry`); this mirrors
/// just that piece — Orbcomm does NOT force source rate or center the
/// way ACARS's airband lock does, only decimation.
///
/// # Why the scanner is refused
///
/// The scanner mutates frontend decimation directly
/// (`handle_scanner_retune` / demod-mode hops), bypassing the forced
/// decim=1 this function establishes. Rather than have the tap's
/// geometry self-check silently fight the scanner block-to-block,
/// engage is refused outright while the scanner is running — the
/// user disables one before enabling the other, same UI-explainable
/// contract as ACARS's own scanner gate. The symmetric refusal lives
/// beside the ACARS check in
/// `controller/scanner.rs::handle_set_scanner_enabled`.
fn engage_orbcomm(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    // Idempotent: already engaged. Re-ack with current state — mirrors
    // ACARS's `engage_refusal` idempotent-reack path.
    if state.orbcomm_pre_decim.is_some() {
        let _ = dsp_tx.send(DspToUi::OrbcommEnabledChanged(true));
        return;
    }

    if state.scanner.is_enabled() {
        tracing::warn!("Orbcomm engage rejected: scanner is running");
        let _ = dsp_tx.send(DspToUi::Error(
            "Orbcomm enable ignored: the scanner is running. Disable the scanner first."
                .to_string(),
        ));
        let _ = dsp_tx.send(DspToUi::OrbcommEnabledChanged(false));
        return;
    }

    let prior_decim = state.frontend.decim_ratio();
    if let Err(e) = force_orbcomm_decim(state, dsp_tx, 1) {
        tracing::warn!("Orbcomm engage: forcing frontend decim=1 failed: {e}");
        // [`force_orbcomm_decim`] mutates in steps: `set_decimation(1)`
        // commits on the live frontend before the frontend/VFO rebuilds
        // run, so a failure in a LATER step leaves the frontend pinned at
        // 1 — and `state.orbcomm_pre_decim` is only set on the success
        // path below, so nothing would remain that could ever restore the
        // user's ratio. Roll it back here (issue #865, CR round 1 on PR
        // #871). Best effort: if the rollback itself fails, the ratio has
        // still been put back by its own `set_decimation` and the only
        // casualty is the dependent rebuild, so log and carry on to the
        // refusal ack rather than masking the original error.
        if let Err(rollback_err) = force_orbcomm_decim(state, dsp_tx, prior_decim) {
            tracing::warn!(
                "Orbcomm engage: rolling the frontend back to decim={prior_decim} \
                 after the failed engage also failed: {rollback_err}"
            );
        }
        let _ = dsp_tx.send(DspToUi::Error(format!("Orbcomm enable failed: {e}")));
        let _ = dsp_tx.send(DspToUi::OrbcommEnabledChanged(false));
        return;
    }

    state.orbcomm_pre_decim = Some(prior_decim);
    state.orbcomm_enabled = true;
    state.orbcomm_bank = None;
    state.orbcomm_init_failed = false;
    state.orbcomm_geometry = None;
    tracing::info!(
        prior_decim,
        "Orbcomm engaged: frontend decimation forced to 1"
    );
    let _ = dsp_tx.send(DspToUi::OrbcommEnabledChanged(true));
}

/// Disengage half of [`handle_set_orbcomm_enabled`] (issue #865, CR
/// round 3). Restores the frontend decimation [`engage_orbcomm`]
/// saved, then clears the bank/latch/geometry and acks. Idempotent: a
/// disable with nothing engaged just re-acks `false`.
///
/// Called both from the `SetOrbcommEnabled(false)` dispatch and from
/// `cleanup()` (source stop) — the restore is a pure `state.frontend`
/// / `state.vfo` rebuild that never touches `state.source`, so it has
/// no ordering dependency on whether the source is still open. ACARS
/// runs its own equivalent (`apply_acars_geometry`) before
/// `source.stop()` for a different reason (it retunes the LIVE
/// source's rate/center), which doesn't apply here — Orbcomm only
/// ever forces decimation.
fn disengage_orbcomm(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    if let Some(prior_decim) = state.orbcomm_pre_decim.take()
        && let Err(e) = force_orbcomm_decim(state, dsp_tx, prior_decim)
    {
        // Best-effort: the frontend may now sit at whatever decim the
        // failed rebuild left it at (likely still 1) rather than the
        // user's prior setting. Still force the logical state off
        // below so the session can't get stuck with the toggle
        // latched on — same "ack the intent even when the live
        // rollback fails" precedent as ACARS's engage/disengage
        // failure paths.
        tracing::warn!("Orbcomm disengage: restoring decim={prior_decim} failed: {e}");
        let _ = dsp_tx.send(DspToUi::Error(format!(
            "Orbcomm disable: could not restore the prior decimation ({prior_decim}): {e}"
        )));
    }
    state.orbcomm_enabled = false;
    state.orbcomm_bank = None;
    state.orbcomm_init_failed = false;
    state.orbcomm_geometry = None;
    tracing::info!("Orbcomm disengaged");
    let _ = dsp_tx.send(DspToUi::OrbcommEnabledChanged(false));
}

/// Set the frontend's decimation ratio and rebuild the frontend + VFO
/// to match — the same sequence `apply_acars_geometry` uses for its
/// `target_frontend_decim` step, scoped to just decimation since
/// Orbcomm doesn't touch source rate or center.
///
/// **Not atomic**: `set_decimation` commits on the live frontend before
/// the two rebuilds run, so an `Err` from either rebuild leaves the ratio
/// already changed. Callers that care must roll it back themselves —
/// [`engage_orbcomm`] does, [`disengage_orbcomm`] deliberately doesn't
/// (there is nothing better to roll back TO once the user's saved ratio
/// is what already failed).
fn force_orbcomm_decim(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    decim: u32,
) -> Result<(), String> {
    state
        .frontend
        .set_decimation(decim)
        .map_err(|e| format!("frontend decim={decim}: {e}"))?;
    rebuild_frontend(state)?;
    rebuild_vfo_echoing(state, dsp_tx)?;
    Ok(())
}
