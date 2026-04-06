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

/// Build a lowpass FIR for AM audio filtering at the given bandwidth.
fn build_am_lpf(bandwidth: f64) -> Result<FirFilter, DspError> {
    let cutoff = bandwidth / 2.0;
    let transition = cutoff * AM_LPF_TRANSITION_RATIO;
    let lpf_taps = taps::low_pass(cutoff, transition, AM_IF_SAMPLE_RATE, false)?;
    FirFilter::new(lpf_taps)
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
        let audio_lpf = build_am_lpf(AM_DEFAULT_BANDWIDTH)?;
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
        // Rebuild internal lowpass at new bandwidth/2
        match build_am_lpf(bw) {
            Ok(new_lpf) => self.audio_lpf = new_lpf,
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
        let mut demod = AmDemodulator::new().unwrap();
        // Feed signal with varying carrier amplitude — carrier AGC should stabilize
        let input: Vec<Complex> = (0..2000)
            .map(|i| {
                // Carrier amplitude ramps up
                let carrier = 0.1 + (i as f32 / 2000.0) * 2.0;
                Complex::new(carrier, 0.0)
            })
            .collect();
        let mut output = vec![Stereo::default(); 2000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 2000);
        // After AGC settles, output should be relatively stable despite carrier ramp
        let late_range: Vec<f32> = output[1500..].iter().map(|s| s.l).collect();
        let late_max = late_range.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let late_min = late_range.iter().cloned().fold(f32::INFINITY, f32::min);
        let range = late_max - late_min;
        assert!(
            range < 2.0,
            "carrier AGC should stabilize output, range = {range}"
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
