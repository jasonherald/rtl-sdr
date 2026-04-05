//! Narrowband FM demodulator.

use sdr_dsp::demod::FmDemod;
use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// IF sample rate for NFM mode (Hz).
const NFM_IF_SAMPLE_RATE: f64 = 50_000.0;

/// AF (audio) sample rate produced by NFM demod (Hz).
const NFM_AF_SAMPLE_RATE: f64 = 50_000.0;

/// Default channel bandwidth for NFM (Hz).
const NFM_DEFAULT_BANDWIDTH: f64 = 12_500.0;

/// Minimum bandwidth for NFM (Hz).
const NFM_MIN_BANDWIDTH: f64 = 1_000.0;

/// Maximum bandwidth for NFM (Hz).
const NFM_MAX_BANDWIDTH: f64 = 25_000.0;

/// Default frequency snap interval for NFM (Hz).
const NFM_SNAP_INTERVAL: f64 = 12_500.0;

/// FM deviation for narrowband FM, computed as half the default bandwidth (Hz).
const NFM_DEVIATION_HZ: f64 = 6_250.0;

/// Narrowband FM demodulator using `FmDemod` from sdr-dsp.
///
/// Produces mono audio converted to stereo.
pub struct NfmDemodulator {
    demod: FmDemod,
    config: DemodConfig,
    mono_buf: Vec<f32>,
}

impl NfmDemodulator {
    /// Create a new NFM demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying FM demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = FmDemod::from_hz(NFM_DEVIATION_HZ, NFM_IF_SAMPLE_RATE)?;
        let config = DemodConfig {
            if_sample_rate: NFM_IF_SAMPLE_RATE,
            af_sample_rate: NFM_AF_SAMPLE_RATE,
            default_bandwidth: NFM_DEFAULT_BANDWIDTH,
            min_bandwidth: NFM_MIN_BANDWIDTH,
            max_bandwidth: NFM_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: NFM_SNAP_INTERVAL,
            vfo_reference: VfoReference::Center,
            deemp_allowed: false,
            post_proc_enabled: true,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: true,
            nb_allowed: true,
            high_pass_allowed: true,
            squelch_allowed: true,
        };
        Ok(Self {
            demod,
            config,
            mono_buf: Vec::new(),
        })
    }
}

impl Demodulator for NfmDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        self.mono_buf.resize(input.len(), 0.0);
        let count = self.demod.process(input, &mut self.mono_buf)?;
        sdr_dsp::convert::mono_to_stereo(&self.mono_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, _bw: f64) {
        // Bandwidth is handled by the VFO channel filter, not the discriminator.
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "NFM"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn test_nfm_config() {
        let demod = NfmDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 50_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 12_500.0).abs() < f64::EPSILON);
        assert!(cfg.fm_if_nr_allowed);
        assert!(cfg.squelch_allowed);
    }

    #[test]
    fn test_nfm_process_fm_signal() {
        let mut demod = NfmDemodulator::new().unwrap();
        // Generate FM-modulated signal: constant frequency offset = constant audio tone
        let freq = 0.1_f32;
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = freq * i as f32;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); 1000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        // After first sample, stereo output should have consistent values (L == R)
        for s in &output[1..] {
            assert!(
                (s.l - s.r).abs() < 1e-6,
                "mono-to-stereo: L and R should match"
            );
        }
    }

    #[test]
    fn test_nfm_process_produces_audio() {
        let mut demod = NfmDemodulator::new().unwrap();
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = 2.0 * PI * 1000.0 * (i as f32) / 50_000.0;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); 1000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        // Output should have non-zero audio
        let peak = output[1..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.01, "NFM should produce audio, peak = {peak}");
    }
}
