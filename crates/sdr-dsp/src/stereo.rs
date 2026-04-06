//! FM stereo multiplex decode.
//!
//! Ports the stereo decode pipeline from C++ SDR++ `broadcast_fm.h`:
//! 1. Bandpass 19 kHz pilot tone from composite baseband
//! 2. PLL locks onto pilot, output phase is doubled to 38 kHz subcarrier
//! 3. Multiply composite by 38 kHz carrier → extracts L−R difference signal
//! 4. Lowpass L−R at 15 kHz
//! 5. Lowpass L+R (mono) at 15 kHz
//! 6. Stereo matrix: L = (L+R + L−R)/2, R = (L+R − L−R)/2

use sdr_types::{DspError, Stereo};

use crate::filter::FirFilter;
use crate::loops::Pll;
use crate::math;
use crate::taps;

/// Pilot tone frequency (Hz).
const PILOT_FREQ_HZ: f64 = 19_000.0;

/// Pilot bandpass filter half-width (Hz) — ±500 Hz around 19 kHz.
const PILOT_BPF_HALF_WIDTH: f64 = 500.0;

/// Pilot bandpass transition width (Hz).
const PILOT_BPF_TRANSITION: f64 = 500.0;

/// Audio lowpass cutoff for L+R and L−R (Hz).
const AUDIO_LPF_CUTOFF: f64 = 15_000.0;

/// Audio lowpass transition width (Hz).
const AUDIO_LPF_TRANSITION: f64 = 4_000.0;

/// PLL bandwidth for pilot tracking.
const PILOT_PLL_BANDWIDTH: f32 = 0.01;

/// FM stereo decoder — extracts L/R channels from FM composite baseband.
///
/// Input: mono FM discriminator output (composite baseband at IF sample rate).
/// Output: stereo audio samples.
pub struct FmStereoDecoder {
    /// Bandpass filter to extract 19 kHz pilot tone.
    pilot_bpf: FirFilter,
    /// PLL locked to 19 kHz pilot.
    pilot_pll: Pll,
    /// Lowpass filter for L+R (mono) signal.
    mono_lpf: FirFilter,
    /// Lowpass filter for L−R (difference) signal.
    diff_lpf: FirFilter,
    /// Delay buffer to compensate pilot BPF group delay on composite signal.
    composite_delay: Vec<f32>,
    /// Group delay of pilot BPF in samples.
    bpf_delay: usize,
    /// Scratch buffers.
    pilot_buf: Vec<f32>,
    mono_buf: Vec<f32>,
    diff_buf: Vec<f32>,
    diff_lpf_buf: Vec<f32>,
}

impl FmStereoDecoder {
    /// Create a new FM stereo decoder.
    ///
    /// - `sample_rate`: composite baseband sample rate in Hz (typically 250 kHz)
    ///
    /// # Errors
    ///
    /// Returns `DspError` if filter or PLL construction fails.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(sample_rate: f64) -> Result<Self, DspError> {
        // 19 kHz pilot bandpass
        let pilot_taps = taps::band_pass(
            PILOT_FREQ_HZ - PILOT_BPF_HALF_WIDTH,
            PILOT_FREQ_HZ + PILOT_BPF_HALF_WIDTH,
            PILOT_BPF_TRANSITION,
            sample_rate,
            true,
        )?;
        // BPF group delay = (tap_count - 1) / 2 for linear-phase FIR
        let bpf_delay = (pilot_taps.len().saturating_sub(1)) / 2;
        let pilot_bpf = FirFilter::new(pilot_taps)?;

        // PLL centered at 19 kHz
        let pilot_omega = math::hz_to_rads(PILOT_FREQ_HZ, sample_rate) as f32;
        let pilot_pll = Pll::new(
            PILOT_PLL_BANDWIDTH,
            0.0,
            pilot_omega,
            pilot_omega * 0.9,
            pilot_omega * 1.1,
        )?;

        // 15 kHz lowpass for L+R and L−R
        let lpf_taps = taps::low_pass(AUDIO_LPF_CUTOFF, AUDIO_LPF_TRANSITION, sample_rate, false)?;
        let mono_lpf = FirFilter::new(lpf_taps.clone())?;
        let diff_lpf = FirFilter::new(lpf_taps)?;

        Ok(Self {
            pilot_bpf,
            pilot_pll,
            mono_lpf,
            diff_lpf,
            composite_delay: vec![0.0; bpf_delay],
            bpf_delay,
            pilot_buf: Vec::new(),
            mono_buf: Vec::new(),
            diff_buf: Vec::new(),
            diff_lpf_buf: Vec::new(),
        })
    }

    /// Reset all internal state.
    pub fn reset(&mut self) {
        self.pilot_bpf.reset();
        self.pilot_pll.reset();
        self.mono_lpf.reset();
        self.diff_lpf.reset();
        self.composite_delay.fill(0.0);
    }

    /// Decode stereo from FM composite baseband.
    ///
    /// - `input`: FM discriminator output (composite baseband, mono f32)
    /// - `output`: stereo audio samples
    ///
    /// Returns the number of stereo samples written (always `input.len()`).
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
    pub fn process(&mut self, input: &[f32], output: &mut [Stereo]) -> Result<usize, DspError> {
        let len = input.len();
        if output.len() < len {
            return Err(DspError::BufferTooSmall {
                need: len,
                got: output.len(),
            });
        }
        if len == 0 {
            return Ok(0);
        }

        // Step 1: Extract 19 kHz pilot tone via bandpass filter.
        self.pilot_buf.resize(len, 0.0);
        self.pilot_bpf.process_f32(input, &mut self.pilot_buf)?;

        // Steps 2-3: PLL locks onto pilot, doubles phase to 38 kHz subcarrier,
        // multiplies delay-compensated composite to extract L−R.
        // The pilot BPF introduces a group delay; we delay the composite signal
        // by the same amount so the subcarrier phase aligns with the composite.
        self.diff_buf.resize(len, 0.0);

        for (i, pilot) in self.pilot_buf.iter().enumerate() {
            // Feed pilot to PLL as complex signal (real=pilot, im=0)
            let pilot_complex = [sdr_types::Complex::new(*pilot, 0.0)];
            let mut pll_out = [sdr_types::Complex::default()];
            self.pilot_pll.process(&pilot_complex, &mut pll_out)?;

            // Double the PLL phase: 19 kHz → 38 kHz subcarrier.
            // cos(2θ) = cos²(θ) − sin²(θ), using the PLL phasor components.
            let cos_t = pll_out[0].re;
            let sin_t = pll_out[0].im;
            let subcarrier = cos_t * cos_t - sin_t * sin_t;

            // Use delay-compensated composite for phase-aligned L−R extraction.
            let delayed = if self.bpf_delay > 0 {
                // Read oldest sample from delay buffer, push current sample in
                let old = self.composite_delay[i % self.bpf_delay];
                self.composite_delay[i % self.bpf_delay] = input[i];
                old
            } else {
                input[i]
            };

            // Multiply delay-matched composite by subcarrier → extracts L−R.
            // Scale by 2.0 to compensate for DSB-SC encoding.
            self.diff_buf[i] = delayed * subcarrier * 2.0;
        }

        // Step 4: Lowpass L+R (mono) at 15 kHz
        self.mono_buf.resize(len, 0.0);
        self.mono_lpf.process_f32(input, &mut self.mono_buf)?;

        // Step 5: Lowpass L−R (difference) at 15 kHz
        self.diff_lpf_buf.resize(len, 0.0);
        self.diff_lpf
            .process_f32(&self.diff_buf, &mut self.diff_lpf_buf)?;

        // Step 6: Stereo matrix
        // L = (L+R + L−R) / 2
        // R = (L+R − L−R) / 2
        for (out, (mono, diff)) in output[..len]
            .iter_mut()
            .zip(self.mono_buf.iter().zip(self.diff_lpf_buf.iter()))
        {
            *out = Stereo {
                l: (mono + diff) * 0.5,
                r: (mono - diff) * 0.5,
            };
        }

        Ok(len)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    const TEST_SAMPLE_RATE: f64 = 250_000.0;

    #[test]
    fn test_stereo_decoder_creation() {
        let decoder = FmStereoDecoder::new(TEST_SAMPLE_RATE);
        assert!(decoder.is_ok());
    }

    #[test]
    fn test_stereo_decoder_mono_passthrough() {
        // Pure mono signal (no pilot, no subcarrier) should produce
        // roughly equal L and R channels
        let mut decoder = FmStereoDecoder::new(TEST_SAMPLE_RATE).unwrap();
        let len = 5000;
        // Generate a 1 kHz tone as mono signal
        let input: Vec<f32> = (0..len)
            .map(|i| (2.0 * PI * 1000.0 * (i as f32) / TEST_SAMPLE_RATE as f32).sin())
            .collect();
        let mut output = vec![Stereo::default(); len];
        let count = decoder.process(&input, &mut output).unwrap();
        assert_eq!(count, len);

        // After filters settle, L and R should be close (mono → equal channels)
        let settle = 1000;
        for s in &output[settle..] {
            assert!(
                (s.l - s.r).abs() < 0.5,
                "mono signal: L and R should be similar, L={}, R={}",
                s.l,
                s.r
            );
        }
    }

    #[test]
    fn test_stereo_decoder_processes_without_crash() {
        // Smoke test with composite-like signal
        let mut decoder = FmStereoDecoder::new(TEST_SAMPLE_RATE).unwrap();
        let len = 10000;
        let input: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / TEST_SAMPLE_RATE as f32;
                // Mono signal + 19kHz pilot + 38kHz subcarrier
                let mono = (2.0 * PI * 1000.0 * t).sin();
                let pilot = 0.1 * (2.0 * PI * 19_000.0 * t).sin();
                let diff = 0.5 * (2.0 * PI * 2000.0 * t).sin();
                let subcarrier = diff * (2.0 * PI * 38_000.0 * t).sin();
                mono + pilot + subcarrier
            })
            .collect();
        let mut output = vec![Stereo::default(); len];
        let count = decoder.process(&input, &mut output).unwrap();
        assert_eq!(count, len);

        // Verify output has non-zero content in both channels
        let l_energy: f32 = output[2000..].iter().map(|s| s.l * s.l).sum();
        let r_energy: f32 = output[2000..].iter().map(|s| s.r * s.r).sum();
        assert!(l_energy > 0.01, "L channel should have energy");
        assert!(r_energy > 0.01, "R channel should have energy");
    }

    #[test]
    fn test_stereo_decoder_buffer_too_small() {
        let mut decoder = FmStereoDecoder::new(TEST_SAMPLE_RATE).unwrap();
        let input = vec![0.0_f32; 100];
        let mut output = vec![Stereo::default(); 50];
        assert!(decoder.process(&input, &mut output).is_err());
    }

    #[test]
    fn test_stereo_decoder_empty_input() {
        let mut decoder = FmStereoDecoder::new(TEST_SAMPLE_RATE).unwrap();
        let input: &[f32] = &[];
        let mut output: Vec<Stereo> = vec![];
        let count = decoder.process(input, &mut output).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_stereo_decoder_reset() {
        let mut decoder = FmStereoDecoder::new(TEST_SAMPLE_RATE).unwrap();
        let input = vec![0.5_f32; 1000];
        let mut output = vec![Stereo::default(); 1000];
        decoder.process(&input, &mut output).unwrap();
        decoder.reset();
        // After reset, should process cleanly
        let count = decoder.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
    }
}
