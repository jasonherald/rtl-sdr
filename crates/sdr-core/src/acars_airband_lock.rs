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

/// Cardinality of every named ACARS channel set. All
/// predefined regions ship with exactly this many channels so
/// the per-channel UI widgets and `[ChannelStats; N]` arrays
/// stay const-sized. Custom channel sets (issue follow-up to
/// #581) will pad / mask to this width too.
pub const ACARS_CHANNEL_COUNT: usize = 6;

/// US-6 channel set (Hz). Default for North-American operation.
pub const US_SIX_CHANNELS_HZ: [f64; ACARS_CHANNEL_COUNT] = [
    131_550_000.0,
    131_525_000.0,
    130_025_000.0,
    130_425_000.0,
    130_450_000.0,
    129_125_000.0,
];

/// Europe-6 channel set (Hz). Primary 131.725 MHz; the rest
/// are the next-most-common European ACARS frequencies. Issue
/// #581. Per the ARINC 618 / EUROCAE ED-89 spec European
/// allocations cluster between 131.4 and 131.9 MHz.
pub const EUROPE_SIX_CHANNELS_HZ: [f64; ACARS_CHANNEL_COUNT] = [
    131_725_000.0, // Primary
    131_525_000.0, // Shared with US-6
    131_550_000.0, // Shared with US-6
    131_825_000.0,
    131_450_000.0,
    131_875_000.0,
];

/// Maximum number of channels in a user-defined custom region.
/// Sized for any realistic ACARS cluster within
/// `MAX_CHANNEL_SPAN_HZ`. Issue #592.
pub const MAX_CUSTOM_CHANNELS: usize = 8;

/// Maximum allowed span (max - min) of a custom channel set
/// in Hz. Set to 2.4 MHz to leave a 100 kHz margin against
/// the 2.5 `MSps` source rate (Nyquist bandwidth ≈ 2.5 MHz).
/// Issue #592.
pub const MAX_CHANNEL_SPAN_HZ: f64 = 2_400_000.0;

/// Error variants returned by [`validate_custom_channels`].
/// `Display` impl produces user-facing toast text. Issue #592.
#[derive(Clone, Debug, PartialEq)]
pub enum CustomChannelError {
    Empty,
    TooMany {
        count: usize,
        max: usize,
    },
    InvalidFrequency {
        value: f64,
    },
    SpanExceeded {
        low_hz: f64,
        high_hz: f64,
        span_hz: f64,
    },
}

impl std::fmt::Display for CustomChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "Custom channel list is empty"),
            Self::TooMany { count, max } => {
                write!(f, "Too many custom channels ({count}); maximum is {max}")
            }
            Self::InvalidFrequency { value } => {
                write!(f, "Invalid custom-channel frequency: {value}")
            }
            Self::SpanExceeded {
                low_hz,
                high_hz,
                span_hz,
            } => {
                let span_mhz = span_hz / 1_000_000.0;
                let low_mhz = low_hz / 1_000_000.0;
                let high_mhz = high_hz / 1_000_000.0;
                write!(
                    f,
                    "Span {span_mhz:.3} MHz exceeds {} MHz limit ({low_mhz:.3} to {high_mhz:.3} MHz)",
                    MAX_CHANNEL_SPAN_HZ / 1_000_000.0
                )
            }
        }
    }
}

impl std::error::Error for CustomChannelError {}

/// Validate a slice of custom-channel frequencies (Hz). Returns
/// `Ok(())` if the list is non-empty, ≤ `MAX_CUSTOM_CHANNELS`,
/// all values are finite + positive, and `max - min ≤
/// MAX_CHANNEL_SPAN_HZ`. Issue #592.
pub fn validate_custom_channels(chans: &[f64]) -> Result<(), CustomChannelError> {
    if chans.is_empty() {
        return Err(CustomChannelError::Empty);
    }
    if chans.len() > MAX_CUSTOM_CHANNELS {
        return Err(CustomChannelError::TooMany {
            count: chans.len(),
            max: MAX_CUSTOM_CHANNELS,
        });
    }
    for &c in chans {
        if !c.is_finite() || c <= 0.0 {
            return Err(CustomChannelError::InvalidFrequency { value: c });
        }
    }
    let (mut min, mut max) = (chans[0], chans[0]);
    for &c in &chans[1..] {
        if c < min {
            min = c;
        }
        if c > max {
            max = c;
        }
    }
    let span = max - min;
    if span > MAX_CHANNEL_SPAN_HZ {
        return Err(CustomChannelError::SpanExceeded {
            low_hz: min,
            high_hz: max,
            span_hz: span,
        });
    }
    Ok(())
}

/// Predefined ACARS channel set. Issue #581. The DSP layer
/// (`sdr_acars::ChannelBank`) is region-agnostic — all this
/// type does is pick which fixed array to feed it and where
/// to center the source. Custom (user-defined) channel sets
/// are a deferred follow-up (#592).
///
/// `#[non_exhaustive]` so a future region addition (e.g. AU,
/// or the deferred `Custom` variant) is a non-breaking change
/// for downstream crates: external consumers can no longer
/// rely on exhaustive matching, which is exactly the contract
/// this enum needs given its forward-compat plan. CR round 1
/// on PR #593.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum AcarsRegion {
    /// North America (default). Six channels in 129.125–
    /// 131.550 MHz. Center 130.3375 MHz.
    #[default]
    Us6,
    /// Europe. Six channels clustered in 131.450–131.875 MHz.
    /// Center derived from the cluster midpoint.
    Europe,
}

impl AcarsRegion {
    /// Channels for this region (Hz).
    #[must_use]
    pub const fn channels(self) -> [f64; ACARS_CHANNEL_COUNT] {
        match self {
            Self::Us6 => US_SIX_CHANNELS_HZ,
            Self::Europe => EUROPE_SIX_CHANNELS_HZ,
        }
    }

    /// Source center frequency for this region (Hz). Computed
    /// as the midpoint of `min(channels)` and `max(channels)`
    /// so the cluster fits symmetrically inside the 2.5 MHz
    /// Nyquist window. The channel order in `channels()` is
    /// not assumed to be sorted.
    #[must_use]
    pub fn center_hz(self) -> f64 {
        let chans = self.channels();
        let mut min = chans[0];
        let mut max = chans[0];
        for &c in &chans[1..] {
            if c < min {
                min = c;
            }
            if c > max {
                max = c;
            }
        }
        f64::midpoint(min, max)
    }

    /// Stable string id used as the `acars_region` config key
    /// value. Round-trips with `from_config_id`.
    #[must_use]
    pub const fn config_id(self) -> &'static str {
        match self {
            Self::Us6 => "us-6",
            Self::Europe => "europe",
        }
    }

    /// Inverse of `config_id`. Falls back to the default on
    /// unrecognised strings (forward-compat with future
    /// regions).
    #[must_use]
    pub fn from_config_id(id: &str) -> Self {
        match id {
            "europe" => Self::Europe,
            // "us-6" + anything else → default. We don't error
            // on unknown values because that would lock users
            // out of the panel after a downgrade or stale
            // config; falling back is the more forgiving
            // behaviour.
            _ => Self::Us6,
        }
    }

    /// Display label for the Aviation panel combo row.
    #[must_use]
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Us6 => "United States (US-6)",
            Self::Europe => "Europe",
        }
    }
}

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

    /// Scanner is currently running. The scanner mutates source
    /// rate / center / decimation directly via
    /// `apply_scanner_commands`, bypassing the `UiToDsp` dispatcher
    /// (and therefore the airband-lock guards on those commands).
    /// Engage refuses while the scanner is enabled — the user
    /// must disable the scanner first. CR round 16 on PR #584.
    #[error("ACARS cannot engage while the scanner is running")]
    ScannerActive,
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
///
/// `region` selects which channel set the resulting plan tunes
/// to (issue #581). The default is `AcarsRegion::Us6` —
/// callers that don't care can pass `Default::default()` and
/// preserve the pre-#581 behaviour.
pub fn engage(
    current: &CurrentSourceState,
    region: AcarsRegion,
) -> Result<EngagePlan, AcarsEnableError> {
    if current.source_type != SourceType::RtlSdr {
        return Err(AcarsEnableError::UnsupportedSourceType(current.source_type));
    }
    Ok(EngagePlan {
        target_source_rate_hz: ACARS_SOURCE_RATE_HZ,
        target_center_hz: region.center_hz(),
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
        let plan =
            engage(&rtl_state(), AcarsRegion::default()).expect("RTL-SDR engage should succeed");
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
            match engage(&state, AcarsRegion::default()) {
                Err(AcarsEnableError::UnsupportedSourceType(t)) => assert_eq!(t, bad),
                other => panic!("source={bad:?} expected UnsupportedSourceType, got {other:?}"),
            }
        }
    }

    #[test]
    fn engage_with_europe_region_targets_europe_center() {
        let plan = engage(&rtl_state(), AcarsRegion::Europe)
            .expect("RTL-SDR + Europe engage should succeed");
        assert_eq!(plan.target_center_hz, AcarsRegion::Europe.center_hz());
        // Sanity: Europe's center is well above US-6's. Pin a
        // ballpark range so a future channel-list edit that
        // accidentally collapses the cluster still trips here.
        assert!(plan.target_center_hz > 131_000_000.0);
        assert!(plan.target_center_hz < 132_000_000.0);
    }

    #[test]
    fn region_config_id_round_trips() {
        for region in [AcarsRegion::Us6, AcarsRegion::Europe] {
            let id = region.config_id();
            assert_eq!(AcarsRegion::from_config_id(id), region);
        }
        // Unknown id falls back to default.
        assert_eq!(
            AcarsRegion::from_config_id("does-not-exist"),
            AcarsRegion::Us6
        );
    }

    #[test]
    fn disengage_returns_snapshotted_geometry_verbatim() {
        let plan = engage(&rtl_state(), AcarsRegion::default()).unwrap();
        let restore = disengage(&plan.snapshot);
        assert_eq!(restore.target_source_rate_hz, 1_024_000.0);
        assert_eq!(restore.target_center_hz, 162_550_000.0);
        assert_eq!(restore.target_frontend_decim, 4);
        assert_eq!(restore.target_vfo_offset_hz, -25_000.0);
    }

    #[test]
    fn engage_then_disengage_is_a_round_trip() {
        let original = rtl_state();
        let plan = engage(&original, AcarsRegion::default()).unwrap();
        let restore = disengage(&plan.snapshot);
        assert_eq!(restore.target_source_rate_hz, original.source_rate_hz);
        assert_eq!(restore.target_center_hz, original.center_freq_hz);
        assert_eq!(restore.target_frontend_decim, original.frontend_decim);
        assert_eq!(restore.target_vfo_offset_hz, original.vfo_offset_hz);
    }

    #[test]
    fn validate_rejects_empty() {
        assert_eq!(
            validate_custom_channels(&[]),
            Err(CustomChannelError::Empty)
        );
    }

    #[test]
    fn validate_accepts_single_channel() {
        assert_eq!(validate_custom_channels(&[131_550_000.0]), Ok(()));
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn validate_accepts_max_count() {
        let chans: Vec<f64> = (0..MAX_CUSTOM_CHANNELS)
            .map(|i| 131_000_000.0 + (i as f64) * 100_000.0)
            .collect();
        assert_eq!(validate_custom_channels(&chans), Ok(()));
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn validate_rejects_too_many() {
        let chans: Vec<f64> = (0..=MAX_CUSTOM_CHANNELS)
            .map(|i| 131_000_000.0 + (i as f64) * 100_000.0)
            .collect();
        assert_eq!(
            validate_custom_channels(&chans),
            Err(CustomChannelError::TooMany {
                count: MAX_CUSTOM_CHANNELS + 1,
                max: MAX_CUSTOM_CHANNELS,
            })
        );
    }

    #[test]
    fn validate_rejects_nan() {
        match validate_custom_channels(&[131_550_000.0, f64::NAN]) {
            Err(CustomChannelError::InvalidFrequency { .. }) => {}
            other => panic!("expected InvalidFrequency, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_inf() {
        match validate_custom_channels(&[131_550_000.0, f64::INFINITY]) {
            Err(CustomChannelError::InvalidFrequency { .. }) => {}
            other => panic!("expected InvalidFrequency, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_negative_or_zero() {
        match validate_custom_channels(&[131_550_000.0, 0.0]) {
            Err(CustomChannelError::InvalidFrequency { value: 0.0 }) => {}
            other => panic!("expected InvalidFrequency(0.0), got {other:?}"),
        }
        match validate_custom_channels(&[131_550_000.0, -1.0]) {
            Err(CustomChannelError::InvalidFrequency { value: -1.0 }) => {}
            other => panic!("expected InvalidFrequency(-1.0), got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_span_just_under() {
        // 2.4 MHz exact span — accepted (the constraint is ≤).
        assert_eq!(
            validate_custom_channels(&[129_125_000.0, 131_525_000.0]),
            Ok(())
        );
    }

    #[test]
    fn validate_rejects_span_just_over() {
        // 2.5 MHz — rejected.
        match validate_custom_channels(&[129_000_000.0, 131_500_000.0]) {
            Err(CustomChannelError::SpanExceeded {
                low_hz,
                high_hz,
                span_hz,
            }) => {
                assert!((low_hz - 129_000_000.0).abs() < 1.0);
                assert!((high_hz - 131_500_000.0).abs() < 1.0);
                assert!((span_hz - 2_500_000.0).abs() < 1.0);
            }
            other => panic!("expected SpanExceeded, got {other:?}"),
        }
    }

    #[test]
    fn custom_channel_error_display_span_exceeded() {
        let err = CustomChannelError::SpanExceeded {
            low_hz: 129_000_000.0,
            high_hz: 131_500_000.0,
            span_hz: 2_500_000.0,
        };
        let s = format!("{err}");
        assert!(s.contains("2.5"), "span value present: {s}");
        assert!(s.contains("129"), "low freq present: {s}");
        assert!(s.contains("131"), "high freq present: {s}");
    }
}
