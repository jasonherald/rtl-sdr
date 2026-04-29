//! Airband-lock state machine for ACARS reception.
//!
//! ACARS sub-project 2 (epic #474). When `SetAcarsEnabled(true)`
//! arrives, the controller snapshots the prior source config and
//! forces airband geometry (2.5 MSps, 130.3375 MHz center,
//! IqFrontend decimation = 1). Toggle off restores the snapshot.
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

/// IqFrontend decimation when ACARS is on. Forces the
/// post-frontend buffer to carry the full source rate so
/// the ACARS tap reads 2.5 MSps IQ unchanged.
pub const ACARS_FRONTEND_DECIM: u32 = 1;

/// Default ring-buffer cap for the recent-message AppState
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
    /// Active source isn't an RTL-SDR (or rtl_tcp) — ACARS
    /// is dongle-only in v1. Spec section "Source-type gate".
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
}
