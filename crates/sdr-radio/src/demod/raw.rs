//! Raw IQ passthrough demodulator.

use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// IF sample rate for Raw mode (Hz).
/// Matches the default audio output rate since there's no decimation.
const RAW_IF_SAMPLE_RATE: f64 = 48_000.0;

/// AF (audio) sample rate produced by Raw demod (Hz).
const RAW_AF_SAMPLE_RATE: f64 = 48_000.0;

/// Default channel bandwidth for Raw (Hz) — full IF passband.
const RAW_DEFAULT_BANDWIDTH: f64 = 48_000.0;

/// Raw IQ passthrough demodulator.
///
/// Converts complex IQ directly to stereo: re -> L, im -> R.
/// Bandwidth is locked since there's no demodulation.
pub struct RawDemodulator {
    config: DemodConfig,
}

impl RawDemodulator {
    /// Create a new Raw passthrough demodulator.
    pub fn new() -> Self {
        let config = DemodConfig {
            if_sample_rate: RAW_IF_SAMPLE_RATE,
            af_sample_rate: RAW_AF_SAMPLE_RATE,
            default_bandwidth: RAW_DEFAULT_BANDWIDTH,
            min_bandwidth: RAW_DEFAULT_BANDWIDTH,
            max_bandwidth: RAW_DEFAULT_BANDWIDTH,
            bandwidth_locked: true,
            default_snap_interval: 0.0,
            vfo_reference: VfoReference::Center,
            deemp_allowed: false,
            post_proc_enabled: false,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: false,
            nb_allowed: false,
            high_pass_allowed: false,
            squelch_allowed: false,
        };
        Self { config }
    }
}

impl Default for RawDemodulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Demodulator for RawDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        sdr_dsp::convert::complex_to_stereo(input, output)
    }

    fn set_bandwidth(&mut self, _bw: f64) {
        // Bandwidth is locked in Raw mode.
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "RAW"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_config() {
        let demod = RawDemodulator::new();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 48_000.0).abs() < f64::EPSILON);
        assert!(cfg.bandwidth_locked);
        assert!(!cfg.post_proc_enabled);
    }

    #[test]
    fn test_raw_passes_iq_to_stereo() {
        let mut demod = RawDemodulator::new();
        let input = [
            Complex::new(1.0, 2.0),
            Complex::new(3.0, 4.0),
            Complex::new(-0.5, 0.7),
        ];
        let mut output = [Stereo::default(); 3];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        assert_eq!(output[0].l, 1.0);
        assert_eq!(output[0].r, 2.0);
        assert_eq!(output[1].l, 3.0);
        assert_eq!(output[1].r, 4.0);
        assert_eq!(output[2].l, -0.5);
        assert!((output[2].r - 0.7).abs() < 1e-6);
    }
}
