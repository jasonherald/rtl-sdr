//! CW (Continuous Wave / Morse code) demodulator.

use sdr_dsp::demod::CwDemod;
use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// IF sample rate for CW mode (Hz).
const CW_IF_SAMPLE_RATE: f64 = 3_000.0;

/// AF (audio) sample rate produced by CW demod (Hz).
const CW_AF_SAMPLE_RATE: f64 = 3_000.0;

/// Default channel bandwidth for CW (Hz).
const CW_DEFAULT_BANDWIDTH: f64 = 200.0;

/// Minimum bandwidth for CW (Hz).
const CW_MIN_BANDWIDTH: f64 = 50.0;

/// Maximum bandwidth for CW (Hz) — C++ SDR++ uses 500 Hz.
const CW_MAX_BANDWIDTH: f64 = 500.0;

/// Default frequency snap interval for CW (Hz).
const CW_SNAP_INTERVAL: f64 = 10.0;

/// BFO tone offset for CW (Hz) — C++ SDR++ uses 800 Hz.
const CW_TONE_OFFSET_HZ: f64 = 800.0;

/// CW demodulator using `CwDemod` from sdr-dsp.
///
/// Applies BFO mixing and AGC to produce an audible sidetone.
pub struct CwDemodulator {
    demod: CwDemod,
    config: DemodConfig,
    mono_buf: Vec<f32>,
}

impl CwDemodulator {
    /// Create a new CW demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying CW demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = CwDemod::from_hz(CW_TONE_OFFSET_HZ, CW_IF_SAMPLE_RATE)?;
        let config = DemodConfig {
            if_sample_rate: CW_IF_SAMPLE_RATE,
            af_sample_rate: CW_AF_SAMPLE_RATE,
            default_bandwidth: CW_DEFAULT_BANDWIDTH,
            min_bandwidth: CW_MIN_BANDWIDTH,
            max_bandwidth: CW_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: CW_SNAP_INTERVAL,
            vfo_reference: VfoReference::Center,
            deemp_allowed: false,
            post_proc_enabled: true,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: false,
            nb_allowed: false,
            high_pass_allowed: false,
            squelch_allowed: false,
        };
        Ok(Self {
            demod,
            config,
            mono_buf: Vec::new(),
        })
    }
}

impl Demodulator for CwDemodulator {
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
        // CW bandwidth is handled by the VFO channel filter.
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "CW"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_cw_config() {
        let demod = CwDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 3_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 200.0).abs() < f64::EPSILON);
        assert!(!cfg.squelch_allowed);
    }

    #[test]
    fn test_cw_produces_tone() {
        let mut demod = CwDemodulator::new().unwrap();
        // Carrier signal should produce a BFO sidetone
        let input = vec![Complex::new(1.0, 0.0); 1000];
        let mut output = vec![Stereo::default(); 1000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        // Output should oscillate (BFO tone)
        let crossings = output
            .windows(2)
            .filter(|w| (w[0].l >= 0.0) != (w[1].l >= 0.0))
            .count();
        assert!(
            crossings > 10,
            "CW should produce tone, got {crossings} crossings"
        );
    }
}
