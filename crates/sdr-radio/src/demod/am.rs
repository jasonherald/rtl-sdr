//! Amplitude modulation demodulator.

use sdr_dsp::demod::AmDemod;
use sdr_dsp::filter::FirFilter;
use sdr_dsp::loops::Agc;
use sdr_dsp::taps;
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
/// AGC attack coefficient for AM mode — matches C++ SDR++ (1/300).
const AM_AGC_ATTACK: f32 = 0.003_333_333;
/// AGC decay coefficient for AM mode — matches C++ SDR++ (1/3000).
const AM_AGC_DECAY: f32 = 0.000_333_333;
/// AGC maximum gain for AM mode.
const AM_AGC_MAX_GAIN: f32 = 1e6;
/// AGC maximum output amplitude for AM mode.
const AM_AGC_MAX_OUTPUT: f32 = 10.0;
/// AGC initial gain for AM mode.
const AM_AGC_INIT_GAIN: f32 = 1.0;

/// Transition width for AM lowpass as a fraction of cutoff.
const AM_LPF_TRANSITION_RATIO: f64 = 0.3;

/// Nyquist guard margin (Hz) for LPF bypass detection.
const AM_LPF_NYQUIST_GUARD_HZ: f64 = 1.0;

/// AM demodulator using `AmDemod` from sdr-dsp.
///
/// Matches C++ SDR++ AM demod architecture:
/// 1. **Carrier AGC** — normalizes complex signal before magnitude extraction
///    (stabilizes carrier level across fading/AGC pumping)
/// 2. **Envelope detection** — magnitude + DC blocking
/// 3. **Audio lowpass** — bandwidth-dependent FIR at bandwidth/2
/// 4. **Audio AGC** — normalizes audio output levels
pub struct AmDemodulator {
    demod: AmDemod,
    carrier_agc: Agc,
    audio_agc: Agc,
    audio_lpf: FirFilter,
    config: DemodConfig,
    carrier_buf: Vec<Complex>,
    mono_buf: Vec<f32>,
    lpf_buf: Vec<f32>,
    agc_buf: Vec<f32>,
}

/// Build lowpass FIR taps for AM audio filtering at the given bandwidth.
/// Returns `None` if cutoff is at or above Nyquist (no filter needed).
fn build_am_lpf_taps(bandwidth: f64) -> Result<Option<Vec<f32>>, DspError> {
    let cutoff = bandwidth / 2.0;
    let nyquist = AM_AF_SAMPLE_RATE / 2.0;
    if cutoff >= nyquist - AM_LPF_NYQUIST_GUARD_HZ {
        return Ok(None); // bandwidth spans full audio rate — bypass LPF
    }
    let transition =
        (cutoff * AM_LPF_TRANSITION_RATIO).min(nyquist - cutoff - AM_LPF_NYQUIST_GUARD_HZ);
    let lpf_taps = taps::low_pass(cutoff, transition, AM_AF_SAMPLE_RATE, false)?;
    Ok(Some(lpf_taps))
}

impl AmDemodulator {
    /// Create a new AM demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the AGC cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = AmDemod::new();
        let carrier_agc = Agc::new(
            AM_AGC_SET_POINT,
            AM_AGC_ATTACK,
            AM_AGC_DECAY,
            AM_AGC_MAX_GAIN,
            AM_AGC_MAX_OUTPUT,
            AM_AGC_INIT_GAIN,
        )?;
        let audio_agc = Agc::new(
            AM_AGC_SET_POINT,
            AM_AGC_ATTACK,
            AM_AGC_DECAY,
            AM_AGC_MAX_GAIN,
            AM_AGC_MAX_OUTPUT,
            AM_AGC_INIT_GAIN,
        )?;
        let audio_lpf = match build_am_lpf_taps(AM_DEFAULT_BANDWIDTH)? {
            Some(taps) => FirFilter::new(taps)?,
            None => FirFilter::new(vec![1.0])?, // passthrough
        };
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
            carrier_agc,
            audio_agc,
            audio_lpf,
            config,
            carrier_buf: Vec::new(),
            mono_buf: Vec::new(),
            lpf_buf: Vec::new(),
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

        // Step 1: Carrier AGC — normalize complex signal before envelope detection.
        // Stabilizes carrier level across fading, matching C++ CARRIER mode.
        self.carrier_buf.resize(input.len(), Complex::default());
        self.carrier_agc
            .process_complex(input, &mut self.carrier_buf)?;

        // Step 2: Envelope detection (magnitude + DC blocking)
        self.mono_buf.resize(input.len(), 0.0);
        let count = self.demod.process(&self.carrier_buf, &mut self.mono_buf)?;

        // Step 3: Audio lowpass — bandwidth-dependent, matching C++ internal LPF
        self.lpf_buf.resize(count, 0.0);
        self.audio_lpf
            .process_f32(&self.mono_buf[..count], &mut self.lpf_buf[..count])?;

        // Step 4: Audio AGC — normalize output audio levels
        self.agc_buf.resize(count, 0.0);
        self.audio_agc
            .process_f32(&self.lpf_buf[..count], &mut self.agc_buf[..count])?;

        sdr_dsp::convert::mono_to_stereo(&self.agc_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, bw: f64) {
        // Retune internal lowpass in place (preserves delay line for seamless transition)
        match build_am_lpf_taps(bw) {
            Ok(Some(taps)) => {
                if let Err(e) = self.audio_lpf.set_taps(taps) {
                    tracing::warn!("AM: set_bandwidth({bw}) set_taps failed: {e}");
                }
            }
            Ok(None) => {
                if let Err(e) = self.audio_lpf.set_taps(vec![1.0]) {
                    tracing::warn!("AM: set_bandwidth({bw}) passthrough set_taps failed: {e}");
                }
            }
            Err(e) => tracing::warn!("AM: set_bandwidth({bw}) LPF failed: {e}"),
        }
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "AM"
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cloned_instead_of_copied
)]
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
        // Output should have non-zero audio after DC blocker + AGC settles
        let peak = output[500..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.01, "AM should extract envelope, peak = {peak}");
        // L and R should match (mono-to-stereo)
        for s in &output {
            assert!(
                (s.l - s.r).abs() < 1e-6,
                "mono-to-stereo: L and R should match"
            );
        }
    }

    #[test]
    fn test_am_carrier_agc_stabilizes() {
        // Feed AM-modulated signals at two very different carrier levels.
        // Carrier AGC should normalize so recovered audio amplitude is
        // similar regardless of carrier strength.
        let mod_freq = 500.0_f32;
        let mod_depth = 0.3;
        let settle = 1500;
        let len = 3000;

        let mut peaks = Vec::new();
        for &carrier_amp in &[0.1_f32, 2.0] {
            let mut demod = AmDemodulator::new().unwrap();
            let input: Vec<Complex> = (0..len)
                .map(|i| {
                    let t = i as f32 / AM_IF_SAMPLE_RATE as f32;
                    let envelope =
                        carrier_amp * (1.0 + mod_depth * (2.0 * PI * mod_freq * t).sin());
                    Complex::new(envelope, 0.0)
                })
                .collect();
            let mut output = vec![Stereo::default(); len];
            demod.process(&input, &mut output).unwrap();
            let peak = output[settle..]
                .iter()
                .map(|s| s.l.abs())
                .fold(0.0_f32, f32::max);
            peaks.push(peak);
        }

        // Both carrier levels should produce similar audio amplitude after AGC
        let ratio = if peaks[0] > peaks[1] {
            peaks[0] / peaks[1].max(1e-10)
        } else {
            peaks[1] / peaks[0].max(1e-10)
        };
        assert!(
            ratio < 10.0,
            "carrier AGC should normalize audio across carrier levels, ratio = {ratio}"
        );
    }

    #[test]
    fn test_am_set_bandwidth() {
        let mut demod = AmDemodulator::new().unwrap();
        // Should not panic
        demod.set_bandwidth(5_000.0);
        demod.set_bandwidth(15_000.0);
    }
}
