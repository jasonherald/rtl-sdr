//! Wideband FM (broadcast) demodulator.

use sdr_dsp::demod::BroadcastFmDemod;
use sdr_dsp::filter::DEEMPHASIS_TAU_EU;
use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// IF sample rate for WFM mode (Hz).
const WFM_IF_SAMPLE_RATE: f64 = 250_000.0;

/// AF (audio) sample rate produced by WFM demod (Hz).
/// Matches the IF rate since stereo decode happens at this rate.
const WFM_AF_SAMPLE_RATE: f64 = 250_000.0;

/// Default channel bandwidth for WFM (Hz).
const WFM_DEFAULT_BANDWIDTH: f64 = 150_000.0;

/// Minimum bandwidth for WFM (Hz).
const WFM_MIN_BANDWIDTH: f64 = 50_000.0;

/// Maximum bandwidth for WFM (Hz).
const WFM_MAX_BANDWIDTH: f64 = 250_000.0;

/// Default frequency snap interval for WFM (Hz) — broadcast FM spacing.
const WFM_SNAP_INTERVAL: f64 = 100_000.0;

/// Wideband FM demodulator using `BroadcastFmDemod` from sdr-dsp.
///
/// Produces mono output (discriminator) that the AF chain can process
/// with deemphasis. Full stereo decode is deferred to a later phase.
pub struct WfmDemodulator {
    demod: BroadcastFmDemod,
    config: DemodConfig,
    mono_buf: Vec<f32>,
}

impl WfmDemodulator {
    /// Create a new WFM demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying FM demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = BroadcastFmDemod::new(WFM_IF_SAMPLE_RATE)?;
        let config = DemodConfig {
            if_sample_rate: WFM_IF_SAMPLE_RATE,
            af_sample_rate: WFM_AF_SAMPLE_RATE,
            default_bandwidth: WFM_DEFAULT_BANDWIDTH,
            min_bandwidth: WFM_MIN_BANDWIDTH,
            max_bandwidth: WFM_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: WFM_SNAP_INTERVAL,
            vfo_reference: VfoReference::Center,
            deemp_allowed: true,
            post_proc_enabled: true,
            default_deemp_tau: DEEMPHASIS_TAU_EU,
            fm_if_nr_allowed: false,
            nb_allowed: true,
            high_pass_allowed: false,
            squelch_allowed: true,
        };
        Ok(Self {
            demod,
            config,
            mono_buf: Vec::new(),
        })
    }
}

impl Demodulator for WfmDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        self.mono_buf.resize(input.len(), 0.0);
        let count = self.demod.process(input, &mut self.mono_buf)?;
        // Convert mono discriminator output to stereo (same signal both channels)
        sdr_dsp::convert::mono_to_stereo(&self.mono_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, _bw: f64) {
        // WFM bandwidth affects the VFO channel filter, not the discriminator.
        // The demod itself always operates at the fixed IF sample rate.
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "WFM"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_wfm_config() {
        let demod = WfmDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 250_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 150_000.0).abs() < f64::EPSILON);
        assert!(cfg.deemp_allowed);
        assert!(cfg.squelch_allowed);
        assert_eq!(cfg.vfo_reference, VfoReference::Center);
    }

    #[test]
    fn test_wfm_process_produces_output() {
        let mut demod = WfmDemodulator::new().unwrap();
        // Generate a simple FM signal: constant frequency = silence
        let input = vec![Complex::new(1.0, 0.0); 1000];
        let mut output = vec![Stereo::default(); 1000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
    }
}
