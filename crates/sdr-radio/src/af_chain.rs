//! AF (Audio Frequency) processing chain.
//!
//! Applies optional deemphasis filtering and sample rate conversion
//! to stereo audio samples after demodulation.

use sdr_dsp::filter::DeemphasisFilter;
use sdr_dsp::multirate::RationalResampler;
use sdr_types::{Complex, DspError, Stereo};

/// Default audio output sample rate (Hz).
const DEFAULT_AUDIO_RATE: f64 = 48_000.0;

/// Default high-pass cutoff frequency (Hz) for voice modes.
const HIGH_PASS_CUTOFF_HZ: f64 = 300.0;

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
/// Single-pole IIR high-pass filter for removing low-frequency hum/rumble.
///
/// Uses the Julius O. Smith textbook topology:
/// `y[n] = x[n] - x[n-1] + R * y[n-1]`
/// where `R = 1 - (2π × f_cutoff / sample_rate)`.
/// Has an explicit zero at DC for perfect DC rejection.
struct HighPassFilter {
    r: f32,
    last_in: f32,
    last_out: f32,
}

impl HighPassFilter {
    fn new(cutoff_hz: f64, sample_rate: f64) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        let r = (1.0 - (core::f64::consts::TAU * cutoff_hz / sample_rate)) as f32;
        Self {
            r,
            last_in: 0.0,
            last_out: 0.0,
        }
    }

    #[inline]
    fn process_sample(&mut self, x: f32) -> f32 {
        let y = x - self.last_in + self.r * self.last_out;
        self.last_in = x;
        self.last_out = y;
        y
    }
}

pub struct AfChain {
    deemp_l: Option<DeemphasisFilter>,
    deemp_r: Option<DeemphasisFilter>,
    deemp_enabled: bool,
    hp_l: Option<HighPassFilter>,
    hp_r: Option<HighPassFilter>,
    hp_enabled: bool,
    resampler: Option<RationalResampler>,
    af_sample_rate: f64,
    audio_sample_rate: f64,
    /// Scratch buffer for deemphasis L input channel.
    deemp_buf_l: Vec<f32>,
    /// Scratch buffer for deemphasis R input channel.
    deemp_buf_r: Vec<f32>,
    /// Scratch buffer for deemphasis L output.
    deemp_out_l: Vec<f32>,
    /// Scratch buffer for deemphasis R output.
    deemp_out_r: Vec<f32>,
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
            hp_l: None,
            hp_r: None,
            hp_enabled: false,
            resampler,
            af_sample_rate,
            audio_sample_rate,
            deemp_buf_l: Vec::new(),
            deemp_out_l: Vec::new(),
            deemp_out_r: Vec::new(),
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
            // Deemphasis runs AFTER resampling, so use the audio output rate.
            // C++ SDR++ applies deemphasis at 48 kHz, not the demod AF rate.
            self.deemp_l = Some(DeemphasisFilter::new(tau, self.audio_sample_rate)?);
            self.deemp_r = Some(DeemphasisFilter::new(tau, self.audio_sample_rate)?);
        } else {
            self.deemp_l = None;
            self.deemp_r = None;
        }
        Ok(())
    }

    /// Enable or disable the high-pass filter (voice modes).
    ///
    /// Removes low-frequency hum and rumble below 300 Hz.
    pub fn set_high_pass_enabled(&mut self, enabled: bool) {
        self.hp_enabled = enabled;
        if enabled && self.hp_l.is_none() {
            self.hp_l = Some(HighPassFilter::new(
                HIGH_PASS_CUTOFF_HZ,
                self.audio_sample_rate,
            ));
            self.hp_r = Some(HighPassFilter::new(
                HIGH_PASS_CUTOFF_HZ,
                self.audio_sample_rate,
            ));
        } else if !enabled {
            self.hp_l = None;
            self.hp_r = None;
        }
    }

    /// Returns whether the high-pass filter is enabled.
    pub fn high_pass_enabled(&self) -> bool {
        self.hp_enabled
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

        // Stage 1: Resample from AF rate to audio output rate.
        // C++ SDR++ order: Resample FIRST, then deemphasis at audio rate.
        let (resampled, resamp_count) = if let Some(resampler) = &mut self.resampler {
            self.resamp_in.resize(n, Complex::default());
            for (i, s) in input.iter().enumerate() {
                self.resamp_in[i] = Complex::new(s.l, s.r);
            }

            let ratio = (self.audio_sample_rate / self.af_sample_rate).ceil() as usize;
            let max_out = n * ratio.max(1) + RESAMPLER_OUTPUT_PADDING;
            self.resamp_out.resize(max_out, Complex::default());

            let out_count = resampler.process(&self.resamp_in[..n], &mut self.resamp_out)?;
            (true, out_count)
        } else {
            (false, n)
        };

        if output.len() < resamp_count {
            return Err(DspError::BufferTooSmall {
                need: resamp_count,
                got: output.len(),
            });
        }

        // Write resampled (or passthrough) samples to output.
        if resampled {
            for (out, c) in output
                .iter_mut()
                .zip(self.resamp_out.iter())
                .take(resamp_count)
            {
                *out = Stereo::new(c.re, c.im);
            }
        } else {
            output[..n].copy_from_slice(input);
        }

        // Stage 2: Deemphasis at the audio output rate (48 kHz).
        // Applied AFTER resampling, matching SDR++ signal chain order.
        if self.deemp_enabled
            && let (Some(deemp_l), Some(deemp_r)) = (&mut self.deemp_l, &mut self.deemp_r)
        {
            self.deemp_buf_l.resize(resamp_count, 0.0);
            self.deemp_buf_r.resize(resamp_count, 0.0);

            for (i, s) in output[..resamp_count].iter().enumerate() {
                self.deemp_buf_l[i] = s.l;
                self.deemp_buf_r[i] = s.r;
            }

            self.deemp_out_l.resize(resamp_count, 0.0);
            self.deemp_out_r.resize(resamp_count, 0.0);
            deemp_l.process(
                &self.deemp_buf_l[..resamp_count],
                &mut self.deemp_out_l[..resamp_count],
            )?;
            deemp_r.process(
                &self.deemp_buf_r[..resamp_count],
                &mut self.deemp_out_r[..resamp_count],
            )?;

            for (out, (&l, &r)) in output[..resamp_count].iter_mut().zip(
                self.deemp_out_l[..resamp_count]
                    .iter()
                    .zip(self.deemp_out_r[..resamp_count].iter()),
            ) {
                *out = Stereo::new(l, r);
            }
        }

        // Stage 3: High-pass filter at audio output rate.
        // Removes low-frequency hum and rumble (cutoff ~300 Hz).
        if self.hp_enabled
            && let (Some(hp_l), Some(hp_r)) = (&mut self.hp_l, &mut self.hp_r)
        {
            for s in &mut output[..resamp_count] {
                s.l = hp_l.process_sample(s.l);
                s.r = hp_r.process_sample(s.r);
            }
        }

        Ok(resamp_count)
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
