//! Gardner symbol-timing recovery for Meteor LRPT QPSK.
//!
//! Non-data-aided timing recovery — uses three consecutive
//! oversampled samples (early / mid / late) to compute a timing-
//! error estimate, drives a 2nd-order loop filter on the
//! symbol-period (`omega`) and fractional-offset (`mu`)
//! accumulators, and produces one decimated output sample per
//! recovered symbol.
//!
//! Linear interpolation between adjacent input samples is
//! sufficient at 2 sps for Meteor's signal characteristics; a
//! polyphase / cubic interpolator would only matter at higher
//! oversampling ratios.
//!
//! Reference (read-only): `original/meteor_demod/dsp/timing.c`.

use sdr_types::Complex;

/// `omega` clamp: ± fractional drift the loop will tolerate
/// before the symbol period is hard-locked. Matches `meteor_demod`.
const OMEGA_LIM: f32 = 0.005;

/// Gardner timing recovery. Single-channel, takes
/// `samples_per_symbol` samples per symbol in (typically 2.0) and
/// emits one symbol per recovered timing tick.
pub struct Gardner {
    /// Fractional offset, conceptually in `[0, 1)`.
    mu: f32,
    /// Current symbol period in samples.
    omega: f32,
    /// Nominal symbol period (= `samples_per_symbol`).
    omega_mid: f32,
    /// Tracking gain on `mu` (fractional offset).
    gain_mu: f32,
    /// Tracking gain on `omega` (symbol period).
    gain_omega: f32,
    /// Mid-point sample from the previous symbol period.
    mid: Complex,
    /// Buffered input awaiting consumption.
    pending: Vec<Complex>,
}

impl Gardner {
    /// `samples_per_symbol` is the input rate (typically 2.0).
    /// `omega_gain` and `mu_gain` are independent loop gains —
    /// see SDR++'s `meteor_demodulator/src/main.cpp` for the
    /// canonical values (1e-6 and 0.01 for Meteor).
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if any input is not
    /// finite, or `samples_per_symbol` is not positive. Project
    /// convention for DSP constructors: NaN / Inf inputs would
    /// silently break timing recovery rather than fail loudly.
    pub fn new(
        samples_per_symbol: f32,
        omega_gain: f32,
        mu_gain: f32,
    ) -> Result<Self, sdr_types::DspError> {
        if !samples_per_symbol.is_finite() || samples_per_symbol <= 0.0 {
            return Err(sdr_types::DspError::InvalidParameter(format!(
                "samples_per_symbol must be finite and positive, got {samples_per_symbol}"
            )));
        }
        if !omega_gain.is_finite() || !mu_gain.is_finite() {
            return Err(sdr_types::DspError::InvalidParameter(format!(
                "omega_gain ({omega_gain}) and mu_gain ({mu_gain}) must both be finite"
            )));
        }
        Ok(Self {
            mu: 0.0,
            omega: samples_per_symbol,
            omega_mid: samples_per_symbol,
            gain_mu: mu_gain,
            gain_omega: omega_gain,
            mid: Complex::new(0.0, 0.0),
            pending: Vec::with_capacity(8),
        })
    }

    /// Push one input sample. May produce 0 or 1 output symbols
    /// depending on where the timing tick lands; returns the
    /// recovered symbol if a tick fired.
    pub fn process(&mut self, x: Complex) -> Option<Complex> {
        self.pending.push(x);
        if self.pending.len() < 3 {
            return None;
        }
        let early = self.pending[0];
        let mid_in = self.pending[1];
        let late = self.pending[2];
        // Linear interpolation between `mid_in` and `late` at
        // fractional position `mu`.
        let interp = mid_in * (1.0 - self.mu) + late * self.mu;
        // Gardner error: Im(conj(mid) · (late - early)) for QPSK.
        let diff = late - early;
        let err_complex = self.mid.conj() * diff;
        let err = err_complex.im;
        // 2nd-order loop filter on `omega` (period) and `mu`
        // (fractional offset).
        self.omega += self.gain_omega * err;
        self.omega = self
            .omega
            .clamp(self.omega_mid - OMEGA_LIM, self.omega_mid + OMEGA_LIM);
        self.mu += self.omega + self.gain_mu * err;
        // Whole-sample portion of `mu` becomes the consume count.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "mu is bounded by omega_mid (~2.0) so floor fits in usize \
                      and back to f32 without precision loss"
        )]
        let (consume, mu_decrement) = {
            let consume = self.mu.floor() as usize;
            (consume, consume as f32)
        };
        self.mu -= mu_decrement;
        let drop = consume.min(self.pending.len());
        self.pending.drain(0..drop);
        self.mid = mid_in;
        Some(interp)
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    #[test]
    fn recovers_symbol_rate_from_2sps_input() {
        // Synthesize 2 sps QPSK with no timing jitter — every
        // other input sample is a symbol, alternating with a
        // (zero) sample between. Gardner should converge to
        // emitting one output per input pair.
        let mut g = Gardner::new(2.0, 1e-6, 0.01).expect("Gardner::new");
        let mut emitted = 0_usize;
        for n in 0..2000 {
            let on_symbol = n % 2 == 0;
            let s = if on_symbol {
                Complex::new(0.707, 0.707)
            } else {
                Complex::new(0.0, 0.0)
            };
            if g.process(s).is_some() {
                emitted += 1;
            }
        }
        assert!(
            (900..1100).contains(&emitted),
            "expected ~1000 emitted, got {emitted}",
        );
    }
}
