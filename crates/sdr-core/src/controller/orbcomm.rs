//! Orbcomm decode tap and the `SetOrbcomm*` command helper.

use super::{DspState, DspToUi, mpsc};

/// Orbcomm channel-bank stats emission throttle. Same cadence as
/// `crate::acars_airband_lock::ACARS_STATS_EMIT_INTERVAL_MS` — kept
/// as a local constant rather than reused directly because that one
/// lives in the ACARS-specific airband-lock module and Orbcomm has
/// no engage/lock machinery to pull it in alongside.
pub(super) const ORBCOMM_STATS_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(1_000);

/// Orbcomm decode tap. Mirrors `acars_decode_tap`'s lazy-init/latch
/// shape (`controller/acars.rs::acars_decode_tap`), minus the
/// output-writer plumbing ACARS has (Orbcomm has no JSONL/UDP sinks
/// yet) and minus a `bytemuck` cast — `sdr_orbcomm::ChannelBank::process`
/// takes `sdr_types::Complex` directly, so there is no layout gap to
/// bridge here.
///
/// Lazy-init: on the first call with `bank.is_none()` and
/// `*init_failed == false`, builds the `ChannelBank` from
/// `(source_rate_hz, center_hz, &ORBCOMM_CHANNELS_HZ)`. If
/// construction fails, sets `*init_failed = true` and skips
/// subsequent calls until the caller clears the flag — source stop,
/// retune, or a sample-rate/decimation change (the geometry
/// invalidation sites in `controller.rs` / `controller/source.rs`),
/// or a fresh `SetOrbcommEnabled` dispatch.
///
/// Per-block: clears `events_scratch` (the caller-owned reuse
/// buffer — avoids a per-block allocation), feeds `iq` through
/// `bank.process(...)`, and forwards each event to `dsp_tx` boxed
/// as `DspToUi::OrbcommEvent`.
pub(super) fn orbcomm_decode_tap(
    bank: &mut Option<sdr_orbcomm::ChannelBank>,
    init_failed: &mut bool,
    source_rate_hz: f64,
    center_hz: f64,
    iq: &[sdr_types::Complex],
    events_scratch: &mut Vec<sdr_orbcomm::OrbcommEvent>,
    dsp_tx: &mpsc::Sender<DspToUi>,
) {
    if *init_failed {
        return;
    }
    if bank.is_none() {
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
/// clears the bank + init-failure latch on BOTH enable and disable
/// so the next tap call lazy-rebuilds against the live geometry.
/// Always acks — unlike ACARS engage, Orbcomm doesn't force source
/// geometry, so there is no synchronous failure mode here; a
/// `ChannelBank::new` failure surfaces later via the tap's latch
/// (mirrors the LRPT pattern).
pub(super) fn handle_set_orbcomm_enabled(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    enable: bool,
) {
    state.orbcomm_enabled = enable;
    state.orbcomm_bank = None;
    state.orbcomm_init_failed = false;
    tracing::info!(enable, "Orbcomm enabled changed");
    let _ = dsp_tx.send(DspToUi::OrbcommEnabledChanged(enable));
}
