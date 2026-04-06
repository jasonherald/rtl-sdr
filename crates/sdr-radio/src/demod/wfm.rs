//! Wideband FM (broadcast) demodulator.

use sdr_dsp::demod::BroadcastFmDemod;
use sdr_dsp::filter::{DEEMPHASIS_TAU_EU, FirFilter};
use sdr_dsp::stereo::FmStereoDecoder;
use sdr_dsp::taps;
use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// Audio lowpass cutoff frequency (Hz) — removes pilot, stereo subcarrier, RDS.
const AUDIO_LOWPASS_CUTOFF_HZ: f64 = 15_000.0;

/// Audio lowpass transition width (Hz).
const AUDIO_LOWPASS_TRANSITION_HZ: f64 = 4_000.0;

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
/// Supports both mono and stereo output:
/// - **Mono** (default): discriminator → 15 kHz LPF → dual-mono stereo
/// - **Stereo**: discriminator → full stereo decode (19 kHz pilot PLL,
///   38 kHz subcarrier demod, L+R/L−R matrixing)
///
/// Stereo decode matches C++ SDR++ `broadcast_fm.h`.
pub struct WfmDemodulator {
    demod: BroadcastFmDemod,
    /// 15 kHz lowpass filter — removes pilot tone, stereo subcarrier, RDS, noise.
    /// Used in mono mode.
    audio_lpf: FirFilter,
    /// FM stereo decoder — pilot PLL, subcarrier extraction, L/R matrixing.
    /// Used in stereo mode.
    stereo_decoder: FmStereoDecoder,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    lpf_buf: Vec<f32>,
    /// When true, perform stereo decode (pilot extraction + L−R matrixing).
    /// Default: false (mono), matching C++ SDR++ `_stereo = false` default.
    stereo: bool,
}

impl WfmDemodulator {
    /// Create a new WFM demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying FM demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = BroadcastFmDemod::new(WFM_IF_SAMPLE_RATE)?;

        // 15 kHz lowpass removes pilot tone (19 kHz), stereo subcarrier
        // (23-53 kHz), RDS (57 kHz), and wideband noise from the FM
        // composite baseband. Matches C++ broadcast_fm.h audioFirTaps.
        let lpf_taps = taps::low_pass(
            AUDIO_LOWPASS_CUTOFF_HZ,
            AUDIO_LOWPASS_TRANSITION_HZ,
            WFM_IF_SAMPLE_RATE,
            false,
        )?;
        let audio_lpf = FirFilter::new(lpf_taps)?;

        let stereo_decoder = FmStereoDecoder::new(WFM_IF_SAMPLE_RATE)?;

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
            fm_if_nr_allowed: true,
            nb_allowed: false,
            high_pass_allowed: true,
            squelch_allowed: true,
        };
        Ok(Self {
            demod,
            audio_lpf,
            stereo_decoder,
            config,
            mono_buf: Vec::new(),
            lpf_buf: Vec::new(),
            stereo: false,
        })
    }

    /// Enable or disable stereo decode.
    ///
    /// When enabled, the demodulator will perform pilot-tone stereo decode
    /// to produce independent L/R channels. When disabled (default), both
    /// channels receive the same mono (L+R) signal.
    pub fn set_stereo(&mut self, enabled: bool) {
        if self.stereo != enabled {
            // Reset stateful blocks to avoid stale history from the inactive path
            self.audio_lpf.reset();
            self.stereo_decoder.reset();
        }
        self.stereo = enabled;
        if enabled {
            tracing::info!("WFM stereo decode enabled");
        } else {
            tracing::info!("WFM stereo decode disabled (mono)");
        }
    }

    /// Returns whether stereo decode is enabled.
    pub fn is_stereo(&self) -> bool {
        self.stereo
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

        if self.stereo {
            // Stereo decode: pilot PLL → 38 kHz subcarrier → L−R → matrix
            self.stereo_decoder
                .process(&self.mono_buf[..count], &mut output[..count])?;
        } else {
            // Mono: 15 kHz lowpass → dual-mono
            self.lpf_buf.resize(count, 0.0);
            self.audio_lpf
                .process_f32(&self.mono_buf[..count], &mut self.lpf_buf[..count])?;
            sdr_dsp::convert::mono_to_stereo(&self.lpf_buf[..count], &mut output[..count])?;
        }

        Ok(count)
    }

    fn set_bandwidth(&mut self, _bw: f64) {
        // WFM bandwidth affects the VFO channel filter, not the discriminator.
        // Unlike NFM, broadcast FM deviation is fixed at 75 kHz by standard,
        // so BroadcastFmDemod does not need rebuilding when bandwidth changes.
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "WFM"
    }

    fn set_stereo(&mut self, enabled: bool) {
        WfmDemodulator::set_stereo(self, enabled);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
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

    #[test]
    fn test_wfm_stereo_mode() {
        let mut demod = WfmDemodulator::new().unwrap();
        assert!(!demod.is_stereo());
        demod.set_stereo(true);
        assert!(demod.is_stereo());

        // Process in stereo mode — should not crash
        let input = vec![Complex::new(1.0, 0.0); 5000];
        let mut output = vec![Stereo::default(); 5000];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 5000);
    }

    #[test]
    fn test_wfm_stereo_produces_different_channels() {
        let mut demod = WfmDemodulator::new().unwrap();
        demod.set_stereo(true);

        // Generate composite FM signal with stereo content
        let len = 10000;
        let input: Vec<Complex> = (0..len)
            .map(|i| {
                let t = i as f32 / 250_000.0;
                // FM with composite: mono + pilot + stereo subcarrier
                let phase = core::f32::consts::PI * 2.0 * 1000.0 * t
                    + 0.1 * (core::f32::consts::PI * 2.0 * 19_000.0 * t).sin()
                    + 0.3 * (core::f32::consts::PI * 2.0 * 38_000.0 * t).sin();
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); len];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, len);

        // Verify channel separation — stereo path should not collapse to dual-mono
        let settle = 2000;
        let mean_sep = output[settle..]
            .iter()
            .map(|s| (s.l - s.r).abs())
            .sum::<f32>()
            / (len - settle) as f32;
        assert!(
            mean_sep > 1e-3,
            "stereo path should not collapse to dual-mono, mean_sep = {mean_sep}"
        );
    }
}
