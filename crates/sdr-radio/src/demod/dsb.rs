//! Double sideband (DSB) demodulator.

use sdr_dsp::demod::{SsbDemod, SsbMode};
use sdr_dsp::loops::Agc;
use sdr_types::{Complex, DspError, Stereo};

use super::{
    DemodConfig, Demodulator, SSB_AGC_ATTACK, SSB_AGC_DECAY, SSB_AGC_INIT_GAIN, SSB_AGC_MAX_GAIN,
    SSB_AGC_MAX_OUTPUT, SSB_AGC_SET_POINT, VfoReference,
};

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
    agc: Agc,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    agc_buf: Vec<f32>,
}

impl DsbDemodulator {
    /// Create a new DSB demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying SSB demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = SsbDemod::new(SsbMode::Dsb, DSB_DEFAULT_BANDWIDTH, DSB_IF_SAMPLE_RATE)?;
        let agc = Agc::new(
            SSB_AGC_SET_POINT,
            SSB_AGC_ATTACK,
            SSB_AGC_DECAY,
            SSB_AGC_MAX_GAIN,
            SSB_AGC_MAX_OUTPUT,
            SSB_AGC_INIT_GAIN,
        )?;
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
            agc,
            config,
            mono_buf: Vec::new(),
            agc_buf: Vec::new(),
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
        super::process_with_agc_to_stereo(
            &mut self.agc,
            &self.mono_buf[..count],
            &mut self.agc_buf,
            &mut output[..count],
        )
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
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
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
    fn test_dsb_produces_audio() {
        let mut demod = DsbDemodulator::new().unwrap();
        // Feed a longer signal so AGC can settle, then verify output is non-zero.
        let input: Vec<Complex> = (0..500)
            .map(|i| {
                let phase = 2.0 * core::f32::consts::PI * 1000.0 * (i as f32) / 24_000.0;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); 500];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 500);
        // After AGC settles, output should have meaningful amplitude.
        let peak = output[100..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "DSB should produce audio, peak = {peak}");
    }
}
