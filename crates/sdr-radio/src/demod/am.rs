//! Amplitude modulation demodulator.

use sdr_dsp::demod::AmDemod;
use sdr_dsp::loops::Agc;
use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// IF sample rate for AM mode (Hz).
const AM_IF_SAMPLE_RATE: f64 = 15_000.0;

/// AF (audio) sample rate produced by AM demod (Hz).
const AM_AF_SAMPLE_RATE: f64 = 15_000.0;

/// Default channel bandwidth for AM (Hz).
const AM_DEFAULT_BANDWIDTH: f64 = 10_000.0;

/// Minimum bandwidth for AM (Hz).
const AM_MIN_BANDWIDTH: f64 = 1_000.0;

/// Maximum bandwidth for AM (Hz).
const AM_MAX_BANDWIDTH: f64 = 15_000.0;

/// Default frequency snap interval for AM (Hz).
const AM_SNAP_INTERVAL: f64 = 1_000.0;

/// AGC set point (target output amplitude) for AM mode.
const AM_AGC_SET_POINT: f32 = 1.0;
/// AGC attack coefficient for AM mode.
const AM_AGC_ATTACK: f32 = 0.001;
/// AGC decay coefficient for AM mode.
const AM_AGC_DECAY: f32 = 0.0001;
/// AGC maximum gain for AM mode.
const AM_AGC_MAX_GAIN: f32 = 1e6;
/// AGC maximum output amplitude for AM mode.
const AM_AGC_MAX_OUTPUT: f32 = 10.0;
/// AGC initial gain for AM mode.
const AM_AGC_INIT_GAIN: f32 = 1.0;

/// AM demodulator using `AmDemod` from sdr-dsp.
///
/// Extracts the amplitude envelope, applies DC blocking and AGC, and outputs stereo.
pub struct AmDemodulator {
    demod: AmDemod,
    agc: Agc,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    agc_buf: Vec<f32>,
}

impl AmDemodulator {
    /// Create a new AM demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the AGC cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = AmDemod::new();
        let agc = Agc::new(
            AM_AGC_SET_POINT,
            AM_AGC_ATTACK,
            AM_AGC_DECAY,
            AM_AGC_MAX_GAIN,
            AM_AGC_MAX_OUTPUT,
            AM_AGC_INIT_GAIN,
        )?;
        let config = DemodConfig {
            if_sample_rate: AM_IF_SAMPLE_RATE,
            af_sample_rate: AM_AF_SAMPLE_RATE,
            default_bandwidth: AM_DEFAULT_BANDWIDTH,
            min_bandwidth: AM_MIN_BANDWIDTH,
            max_bandwidth: AM_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: AM_SNAP_INTERVAL,
            vfo_reference: VfoReference::Center,
            deemp_allowed: false,
            post_proc_enabled: true,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: false,
            nb_allowed: true,
            high_pass_allowed: true,
            squelch_allowed: true,
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

impl Demodulator for AmDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        self.mono_buf.resize(input.len(), 0.0);
        let count = self.demod.process(input, &mut self.mono_buf)?;

        // Apply AGC to normalize AM audio levels before stereo conversion.
        self.agc_buf.resize(count, 0.0);
        self.agc
            .process_f32(&self.mono_buf[..count], &mut self.agc_buf[..count])?;

        sdr_dsp::convert::mono_to_stereo(&self.agc_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, _bw: f64) {
        // Bandwidth is handled by the VFO channel filter.
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "AM"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn test_am_config() {
        let demod = AmDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 15_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 10_000.0).abs() < f64::EPSILON);
        assert!(!cfg.deemp_allowed);
        assert!(cfg.nb_allowed);
    }

    #[test]
    fn test_am_process_envelope() {
        let mut demod = AmDemodulator::new().unwrap();
        // AM signal: carrier with modulated amplitude
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let amp = 1.0 + 0.5 * (2.0 * PI * 0.01 * i as f32).sin();
                Complex::new(amp, 0.0)
            })
            .collect();
        let mut output = vec![Stereo::default(); 1000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 1000);
        // Output should have non-zero audio after DC blocker settles
        let peak = output[500..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "AM should extract envelope, peak = {peak}");
        // L and R should match (mono-to-stereo)
        for s in &output {
            assert!(
                (s.l - s.r).abs() < 1e-6,
                "mono-to-stereo: L and R should match"
            );
        }
    }
}
