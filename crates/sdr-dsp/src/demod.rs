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
    first_sample: bool,
}

impl Quadrature {
    /// Create a new quadrature demodulator.
    ///
    /// - `deviation`: maximum frequency deviation in radians/sample
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if `deviation` is zero or non-finite.
    pub fn new(deviation: f32) -> Result<Self, DspError> {
        if !deviation.is_finite() || deviation <= 0.0 {
            return Err(DspError::InvalidParameter(format!(
                "deviation must be positive and finite, got {deviation}"
            )));
        }
        Ok(Self {
            inv_deviation: 1.0 / deviation,
            last_phase: 0.0,
            first_sample: true,
        })
    }

    /// Create from deviation in Hz and sample rate.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if parameters are invalid.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_hz(deviation_hz: f64, sample_rate: f64) -> Result<Self, DspError> {
        let dev = math::hz_to_rads(deviation_hz, sample_rate) as f32;
        Self::new(dev)
    }

    /// Reset the demodulator state.
    pub fn reset(&mut self) {
        self.last_phase = 0.0;
        self.first_sample = true;
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
            if self.first_sample {
                // Seed phase from first sample to avoid startup click
                output[i] = 0.0;
                self.first_sample = false;
            } else {
                output[i] =
                    math::normalize_phase(current_phase - self.last_phase) * self.inv_deviation;
            }
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
            // Inline DC blocking
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

    /// Process complex samples, outputting mono FM audio.
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
/// Ports SDR++ `dsp::demod::SSB`. Extracts audio from a complex baseband
/// SSB signal using Hilbert-style demodulation:
/// - USB: `re + im` (selects upper sideband)
/// - LSB: `re - im` (selects lower sideband via spectrum flip)
/// - DSB: `re` (both sidebands, no sideband selection)
pub struct SsbDemod {
    mode: SsbMode,
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
    pub fn new(mode: SsbMode) -> Self {
        Self { mode }
    }

    /// Process complex samples, outputting demodulated audio.
    ///
    /// - USB: `output = re + im` (upper sideband via Hilbert demod)
    /// - LSB: `output = re - im` (lower sideband via spectrum flip)
    /// - DSB: `output = re` (both sidebands)
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
        match self.mode {
            SsbMode::Usb => {
                // USB: sum of re and im (Hilbert demod, upper sideband)
                for (i, &s) in input.iter().enumerate() {
                    output[i] = s.re + s.im;
                }
            }
            SsbMode::Lsb => {
                // LSB: difference of re and im (spectrum flip)
                for (i, &s) in input.iter().enumerate() {
                    output[i] = s.re - s.im;
                }
            }
            SsbMode::Dsb => {
                // DSB: take real part (both sidebands)
                for (i, &s) in input.iter().enumerate() {
                    output[i] = s.re;
                }
            }
        }
        Ok(input.len())
    }
}

/// CW (Continuous Wave / Morse) demodulator.
///
/// Ports SDR++ `dsp::demod::CW`. Mixes the input with a BFO (beat frequency
/// oscillator) at a configurable offset, then extracts the real part.
pub struct CwDemod {
    bfo_phase: f32,
    bfo_phase_inc: f32,
}

impl CwDemod {
    /// Create a new CW demodulator.
    ///
    /// - `tone_offset`: BFO offset in radians/sample (typically ~700-1000 Hz worth)
    pub fn new(tone_offset: f32) -> Self {
        Self {
            bfo_phase: 0.0,
            bfo_phase_inc: tone_offset,
        }
    }

    /// Create from tone offset in Hz and sample rate.
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_hz(tone_offset_hz: f64, sample_rate: f64) -> Self {
        Self::new(math::hz_to_rads(tone_offset_hz, sample_rate) as f32)
    }

    /// Reset the BFO phase.
    pub fn reset(&mut self) {
        self.bfo_phase = 0.0;
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
            // Mix with BFO and extract real part directly:
            // Re(s * bfo) = s.re * cos - s.im * sin
            let (sin, cos) = self.bfo_phase.sin_cos();
            output[i] = s.re * cos - s.im * sin;
            // Advance BFO phase
            self.bfo_phase += self.bfo_phase_inc;
            self.bfo_phase = math::normalize_phase(self.bfo_phase);
        }
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
        assert!(Quadrature::new(f32::NAN).is_err());
        assert!(Quadrature::new(f32::INFINITY).is_err());
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
    fn test_ssb_usb() {
        let mut demod = SsbDemod::new(SsbMode::Usb);
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [0.0_f32; 2];
        demod.process(&input, &mut output).unwrap();
        // USB: re + im
        assert!((output[0] - 3.0).abs() < 1e-6); // 1+2
        assert!((output[1] - 7.0).abs() < 1e-6); // 3+4
    }

    #[test]
    fn test_ssb_lsb_differs_from_usb() {
        let mut usb = SsbDemod::new(SsbMode::Usb);
        let mut lsb = SsbDemod::new(SsbMode::Lsb);
        let input = [Complex::new(1.0, 2.0)];
        let mut usb_out = [0.0_f32; 1];
        let mut lsb_out = [0.0_f32; 1];
        usb.process(&input, &mut usb_out).unwrap();
        lsb.process(&input, &mut lsb_out).unwrap();
        // USB: 1+2=3, LSB: 1-2=-1 — they must differ
        assert!((usb_out[0] - 3.0).abs() < 1e-6);
        assert!((lsb_out[0] - (-1.0)).abs() < 1e-6);
        assert!(
            (usb_out[0] - lsb_out[0]).abs() > 1.0,
            "USB and LSB must differ"
        );
    }

    #[test]
    fn test_ssb_dsb() {
        let mut demod = SsbDemod::new(SsbMode::Dsb);
        let input = [Complex::new(1.0, 2.0)];
        let mut output = [0.0_f32; 1];
        demod.process(&input, &mut output).unwrap();
        // DSB: re only
        assert!((output[0] - 1.0).abs() < 1e-6);
    }

    // --- CW tests ---

    #[test]
    fn test_cw_produces_tone() {
        let mut demod = CwDemod::from_hz(700.0, 48_000.0);
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
        let mut demod = CwDemod::from_hz(700.0, 48_000.0);
        let input = vec![Complex::new(1.0, 0.0); 100];
        let mut output = vec![0.0_f32; 100];
        demod.process(&input, &mut output).unwrap();
        demod.reset();
        assert!((demod.bfo_phase).abs() < 1e-6);
    }

    #[test]
    fn test_buffer_too_small() {
        let mut demod = Quadrature::new(1.0).unwrap();
        let input = [Complex::default(); 10];
        let mut output = [0.0_f32; 5];
        assert!(demod.process(&input, &mut output).is_err());
    }
}
