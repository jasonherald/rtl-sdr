//! Lower sideband (LSB) demodulator.

use sdr_dsp::demod::{SsbDemod, SsbMode};
use sdr_dsp::loops::Agc;
use sdr_types::{Complex, DspError, Stereo};

use super::{
    DemodConfig, Demodulator, SSB_AGC_ATTACK, SSB_AGC_DECAY, SSB_AGC_INIT_GAIN, SSB_AGC_MAX_GAIN,
    SSB_AGC_MAX_OUTPUT, SSB_AGC_SET_POINT, VfoReference,
};

/// IF sample rate for LSB mode (Hz).
const LSB_IF_SAMPLE_RATE: f64 = 24_000.0;

/// AF (audio) sample rate produced by LSB demod (Hz).
const LSB_AF_SAMPLE_RATE: f64 = 24_000.0;

/// Default channel bandwidth for LSB (Hz).
const LSB_DEFAULT_BANDWIDTH: f64 = 2_800.0;

/// Minimum bandwidth for LSB (Hz).
const LSB_MIN_BANDWIDTH: f64 = 500.0;

/// Maximum bandwidth for LSB (Hz).
const LSB_MAX_BANDWIDTH: f64 = 12_000.0;

/// Default frequency snap interval for LSB (Hz).
const LSB_SNAP_INTERVAL: f64 = 100.0;

/// Lower sideband demodulator using `SsbDemod(Lsb)` from sdr-dsp.
pub struct LsbDemodulator {
    demod: SsbDemod,
    agc: Agc,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    agc_buf: Vec<f32>,
}

impl LsbDemodulator {
    /// Create a new LSB demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying SSB demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = SsbDemod::new(SsbMode::Lsb, LSB_DEFAULT_BANDWIDTH, LSB_IF_SAMPLE_RATE)?;
        let agc = Agc::new(
            SSB_AGC_SET_POINT,
            SSB_AGC_ATTACK,
            SSB_AGC_DECAY,
            SSB_AGC_MAX_GAIN,
            SSB_AGC_MAX_OUTPUT,
            SSB_AGC_INIT_GAIN,
        )?;
        let config = DemodConfig {
            if_sample_rate: LSB_IF_SAMPLE_RATE,
            af_sample_rate: LSB_AF_SAMPLE_RATE,
            default_bandwidth: LSB_DEFAULT_BANDWIDTH,
            min_bandwidth: LSB_MIN_BANDWIDTH,
            max_bandwidth: LSB_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: LSB_SNAP_INTERVAL,
            vfo_reference: VfoReference::Upper,
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

impl Demodulator for LsbDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        self.mono_buf.resize(input.len(), 0.0);
        let count = self.demod.process(input, &mut self.mono_buf)?;

        // Apply AGC to normalize SSB audio levels before stereo conversion.
        self.agc_buf.resize(count, 0.0);
        self.agc
            .process_f32(&self.mono_buf[..count], &mut self.agc_buf[..count])?;

        sdr_dsp::convert::mono_to_stereo(&self.agc_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, bw: f64) {
        if let Err(e) = self.demod.set_bandwidth(bw) {
            tracing::warn!("LSB: set_bandwidth({bw}) failed: {e}");
        }
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "LSB"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn test_lsb_config() {
        let demod = LsbDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 24_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 2_800.0).abs() < f64::EPSILON);
        assert_eq!(cfg.vfo_reference, VfoReference::Upper);
    }

    #[test]
    fn test_lsb_process_produces_audio() {
        let mut demod = LsbDemodulator::new().unwrap();
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = 2.0 * PI * 1000.0 * (i as f32) / 24_000.0;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); 1000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        let peak = output[100..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.3, "LSB should produce audio, peak = {peak}");
    }
}
