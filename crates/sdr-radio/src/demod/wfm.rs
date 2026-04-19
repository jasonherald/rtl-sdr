//! Wideband FM (broadcast) demodulator.

use sdr_dsp::demod::BroadcastFmDemod;
use sdr_dsp::filter::{DEEMPHASIS_TAU_EU, FirFilter};
use sdr_dsp::loops::Agc;
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

// Audio AGC parameters — same shape as NFM / AM so all three
// demodulators normalize to comparable output loudness. Broadcast
// FM stations follow a ±75 kHz deviation standard, but modulation
// practice varies (compressed / uncompressed music, talk, etc.)
// and RF path losses shift the discriminator output level too.
// AGC closes both loops.
/// Audio AGC set point (target output amplitude).
const WFM_AGC_SET_POINT: f32 = 1.0;
/// Audio AGC attack coefficient.
const WFM_AGC_ATTACK: f32 = 0.003_333_333;
/// Audio AGC decay coefficient.
const WFM_AGC_DECAY: f32 = 0.000_333_333;
/// Audio AGC maximum gain ceiling.
const WFM_AGC_MAX_GAIN: f32 = 1e6;
/// Audio AGC maximum output amplitude (look-ahead clipping cap).
const WFM_AGC_MAX_OUTPUT: f32 = 10.0;
/// Audio AGC initial gain (pre-settling).
const WFM_AGC_INIT_GAIN: f32 = 1.0;

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
    /// Audio-level AGC — normalizes mono output loudness. In
    /// stereo mode we apply the same AGC's gain to both L and R
    /// via a shared-gain pass so stereo imaging is preserved
    /// (independent per-channel AGCs would drift L vs R).
    audio_agc: Agc,
    /// Scratch buffer for the mono signal that drives the AGC
    /// envelope. For the mono path this is the post-LPF output;
    /// for the stereo path it's the per-sample RMS energy
    /// estimate `sqrt((L² + R²) / 2)` computed from the stereo-
    /// decoder output. The RMS form (vs. the naive `(L + R) / 2`)
    /// avoids anti-phase cancellation and the 3 dB under-count on
    /// mono-in-one-channel content — see the stereo branch in
    /// `process` and the `SquelchAudioEnvelope` / #332 notes for
    /// the full reasoning.
    agc_mono_buf: Vec<f32>,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    lpf_buf: Vec<f32>,
    agc_buf: Vec<f32>,
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

        let audio_agc = Agc::new(
            WFM_AGC_SET_POINT,
            WFM_AGC_ATTACK,
            WFM_AGC_DECAY,
            WFM_AGC_MAX_GAIN,
            WFM_AGC_MAX_OUTPUT,
            WFM_AGC_INIT_GAIN,
        )?;

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
            audio_agc,
            agc_mono_buf: Vec::new(),
            config,
            mono_buf: Vec::new(),
            lpf_buf: Vec::new(),
            agc_buf: Vec::new(),
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
            // Reset stateful blocks to avoid stale history from
            // the inactive path. Audio AGC is reset too — mono
            // and stereo paths feed the envelope tracker with
            // different amplitude scales (stereo has the L+R
            // mono sum which averages vs. the mono LPF output),
            // so carrying envelope state across the flip would
            // produce a transient loudness jump.
            self.audio_lpf.reset();
            self.stereo_decoder.reset();
            self.audio_agc.reset();
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

            // Apply audio AGC with shared L/R gain so stereo
            // imaging is preserved. Drive the envelope from a
            // non-cancelling RMS energy estimate —
            // `sqrt((L² + R²) / 2)` — instead of the `(L+R)/2`
            // mono sum: the sum cancels to zero on anti-phase
            // material (L and R equal and opposite) and drops
            // 3 dB on left- or right-only content, both of which
            // would steer the AGC wrong even though the user is
            // hearing real energy. Energy estimate is always
            // non-negative and matches what the channels
            // actually put through the speakers.
            self.agc_mono_buf.resize(count, 0.0);
            for (i, s) in output[..count].iter().enumerate() {
                self.agc_mono_buf[i] = (0.5 * (s.l * s.l + s.r * s.r)).sqrt();
            }
            self.agc_buf.resize(count, 0.0);
            self.audio_agc
                .process_f32(&self.agc_mono_buf[..count], &mut self.agc_buf[..count])?;
            for (i, s) in output[..count].iter_mut().enumerate() {
                // `gain = agc_output / rms_energy`. The energy
                // estimate is non-negative by construction, so a
                // single positive-magnitude guard covers the
                // silent-input case without needing `.abs()`.
                let gain = if self.agc_mono_buf[i] > f32::MIN_POSITIVE {
                    self.agc_buf[i] / self.agc_mono_buf[i]
                } else {
                    1.0
                };
                s.l *= gain;
                s.r *= gain;
            }
        } else {
            // Mono: 15 kHz lowpass → AGC → dual-mono
            self.lpf_buf.resize(count, 0.0);
            self.audio_lpf
                .process_f32(&self.mono_buf[..count], &mut self.lpf_buf[..count])?;
            // Audio AGC — see NFM's AGC stage for the rationale.
            // WFM's deviation is fixed at ±75 kHz by standard but
            // RF path loss and station modulation practice still
            // produce varying output levels; AGC normalizes.
            self.agc_buf.resize(count, 0.0);
            self.audio_agc
                .process_f32(&self.lpf_buf[..count], &mut self.agc_buf[..count])?;
            sdr_dsp::convert::mono_to_stereo(&self.agc_buf[..count], &mut output[..count])?;
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
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]
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
    fn test_wfm_mono_audio_agc_preserves_channel_match() {
        // Audio AGC must apply the SAME gain to L and R in mono
        // mode (they come from a shared mono buffer via
        // mono_to_stereo), so per-sample L and R should stay
        // within float epsilon of each other. Pins the invariant
        // that we're not accidentally running independent L/R
        // AGCs and losing the mono guarantee.
        use core::f32::consts::PI;
        let mut demod = WfmDemodulator::new().unwrap();
        let mod_freq = 1_000.0_f32;
        let deviation_hz = 50_000.0_f32;
        let n = 3000;
        let input: Vec<Complex> = (0..n)
            .map(|i| {
                let t = i as f32 / WFM_IF_SAMPLE_RATE as f32;
                let phase = deviation_hz * (2.0 * PI * mod_freq * t).sin() / mod_freq;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut output = vec![Stereo::default(); n];
        demod.process(&input, &mut output).unwrap();
        for (i, s) in output.iter().enumerate().skip(1500) {
            assert!(
                (s.l - s.r).abs() < 1e-6,
                "mono WFM must preserve L == R after AGC at sample {i}, got L={} R={}",
                s.l,
                s.r
            );
        }
    }

    #[test]
    fn test_wfm_stereo_audio_agc_preserves_imaging() {
        use core::f32::consts::PI;
        // Synthesize an FM broadcast MPX signal carrying
        // asymmetric stereo content — L four times louder than
        // R — and confirm the relationship survives the shared-
        // gain AGC. An accidental switch to independent L/R
        // AGCs (each channel normalized to its own envelope)
        // would flatten the L:R ratio toward 1.0 and fail this
        // test, where the vacuous silence-only version it
        // replaces would have passed regardless.
        let sample_rate = WFM_IF_SAMPLE_RATE as f32;
        let deviation_hz: f32 = 50_000.0;
        let audio_freq_hz: f32 = 1_000.0;
        let pilot_freq_hz: f32 = 19_000.0;
        let subcarrier_freq_hz: f32 = 2.0 * pilot_freq_hz;
        // L >> R on the same 1 kHz tone. Amplitudes chosen to
        // keep the MPX peak under FM's ±75 kHz deviation headroom
        // and produce a large, measurable L/R ratio after decode.
        let left_amp: f32 = 0.4;
        let right_amp: f32 = 0.1;

        // Phase accumulation over 20_000 samples (80 ms at
        // 250 kHz). Generous settling time for the stereo
        // decoder's 19 kHz pilot PLL plus the audio AGC's
        // ~300-sample attack.
        let n = 20_000;
        let mut phase: f32 = 0.0;
        let phase_scale = 2.0 * PI * deviation_hz / sample_rate;
        let input: Vec<Complex> = (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate;
                let left = left_amp * (2.0 * PI * audio_freq_hz * t).sin();
                let right = right_amp * (2.0 * PI * audio_freq_hz * t).sin();
                // Broadcast MPX: mono sum + pilot + DSBSC(L-R).
                let mono = left + right;
                let diff = left - right;
                let pilot = 0.1 * (2.0 * PI * pilot_freq_hz * t).cos();
                let subcarrier = diff * (2.0 * PI * subcarrier_freq_hz * t).cos();
                let mpx = mono + pilot + subcarrier;
                phase += phase_scale * mpx;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();

        let mut demod = WfmDemodulator::new().unwrap();
        demod.set_stereo(true);
        let mut output = vec![Stereo::default(); n];
        demod.process(&input, &mut output).unwrap();

        // Skip PLL + AGC settling, then measure the L / R
        // envelope-level ratio over the steady-state portion.
        let settle = 10_000;
        let mean_abs_l =
            output[settle..].iter().map(|s| s.l.abs()).sum::<f32>() / (n - settle) as f32;
        let mean_abs_r =
            output[settle..].iter().map(|s| s.r.abs()).sum::<f32>() / (n - settle) as f32;

        // Shared-gain AGC must preserve the input 4:1 imbalance
        // roughly. Floor the ratio at 2.0× so an independent
        // per-channel AGC (which would push this toward 1.0)
        // fails decisively, and ceil it at 10× so a catastrophic
        // bug amplifying one channel wildly also fails.
        assert!(
            mean_abs_r > 1e-5,
            "stereo AGC produced a near-zero R channel: mean_abs_r = {mean_abs_r}"
        );
        let ratio = mean_abs_l / mean_abs_r;
        assert!(
            ratio > 2.0 && ratio < 10.0,
            "stereo imaging not preserved: mean_abs_l = {mean_abs_l}, mean_abs_r = {mean_abs_r}, ratio = {ratio}"
        );
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
