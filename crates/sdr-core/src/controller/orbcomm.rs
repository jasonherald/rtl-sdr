//! Orbcomm decode tap and the `SetOrbcomm*` command helper.

use super::{DspState, DspToUi, mpsc};

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
/// call sites can't be enumerated and clear-guarded exhaustively.
/// `controller/scanner.rs::handle_scanner_retune`, for one, writes
/// `state.center_freq` and calls `frontend.set_decimation(...)`
/// directly, bypassing `handle_tune` / `handle_set_decimation`
/// entirely. So the tap self-checks instead: `geometry` tracks the
/// `(source_rate_hz, center_hz)` pair the current `bank` /
/// `init_failed` state was last attempted at — success OR failure.
/// On every call, if the caller's `(source_rate_hz, center_hz)` no
/// longer bit-matches `*geometry`, the tap drops the stale bank AND
/// clears the failure latch (a geometry change may make a
/// previously-failing attempt succeed, or a previously-succeeding
/// one now decode the wrong channels' NCO mix) before proceeding.
/// This makes every current and future geometry-mutating call site
/// safe by construction, with no per-site invalidation clear needed
/// — the only clear that remains is `cleanup()`'s, which releases
/// the bank on source stop rather than reacting to a geometry change.
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
        return;
    }
    if bank.is_none() {
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
                return;
            }
        }
    }
    let Some(bank) = bank.as_mut() else { return };
    events_scratch.clear();
    bank.process(iq, events_scratch);
    for event in events_scratch.drain(..) {
        let _ = dsp_tx.send(DspToUi::OrbcommEvent(Box::new(event)));
    }
}

/// Handler for `UiToDsp::SetOrbcommEnabled`. Sets the flag and
/// clears the bank / init-failure latch / tracked geometry on BOTH
/// enable and disable so the next tap call lazy-rebuilds against the
/// live geometry. Always acks — unlike ACARS engage, Orbcomm doesn't
/// force source geometry, so there is no synchronous failure mode
/// here; a `ChannelBank::new` failure surfaces later via the tap's
/// latch (mirrors the LRPT pattern).
///
/// Also the routing point [`super::cleanup`] uses to force the
/// toggle off on source stop (issue #865, CR round 2) — `orbcomm_enabled`
/// is the actual on/off state (unlike `acars_region`, a config
/// preference that legitimately persists across a stop), so leaving
/// it set across `cleanup()` would strand the UI toggle latched on
/// with no live tap behind it.
pub(super) fn handle_set_orbcomm_enabled(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    enable: bool,
) {
    state.orbcomm_enabled = enable;
    state.orbcomm_bank = None;
    state.orbcomm_init_failed = false;
    state.orbcomm_geometry = None;
    tracing::info!(enable, "Orbcomm enabled changed");
    let _ = dsp_tx.send(DspToUi::OrbcommEnabledChanged(enable));
}
