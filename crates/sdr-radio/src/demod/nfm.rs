//! Narrowband FM demodulator.

use sdr_dsp::demod::FmDemod;
use sdr_dsp::filter::{DEEMPHASIS_TAU_US, FirFilter};
use sdr_dsp::loops::Agc;
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

/// Nyquist guard margin (Hz) for LPF bypass detection.
const NFM_NYQUIST_GUARD_HZ: f64 = 1.0;

/// Passthrough FIR tap (identity filter).
const NFM_PASSTHROUGH_TAPS: [f32; 1] = [1.0];

// Audio AGC parameters — mirror the AM demod's audio AGC
// (set_point=1.0, attack=1/300, decay=1/3000, max_gain=1e6,
// max_output=10.0, init_gain=1.0) so NFM audio levels are
// normalized across stations with different deviations. Without
// AGC, a tight-deviation commercial NFM signal (±2.5 kHz) sounds
// much quieter than a wider-deviation ham signal (±5 kHz) even
// though the RF level is the same — see #332.
/// Audio AGC set point (target output amplitude).
const NFM_AGC_SET_POINT: f32 = 1.0;
/// Audio AGC attack coefficient.
const NFM_AGC_ATTACK: f32 = 0.003_333_333;
/// Audio AGC decay coefficient.
const NFM_AGC_DECAY: f32 = 0.000_333_333;
/// Audio AGC maximum gain ceiling.
const NFM_AGC_MAX_GAIN: f32 = 1e6;
/// Audio AGC maximum output amplitude (look-ahead clipping cap).
const NFM_AGC_MAX_OUTPUT: f32 = 10.0;
/// Audio AGC initial gain (pre-settling).
const NFM_AGC_INIT_GAIN: f32 = 1.0;

/// Narrowband FM demodulator using `FmDemod` from sdr-dsp.
///
/// Produces mono audio converted to stereo. Includes a post-discriminator
/// lowpass filter at `bandwidth/2` matching C++ SDR++ `_lowPass` flag
/// (default enabled).
pub struct NfmDemodulator {
    demod: FmDemod,
    /// Post-discriminator lowpass filter at bandwidth/2.
    audio_lpf: FirFilter,
    /// Audio-level AGC — normalizes output amplitude so stations
    /// with different FM deviations play at comparable loudness.
    /// Applied downstream of the LPF so we're tracking the
    /// finished audio, not the raw discriminator output (which
    /// includes above-cutoff noise on weak signals that would
    /// bias the envelope tracker).
    audio_agc: Agc,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    lpf_buf: Vec<f32>,
    /// Scratch buffer for the post-LPF AGC stage.
    agc_buf: Vec<f32>,
}

/// Build lowpass FIR taps for post-discriminator filtering at the given bandwidth.
/// Returns `None` if cutoff is at or above Nyquist (no filter needed).
fn build_nfm_lpf_taps(bandwidth: f64) -> Result<Option<Vec<f32>>, DspError> {
    let cutoff = bandwidth / 2.0;
    let nyquist = NFM_AF_SAMPLE_RATE / 2.0;
    if cutoff >= nyquist - NFM_NYQUIST_GUARD_HZ {
        return Ok(None); // bandwidth spans full audio rate — bypass LPF
    }
    let transition =
        (cutoff * NFM_LPF_TRANSITION_RATIO).min(nyquist - cutoff - NFM_NYQUIST_GUARD_HZ);
    let lpf_taps = taps::low_pass(cutoff, transition, NFM_AF_SAMPLE_RATE, false)?;
    Ok(Some(lpf_taps))
}

impl NfmDemodulator {
    /// Create a new NFM demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying FM demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = FmDemod::from_hz(NFM_DEVIATION_HZ, NFM_IF_SAMPLE_RATE)?;
        let audio_lpf = match build_nfm_lpf_taps(NFM_DEFAULT_BANDWIDTH)? {
            Some(taps) => FirFilter::new(taps)?,
            None => FirFilter::new(NFM_PASSTHROUGH_TAPS.to_vec())?, // passthrough
        };
        let audio_agc = Agc::new(
            NFM_AGC_SET_POINT,
            NFM_AGC_ATTACK,
            NFM_AGC_DECAY,
            NFM_AGC_MAX_GAIN,
            NFM_AGC_MAX_OUTPUT,
            NFM_AGC_INIT_GAIN,
        )?;
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
            default_deemp_tau: DEEMPHASIS_TAU_US,
            fm_if_nr_allowed: true,
            nb_allowed: false,
            high_pass_allowed: true,
            squelch_allowed: true,
        };
        Ok(Self {
            demod,
            audio_lpf,
            audio_agc,
            config,
            mono_buf: Vec::new(),
            lpf_buf: Vec::new(),
            agc_buf: Vec::new(),
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

        // Audio-level AGC — normalizes output loudness across
        // stations with different FM deviations. Closes the FM
        // side of the "audio distortion with AGC on" bug (#332)
        // by removing the level dependence on RF input strength,
        // so the tuner-side AGC's imperfect RF gain tracking no
        // longer propagates into audible distortion.
        self.agc_buf.resize(count, 0.0);
        self.audio_agc
            .process_f32(&self.lpf_buf[..count], &mut self.agc_buf[..count])?;

        sdr_dsp::convert::mono_to_stereo(&self.agc_buf[..count], &mut output[..count])?;
        Ok(count)
    }

    fn set_bandwidth(&mut self, bw: f64) {
        if !bw.is_finite() || !(NFM_MIN_BANDWIDTH..=NFM_MAX_BANDWIDTH).contains(&bw) {
            tracing::warn!(
                "NFM: ignoring invalid bandwidth {bw} Hz (expected {NFM_MIN_BANDWIDTH}..={NFM_MAX_BANDWIDTH} Hz)"
            );
            return;
        }
        // Stage LPF taps before committing — avoids half-retuned state.
        let new_taps = match build_nfm_lpf_taps(bw) {
            Ok(Some(taps)) => taps,
            Ok(None) => NFM_PASSTHROUGH_TAPS.to_vec(),
            Err(e) => {
                tracing::warn!("NFM: set_bandwidth({bw}) LPF failed: {e}");
                return;
            }
        };

        // Update deviation in-place (preserves phase state — no transient pop)
        if let Err(e) = self.demod.set_deviation_hz(bw / 2.0, NFM_IF_SAMPLE_RATE) {
            tracing::warn!("NFM: set_bandwidth({bw}) demod failed: {e}");
            return;
        }
        if let Err(e) = self.audio_lpf.set_taps(new_taps) {
            tracing::warn!("NFM: set_bandwidth({bw}) set_taps failed: {e}");
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
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn test_nfm_config() {
        let demod = NfmDemodulator::new().unwrap();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 50_000.0).abs() < f64::EPSILON);
        assert!((cfg.default_bandwidth - 12_500.0).abs() < f64::EPSILON);
        assert!((cfg.max_bandwidth - NFM_MAX_BANDWIDTH).abs() < f64::EPSILON);
        assert!((cfg.default_snap_interval - NFM_SNAP_INTERVAL).abs() < f64::EPSILON);
        assert!(cfg.fm_if_nr_allowed);
        assert!(cfg.squelch_allowed);
        assert!(cfg.deemp_allowed);
        assert!(
            cfg.default_deemp_tau > 0.0,
            "NFM should default to active deemphasis"
        );
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
        // Compare filtered NFM output against an unfiltered baseline to verify
        // the LPF actually reduces high-frequency jumps.
        let input: Vec<Complex> = (0..2000)
            .map(|i| {
                if i % 2 == 0 {
                    Complex::new(1.0, 0.0)
                } else {
                    Complex::new(0.0, 1.0)
                }
            })
            .collect();

        // Baseline: raw FM discriminator (no LPF)
        let mut raw_demod =
            sdr_dsp::demod::FmDemod::from_hz(NFM_DEVIATION_HZ, NFM_IF_SAMPLE_RATE).unwrap();
        let mut raw_buf = vec![0.0_f32; 2000];
        raw_demod.process(&input, &mut raw_buf).unwrap();
        let baseline_jump = raw_buf[500..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0_f32, f32::max);

        // Filtered: full NFM demod with LPF
        let mut demod = NfmDemodulator::new().unwrap();
        let mut output = vec![Stereo::default(); 2000];
        demod.process(&input, &mut output).unwrap();
        let filtered_jump = output[500..]
            .windows(2)
            .map(|w| (w[1].l - w[0].l).abs())
            .fold(0.0_f32, f32::max);

        // LPF should meaningfully reduce jumps compared to raw discriminator
        assert!(
            filtered_jump < baseline_jump * 0.8,
            "LPF should reduce jumps: filtered={filtered_jump}, baseline={baseline_jump}"
        );
    }

    #[test]
    fn test_nfm_audio_agc_normalizes_across_deviations() {
        // Two FM signals with very different deviations should
        // produce similar audio levels after the audio AGC
        // converges. Without AGC, a ±2 kHz-deviation signal
        // would be audibly quieter than a ±5 kHz one even though
        // the RF power is the same — the bug that #332 closes.
        let settle = 2000;
        let n = 4000;
        let mod_freq = 1_000.0_f32;

        let mut peaks = Vec::new();
        for &deviation_hz in &[2_000.0_f32, 5_000.0_f32] {
            let mut demod = NfmDemodulator::new().unwrap();
            let input: Vec<Complex> = (0..n)
                .map(|i| {
                    let t = i as f32 / NFM_IF_SAMPLE_RATE as f32;
                    // Integrated sinusoidal modulation: phase(t) =
                    // deviation * sin(2π·mod_freq·t) / mod_freq.
                    let phase = deviation_hz * (2.0 * PI * mod_freq * t).sin() / mod_freq;
                    Complex::new(phase.cos(), phase.sin())
                })
                .collect();
            let mut output = vec![Stereo::default(); n];
            demod.process(&input, &mut output).unwrap();
            let peak = output[settle..]
                .iter()
                .map(|s| s.l.abs())
                .fold(0.0_f32, f32::max);
            peaks.push(peak);
        }

        // Post-AGC peak ratio should be bounded. Without AGC the
        // no-AGC baseline matches the deviation ratio exactly —
        // 5000/2000 = 2.5× — so the assertion bound must sit
        // BELOW 2.5 for the test to actually catch an AGC bypass.
        // A ratio of 2.0 leaves ~20% margin for the envelope
        // follower's finite settling time at the 2000-sample
        // mark while still failing decisively if the AGC stage
        // is removed or short-circuited.
        let ratio = if peaks[0] > peaks[1] {
            peaks[0] / peaks[1].max(1e-10)
        } else {
            peaks[1] / peaks[0].max(1e-10)
        };
        assert!(
            ratio < 2.0,
            "audio AGC should normalize across FM deviations, peaks = {peaks:?}, ratio = {ratio}"
        );
    }

    #[test]
    fn test_nfm_set_bandwidth_continuity() {
        // Verify mid-stream bandwidth retune doesn't cause a boundary pop.
        // Process a steady FM tone, retune mid-stream, check the first
        // post-retune sample stays close to its neighbors.
        let mut demod = NfmDemodulator::new().unwrap();
        let freq = 0.05_f32;
        let n = 2000;
        let input: Vec<Complex> = (0..n)
            .map(|i| {
                let phase = freq * i as f32;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();

        // Process first half
        let mut out1 = vec![Stereo::default(); 1000];
        demod.process(&input[..1000], &mut out1).unwrap();
        let pre_retune = out1[999].l;

        // Retune mid-stream
        demod.set_bandwidth(25_000.0);

        // Process second half
        let mut out2 = vec![Stereo::default(); 1000];
        demod.process(&input[1000..], &mut out2).unwrap();
        let post_retune = out2[0].l;

        // The boundary should be smooth — no large transient pop
        let jump = (post_retune - pre_retune).abs();
        assert!(
            jump < 1.0,
            "retune boundary should be smooth, jump = {jump}"
        );

        // Also verify passthrough path at max bandwidth
        demod.set_bandwidth(NFM_MAX_BANDWIDTH);
    }
}
