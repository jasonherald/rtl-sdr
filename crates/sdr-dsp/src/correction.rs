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
    /// Returns `DspError::InvalidParameter` if `rate` is not finite or not in
    /// (0, 1), or if the `f32` pole coefficient `R = 1 - rate` the filter
    /// actually runs with is no longer strictly inside (0, 1) (a rate that
    /// vanishes in `f32` would leave `R == 1.0` and never reject DC).
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(rate: f64) -> Result<Self, DspError> {
        if !rate.is_finite() || rate <= 0.0 || rate >= 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "rate must be in (0, 1), got {rate}"
            )));
        }
        let r = (1.0 - rate) as f32;
        if r <= 0.0 || r >= 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "rate {rate} leaves the f32 pole coefficient at {r}, outside (0, 1)"
            )));
        }
        Ok(Self {
            r,
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

/// Default adaptation rate for [`IqCorrector`] — the value SDR++'s source
/// modules pass to `dsp::correction::IQCorrector::init` (`0.00001`).
/// Small enough that the estimate integrates over ~10⁵ samples (tens of
/// milliseconds at typical SDR rates) instead of chasing the modulation.
pub const IQ_CORRECTION_DEFAULT_RATE: f64 = 0.000_01;

/// Adaptive IQ-imbalance corrector.
///
/// Ports SDR++ `dsp::correction::IQCorrector` (Moseley–Slump blind
/// compensation). A receiver with mismatched I/Q gain or phase produces an
/// image of every signal mirrored about DC; the corrector tracks a single
/// complex coefficient `c` that minimises the correlation between the
/// output and its own conjugate:
///
/// ```text
/// y[n] = x[n] − conj(x[n]) · c
/// c   += y[n]² · rate
/// ```
///
/// It is a plain LMS loop: no divisions, no trig, one complex multiply-add
/// per sample, and it converges from a cold start on any signal that is not
/// itself conjugate-symmetric (i.e. anything but a pure real baseband).
pub struct IqCorrector {
    correction: Complex,
    rate: f32,
}

impl IqCorrector {
    /// Create a corrector with the given adaptation rate.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `rate` is not finite, not in
    /// (0, 1), or no longer strictly inside (0, 1) after conversion to the
    /// `f32` the loop runs with (underflow to `0.0` or rounding up to `1.0`).
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(rate: f64) -> Result<Self, DspError> {
        if !rate.is_finite() || rate <= 0.0 || rate >= 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "IQ correction rate must be in (0, 1), got {rate}"
            )));
        }
        // Validate the value the loop will actually use: a rate that is in
        // (0, 1) in f64 can underflow to 0.0 (never adapts) or round up to
        // 1.0 (unstable) in f32.
        let rate_f32 = rate as f32;
        if rate_f32 <= 0.0 || rate_f32 >= 1.0 {
            return Err(DspError::InvalidParameter(format!(
                "IQ correction rate {rate} is not strictly inside (0, 1) as f32 ({rate_f32})"
            )));
        }
        Ok(Self {
            correction: Complex::default(),
            rate: rate_f32,
        })
    }

    /// Current imbalance estimate (zero = no correction applied).
    pub fn correction(&self) -> Complex {
        self.correction
    }

    /// Forget the imbalance estimate (e.g. on retune / source restart).
    pub fn reset(&mut self) {
        self.correction = Complex::default();
    }

    /// Process complex samples, cancelling the I/Q image.
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
        let mut c = self.correction;
        for (x, y) in input.iter().zip(output.iter_mut()) {
            let out = *x - x.conj() * c;
            c += (out * out) * self.rate;
            *y = out;
        }
        self.correction = c;
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

    /// Synthesize a complex tone with gain + phase imbalance between I and Q
    /// (the classic image-producing receiver defect).
    #[allow(clippy::cast_precision_loss)]
    fn imbalanced_tone(n: usize, bin: usize, gain_err: f32, phase_err: f32) -> Vec<Complex> {
        (0..n)
            .map(|i| {
                let theta = 2.0 * std::f32::consts::PI * bin as f32 * i as f32 / n as f32;
                Complex::new((1.0 + gain_err) * theta.cos(), (theta + phase_err).sin())
            })
            .collect()
    }

    /// Power at DFT bin `k` over `x` (k may be negative for the image).
    #[allow(clippy::cast_precision_loss)]
    fn bin_power(x: &[Complex], k: i64) -> f32 {
        let n = x.len() as f32;
        let (mut re, mut im) = (0.0_f32, 0.0_f32);
        for (i, s) in x.iter().enumerate() {
            let a = -2.0 * std::f32::consts::PI * k as f32 * i as f32 / n;
            re += s.re * a.cos() - s.im * a.sin();
            im += s.re * a.sin() + s.im * a.cos();
        }
        (re * re + im * im) / (n * n)
    }

    /// Rates that pass the f64 range check but whose f32 coefficient
    /// rounds to exactly 1.0 would build a blocker that never attenuates DC.
    #[test]
    fn dc_blocker_rejects_rate_that_vanishes_in_f32() {
        assert!(DcBlocker::new(1e-50).is_err());
        assert!(DcBlocker::new(f64::MIN_POSITIVE).is_err());
        assert!(DcBlocker::new(1e-4).is_ok());
    }

    #[test]
    fn iq_corrector_rejects_invalid_rate() {
        assert!(IqCorrector::new(0.0).is_err());
        assert!(IqCorrector::new(-1e-5).is_err());
        assert!(IqCorrector::new(f64::NAN).is_err());
        assert!(IqCorrector::new(1.5).is_err());
        assert!(IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).is_ok());
        // Positive in f64 but underflows to 0.0 in f32 — would construct a
        // corrector that never adapts.
        assert!(IqCorrector::new(f64::MIN_POSITIVE).is_err());
        assert!(IqCorrector::new(1e-50).is_err());
        // Below 1.0 in f64 but rounds to exactly 1.0 in f32.
        assert!(IqCorrector::new(1.0 - 1e-12).is_err());
    }

    #[test]
    #[allow(clippy::cast_possible_wrap)]
    fn iq_corrector_improves_image_rejection() {
        // 64 k samples of a tone at +fs/8 with 10 % gain and 0.1 rad phase error.
        const N: usize = 65_536;
        const BIN: usize = N / 8;
        let input = imbalanced_tone(N, BIN, 0.10, 0.10);
        let tail = &input[N - 8192..];
        let before = bin_power(tail, -(BIN as i64 / 8)) / bin_power(tail, BIN as i64 / 8);

        // LMS time constant is 1/(2·rate) = 50 k samples; run ~6 time
        // constants (the tone is periodic in N, so repeated passes are
        // a continuous signal).
        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let mut output = vec![Complex::default(); N];
        for _ in 0..5 {
            let n = corr.process(&input, &mut output).unwrap();
            assert_eq!(n, N);
        }
        let tail = &output[N - 8192..];
        let after = bin_power(tail, -(BIN as i64 / 8)) / bin_power(tail, BIN as i64 / 8);

        let improvement_db = 10.0 * (before / after).log10();
        assert!(
            improvement_db > 20.0,
            "image rejection should improve by > 20 dB once converged, got {improvement_db:.1} dB (before {before:e}, after {after:e})"
        );
    }

    #[test]
    fn iq_corrector_passes_balanced_signal_unchanged() {
        let input = imbalanced_tone(4096, 512, 0.0, 0.0);
        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let mut output = vec![Complex::default(); 4096];
        corr.process(&input, &mut output).unwrap();
        for (a, b) in input.iter().zip(&output) {
            assert!((a.re - b.re).abs() < 1e-3 && (a.im - b.im).abs() < 1e-3);
        }
    }

    #[test]
    fn iq_corrector_reset_clears_estimate() {
        let input = imbalanced_tone(16_384, 2048, 0.2, 0.0);
        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let mut output = vec![Complex::default(); input.len()];
        corr.process(&input, &mut output).unwrap();
        assert!(corr.correction().amplitude() > 0.0);
        corr.reset();
        assert!(corr.correction().amplitude() < f32::EPSILON);
    }

    #[test]
    fn iq_corrector_buffer_too_small() {
        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let input = vec![Complex::default(); 8];
        let mut output = vec![Complex::default(); 4];
        assert!(matches!(
            corr.process(&input, &mut output),
            Err(DspError::BufferTooSmall { .. })
        ));
    }
}
