//! Meteor-M LRPT receive "demodulator" — at this layer it's a
//! passthrough that produces silent stereo audio. The actual QPSK
//! demod + FEC chain lives in `sdr_dsp::lrpt::LrptDemod` +
//! `sdr_lrpt::LrptPipeline`, driven by the controller's LRPT tap
//! against the post-VFO IQ buffer (which is at this mode's IF
//! rate of 144 ksps — the LRPT working rate per
//! `sdr_dsp::lrpt::SAMPLE_RATE_HZ`).
//!
//! Why a passthrough demod here at all:
//!
//! 1. The `RadioModule` is the single source of truth for
//!    "current IF sample rate". Adding `DemodMode::Lrpt` as a
//!    real demod variant lets the existing VFO + IF-chain
//!    plumbing (resample to `if_sample_rate`, pin the channel
//!    bandwidth at 144 kHz) just work — no bypass plumbing in
//!    the controller, no parallel path that has to track
//!    sample-rate changes.
//! 2. The LRPT tap reads `radio_input` (the post-VFO IQ slice
//!    fed to `RadioModule::process`) BEFORE this passthrough
//!    runs. So `radio_input` is already at 144 ksps thanks to
//!    the VFO; the QPSK demod gets the right rate; the
//!    "demod" here just produces zero audio because there is
//!    no listenable signal mid-pass — the imagery is the
//!    artifact.
//! 3. Squelch / NB / FM-IF-NR / deemphasis / high-pass /
//!    voice-squelch are all disabled (`*_allowed: false`)
//!    because none of them apply to a QPSK signal — applying
//!    them would risk shaping the IQ before the LRPT tap reads
//!    it (which would corrupt the carrier).

use sdr_types::{Complex, DspError, Stereo};

use super::{DemodConfig, Demodulator, VfoReference};

/// LRPT working sample rate (Hz). Matches
/// `sdr_dsp::lrpt::SAMPLE_RATE_HZ` exactly — pinned here as a
/// `f64` so it slots into the `DemodConfig` numeric type without
/// an extra cast at every read site. A `const_assert!` below
/// catches drift if the DSP-layer constant ever changes.
const LRPT_IF_SAMPLE_RATE: f64 = 144_000.0;
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const _: () = assert!(LRPT_IF_SAMPLE_RATE as u32 == sdr_dsp::lrpt::SAMPLE_RATE_HZ as u32);

/// AF (audio) sample rate produced by the passthrough. Matches
/// the IF rate so the AF chain doesn't resample (and the silent
/// audio buffer doesn't grow).
const LRPT_AF_SAMPLE_RATE: f64 = LRPT_IF_SAMPLE_RATE;

/// Default channel bandwidth for LRPT (Hz). Wider than NFM's
/// 38 kHz default so the QPSK sidebands aren't clipped by the
/// VFO's channel filter. Pinned at the IF rate; the bandwidth
/// row is locked because LRPT is a fixed-symbol-rate signal.
const LRPT_DEFAULT_BANDWIDTH: f64 = LRPT_IF_SAMPLE_RATE;

/// LRPT passthrough demodulator. Produces silent stereo audio;
/// the actual decoding happens in the controller's LRPT tap
/// against the post-VFO IQ buffer.
pub struct LrptDemodulator {
    config: DemodConfig,
}

impl LrptDemodulator {
    /// Create a new LRPT passthrough demodulator.
    #[must_use]
    pub fn new() -> Self {
        let config = DemodConfig {
            if_sample_rate: LRPT_IF_SAMPLE_RATE,
            af_sample_rate: LRPT_AF_SAMPLE_RATE,
            default_bandwidth: LRPT_DEFAULT_BANDWIDTH,
            min_bandwidth: LRPT_DEFAULT_BANDWIDTH,
            max_bandwidth: LRPT_DEFAULT_BANDWIDTH,
            bandwidth_locked: true,
            default_snap_interval: 0.0,
            vfo_reference: VfoReference::Center,
            deemp_allowed: false,
            post_proc_enabled: false,
            default_deemp_tau: 0.0,
            fm_if_nr_allowed: false,
            nb_allowed: false,
            high_pass_allowed: false,
            squelch_allowed: false,
        };
        Self { config }
    }
}

impl Default for LrptDemodulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Demodulator for LrptDemodulator {
    fn process(&mut self, input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
        // Silent output. The IQ at `input` was already harvested
        // by the controller's LRPT tap before this demod ran;
        // the AF chain downstream just gets zeros (which the
        // sink writes as silence). Length matches input so the
        // controller's `audio_count` accounting stays consistent
        // with non-LRPT modes.
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }
        for slot in output.iter_mut().take(input.len()) {
            *slot = Stereo::default();
        }
        Ok(input.len())
    }

    fn set_bandwidth(&mut self, _bw: f64) {
        // Bandwidth is locked in LRPT mode (fixed-symbol-rate signal).
    }

    fn config(&self) -> &DemodConfig {
        &self.config
    }

    fn name(&self) -> &'static str {
        "LRPT"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_lrpt_config_matches_dsp_constant() {
        let demod = LrptDemodulator::new();
        let cfg = demod.config();
        assert!((cfg.if_sample_rate - 144_000.0).abs() < f64::EPSILON);
        assert!((cfg.af_sample_rate - cfg.if_sample_rate).abs() < f64::EPSILON);
        assert!(cfg.bandwidth_locked);
        assert!(!cfg.post_proc_enabled);
        // None of the voice-mode features apply to a QPSK signal —
        // make sure the config gates them off so a future "enable
        // squelch on every demod" refactor can't accidentally
        // shape the IQ before the LRPT tap reads it.
        assert!(!cfg.fm_if_nr_allowed);
        assert!(!cfg.nb_allowed);
        assert!(!cfg.high_pass_allowed);
        assert!(!cfg.squelch_allowed);
        assert!(!cfg.deemp_allowed);
    }

    #[test]
    fn test_lrpt_produces_silent_stereo() {
        let mut demod = LrptDemodulator::new();
        let input = [
            Complex::new(0.5, -0.3),
            Complex::new(-1.0, 0.7),
            Complex::new(0.0, 0.0),
        ];
        let mut output = [Stereo::default(); 3];
        let count = demod.process(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        // All output is silent — the LRPT tap consumes the IQ
        // upstream; the AF chain has nothing to emit.
        for s in &output {
            assert_eq!(s.l, 0.0);
            assert_eq!(s.r, 0.0);
        }
    }

    #[test]
    fn test_lrpt_rejects_undersized_output() {
        let mut demod = LrptDemodulator::new();
        let input = [Complex::new(1.0, 0.0); 4];
        let mut output = [Stereo::default(); 2];
        let err = demod.process(&input, &mut output);
        assert!(matches!(
            err,
            Err(DspError::BufferTooSmall { need: 4, got: 2 })
        ));
    }

    #[test]
    fn test_lrpt_set_bandwidth_is_no_op() {
        // Locked-bandwidth contract: set_bandwidth doesn't error
        // (so callers don't have to special-case LRPT) but
        // doesn't change anything either.
        let mut demod = LrptDemodulator::new();
        let before = demod.config().default_bandwidth;
        demod.set_bandwidth(38_000.0);
        let after = demod.config().default_bandwidth;
        assert_eq!(before, after);
    }
}
