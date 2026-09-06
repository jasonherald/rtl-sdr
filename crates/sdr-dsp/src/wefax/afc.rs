//! Automatic frequency control (AFC): adaptive brightness mapper.
//!
//! The [`super::discriminator::Discriminator`] yields deviation from the fixed
//! 1900 Hz center. A drifting signal (e.g. `SpyVerter` warm-up, ~10 Hz/s) moves
//! the black/white tones away from ±400 Hz, so a fixed −400→0 / +400→255
//! mapping clips and washes the chart to one shade. This mapper instead tracks
//! the black (low) and white (high) tone references adaptively and maps
//! brightness against them, so the scale follows the drift.
//!
//! Tracking is a **decaying-histogram percentile** estimator, not a min/max
//! envelope follower: min/max peak-tracking chases the ±2000 Hz noise tails of
//! real audio and over-widens the scale (washing a clean chart to grey). A
//! decaying histogram of recent deviations, read at fixed low/high percentiles,
//! is robust to those outliers — the tails are a negligible fraction of the
//! mass — while the exponential decay lets the estimate follow real drift.

use super::SUBCARRIER_DEV_HZ;

/// Histogram span (Hz) and bin width. Deviations are clamped into
/// [`AFC_HIST_LO_HZ`, `AFC_HIST_HI_HZ`] before binning, so the far noise tails
/// pile harmlessly into the edge bins instead of dragging a min/max.
const AFC_HIST_LO_HZ: f64 = -1000.0;
const AFC_HIST_HI_HZ: f64 = 1000.0;
const AFC_HIST_BIN_HZ: f64 = 20.0;
/// Number of histogram bins = span / bin width = 100.
const AFC_HIST_BINS: usize = 100;
/// Exponential decay time constant (s) of the histogram — older deviations
/// fade so the percentile estimate follows drift.
const AFC_DECAY_TC_SEC: f64 = 2.5;
/// How often (s) the references are recomputed and the histogram decayed.
const AFC_UPDATE_INTERVAL_SEC: f64 = 0.05;
/// Percentiles read as the black (low) and white (high) tone references.
const AFC_PCT_BLACK: f64 = 15.0;
const AFC_PCT_WHITE: f64 = 85.0;
/// Floor on white−black separation (Hz): references only update when the
/// percentiles are at least this far apart, so a contrast-free signal (all one
/// bin) leaves the mapping at its last sane scale instead of collapsing.
const AFC_MIN_SEPARATION_HZ: f64 = 200.0;

/// Adaptive black/white brightness mapper. Consumes per-sample deviation (Hz)
/// and emits 0..=255 brightness, tracking the tone references over time via a
/// decaying histogram read at low/high percentiles.
pub(crate) struct AfcMapper {
    hist: [f64; AFC_HIST_BINS],
    black_ref: f64,
    white_ref: f64,
    sample_count: u64,
    update_interval_samples: u64,
    decay_per_update: f64,
}

impl AfcMapper {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn new(sr: f64) -> Self {
        let update_interval_samples = (AFC_UPDATE_INTERVAL_SEC * sr).round() as u64;
        Self {
            hist: [0.0; AFC_HIST_BINS],
            black_ref: -SUBCARRIER_DEV_HZ,
            white_ref: SUBCARRIER_DEV_HZ,
            sample_count: 0,
            // Never zero: a sub-20 ms `sr` would be rejected upstream, but guard
            // anyway so the modulo below can't divide by zero.
            update_interval_samples: update_interval_samples.max(1),
            decay_per_update: (-(AFC_UPDATE_INTERVAL_SEC / AFC_DECAY_TC_SEC)).exp(),
        }
    }

    /// Map one deviation sample (Hz) to brightness (0=black … 255=white),
    /// accumulating it into the histogram and periodically re-estimating the
    /// references.
    // `norm` is clamped to 0.0..=1.0 before scaling, so the `as u8` is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn push(&mut self, dev_hz: f64) -> u8 {
        let c = dev_hz.clamp(AFC_HIST_LO_HZ, AFC_HIST_HI_HZ);
        let idx = (((c - AFC_HIST_LO_HZ) / AFC_HIST_BIN_HZ) as usize).min(AFC_HIST_BINS - 1);
        self.hist[idx] += 1.0;

        self.sample_count += 1;
        if self
            .sample_count
            .is_multiple_of(self.update_interval_samples)
        {
            self.update_refs();
        }

        let norm = (dev_hz - self.black_ref) / (self.white_ref - self.black_ref);
        (norm.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    /// Decay the histogram one update step and, if the black/white percentiles
    /// are meaningfully separated, adopt them as the new references.
    fn update_refs(&mut self) {
        for h in &mut self.hist {
            *h *= self.decay_per_update;
        }
        let (Some(pb), Some(pw)) = (
            self.percentile(AFC_PCT_BLACK),
            self.percentile(AFC_PCT_WHITE),
        ) else {
            return;
        };
        if pw - pb > AFC_MIN_SEPARATION_HZ {
            self.black_ref = pb;
            self.white_ref = pw;
        }
    }

    /// Deviation (Hz) at the `p`-th percentile of the decaying histogram, or
    /// `None` if the histogram carries no mass. Returns the center of the bin
    /// where the cumulative count first crosses `p/100 · total`.
    #[allow(clippy::cast_precision_loss)]
    fn percentile(&self, p: f64) -> Option<f64> {
        let total: f64 = self.hist.iter().sum();
        if total <= 0.0 {
            return None;
        }
        let target = (p / 100.0) * total;
        let mut cum = 0.0;
        for (i, &h) in self.hist.iter().enumerate() {
            cum += h;
            if cum >= target {
                return Some(AFC_HIST_LO_HZ + (i as f64 + 0.5) * AFC_HIST_BIN_HZ);
            }
        }
        Some(AFC_HIST_HI_HZ - 0.5 * AFC_HIST_BIN_HZ)
    }

    /// Current tracked black reference (deviation Hz). Test-only observer.
    #[cfg(test)]
    pub(crate) fn black_ref(&self) -> f64 {
        self.black_ref
    }
}
