//! Airband-lock state machine for ACARS reception.
//!
//! ACARS sub-project 2 (epic #474). When `SetAcarsEnabled(true)`
//! arrives, the controller snapshots the prior source config and
//! forces airband geometry (2.5 `MSps`, 130.3375 MHz center,
//! `IqFrontend` decimation = 1). Toggle off restores the snapshot.
//!
//! This module is pure (no controller, no I/O, no GTK). It
//! reports what should change; the controller applies the
//! changes to its `DspState`. That split lets us TDD the
//! engage/disengage math without spinning up the full DSP
//! thread.

use crate::messages::SourceType;
use thiserror::Error;

/// Locked source rate when ACARS is on. Spec section
/// "Airband-lock mechanism".
pub const ACARS_SOURCE_RATE_HZ: f64 = 2_500_000.0;

/// Locked source center frequency when ACARS is on. Midpoint
/// of the US-6 cluster (129.125–131.550 MHz).
pub const ACARS_CENTER_HZ: f64 = 130_337_500.0;

/// `IqFrontend` decimation when ACARS is on. Forces the
/// post-frontend buffer to carry the full source rate so
/// the ACARS tap reads 2.5 `MSps` IQ unchanged.
pub const ACARS_FRONTEND_DECIM: u32 = 1;

/// Default ring-buffer cap for the recent-message `AppState`
/// ring. Spec config key `acars_recent_keep_count`.
pub const ACARS_RECENT_DEFAULT_KEEP: u32 = 500;

/// US-6 channel set (Hz). The only `acars_channel_set` value
/// supported in v1.
pub const US_SIX_CHANNELS_HZ: [f64; 6] = [
    131_550_000.0,
    131_525_000.0,
    130_025_000.0,
    130_425_000.0,
    130_450_000.0,
    129_125_000.0,
];

/// Cardinality of the v1 ACARS channel set. Single source of
/// truth for the array width — call sites that resize/reset
/// `[ChannelStats; N]` reference this rather than hardcoding
/// `6` so a future channel-set rev only edits one place.
pub const US_SIX_CHANNEL_COUNT: usize = US_SIX_CHANNELS_HZ.len();

/// Minimum interval between `DspToUi::AcarsChannelStats`
/// emissions. Spec calls out ~1 Hz cadence.
pub const ACARS_STATS_EMIT_INTERVAL_MS: u64 = 1_000;

/// Pre-lock config snapshot. Captured on `SetAcarsEnabled(true)`,
/// applied verbatim on `SetAcarsEnabled(false)` to restore the
/// user's prior tuning.
#[derive(Clone, Debug, PartialEq)]
pub struct PreLockSnapshot {
    /// Source sample rate before the lock engaged (Hz).
    pub source_rate_hz: f64,
    /// Source center frequency before the lock engaged (Hz).
    pub center_freq_hz: f64,
    /// VFO offset (relative to center) before the lock (Hz).
    pub vfo_offset_hz: f64,
    /// Source type at the moment of engage. Used by
    /// source-type-change auto-disable to verify the user
    /// is restoring to the same kind of source.
    pub source_type: SourceType,
    /// Frontend decimation ratio prior to engage. Restored
    /// verbatim on disengage; the controller's auto-decim
    /// logic re-derives a fresh value if the user toggles
    /// the demod mode after disengage.
    pub frontend_decim: u32,
}

/// Failure modes for `SetAcarsEnabled(true)`. Sent back to
/// the UI inside `DspToUi::AcarsEnabledChanged(Err(...))`.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum AcarsEnableError {
    /// Active source isn't an RTL-SDR — ACARS is local-USB
    /// only in v1 (network/file/`rtl_tcp` sources are rejected
    /// by `engage` to avoid retuning a remote dongle the user
    /// isn't physically driving). Spec section "Source-type
    /// gate".
    #[error("ACARS reception requires an RTL-SDR source (current: {0:?})")]
    UnsupportedSourceType(SourceType),

    /// `ChannelBank::new` rejected the channel list. Wraps
    /// the lower-layer error message so the UI can surface
    /// it to the user.
    #[error("ChannelBank construction failed: {0}")]
    ChannelBankInit(String),

    /// Source backend rejected `set_sample_rate` or `tune`
    /// while engaging the lock.
    #[error("source rejected airband-lock retune: {0}")]
    SourceRetuneFailed(String),

    /// Frontend rejected the forced decimation factor.
    #[error("frontend rejected decim={ACARS_FRONTEND_DECIM}: {0}")]
    FrontendDecimFailed(String),

    /// `rebuild_frontend` failed after rate/center change. The
    /// `IqFrontend`'s filter taps + DC blocker are sized off the
    /// active sample rate, so a rate change without a rebuild
    /// leaves the DSP graph configured for the wrong geometry.
    #[error("frontend rebuild failed: {0}")]
    FrontendRebuildFailed(String),

    /// `rebuild_vfo` failed after frontend rebuild. The `RxVfo`
    /// resamples from frontend-effective rate to demod IF rate,
    /// so a rate change requires a fresh VFO with the new
    /// resample ratio.
    #[error("VFO rebuild failed: {0}")]
    VfoRebuildFailed(String),
}

/// Current source-side configuration the airband-lock state
/// machine reads to compute what to change. The controller
/// fills this from `DspState` at the moment of toggle.
#[derive(Clone, Debug, PartialEq)]
pub struct CurrentSourceState {
    pub source_rate_hz: f64,
    pub center_freq_hz: f64,
    pub vfo_offset_hz: f64,
    pub source_type: SourceType,
    pub frontend_decim: u32,
}

/// What `engage` decides should happen. The controller
/// applies these and stores the snapshot in `DspState`.
#[derive(Clone, Debug, PartialEq)]
pub struct EngagePlan {
    pub target_source_rate_hz: f64,
    pub target_center_hz: f64,
    pub target_frontend_decim: u32,
    pub snapshot: PreLockSnapshot,
}

/// What `disengage` decides should happen.
#[derive(Clone, Debug, PartialEq)]
pub struct DisengagePlan {
    pub target_source_rate_hz: f64,
    pub target_center_hz: f64,
    pub target_vfo_offset_hz: f64,
    pub target_frontend_decim: u32,
}

/// Compute the changes that engage the airband lock. Pure —
/// the controller calls this BEFORE touching any source state.
///
/// # Errors
///
/// Returns [`AcarsEnableError::UnsupportedSourceType`] if the
/// active source isn't `SourceType::RtlSdr`. Source-type gate
/// in v1 — `rtl_tcp` / network / file sources are not supported.
pub fn engage(current: &CurrentSourceState) -> Result<EngagePlan, AcarsEnableError> {
    if current.source_type != SourceType::RtlSdr {
        return Err(AcarsEnableError::UnsupportedSourceType(current.source_type));
    }
    Ok(EngagePlan {
        target_source_rate_hz: ACARS_SOURCE_RATE_HZ,
        target_center_hz: ACARS_CENTER_HZ,
        target_frontend_decim: ACARS_FRONTEND_DECIM,
        snapshot: PreLockSnapshot {
            source_rate_hz: current.source_rate_hz,
            center_freq_hz: current.center_freq_hz,
            vfo_offset_hz: current.vfo_offset_hz,
            source_type: current.source_type,
            frontend_decim: current.frontend_decim,
        },
    })
}

/// Compute the changes that release the airband lock and
/// restore the user's prior config. Pure.
#[must_use]
pub fn disengage(snapshot: &PreLockSnapshot) -> DisengagePlan {
    DisengagePlan {
        target_source_rate_hz: snapshot.source_rate_hz,
        target_center_hz: snapshot.center_freq_hz,
        target_vfo_offset_hz: snapshot.vfo_offset_hz,
        target_frontend_decim: snapshot.frontend_decim,
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn rtl_state() -> CurrentSourceState {
        CurrentSourceState {
            source_rate_hz: 1_024_000.0,
            center_freq_hz: 162_550_000.0,
            vfo_offset_hz: -25_000.0,
            source_type: SourceType::RtlSdr,
            frontend_decim: 4,
        }
    }

    #[test]
    fn engage_snapshots_and_emits_target_geometry() {
        let plan = engage(&rtl_state()).expect("RTL-SDR engage should succeed");
        assert_eq!(plan.target_source_rate_hz, ACARS_SOURCE_RATE_HZ);
        assert_eq!(plan.target_center_hz, ACARS_CENTER_HZ);
        assert_eq!(plan.target_frontend_decim, ACARS_FRONTEND_DECIM);
        assert_eq!(plan.snapshot.source_rate_hz, 1_024_000.0);
        assert_eq!(plan.snapshot.center_freq_hz, 162_550_000.0);
        assert_eq!(plan.snapshot.vfo_offset_hz, -25_000.0);
        assert_eq!(plan.snapshot.source_type, SourceType::RtlSdr);
        assert_eq!(plan.snapshot.frontend_decim, 4);
    }

    #[test]
    fn engage_rejects_non_rtl_sources() {
        for bad in [SourceType::Network, SourceType::File, SourceType::RtlTcp] {
            let mut state = rtl_state();
            state.source_type = bad;
            match engage(&state) {
                Err(AcarsEnableError::UnsupportedSourceType(t)) => assert_eq!(t, bad),
                other => panic!("source={bad:?} expected UnsupportedSourceType, got {other:?}"),
            }
        }
    }

    #[test]
    fn disengage_returns_snapshotted_geometry_verbatim() {
        let plan = engage(&rtl_state()).unwrap();
        let restore = disengage(&plan.snapshot);
        assert_eq!(restore.target_source_rate_hz, 1_024_000.0);
        assert_eq!(restore.target_center_hz, 162_550_000.0);
        assert_eq!(restore.target_frontend_decim, 4);
        assert_eq!(restore.target_vfo_offset_hz, -25_000.0);
    }

    #[test]
    fn engage_then_disengage_is_a_round_trip() {
        let original = rtl_state();
        let plan = engage(&original).unwrap();
        let restore = disengage(&plan.snapshot);
        assert_eq!(restore.target_source_rate_hz, original.source_rate_hz);
        assert_eq!(restore.target_center_hz, original.center_freq_hz);
        assert_eq!(restore.target_frontend_decim, original.frontend_decim);
        assert_eq!(restore.target_vfo_offset_hz, original.vfo_offset_hz);
    }
}
