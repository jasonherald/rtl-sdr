//! Analytic-signal FM discriminator: subcarrier audio → deviation (Hz).
//! Mix down by the 1900 Hz center, low-pass, then per-sample
//! instantaneous-frequency (phase difference) = deviation from center.
//! The brightness mapping lives downstream in [`super::afc::AfcMapper`],
//! which tracks the black/white references adaptively (AFC).

use std::f64::consts::PI;

use super::SUBCARRIER_CENTER_HZ;

/// One-pole low-pass cutoff (Hz) on the complex baseband — passes the
/// per-line detail (< ~1.8 kHz) while rejecting the sum image at 2·center.
const LPF_CUTOFF_HZ: f64 = 1600.0;
/// Cascaded one-pole stages. A single pole at `LPF_CUTOFF_HZ` is too gentle
/// to reject the sum image (~2·center ≈ 3.4 kHz), which then beats against
/// the wanted difference tone and — after the 0..=255 clamp — pulls every
/// steady tone toward mid-grey. Four cascaded poles give the ~4th-order
/// rolloff needed to suppress that image while keeping the 1600 Hz corner.
const LPF_POLES: usize = 4;

pub(crate) struct Discriminator {
    sr: f64,
    phase: f64,
    phase_inc: f64,
    lpf_i: [f64; LPF_POLES],
    lpf_q: [f64; LPF_POLES],
    lpf_alpha: f64,
    prev_i: f64,
    prev_q: f64,
    have_prev: bool,
}

impl Discriminator {
    pub(crate) fn new(sr: f64) -> Self {
        // one-pole smoothing factor for the chosen cutoff
        let alpha = 1.0 - (-2.0 * PI * LPF_CUTOFF_HZ / sr).exp();
        Self {
            sr,
            phase: 0.0,
            phase_inc: 2.0 * PI * SUBCARRIER_CENTER_HZ / sr,
            lpf_i: [0.0; LPF_POLES],
            lpf_q: [0.0; LPF_POLES],
            lpf_alpha: alpha,
            prev_i: 0.0,
            prev_q: 0.0,
            have_prev: false,
        }
    }

    /// Advance the cascaded one-pole low-pass by one complex sample.
    fn low_pass(&mut self, mut bi: f64, mut bq: f64) -> (f64, f64) {
        for k in 0..LPF_POLES {
            self.lpf_i[k] += self.lpf_alpha * (bi - self.lpf_i[k]);
            self.lpf_q[k] += self.lpf_alpha * (bq - self.lpf_q[k]);
            bi = self.lpf_i[k];
            bq = self.lpf_q[k];
        }
        (bi, bq)
    }

    /// Instantaneous frequency deviation from the 1900 Hz center, in Hz, for
    /// one real audio sample. Black tone ≈ −400 Hz, white tone ≈ +400 Hz. The
    /// adaptive brightness mapping is applied downstream by [`super::afc`].
    pub(crate) fn push(&mut self, s: f64) -> f64 {
        let (sn, cs) = self.phase.sin_cos();
        let bi = s * cs; // Re{ s·e^{-jθ} }
        let bq = -s * sn; // Im{ s·e^{-jθ} }
        self.phase += self.phase_inc;
        if self.phase >= 2.0 * PI {
            self.phase -= 2.0 * PI;
        }
        let (i, q) = self.low_pass(bi, bq);
        let dev_hz = if self.have_prev {
            // angle( cur · conj(prev) ) · sr / 2π  = deviation from center
            let re = i * self.prev_i + q * self.prev_q;
            let im = q * self.prev_i - i * self.prev_q;
            im.atan2(re) * self.sr / (2.0 * PI)
        } else {
            0.0
        };
        self.prev_i = i;
        self.prev_q = q;
        self.have_prev = true;
        dev_hz
    }
}
