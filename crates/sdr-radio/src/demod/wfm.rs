//! Wideband FM (broadcast) demodulator.

use sdr_dsp::demod::BroadcastFmDemod;
use sdr_dsp::filter::{DEEMPHASIS_TAU_EU, FirFilter};
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
/// Produces dual-mono output (discriminator through 15 kHz LPF, same
/// signal to both L and R channels).
///
/// A `stereo` flag is available (opt-in, default off) for future stereo
/// decode matching C++ `broadcast_fm.h`:
///   1. Extract 19 kHz pilot via bandpass filter
///   2. PLL lock onto pilot, double to 38 kHz carrier
///   3. Multiply baseband by 38 kHz carrier to extract L-R
///   4. LPF the L-R signal at 15 kHz
///   5. Matrix: L = (L+R + L-R) / 2, R = (L+R - L-R) / 2
///
/// Until the stereo pipeline is implemented, output is always dual-mono
/// regardless of the `stereo` flag.
pub struct WfmDemodulator {
    demod: BroadcastFmDemod,
    /// 15 kHz lowpass filter — removes pilot tone, stereo subcarrier, RDS, noise.
    audio_lpf: FirFilter,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    lpf_buf: Vec<f32>,
    /// When true, perform stereo decode (pilot extraction + L-R matrixing).
    /// Default: false (mono), matching C++ SDR++ `_stereo = false` default.
    // TODO(issue #92): implement full stereo decode pipeline (pilot BPF, PLL,
    // 38 kHz carrier multiply, L-R extraction, stereo matrix).
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
    ///
    /// Note: stereo decode is not yet implemented — this flag is plumbed for
    /// future use. Currently always outputs mono regardless of this setting.
    pub fn set_stereo(&mut self, enabled: bool) {
        self.stereo = enabled;
        if enabled {
            tracing::info!("WFM stereo decode requested (not yet implemented, outputting mono)");
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

        // Apply 15 kHz lowpass to remove pilot, subcarrier, RDS, and noise.
        self.lpf_buf.resize(count, 0.0);
        self.audio_lpf
            .process_f32(&self.mono_buf[..count], &mut self.lpf_buf[..count])?;

        // Convert filtered mono to stereo (same signal both channels)
        sdr_dsp::convert::mono_to_stereo(&self.lpf_buf[..count], &mut output[..count])?;
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
