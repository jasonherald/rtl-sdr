//! Double sideband (DSB) demodulator.

use sdr_dsp::demod::{SsbDemod, SsbMode};
use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// IF sample rate for DSB mode (Hz).
const DSB_IF_SAMPLE_RATE: f64 = 24_000.0;

/// AF (audio) sample rate produced by DSB demod (Hz).
const DSB_AF_SAMPLE_RATE: f64 = 24_000.0;

/// Default channel bandwidth for DSB (Hz).
const DSB_DEFAULT_BANDWIDTH: f64 = 4_600.0;

/// Minimum bandwidth for DSB (Hz).
const DSB_MIN_BANDWIDTH: f64 = 1_000.0;

/// Maximum bandwidth for DSB (Hz).
const DSB_MAX_BANDWIDTH: f64 = 12_000.0;

/// Default frequency snap interval for DSB (Hz).
const DSB_SNAP_INTERVAL: f64 = 100.0;

/// Double sideband demodulator using `SsbDemod(Dsb)` from sdr-dsp.
pub struct DsbDemodulator {
    demod: SsbDemod,
    config: DemodConfig,
    mono_buf: Vec<f32>,
}

impl DsbDemodulator {
    /// Create a new DSB demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying SSB demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = SsbDemod::new(SsbMode::Dsb, DSB_DEFAULT_BANDWIDTH, DSB_IF_SAMPLE_RATE)?;
        let config = DemodConfig {
            if_sample_rate: DSB_IF_SAMPLE_RATE,
            af_sample_rate: DSB_AF_SAMPLE_RATE,
            default_bandwidth: DSB_DEFAULT_BANDWIDTH,
            min_bandwidth: DSB_MIN_BANDWIDTH,
            max_bandwidth: DSB_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: DSB_SNAP_INTERVAL,
            vfo_reference: VfoReference::Center,
            deemp_allowed: false,
            post_proc_enabled: true,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: false,
            nb_allowed: true,
            high_pass_allowed: true,
            squelch_allowed: false,
        };
        Ok(Self {
            demod,
            config,
            mono_buf: Vec::new(),
        })
    }
}

impl Demodulator for DsbDemodulator {
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

    fn set_bandwidth(&mut self, bw: f64) {
        if let Err(e) = self.demod.set_bandwidth(bw) {
            tracing::warn!("DSB: set_bandwidth({bw}) failed: {e}");
        }
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "DSB"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_dsb_config() {
        let demod = DsbDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 24_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 4_600.0).abs() < f64::EPSILON);
        assert_eq!(cfg.vfo_reference, VfoReference::Center);
    }

    #[test]
    fn test_dsb_extracts_real_part() {
        let mut demod = DsbDemodulator::new().unwrap();
        // DSB with no translation should extract the real part
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [Stereo::default(); 2];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 2);
        // DSB extracts real part (no frequency translation)
        assert!((output[0].l - 1.0).abs() < 1e-5);
        assert!((output[1].l - 3.0).abs() < 1e-5);
    }
}
