//! Demodulator implementations.
//!
//! Ports SDR++ `dsp::demod` namespace. These are the core demodulation
//! algorithms that extract audio from modulated IQ signals.

use sdr_types::{Complex, DspError};

use crate::math;

/// Quadrature FM demodulator — extracts instantaneous frequency from IQ.
///
/// Ports SDR++ `dsp::demod::Quadrature`. Computes the phase difference
/// between successive samples, normalized by the deviation:
/// `out[i] = normalize_phase(phase[i] - phase[i-1]) * inv_deviation`
pub struct Quadrature {
    inv_deviation: f32,
    last_phase: f32,
}

impl Quadrature {
    /// Create a new quadrature demodulator.
    ///
    /// - `deviation`: maximum frequency deviation in radians/sample
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `deviation` is non-positive or non-finite.
    pub fn new(deviation: f32) -> Result<Self, DspError> {
        if !deviation.is_finite() || deviation <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "deviation must be positive and finite, got {deviation}"
            )));
        }
        let inv_deviation = 1.0 / deviation;
        if !inv_deviation.is_finite() {
            return Err(DspError::InvalidParameter(format!(
                "deviation is too small to represent safely, got {deviation}"
            )));
        }
        Ok(Self {
            inv_deviation,
            last_phase: 0.0,
        })
    }

    /// Create from deviation in Hz and sample rate.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are invalid.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_hz(deviation_hz: f64, sample_rate: f64) -> Result<Self, DspError> {
        if !deviation_hz.is_finite() || deviation_hz <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "deviation_hz must be positive and finite, got {deviation_hz}"
            )));
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "sample_rate must be positive and finite, got {sample_rate}"
            )));
        }
        #[allow(clippy::cast_possible_truncation)]
        let dev = math::hz_to_rads(deviation_hz, sample_rate) as f32;
        Self::new(dev)
    }

    /// Reset the demodulator state.
    pub fn reset(&mut self) {
        self.last_phase = 0.0;
    }

    /// Process complex samples, outputting demodulated audio.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[Complex], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        for (i, &s) in input.iter().enumerate() {
            let current_phase = s.phase();
            output[i] = math::normalize_phase(current_phase - self.last_phase) * self.inv_deviation;
            self.last_phase = current_phase;
        }
        Ok(input.len())
    }
}

/// AM demodulator — extracts amplitude envelope from IQ.
///
/// Ports SDR++ `dsp::demod::AM`. Computes the magnitude of each
/// complex sample: `out[i] = sqrt(re^2 + im^2)`.
///
/// Typically followed by DC blocking and AGC in the radio module.
pub struct AmDemod {
    dc_offset: f32,
    dc_rate: f32,
}

/// DC blocker convergence rate for AM demodulator.
const AM_DC_RATE: f32 = 0.001;

impl AmDemod {
    /// Create a new AM demodulator with built-in DC blocking.
    pub fn new() -> Self {
        Self {
            dc_offset: 0.0,
            dc_rate: AM_DC_RATE,
        }
    }

    /// Reset the demodulator state.
    pub fn reset(&mut self) {
        self.dc_offset = 0.0;
    }

    /// Process complex samples, outputting demodulated audio.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[Complex], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        for (i, &s) in input.iter().enumerate() {
            let amp = s.amplitude();
            // Inline DC blocking matching C++ pattern
            let out = amp - self.dc_offset;
            self.dc_offset += out * self.dc_rate;
            output[i] = out;
        }
        Ok(input.len())
    }
}

impl Default for AmDemod {
    fn default() -> Self {
        Self::new()
    }
}

/// FM demodulator — quadrature discriminator with deviation normalization.
///
/// Ports SDR++ `dsp::demod::FM`. This is essentially a `Quadrature` demod
/// with the deviation set to the FM channel bandwidth.
pub struct FmDemod {
    quad: Quadrature,
}

impl FmDemod {
    /// Create a new FM demodulator.
    ///
    /// - `deviation`: maximum frequency deviation in radians/sample
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `deviation` is invalid.
    pub fn new(deviation: f32) -> Result<Self, DspError> {
        Ok(Self {
            quad: Quadrature::new(deviation)?,
        })
    }

    /// Create from deviation in Hz and sample rate.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are invalid.
    pub fn from_hz(deviation_hz: f64, sample_rate: f64) -> Result<Self, DspError> {
        Ok(Self {
            quad: Quadrature::from_hz(deviation_hz, sample_rate)?,
        })
    }

    /// Reset the demodulator state.
    pub fn reset(&mut self) {
        self.quad.reset();
    }

    /// Process complex samples, outputting demodulated audio.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[Complex], output: &mut [f32]) -> Result<usize, DspError> {
        self.quad.process(input, output)
    }
}

/// Broadcast FM demodulator — wideband FM discrimination stage.
///
/// Ports the discrimination stage of SDR++ `dsp::demod::BroadcastFM`.
/// Uses quadrature discrimination with deviation set for broadcast FM (75kHz).
/// Deemphasis filtering, stereo decode, and RDS are handled at the radio
/// module level (`sdr-radio`), not in this discriminator.
pub struct BroadcastFmDemod {
    quad: Quadrature,
}

/// Standard broadcast FM deviation: 75 kHz.
const BROADCAST_FM_DEVIATION_HZ: f64 = 75_000.0;

impl BroadcastFmDemod {
    /// Create a new broadcast FM demodulator.
    ///
    /// - `sample_rate`: input sample rate in Hz
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `sample_rate` is invalid.
    pub fn new(sample_rate: f64) -> Result<Self, DspError> {
        Ok(Self {
            quad: Quadrature::from_hz(BROADCAST_FM_DEVIATION_HZ, sample_rate)?,
        })
    }

    /// Reset the demodulator state.
    pub fn reset(&mut self) {
        self.quad.reset();
    }

    /// Process complex samples, outputting raw FM discriminator output.
    ///
    /// The output is the broadcast FM composite baseband signal, not final audio.
    /// Deemphasis and stereo decode are applied downstream in `sdr-radio`.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[Complex], output: &mut [f32]) -> Result<usize, DspError> {
        self.quad.process(input, output)
    }
}

/// SSB demodulator — single-sideband demodulation via frequency translation.
///
/// Faithful port of SDR++ `dsp::demod::SSB`. Uses a frequency translator
/// to shift the sideband to baseband before extracting the real part:
/// - USB: translate by `+bandwidth/2`, then extract real
/// - LSB: translate by `-bandwidth/2`, then extract real
/// - DSB: no translation, extract real
pub struct SsbDemod {
    mode: SsbMode,
    xlator: crate::channel::FrequencyXlator,
    xlator_buf: Vec<Complex>,
    bandwidth: f64,
    sample_rate: f64,
}

/// SSB demodulation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SsbMode {
    /// Upper sideband.
    Usb,
    /// Lower sideband.
    Lsb,
    /// Double sideband (both sidebands).
    Dsb,
}

impl SsbDemod {
    /// Create a new SSB demodulator.
    ///
    /// - `mode`: USB, LSB, or DSB
    /// - `bandwidth`: channel bandwidth in Hz
    /// - `sample_rate`: sample rate in Hz
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `bandwidth` or `sample_rate` are invalid.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(mode: SsbMode, bandwidth: f64, sample_rate: f64) -> Result<Self, DspError> {
        if !bandwidth.is_finite() || bandwidth <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "bandwidth must be positive and finite, got {bandwidth}"
            )));
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "sample_rate must be positive and finite, got {sample_rate}"
            )));
        }
        let translation = Self::get_translation(mode, bandwidth);
        let xlator = crate::channel::FrequencyXlator::from_hz(translation, sample_rate);
        Ok(Self {
            mode,
            xlator,
            xlator_buf: Vec::new(),
            bandwidth,
            sample_rate,
        })
    }

    /// Set the demodulation mode.
    pub fn set_mode(&mut self, mode: SsbMode) {
        self.mode = mode;
        let translation = Self::get_translation(mode, self.bandwidth);
        self.xlator = crate::channel::FrequencyXlator::from_hz(translation, self.sample_rate);
    }

    /// Set the bandwidth.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `bandwidth` is non-positive or non-finite.
    pub fn set_bandwidth(&mut self, bandwidth: f64) -> Result<(), DspError> {
        if !bandwidth.is_finite() || bandwidth <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "bandwidth must be positive and finite, got {bandwidth}"
            )));
        }
        self.bandwidth = bandwidth;
        let translation = Self::get_translation(self.mode, bandwidth);
        self.xlator = crate::channel::FrequencyXlator::from_hz(translation, self.sample_rate);
        Ok(())
    }

    /// Get the frequency translation for the given mode and bandwidth.
    ///
    /// Ports `SSB::getTranslation()`.
    fn get_translation(mode: SsbMode, bandwidth: f64) -> f64 {
        match mode {
            SsbMode::Usb => bandwidth / 2.0,
            SsbMode::Lsb => -bandwidth / 2.0,
            SsbMode::Dsb => 0.0,
        }
    }

    /// Process complex samples, outputting demodulated audio.
    ///
    /// Translates the sideband to baseband, then extracts the real part.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[Complex], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }

        // Step 1: Frequency translate to move sideband to baseband
        self.xlator_buf.resize(input.len(), Complex::default());
        self.xlator.process(input, &mut self.xlator_buf)?;

        // Step 2: Extract real part (ComplexToReal)
        for (i, &s) in self.xlator_buf.iter().enumerate() {
            output[i] = s.re;
        }

        Ok(input.len())
    }
}

/// CW (Continuous Wave / Morse) demodulator.
///
/// Faithful port of SDR++ `dsp::demod::CW`. Uses `FrequencyXlator` for BFO
/// mixing, then extracts real part and applies AGC for consistent amplitude.
pub struct CwDemod {
    xlator: crate::channel::FrequencyXlator,
    agc: crate::loops::Agc,
    xlator_buf: Vec<Complex>,
    agc_buf: Vec<f32>,
}

/// Default AGC attack coefficient for CW.
const CW_AGC_ATTACK: f32 = 0.1;
/// Default AGC decay coefficient for CW.
const CW_AGC_DECAY: f32 = 0.01;
/// AGC maximum gain (matching C++ 10e6).
const CW_AGC_MAX_GAIN: f32 = 10e6;
/// AGC maximum output amplitude (matching C++ 10.0).
const CW_AGC_MAX_OUTPUT: f32 = 10.0;
/// AGC set point (target amplitude).
const CW_AGC_SET_POINT: f32 = 1.0;
/// AGC initial gain.
const CW_AGC_INIT_GAIN: f32 = 1.0;

impl CwDemod {
    /// Create a new CW demodulator.
    ///
    /// - `tone_offset_hz`: BFO tone offset in Hz (typically 700-1000 Hz)
    /// - `sample_rate`: sample rate in Hz
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are invalid.
    pub fn from_hz(tone_offset_hz: f64, sample_rate: f64) -> Result<Self, DspError> {
        if !tone_offset_hz.is_finite() {
            return Err(DspError::InvalidParameter(format!(
                "tone_offset_hz must be finite, got {tone_offset_hz}"
            )));
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "sample_rate must be positive and finite, got {sample_rate}"
            )));
        }
        let xlator = crate::channel::FrequencyXlator::from_hz(tone_offset_hz, sample_rate);
        let agc = crate::loops::Agc::new(
            CW_AGC_SET_POINT,
            CW_AGC_ATTACK,
            CW_AGC_DECAY,
            CW_AGC_MAX_GAIN,
            CW_AGC_MAX_OUTPUT,
            CW_AGC_INIT_GAIN,
        )?;
        Ok(Self {
            xlator,
            agc,
            xlator_buf: Vec::new(),
            agc_buf: Vec::new(),
        })
    }

    /// Reset the demodulator state.
    pub fn reset(&mut self) {
        self.xlator.reset();
        self.agc.reset();
    }

    /// Process complex samples, outputting demodulated audio.
    ///
    /// Translates with BFO, extracts real part, applies AGC.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[Complex], output: &mut [f32]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }

        // Step 1: Frequency translate (BFO mixing)
        self.xlator_buf.resize(input.len(), Complex::default());
        self.xlator.process(input, &mut self.xlator_buf)?;

        // Step 2: Extract real part
        for (i, &s) in self.xlator_buf.iter().enumerate() {
            output[i] = s.re;
        }

        // Step 3: AGC for consistent amplitude (use pre-allocated buffer)
        self.agc_buf.resize(input.len(), 0.0);
        self.agc_buf.copy_from_slice(&output[..input.len()]);
        self.agc
            .process_f32(&self.agc_buf, &mut output[..input.len()])?;

        Ok(input.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    // --- Quadrature tests ---

    #[test]
    fn test_quadrature_new_invalid() {
        assert!(Quadrature::new(0.0).is_err());
        assert!(Quadrature::new(-1.0).is_err());
        assert!(Quadrature::new(1e-40).is_err()); // tiny subnormal: reciprocal overflows
        assert!(Quadrature::new(f32::NAN).is_err());
        assert!(Quadrature::new(f32::INFINITY).is_err());
        // from_hz rejects negative inputs even if they'd produce positive rads
        assert!(Quadrature::from_hz(-1000.0, -48000.0).is_err());
        assert!(Quadrature::from_hz(1000.0, -48000.0).is_err());
    }

    #[test]
    fn test_quadrature_constant_freq() {
        // A complex signal with constant frequency should give constant output
        let freq = 0.1_f32;
        let deviation = 0.5_f32;
        let mut demod = Quadrature::new(deviation).unwrap();
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = freq * i as f32;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![0.0_f32; 1000];
        demod.process(&input, &mut output).unwrap();
        // After first sample, output should be approximately freq / deviation
        let expected = freq / deviation;
        for &v in &output[1..] {
            assert!((v - expected).abs() < 0.05, "expected ~{expected}, got {v}");
        }
    }

    #[test]
    fn test_quadrature_silence() {
        // DC signal (constant phase) should give zero output
        let mut demod = Quadrature::new(1.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![0.0_f32; 100];
        demod.process(&input, &mut output).unwrap();
        for &v in &output[1..] {
            assert!(v.abs() < 1e-5, "DC should give ~0, got {v}");
        }
    }

    // --- AM tests ---

    #[test]
    fn test_am_envelope() {
        let mut demod = AmDemod::new();
        // AM signal: carrier with varying amplitude
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let amp = 1.0 + 0.5 * (2.0 * PI * 0.01 * i as f32).sin();
                Complex::new(amp, 0.0)
            })
            .collect();
        let mut output = vec![0.0_f32; 1000];
        demod.process(&input, &mut output).unwrap();
        // Output should follow the envelope (after DC blocking settles)
        let peak = output[500..]
            .iter()
            .map(|x| x.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "AM should extract envelope, peak = {peak}");
    }

    // --- FM tests ---

    #[test]
    fn test_fm_constant_tone() {
        let mut demod = FmDemod::new(1.0).unwrap();
        let freq = 0.2_f32;
        let input: Vec<Complex> = (0..500)
            .map(|i| {
                let phase = freq * i as f32;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![0.0_f32; 500];
        demod.process(&input, &mut output).unwrap();
        // Should give constant output proportional to frequency
        let expected = freq;
        for &v in &output[1..] {
            assert!((v - expected).abs() < 0.05, "expected ~{expected}, got {v}");
        }
    }

    // --- SSB tests ---

    #[test]
    fn test_ssb_dsb_extracts_real() {
        // DSB mode: no frequency translation, just extract real part
        let mut demod = SsbDemod::new(SsbMode::Dsb, 3000.0, 48_000.0).unwrap();
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [0.0_f32; 2];
        demod.process(&input, &mut output).unwrap();
        // DSB: real part only (no translation)
        assert!((output[0] - 1.0).abs() < 1e-5);
        assert!((output[1] - 3.0).abs() < 1e-5);
    }

    #[test]
    fn test_ssb_usb_lsb_differ() {
        // USB and LSB should produce different output for asymmetric signals
        let mut usb = SsbDemod::new(SsbMode::Usb, 3000.0, 48_000.0).unwrap();
        let mut lsb = SsbDemod::new(SsbMode::Lsb, 3000.0, 48_000.0).unwrap();
        // Generate a tone signal (not DC) so translation matters
        let input: Vec<Complex> = (0..100)
            .map(|i| {
                let phase = 2.0 * PI * 500.0 * (i as f32) / 48_000.0;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut usb_out = vec![0.0_f32; 100];
        let mut lsb_out = vec![0.0_f32; 100];
        usb.process(&input, &mut usb_out).unwrap();
        lsb.process(&input, &mut lsb_out).unwrap();
        // USB and LSB should differ for a non-DC signal
        let diff: f32 = usb_out
            .iter()
            .zip(&lsb_out)
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(
            diff > 1.0,
            "USB and LSB should produce different output, diff={diff}"
        );
    }

    #[test]
    fn test_ssb_produces_audio() {
        // Verify SSB produces non-zero audio output from a tone
        let mut demod = SsbDemod::new(SsbMode::Usb, 3000.0, 48_000.0).unwrap();
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = 2.0 * PI * 1000.0 * (i as f32) / 48_000.0;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![0.0_f32; 1000];
        demod.process(&input, &mut output).unwrap();
        let peak = output[100..]
            .iter()
            .map(|x| x.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.3, "SSB should produce audible output, peak={peak}");
    }

    // --- CW tests ---

    #[test]
    fn test_cw_produces_tone() {
        let mut demod = CwDemod::from_hz(700.0, 48_000.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 1000];
        let mut output = vec![0.0_f32; 1000];
        demod.process(&input, &mut output).unwrap();
        // Should produce an oscillating signal (the BFO tone)
        let crossings = output
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        assert!(
            crossings > 10,
            "CW should produce oscillations, got {crossings} crossings"
        );
    }

    #[test]
    fn test_cw_reset() {
        let mut demod = CwDemod::from_hz(700.0, 48_000.0).unwrap();
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![0.0_f32; 100];
        demod.process(&input, &mut output).unwrap();
        demod.reset();
        // After reset, processing should start fresh
    }

    #[test]
    fn test_buffer_too_small() {
        let mut demod = Quadrature::new(1.0).unwrap();
        let input = [Complex::default(); 10];
        let mut output = [0.0_f32; 5];
        assert!(demod.process(&input, &mut output).is_err());
    }
}
