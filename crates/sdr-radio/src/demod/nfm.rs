//! Narrowband FM demodulator.

use sdr_dsp::demod::FmDemod;
use sdr_dsp::filter::FirFilter;
use sdr_dsp::taps;
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

/// Maximum bandwidth for NFM (Hz) — matches IF sample rate (C++ SDR++).
const NFM_MAX_BANDWIDTH: f64 = 50_000.0;

/// Default frequency snap interval for NFM (Hz) — C++ uses 2500 Hz.
const NFM_SNAP_INTERVAL: f64 = 2_500.0;

/// FM deviation for narrowband FM, computed as half the default bandwidth (Hz).
const NFM_DEVIATION_HZ: f64 = 6_250.0;

/// Transition width for post-discriminator lowpass as a fraction of cutoff.
const NFM_LPF_TRANSITION_RATIO: f64 = 0.3;

/// Narrowband FM demodulator using `FmDemod` from sdr-dsp.
///
/// Produces mono audio converted to stereo. Includes a post-discriminator
/// lowpass filter at `bandwidth/2` matching C++ SDR++ `_lowPass` flag
/// (default enabled).
pub struct NfmDemodulator {
    demod: FmDemod,
    /// Post-discriminator lowpass filter at bandwidth/2.
    audio_lpf: FirFilter,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    lpf_buf: Vec<f32>,
}

/// Build a lowpass FIR for post-discriminator filtering at the given bandwidth.
fn build_nfm_lpf(bandwidth: f64) -> Result<FirFilter, DspError> {
    let cutoff = bandwidth / 2.0;
    let transition = cutoff * NFM_LPF_TRANSITION_RATIO;
    let lpf_taps = taps::low_pass(cutoff, transition, NFM_IF_SAMPLE_RATE, false)?;
    FirFilter::new(lpf_taps)
}

impl NfmDemodulator {
    /// Create a new NFM demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying FM demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = FmDemod::from_hz(NFM_DEVIATION_HZ, NFM_IF_SAMPLE_RATE)?;
        let audio_lpf = build_nfm_lpf(NFM_DEFAULT_BANDWIDTH)?;
        let config = DemodConfig {
            if_sample_rate: NFM_IF_SAMPLE_RATE,
            af_sample_rate: NFM_AF_SAMPLE_RATE,
            default_bandwidth: NFM_DEFAULT_BANDWIDTH,
            min_bandwidth: NFM_MIN_BANDWIDTH,
            max_bandwidth: NFM_MAX_BANDWIDTH,
            bandwidth_locked: false,
            default_snap_interval: NFM_SNAP_INTERVAL,
            vfo_reference: VfoReference::Center,
            deemp_allowed: true,
            post_proc_enabled: true,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: true,
            nb_allowed: false,
            high_pass_allowed: true,
            squelch_allowed: true,
        };
        Ok(Self {
            demod,
            audio_lpf,
            config,
            mono_buf: Vec::new(),
            lpf_buf: Vec::new(),
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

        // Post-discriminator lowpass at bandwidth/2 — matches C++ _lowPass flag.
        // Reduces noise on weak signals by filtering above the audio passband.
        self.lpf_buf.resize(count, 0.0);
        self.audio_lpf
            .process_f32(&self.mono_buf[..count], &mut self.lpf_buf[..count])?;

        sdr_dsp::convert::mono_to_stereo(&self.lpf_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, bw: f64) {
        // Rebuild the FM discriminator with deviation = bw/2 so the
        // demodulator sensitivity tracks the channel bandwidth.
        match FmDemod::from_hz(bw / 2.0, NFM_IF_SAMPLE_RATE) {
            Ok(new_demod) => self.demod = new_demod,
            Err(e) => {
                tracing::warn!("NFM: set_bandwidth({bw}) demod failed: {e}");
                return;
            }
        }
        // Rebuild post-discriminator lowpass at new bandwidth/2.
        match build_nfm_lpf(bw) {
            Ok(new_lpf) => self.audio_lpf = new_lpf,
            Err(e) => tracing::warn!("NFM: set_bandwidth({bw}) LPF failed: {e}"),
        }
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
        assert!(cfg.deemp_allowed);
        assert!(!cfg.nb_allowed);
    }

    #[test]
    fn test_nfm_process_fm_signal() {
        let mut demod = NfmDemodulator::new().unwrap();
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
        let peak = output[1..]
            .iter()
            .map(|s| s.l.abs())
            .fold(0.0_f32, f32::max);
        assert!(peak > 0.001, "NFM should produce audio, peak = {peak}");
    }

    #[test]
    fn test_nfm_lpf_smooths_output() {
        let mut demod = NfmDemodulator::new().unwrap();
        // Feed noise-like signal — LPF should smooth the discriminator output.
        // Alternating I/Q values create abrupt phase changes that the LPF filters.
        let input: Vec<Complex> = (0..2000)
            .map(|i| {
                if i % 2 == 0 {
                    Complex::new(1.0, 0.0)
                } else {
                    Complex::new(0.0, 1.0)
                }
            })
            .collect();
        let mut output = vec![Stereo::default(); 2000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 2000);
        // Verify the LPF is active: output should be smoother than raw discriminator.
        // Consecutive samples should not have huge jumps after LPF settles.
        let max_jump = output[500..count]
            .windows(2)
            .map(|w| (w[1].l - w[0].l).abs())
            .fold(0.0_f32, f32::max);
        // Without LPF, jumps would be very large; with LPF they're smoothed
        assert!(
            max_jump < 5.0,
            "LPF should smooth output, max_jump = {max_jump}"
        );
    }

    #[test]
    fn test_nfm_set_bandwidth() {
        let mut demod = NfmDemodulator::new().unwrap();
        // Should not panic
        demod.set_bandwidth(25_000.0);
        demod.set_bandwidth(5_000.0);
    }
}
