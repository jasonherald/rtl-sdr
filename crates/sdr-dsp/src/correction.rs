//! Signal correction processors.
//!
//! Ports SDR++ `dsp::correction` namespace.

use sdr_types::{Complex, DspError};

/// DC blocking filter — removes DC offset from a signal.
///
/// Uses the Julius O. Smith textbook topology:
/// `y[n] = x[n] - x[n-1] + R * y[n-1]`
/// where `R = 1 - (2π × cutoff / sample_rate)`.
///
/// This filter has an explicit zero at DC (z=1), guaranteeing perfect
/// DC rejection at steady state. The pole near z=R provides the
/// high-pass cutoff frequency.
pub struct DcBlocker {
    r: f32,
    last_in_re: f32,
    last_in_im: f32,
    last_out_re: f32,
    last_out_im: f32,
}

impl DcBlocker {
    /// Create a new DC blocker with the given convergence rate.
    ///
    /// The `rate` parameter sets the cutoff: `R = 1 - rate`.
    /// Typical values: 0.0001 to 0.01 (lower = narrower notch at DC).
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `rate` is not finite or not in (0, 1).
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(rate: f64) -> Result<Self, DspError> {
        if !rate.is_finite() || rate <= 0.0 || rate >= 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "rate must be in (0, 1), got {rate}"
            )));
        }
        Ok(Self {
            r: (1.0 - rate) as f32,
            last_in_re: 0.0,
            last_in_im: 0.0,
            last_out_re: 0.0,
            last_out_im: 0.0,
        })
    }

    /// Create a DC blocker from a rate in Hz and sample rate.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters produce an invalid rate.
    pub fn from_hz(rate_hz: f64, sample_rate: f64) -> Result<Self, DspError> {
        Self::new(rate_hz / sample_rate)
    }

    /// Reset the DC blocker state.
    pub fn reset(&mut self) {
        self.last_in_re = 0.0;
        self.last_in_im = 0.0;
        self.last_out_re = 0.0;
        self.last_out_im = 0.0;
    }

    /// Process complex samples, removing DC offset.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Complex],
    ) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        for (i, &s) in input.iter().enumerate() {
            // y[n] = x[n] - x[n-1] + R * y[n-1]
            let out_re = s.re - self.last_in_re + self.r * self.last_out_re;
            let out_im = s.im - self.last_in_im + self.r * self.last_out_im;
            self.last_in_re = s.re;
            self.last_in_im = s.im;
            self.last_out_re = out_re;
            self.last_out_im = out_im;
            output[i] = Complex::new(out_re, out_im);
        }
        Ok(input.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_dc_blocker_new_invalid() {
        assert!(DcBlocker::new(0.0).is_err());
        assert!(DcBlocker::new(-0.1).is_err());
        assert!(DcBlocker::new(1.0).is_err());
        assert!(DcBlocker::new(f64::NAN).is_err());
    }

    #[test]
    fn test_dc_blocker_removes_offset() {
        let mut dc = DcBlocker::new(0.01).unwrap();
        // Signal with DC offset of 5.0
        let input = vec![Complex::new(5.5, 3.0); 5000];
        let mut output = vec![Complex::default(); 5000];
        dc.process(&input, &mut output).unwrap();
        // After convergence, output should be near zero DC
        let last = output[4999];
        assert!(
            last.re.abs() < 0.5,
            "DC should be removed, re = {}",
            last.re
        );
    }

    #[test]
    fn test_dc_blocker_perfect_dc_rejection() {
        // The textbook topology has an explicit zero at DC — verify
        // that steady-state DC is perfectly rejected (not just reduced).
        let mut dc = DcBlocker::new(0.001).unwrap();
        let input = vec![Complex::new(1.0, 0.5); 50_000];
        let mut output = vec![Complex::default(); 50_000];
        dc.process(&input, &mut output).unwrap();
        // With the zero at DC, the output converges to exactly 0.0
        let last = output[49_999];
        assert!(
            last.re.abs() < 0.01,
            "DC should be perfectly rejected, re = {}",
            last.re
        );
    }

    #[test]
    fn test_dc_blocker_passes_ac() {
        let mut dc = DcBlocker::new(0.001).unwrap();
        // AC signal (alternating) with no DC
        let input: Vec<Complex> = (0..2000)
            .map(|i| {
                let v = if i % 2 == 0 { 1.0 } else { -1.0 };
                Complex::new(v, 0.0)
            })
            .collect();
        let mut output = vec![Complex::default(); 2000];
        dc.process(&input, &mut output).unwrap();
        // AC should be preserved — check amplitude in steady state
        let peak = output[1000..]
            .iter()
            .map(|s| s.re.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.9, "AC should be preserved, peak = {peak}");
    }

    #[test]
    fn test_dc_blocker_reset() {
        let mut dc = DcBlocker::new(0.01).unwrap();
        let input = vec![Complex::new(10.0, 0.0); 100];
        let mut output = vec![Complex::default(); 100];
        dc.process(&input, &mut output).unwrap();
        dc.reset();
        // After reset, state should be zero
        let zeros = vec![Complex::new(0.0, 0.0); 10];
        let mut out2 = vec![Complex::default(); 10];
        dc.process(&zeros, &mut out2).unwrap();
        assert!(out2[0].re.abs() < 1e-6, "after reset, output should be ~0");
    }
}
