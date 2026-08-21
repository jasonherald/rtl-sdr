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

/// Largest per-sample output power (|y|²) for which the LMS estimate is
/// updated. The SDR++ loop is tuned for IQ normalised to ±1 (|y|² ≤ 2 for
/// a full-scale sample); anything an order of magnitude above that is
/// out-of-contract input (float sources pass samples through unscaled),
/// and adapting on it would drive the `f32` estimate to infinity within a
/// few samples. Such samples are still corrected with the current
/// estimate; they just do not train it.
pub const IQ_CORRECTION_MAX_UPDATE_POWER: f32 = 16.0;

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
///
/// # Input scale
///
/// Like the SDR++ original, the step size is proportional to sample power
/// (`y²·rate`), so the loop is tuned for IQ normalised to roughly ±1 —
/// which is what [`crate::convert`] produces from 8-bit samples and what
/// `IqFrontend` feeds it. Grossly over-scaled input (|x| ≫ 1)
/// would overflow the `f32` estimate within a few samples. Because float
/// sources (`FileSource`, `NetworkSource::Float32`) hand samples through
/// unscaled, `process` therefore only trains on samples whose output
/// power is at most [`IQ_CORRECTION_MAX_UPDATE_POWER`] — over-scaled
/// samples are still corrected with the current estimate, so the output
/// stays finite — and additionally re-arms the estimate to zero if it ever
/// stops being finite. For in-contract input the update is identical to
/// the SDR++ original.
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
            let power = out.re * out.re + out.im * out.im;
            if power <= IQ_CORRECTION_MAX_UPDATE_POWER {
                c += (out * out) * self.rate;
            }
            *y = out;
        }
        // Self-heal: an out-of-contract block (see "Input scale") can blow
        // the estimate up to inf/NaN; never carry that into the next block.
        self.correction = if c.re.is_finite() && c.im.is_finite() {
            c
        } else {
            Complex::default()
        };
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

    // ---- IqCorrector test fixture parameters -------------------------------
    /// Length of the synthesized tone buffer; the tone is periodic in this
    /// length, so repeated passes over it form one continuous signal.
    const IQ_TEST_LEN: usize = 65_536;
    /// Tone frequency as a fraction of the sample rate (fs / 8).
    const IQ_TEST_TONE_DIV: usize = 8;
    /// Number of trailing samples measured by the DFT.
    const IQ_TEST_TAIL_LEN: usize = 8_192;
    /// DFT bin of the tone within a `IQ_TEST_TAIL_LEN`-sample window: a tone
    /// at fs/8 completes `TAIL_LEN / 8` cycles per window. Its image is at
    /// the negative of this bin.
    #[allow(clippy::cast_possible_wrap)] // 1024 — cannot wrap
    const IQ_TEST_TAIL_TONE_BIN: i64 = (IQ_TEST_TAIL_LEN / IQ_TEST_TONE_DIV) as i64;
    /// Injected I/Q gain error (10 %) and phase error (0.1 rad).
    const IQ_TEST_GAIN_ERR: f32 = 0.10;
    const IQ_TEST_PHASE_ERR: f32 = 0.10;
    /// LMS time constant is 1/(2·rate) = 50 k samples; five passes over
    /// 64 k samples ≈ 6.5 time constants.
    const IQ_TEST_PASSES: usize = 5;
    /// Required image-rejection improvement once converged.
    const IQ_TEST_MIN_IMPROVEMENT_DB: f32 = 20.0;
    /// Buffer sizes for the short passthrough / reset fixtures.
    const IQ_TEST_SHORT_LEN: usize = 4_096;
    const IQ_TEST_RESET_LEN: usize = 16_384;
    /// Sample-equality tolerance for the untouched-passthrough check.
    const IQ_TEST_PASSTHROUGH_TOL: f32 = 1e-3;
    /// Out-of-contract amplitude (the corrector expects |x| ≲ 1).
    const IQ_TEST_OVERSCALE_AMPLITUDE: f32 = 400.0;
    const IQ_TEST_OVERSCALE_LEN: usize = 64;

    /// Synthesize a complex tone with gain + phase imbalance between I and Q
    /// (the classic image-producing receiver defect). The tone completes
    /// `n / tone_div` cycles over `n` samples.
    #[allow(clippy::cast_precision_loss)]
    fn imbalanced_tone(n: usize, tone_div: usize, gain_err: f32, phase_err: f32) -> Vec<Complex> {
        let cycles = (n / tone_div) as f32;
        (0..n)
            .map(|i| {
                let theta = 2.0 * std::f32::consts::PI * cycles * i as f32 / n as f32;
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

    /// Image-to-tone power ratio over the last `IQ_TEST_TAIL_LEN` samples.
    fn image_ratio(x: &[Complex]) -> f32 {
        let tail = &x[x.len() - IQ_TEST_TAIL_LEN..];
        bin_power(tail, -IQ_TEST_TAIL_TONE_BIN) / bin_power(tail, IQ_TEST_TAIL_TONE_BIN)
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
    fn iq_corrector_improves_image_rejection() {
        let input = imbalanced_tone(
            IQ_TEST_LEN,
            IQ_TEST_TONE_DIV,
            IQ_TEST_GAIN_ERR,
            IQ_TEST_PHASE_ERR,
        );
        let before = image_ratio(&input);

        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let mut output = vec![Complex::default(); IQ_TEST_LEN];
        for _ in 0..IQ_TEST_PASSES {
            let n = corr.process(&input, &mut output).unwrap();
            assert_eq!(n, IQ_TEST_LEN);
        }
        let after = image_ratio(&output);

        let improvement_db = 10.0 * (before / after).log10();
        assert!(
            improvement_db > IQ_TEST_MIN_IMPROVEMENT_DB,
            "image rejection should improve by > {IQ_TEST_MIN_IMPROVEMENT_DB} dB once converged, got {improvement_db:.1} dB (before {before:e}, after {after:e})"
        );
    }

    #[test]
    fn iq_corrector_passes_balanced_signal_unchanged() {
        let input = imbalanced_tone(IQ_TEST_SHORT_LEN, IQ_TEST_TONE_DIV, 0.0, 0.0);
        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let mut output = vec![Complex::default(); IQ_TEST_SHORT_LEN];
        corr.process(&input, &mut output).unwrap();
        for (a, b) in input.iter().zip(&output) {
            assert!(
                (a.re - b.re).abs() < IQ_TEST_PASSTHROUGH_TOL
                    && (a.im - b.im).abs() < IQ_TEST_PASSTHROUGH_TOL
            );
        }
    }

    #[test]
    fn iq_corrector_reset_clears_estimate() {
        let input = imbalanced_tone(IQ_TEST_RESET_LEN, IQ_TEST_TONE_DIV, 0.2, 0.0);
        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let mut output = vec![Complex::default(); input.len()];
        corr.process(&input, &mut output).unwrap();
        assert!(corr.correction().amplitude() > 0.0);
        corr.reset();
        assert!(corr.correction().amplitude() < f32::EPSILON);
    }

    /// Input far outside the documented ±1 scale must not poison the
    /// estimate permanently: the state is re-armed and the corrector keeps
    /// working on the next in-contract block.
    #[test]
    fn iq_corrector_recovers_from_overscaled_input() {
        let mut corr = IqCorrector::new(IQ_CORRECTION_DEFAULT_RATE).unwrap();
        let blast = vec![Complex::new(IQ_TEST_OVERSCALE_AMPLITUDE, 0.0); IQ_TEST_OVERSCALE_LEN];
        let mut out = vec![Complex::default(); IQ_TEST_OVERSCALE_LEN];
        corr.process(&blast, &mut out).unwrap();
        // Float sources (FileSource, NetworkSource::Float32) pass samples
        // through unscaled, so the output must stay finite *during* the
        // over-scaled block too, not just after it.
        assert!(
            out.iter().all(|s| s.re.is_finite() && s.im.is_finite()),
            "output must stay finite during an over-scaled block"
        );
        assert!(
            corr.correction().re.is_finite() && corr.correction().im.is_finite(),
            "estimate must be finite after an over-scaled block, got {:?}",
            corr.correction()
        );

        let input = imbalanced_tone(IQ_TEST_SHORT_LEN, IQ_TEST_TONE_DIV, 0.0, 0.0);
        let mut output = vec![Complex::default(); IQ_TEST_SHORT_LEN];
        corr.process(&input, &mut output).unwrap();
        assert!(output.iter().all(|s| s.re.is_finite() && s.im.is_finite()));
        for (a, b) in input.iter().zip(&output) {
            assert!(
                (a.re - b.re).abs() < IQ_TEST_PASSTHROUGH_TOL
                    && (a.im - b.im).abs() < IQ_TEST_PASSTHROUGH_TOL,
                "corrector must resume clean passthrough after recovery"
            );
        }
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
