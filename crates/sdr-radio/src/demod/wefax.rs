//! WEFAX (radiofax) demodulator.
//!
//! WEFAX is transmitted as an FM-subcarrier signal riding on an
//! upper-sideband channel: the fax modulator drives a 1500-2300 Hz
//! audio subcarrier whose instantaneous frequency encodes pixel
//! brightness (black = 1500 Hz, white = 2300 Hz), and that
//! subcarrier is what actually gets demodulated into the fax
//! image by the (later) decode tap. At the SSB-demod layer this
//! is just USB — the same `SsbDemod(Usb)` primitive `usb.rs`
//! uses — so this module delegates to it rather than
//! reimplementing SSB math.
//!
//! Unlike plain USB, the passband is locked: the fax subcarrier
//! occupies a fixed, known slice of the audio band and letting
//! the user drag the channel filter would clip the 1500-2300 Hz
//! tones the decoder depends on. Mirrors the `bandwidth_locked`
//! pattern in `lrpt.rs`.

use sdr_dsp::channel::FrequencyXlator;
use sdr_dsp::demod::{SsbDemod, SsbMode};
use sdr_dsp::loops::Agc;
use sdr_dsp::wefax::SUBCARRIER_CENTER_HZ;
use sdr_types::{Complex, DspError, Stereo};

use super::{
    DemodConfig, Demodulator, SSB_AGC_ATTACK, SSB_AGC_DECAY, SSB_AGC_INIT_GAIN, SSB_AGC_MAX_GAIN,
    SSB_AGC_MAX_OUTPUT, SSB_AGC_SET_POINT, VfoReference,
};

/// IF sample rate for WEFAX mode (Hz).
const WEFAX_IF_SAMPLE_RATE: f64 = 24_000.0;

/// AF (audio) sample rate produced by WEFAX demod (Hz).
const WEFAX_AF_SAMPLE_RATE: f64 = 24_000.0;

/// Default (and only, since the passband is locked) channel
/// bandwidth for WEFAX (Hz). Wide enough to comfortably pass the
/// 1500-2300 Hz fax subcarrier band with margin on both edges.
const WEFAX_DEFAULT_BANDWIDTH: f64 = 2_400.0;

/// Default frequency snap interval for WEFAX (Hz). Matches the
/// USB convention — fax stations are tuned/published on round
/// dial frequencies.
const WEFAX_SNAP_INTERVAL: f64 = 100.0;

/// Pre-translation applied to the IF so tuning to the **published** fax
/// frequency lands the subcarrier center at the decoder's expected
/// [`SUBCARRIER_CENTER_HZ`] (1900 Hz), rather than requiring the operator
/// to hand-tune the dial-offset convention (issue #911).
///
/// The published/assigned fax frequency is the subcarrier center in RF,
/// so tuning to it places the subcarrier at IF DC (0 Hz). `SsbDemod(Usb)`
/// then translates by `+bandwidth/2` (= `WEFAX_DEFAULT_BANDWIDTH/2` =
/// 1200 Hz), which alone would leave the subcarrier at 1200 Hz AF — 700 Hz
/// short of the decoder's 1900 Hz. This extra `+700 Hz` pre-shift closes
/// that gap: IF DC → +700 (here) → +1900 (after the SSB translation). Any
/// residual station/tuning error within a few hundred Hz is absorbed by
/// the decoder's AFC.
const WEFAX_SUBCARRIER_OFFSET: f64 = SUBCARRIER_CENTER_HZ - WEFAX_DEFAULT_BANDWIDTH / 2.0;

/// WEFAX (radiofax) demodulator. A USB demod (`SsbDemod(Usb)`)
/// with a locked passband sized to the fax subcarrier band.
pub struct WefaxDemodulator {
    /// Pre-translates the IF by [`WEFAX_SUBCARRIER_OFFSET`] so the
    /// published-frequency subcarrier lands at 1900 Hz AF (#911).
    xlator: FrequencyXlator,
    xlator_buf: Vec<Complex>,
    demod: SsbDemod,
    agc: Agc,
    config: DemodConfig,
    mono_buf: Vec<f32>,
    agc_buf: Vec<f32>,
}

impl WefaxDemodulator {
    /// Create a new WEFAX demodulator.
    ///
    /// # Errors
    ///
    /// Returns `DspError` if the underlying SSB demod cannot be created.
    pub fn new() -> Result<Self, DspError> {
        let demod = SsbDemod::new(SsbMode::Usb, WEFAX_DEFAULT_BANDWIDTH, WEFAX_IF_SAMPLE_RATE)?;
        let agc = Agc::new(
            SSB_AGC_SET_POINT,
            SSB_AGC_ATTACK,
            SSB_AGC_DECAY,
            SSB_AGC_MAX_GAIN,
            SSB_AGC_MAX_OUTPUT,
            SSB_AGC_INIT_GAIN,
        )?;
        let config = DemodConfig {
            if_sample_rate: WEFAX_IF_SAMPLE_RATE,
            af_sample_rate: WEFAX_AF_SAMPLE_RATE,
            default_bandwidth: WEFAX_DEFAULT_BANDWIDTH,
            min_bandwidth: WEFAX_DEFAULT_BANDWIDTH,
            max_bandwidth: WEFAX_DEFAULT_BANDWIDTH,
            bandwidth_locked: true,
            default_snap_interval: WEFAX_SNAP_INTERVAL,
            vfo_reference: VfoReference::Lower,
            deemp_allowed: false,
            if_agc_allowed: true,
            fm_if_nr_allowed: false,
            nb_allowed: true,
            high_pass_allowed: false,
            squelch_allowed: false,
        };
        let xlator = FrequencyXlator::from_hz(WEFAX_SUBCARRIER_OFFSET, WEFAX_IF_SAMPLE_RATE);
        Ok(Self {
            xlator,
            xlator_buf: Vec::new(),
            demod,
            agc,
            config,
            mono_buf: Vec::new(),
            agc_buf: Vec::new(),
        })
    }
}

impl Demodulator for WefaxDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        // Shift the IF so the published-frequency subcarrier (at IF DC)
        // reaches the decoder's 1900 Hz center after the SSB demod (#911).
        self.xlator_buf.resize(input.len(), Complex::default());
        self.xlator.process(input, &mut self.xlator_buf)?;
        self.mono_buf.resize(input.len(), 0.0);
        let count = self.demod.process(&self.xlator_buf, &mut self.mono_buf)?;
        super::process_with_agc_to_stereo(
            &mut self.agc,
            &self.mono_buf[..count],
            &mut self.agc_buf,
            &mut output[..count],
        )
    }

    fn set_bandwidth(&mut self, _bw: f64) {
        // Bandwidth is locked in WEFAX mode — the fax subcarrier
        // band is fixed, so there's nothing sensible for the user
        // to tune here (mirrors LRPT's no-op).
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "WEFAX"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss, clippy::float_cmp)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn wefax_mode_uses_locked_usb_passband() {
        let demod = WefaxDemodulator::new().unwrap();
        let cfg = demod.config();
        assert_eq!(cfg.af_sample_rate, 24_000.0);
        assert!(cfg.bandwidth_locked);
        assert_eq!(cfg.vfo_reference, VfoReference::Lower);
        // Locked passband must comfortably pass the 1500-2300 Hz
        // fax subcarrier band on both edges.
        assert!(cfg.default_bandwidth > 2_300.0);
    }

    #[test]
    fn wefax_set_bandwidth_is_no_op() {
        let mut demod = WefaxDemodulator::new().unwrap();
        let before = demod.config().default_bandwidth;
        demod.set_bandwidth(5_000.0);
        assert_eq!(demod.config().default_bandwidth, before);
    }

    #[test]
    fn wefax_process_produces_audio() {
        let mut demod = WefaxDemodulator::new().unwrap();
        let input: Vec<Complex> = (0..1000)
            .map(|i| {
                let phase = 2.0 * PI * 1900.0 * (i as f32) / 24_000.0;
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
        assert!(peak > 0.3, "WEFAX should produce audio, peak = {peak}");
    }

    /// Estimate the dominant frequency of a real AF buffer by counting
    /// zero crossings. Adequate for a clean single tone.
    fn dominant_hz(af: &[f32], rate: f64) -> f64 {
        let crossings = af
            .windows(2)
            .filter(|w| (w[0] <= 0.0 && w[1] > 0.0) || (w[0] >= 0.0 && w[1] < 0.0))
            .count();
        // full cycles = crossings / 2
        (crossings as f64 / 2.0) / (af.len() as f64 / rate)
    }

    #[test]
    fn subcarrier_at_published_freq_lands_at_1900hz() {
        // The published/assigned fax frequency = the subcarrier CENTER in
        // RF, so tuning to it puts the subcarrier at the IF DC (0 Hz). The
        // decoder expects that center at SUBCARRIER_CENTER_HZ (1900 Hz), so
        // the demod MUST map IF DC -> 1900 Hz AF. (Issue #911.)
        let mut demod = WefaxDemodulator::new().unwrap();
        // A DC complex input = a tone sitting exactly at the tuned freq.
        let input = vec![Complex::new(1.0, 0.0); 24_000];
        let mut output = vec![Stereo::default(); 24_000];
        let count = demod.process(&input, &mut output).unwrap();
        let af: Vec<f32> = output[2_000..count].iter().map(|s| s.l).collect();
        let hz = dominant_hz(&af, WEFAX_AF_SAMPLE_RATE);
        assert!(
            (hz - 1_900.0).abs() < 120.0,
            "IF-DC (published freq) must land at ~1900 Hz AF, got {hz:.0} Hz"
        );
    }
}
