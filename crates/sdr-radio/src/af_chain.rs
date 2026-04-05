//! AF (Audio Frequency) processing chain.
//!
//! Applies optional deemphasis filtering and sample rate conversion
//! to stereo audio samples after demodulation.

use sdr_dsp::filter::DeemphasisFilter;
use sdr_dsp::multirate::RationalResampler;
use sdr_types::{Complex, DspError, Stereo};

/// Default audio output sample rate (Hz).
const DEFAULT_AUDIO_RATE: f64 = 48_000.0;

/// Guard padding for resampler output buffer to handle worst-case rounding.
const RESAMPLER_OUTPUT_PADDING: usize = 16;

/// Tolerance in Hz for considering two sample rates equal (skip resampling).
const RATE_EQUALITY_TOLERANCE: f64 = 1.0;

/// AF processing chain — applied to stereo audio after demodulation.
///
/// Contains optional processors:
/// 1. Deemphasis filter — single-pole IIR lowpass for FM deemphasis (L and R)
/// 2. Rational resampler — converts from demod AF rate to audio output rate
///
/// The resampler operates on Complex samples (Stereo -> Complex -> resample -> Stereo)
/// since `RationalResampler` is defined for Complex data.
pub struct AfChain {
    deemp_l: Option<DeemphasisFilter>,
    deemp_r: Option<DeemphasisFilter>,
    deemp_enabled: bool,
    resampler: Option<RationalResampler>,
    af_sample_rate: f64,
    audio_sample_rate: f64,
    /// Scratch buffer for deemphasis L channel.
    deemp_buf_l: Vec<f32>,
    /// Scratch buffer for deemphasis R channel.
    deemp_buf_r: Vec<f32>,
    /// Scratch buffer for complex resampler input.
    resamp_in: Vec<Complex>,
    /// Scratch buffer for complex resampler output.
    resamp_out: Vec<Complex>,
}

impl AfChain {
    /// Create a new AF chain.
    ///
    /// - `af_sample_rate`: sample rate from the demodulator (Hz)
    /// - `audio_sample_rate`: target audio output rate (Hz), typically 48 kHz
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the resampler cannot be created.
    pub fn new(af_sample_rate: f64, audio_sample_rate: f64) -> Result<Self, DspError> {
        let needs_resample = (af_sample_rate - audio_sample_rate).abs() >= RATE_EQUALITY_TOLERANCE;
        let resampler = if needs_resample {
            Some(RationalResampler::new(af_sample_rate, audio_sample_rate)?)
        } else {
            None
        };

        Ok(Self {
            deemp_l: None,
            deemp_r: None,
            deemp_enabled: false,
            resampler,
            af_sample_rate,
            audio_sample_rate,
            deemp_buf_l: Vec::new(),
            deemp_buf_r: Vec::new(),
            resamp_in: Vec::new(),
            resamp_out: Vec::new(),
        })
    }

    /// Create a new AF chain with the default audio output rate (48 kHz).
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the resampler cannot be created.
    pub fn with_default_rate(af_sample_rate: f64) -> Result<Self, DspError> {
        Self::new(af_sample_rate, DEFAULT_AUDIO_RATE)
    }

    /// Enable deemphasis filtering with the given time constant.
    ///
    /// - `tau`: time constant in seconds (e.g., 75e-6 for US, 50e-6 for EU)
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the filter cannot be created.
    pub fn set_deemp_enabled(&mut self, enabled: bool, tau: f64) -> Result<(), DspError> {
        self.deemp_enabled = enabled;
        if enabled && tau > 0.0 {
            self.deemp_l = Some(DeemphasisFilter::new(tau, self.af_sample_rate)?);
            self.deemp_r = Some(DeemphasisFilter::new(tau, self.af_sample_rate)?);
        } else {
            self.deemp_l = None;
            self.deemp_r = None;
        }
        Ok(())
    }

    /// Returns whether deemphasis is enabled.
    pub fn deemp_enabled(&self) -> bool {
        self.deemp_enabled
    }

    /// Returns the audio output sample rate.
    pub fn audio_sample_rate(&self) -> f64 {
        self.audio_sample_rate
    }

    /// Returns the demod AF sample rate (input rate).
    pub fn af_sample_rate(&self) -> f64 {
        self.af_sample_rate
    }

    /// Process stereo audio through the AF chain.
    ///
    /// Returns the number of output samples written. This may differ from
    /// `input.len()` when resampling is active.
    ///
    /// # Errors
    ///
    /// Returns `DspError` on buffer size or processing errors.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn process(&mut self, input: &[Stereo], output: &mut [Stereo]) -> Result<usize, DspError> {
        if input.is_empty() {
            return Ok(0);
        }

        let n = input.len();

        // Stage 1: Deemphasis (operates on L and R channels separately)
        let deemph_applied = if self.deemp_enabled {
            if let (Some(deemp_l), Some(deemp_r)) = (&mut self.deemp_l, &mut self.deemp_r) {
                self.deemp_buf_l.resize(n, 0.0);
                self.deemp_buf_r.resize(n, 0.0);

                // Extract L and R channels
                for (i, s) in input.iter().enumerate() {
                    self.deemp_buf_l[i] = s.l;
                    self.deemp_buf_r[i] = s.r;
                }

                // Apply deemphasis to each channel in-place
                let mut out_l = vec![0.0_f32; n];
                let mut out_r = vec![0.0_f32; n];
                deemp_l.process(&self.deemp_buf_l[..n], &mut out_l)?;
                deemp_r.process(&self.deemp_buf_r[..n], &mut out_r)?;

                // Reassemble stereo
                self.deemp_buf_l[..n].copy_from_slice(&out_l);
                self.deemp_buf_r[..n].copy_from_slice(&out_r);
                true
            } else {
                false
            }
        } else {
            false
        };

        // Stage 2: Resampling
        if let Some(resampler) = &mut self.resampler {
            // Convert stereo to complex for resampling
            self.resamp_in.resize(n, Complex::default());
            if deemph_applied {
                for i in 0..n {
                    self.resamp_in[i] = Complex::new(self.deemp_buf_l[i], self.deemp_buf_r[i]);
                }
            } else {
                for (i, s) in input.iter().enumerate() {
                    self.resamp_in[i] = Complex::new(s.l, s.r);
                }
            }

            // Allocate output buffer with headroom
            let ratio = (self.audio_sample_rate / self.af_sample_rate).ceil() as usize;
            let max_out = n * ratio.max(1) + RESAMPLER_OUTPUT_PADDING;
            self.resamp_out.resize(max_out, Complex::default());

            let out_count = resampler.process(&self.resamp_in[..n], &mut self.resamp_out)?;

            if output.len() < out_count {
                return Err(DspError::BufferTooSmall {
                    need: out_count,
                    got: output.len(),
                });
            }

            // Convert complex back to stereo
            for (out, c) in output
                .iter_mut()
                .zip(self.resamp_out.iter())
                .take(out_count)
            {
                *out = Stereo::new(c.re, c.im);
            }
            Ok(out_count)
        } else {
            // No resampling needed
            if output.len() < n {
                return Err(DspError::BufferTooSmall {
                    need: n,
                    got: output.len(),
                });
            }
            if deemph_applied {
                for (out, (&l, &r)) in output
                    .iter_mut()
                    .zip(self.deemp_buf_l.iter().zip(self.deemp_buf_r.iter()))
                    .take(n)
                {
                    *out = Stereo::new(l, r);
                }
            } else {
                output[..n].copy_from_slice(input);
            }
            Ok(n)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    clippy::manual_range_contains
)]
mod tests {
    use super::*;
    use sdr_dsp::filter::DEEMPHASIS_TAU_US;

    #[test]
    fn test_af_chain_passthrough_same_rate() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        let input = vec![Stereo::new(0.5, -0.5); 100];
        let mut output = vec![Stereo::default(); 100];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        assert_eq!(output[0].l, 0.5);
        assert_eq!(output[0].r, -0.5);
    }

    #[test]
    fn test_af_chain_resample_downsample() {
        // 250kHz (WFM AF rate) -> 48kHz
        let mut chain = AfChain::new(250_000.0, 48_000.0).unwrap();
        let input = vec![Stereo::new(1.0, -1.0); 2500];
        let mut output = vec![Stereo::default(); 2500];
        let count = chain.process(&input, &mut output).unwrap();
        // Should produce roughly 2500 * 48000/250000 = 480 samples
        assert!(
            count >= 350 && count <= 600,
            "expected ~480 samples, got {count}"
        );
    }

    #[test]
    fn test_af_chain_resample_upsample() {
        // 3kHz (CW AF rate) -> 48kHz
        let mut chain = AfChain::new(3_000.0, 48_000.0).unwrap();
        let input = vec![Stereo::new(0.5, 0.5); 300];
        let mut output = vec![Stereo::default(); 6000];
        let count = chain.process(&input, &mut output).unwrap();
        // Should produce roughly 300 * 48000/3000 = 4800 samples
        assert!(
            count >= 4000 && count <= 5600,
            "expected ~4800 samples, got {count}"
        );
    }

    #[test]
    fn test_af_chain_deemphasis_attenuates_high_freq() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_deemp_enabled(true, DEEMPHASIS_TAU_US).unwrap();
        assert!(chain.deemp_enabled());

        // High frequency alternating signal
        let input: Vec<Stereo> = (0..1000)
            .map(|i| {
                let v = if i % 2 == 0 { 1.0 } else { -1.0 };
                Stereo::new(v, v)
            })
            .collect();
        let mut output = vec![Stereo::default(); 1000];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);

        // Peak output should be attenuated compared to input
        let peak = output[500..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(
            peak < 0.5,
            "deemphasis should attenuate high freq, peak = {peak}"
        );
    }

    #[test]
    fn test_af_chain_empty_input() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        let mut output = vec![Stereo::default(); 10];
        let count = chain.process(&[], &mut output).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_af_chain_deemphasis_disabled_passthrough() {
        let mut chain = AfChain::new(48_000.0, 48_000.0).unwrap();
        chain.set_deemp_enabled(false, 0.0).unwrap();
        assert!(!chain.deemp_enabled());

        let input = vec![Stereo::new(0.5, -0.3); 100];
        let mut output = vec![Stereo::default(); 100];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 100);
        assert_eq!(output[0].l, 0.5);
        assert_eq!(output[0].r, -0.3);
    }

    #[test]
    fn test_af_chain_with_default_rate() {
        let chain = AfChain::with_default_rate(24_000.0).unwrap();
        assert!((chain.audio_sample_rate() - 48_000.0).abs() < 1.0);
        assert!((chain.af_sample_rate() - 24_000.0).abs() < 1.0);
    }
}
