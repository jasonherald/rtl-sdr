//! Orbcomm downlink decoder — SDPSK 4800 bps, multi-channel.
//! Pure decode: no I/O, no threads, no GTK. Issue #865; protocol
//! reference in docs/superpowers/specs/2026-08-29-orbcomm-decoder-design.md.

pub mod channelizer;
pub mod deframe;
pub mod demod;
pub mod packet;
pub mod reassembly;
pub mod sat_names;
#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) mod testutil;

pub use channelizer::{ChannelBank, ChannelStats, OrbcommEvent, OrbcommEventKind};

/// Active Orbcomm subscriber downlink channels (Hz), low to high.
pub const ORBCOMM_CHANNELS_HZ: [f64; 9] = [
    137_225_000.0,
    137_250_000.0,
    137_440_000.0,
    137_460_000.0,
    137_662_500.0,
    137_687_500.0,
    137_717_500.0,
    137_737_500.0,
    137_800_000.0,
];
/// SDPSK symbol rate.
pub const SYMBOL_RATE_HZ: f64 = 4800.0;
/// Per-channel complex sample rate after decimation (4 samples/symbol).
pub const CHANNEL_SAMPLE_RATE_HZ: f64 = 19_200.0;
/// Samples per symbol at [`CHANNEL_SAMPLE_RATE_HZ`].
pub const SAMPLES_PER_SYMBOL: usize = 4;

/// Errors surfaced by [`ChannelBank`] construction and processing.
#[derive(Debug, thiserror::Error)]
pub enum OrbcommError {
    /// A DSP building block failed to construct.
    #[error("orbcomm DSP init failed: {0}")]
    Dsp(#[from] sdr_types::DspError),
    /// No requested channel fits inside the source span.
    #[error(
        "no orbcomm channel inside the source span (center {center_hz} Hz, rate {source_rate_hz} Hz)"
    )]
    NoChannelsInSpan { center_hz: f64, source_rate_hz: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_list_is_sorted_and_in_band() {
        assert_eq!(ORBCOMM_CHANNELS_HZ.len(), 9);
        for w in ORBCOMM_CHANNELS_HZ.windows(2) {
            assert!(w[0] < w[1]);
        }
        for f in ORBCOMM_CHANNELS_HZ {
            assert!((137_200_000.0..=137_800_000.0).contains(&f));
        }
        #[allow(clippy::cast_precision_loss)]
        let expected_ratio = SAMPLES_PER_SYMBOL as f64;
        assert!((CHANNEL_SAMPLE_RATE_HZ / SYMBOL_RATE_HZ - expected_ratio).abs() < f64::EPSILON);
    }
}
