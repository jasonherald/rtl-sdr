//! Radio decoder — demodulator selection, IF/AF chains, mode switching.
//!
//! This crate sits between the IQ pipeline and audio output. It applies
//! IF processing (noise blanker, squelch), demodulation, and AF processing
//! (deemphasis, resampling) to convert complex IQ samples into stereo audio.

pub mod af_chain;
pub mod apt_image;
pub mod apt_telemetry;
pub mod demod;
pub mod if_chain;
pub mod lrpt_decoder;
pub mod lrpt_image;
pub mod sstv_image;

use sdr_dsp::filter::{DEEMPHASIS_TAU_EU, DEEMPHASIS_TAU_US};
use sdr_dsp::multirate::RationalResampler;
use sdr_types::{Complex, DemodMode, DspError, Stereo};

use af_chain::{AfChain, CtcssMode};
use demod::{DemodConfig, Demodulator, create_demodulator};
use sdr_dsp::voice_squelch::VoiceSquelchMode;

/// Tolerance for considering two sample rates equal (skip resampling).
const RATE_TOLERANCE: f64 = 1.0;
use if_chain::IfChain;

/// Default audio output sample rate (Hz).
const DEFAULT_AUDIO_SAMPLE_RATE: f64 = 48_000.0;

/// Diagnostic stage-amplitude log cadence, in seconds of input data.
/// `process()` derives a per-call sample threshold from this and the
/// active input sample rate so the cadence is wall-clock consistent
/// across IF rates (NFM ~50 kHz, WFM ~200 kHz, LRPT ~144 kHz, etc.).
const STAGE_AMP_LOG_PERIOD_SECS: f64 = 1.0;

/// Mean of `|c|` across a complex slice, used by `STAGE_AMP_DUMP`.
/// Returns 0.0 on empty input. Pure observational helper.
fn mean_abs_complex(s: &[Complex]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let inv_len = 1.0 / s.len() as f32;
    s.iter()
        .map(|c| (c.re * c.re + c.im * c.im).sqrt())
        .sum::<f32>()
        * inv_len
}

/// Mean of `(|l| + |r|) / 2` across a stereo slice, used by `STAGE_AMP_DUMP`.
/// Returns 0.0 on empty input. Pure observational helper.
fn mean_abs_stereo(s: &[Stereo]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let inv_len = 1.0 / s.len() as f32;
    s.iter()
        .map(|t| f32::midpoint(t.l.abs(), t.r.abs()))
        .sum::<f32>()
        * inv_len
}

/// Deemphasis mode for FM broadcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeemphasisMode {
    /// US/Japan: 75 microsecond time constant.
    Us75,
    /// Europe/Australia: 50 microsecond time constant.
    Eu50,
    /// No deemphasis.
    None,
}

impl DeemphasisMode {
    /// Get the time constant in seconds for this mode, or 0.0 for None.
    pub fn tau(self) -> f64 {
        match self {
            Self::Us75 => DEEMPHASIS_TAU_US,
            Self::Eu50 => DEEMPHASIS_TAU_EU,
            Self::None => 0.0,
        }
    }
}

/// Errors from radio module operations.
#[derive(Debug, thiserror::Error)]
pub enum RadioError {
    /// A DSP processing error occurred.
    #[error("DSP error: {0}")]
    Dsp(#[from] DspError),

    /// The requested mode switch failed.
    #[error("mode switch failed: {0}")]
    ModeSwitchFailed(String),
}

/// Complete radio decoder module — IF chain, demodulator, AF chain.
///
/// Processes complex IQ samples through the full signal path:
/// 1. IF chain: noise blanker, squelch, FM IF NR
/// 2. Demodulator: mode-specific IQ-to-audio conversion
/// 3. AF chain: deemphasis, sample rate conversion to audio output rate
pub struct RadioModule {
    mode: DemodMode,
    demod: Box<dyn Demodulator + Send>,
    if_chain: IfChain,
    af_chain: AfChain,
    deemp_mode: DeemphasisMode,
    high_pass_enabled: bool,
    /// The user's software-IF-AGC preference. Applied to the IF chain
    /// only in modes whose config allows it (#738).
    software_agc_enabled: bool,
    notch_enabled: bool,
    notch_frequency: f32,
    /// Persisted CTCSS squelch mode. Reapplied to the new AF chain
    /// on mode switch (when the demod rate changes the AF chain is
    /// rebuilt from scratch, so the CTCSS state has to be restored
    /// the same way deemphasis / notch / high-pass are).
    ctcss_mode: CtcssMode,
    /// Persisted CTCSS detection threshold, paired with
    /// `ctcss_mode`. Same reapply-on-rebuild pattern.
    ctcss_threshold: f32,
    /// Persisted voice-activity squelch mode (Off / Syllabic /
    /// Snr). Reapplied to the new AF chain on mode switch the
    /// same way CTCSS is.
    voice_squelch_mode: VoiceSquelchMode,
    audio_sample_rate: f64,
    /// Input sample rate from the IQ frontend (Hz).
    input_sample_rate: f64,
    /// Resampler from input rate to demod IF rate (None if rates match).
    input_resampler: Option<RationalResampler>,
    /// Scratch buffer for IF chain output (complex, at input sample rate).
    if_buf: Vec<Complex>,
    /// Scratch buffer for resampled IQ (at demod IF rate).
    resamp_buf: Vec<Complex>,
    /// Scratch buffer for demod output (stereo, at AF sample rate).
    demod_buf: Vec<Stereo>,
    /// Per-sample attack / release envelope on the post-demod
    /// stereo output, driven by the IF-chain squelch's gate
    /// state. Lives in the AF path because FM discriminators are
    /// amplitude-invariant — an IQ-side envelope has zero effect
    /// on FM audio output. See #331 and the
    /// `SquelchAudioEnvelope` docstring for the full reasoning.
    squelch_envelope: sdr_dsp::noise::SquelchAudioEnvelope,
    /// Sample-count accumulator for diagnostic stage-amplitude
    /// logging in `process()`. We log mean-abs of every stage in
    /// the chain (input IQ → IF chain → demod → AF) once every
    /// ~1 second of input data, so a `grep stage_amp` of the log
    /// during a satellite pass tells us at which stage signal
    /// becomes flat noise. Pure diagnostic — no processing impact.
    /// Per silent-fail demod investigation following the May 2026
    /// NOAA APT regression.
    samples_since_last_amp_log: u64,
}

impl RadioModule {
    /// Create a new radio module with default NFM mode.
    ///
    /// - `audio_sample_rate`: target audio output rate (Hz), typically 48 kHz
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if initialization fails.
    pub fn new(audio_sample_rate: f64) -> Result<Self, RadioError> {
        let mode = DemodMode::Wfm;
        let demod = create_demodulator(mode)?;
        let mut if_chain = IfChain::new()?;
        // The power squelch only tracks the gate here; the exact-zero
        // mute is applied post-demod in `process` so the imaging taps
        // can read ungated audio via `pre_gate_audio` (#734).
        if_chain.set_squelch_mutes_iq(false);
        let af_chain = AfChain::new(demod.config().af_sample_rate, audio_sample_rate)?;
        // Audio-path envelope for smoothing squelch open / close
        // transitions. Runs at the final `audio_sample_rate`
        // because it's applied to the stereo output after AfChain's
        // resampler — see `process()` and #331's bug-fix notes for
        // why this has to live in the AF path and not at IF.
        // `SquelchAudioEnvelope::new` rejects non-finite /
        // non-positive rates; error propagates via `RadioError::Dsp`
        // rather than silently collapsing to the pre-envelope hard
        // gate it was meant to replace.
        #[allow(clippy::cast_possible_truncation)]
        let squelch_envelope = sdr_dsp::noise::SquelchAudioEnvelope::new(audio_sample_rate as f32)?;

        Ok(Self {
            mode,
            demod,
            if_chain,
            af_chain,
            deemp_mode: DeemphasisMode::None,
            software_agc_enabled: false,
            high_pass_enabled: false,
            notch_enabled: false,
            notch_frequency: sdr_dsp::filter::DEFAULT_NOTCH_FREQ_HZ,
            ctcss_mode: CtcssMode::Off,
            ctcss_threshold: sdr_dsp::tone_detect::CTCSS_DEFAULT_THRESHOLD,
            voice_squelch_mode: VoiceSquelchMode::Off,
            audio_sample_rate,
            input_sample_rate: 0.0,
            input_resampler: None,
            if_buf: Vec::new(),
            resamp_buf: Vec::new(),
            demod_buf: Vec::new(),
            squelch_envelope,
            samples_since_last_amp_log: 0,
        })
    }

    /// Create a new radio module with the default audio sample rate (48 kHz).
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if initialization fails.
    pub fn with_default_rate() -> Result<Self, RadioError> {
        Self::new(DEFAULT_AUDIO_SAMPLE_RATE)
    }

    /// Switch to a new demodulation mode.
    ///
    /// This reconfigures the demodulator, IF chain feature flags, and AF chain
    /// (including resampler) to match the new mode's requirements.
    ///
    /// IF chain features (noise blanker, squelch, FM IF NR) are **only disabled**
    /// when the new mode doesn't support them. They are not automatically
    /// re-enabled on mode switch, preserving the user's explicit disable choice.
    /// Call `set_squelch_enabled(true)` etc. to re-enable after switching.
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if the new demodulator or AF chain cannot be created.
    pub fn set_mode(&mut self, mode: DemodMode) -> Result<(), RadioError> {
        let new_demod = create_demodulator(mode).map_err(|e| {
            RadioError::ModeSwitchFailed(format!("failed to create demod for {mode:?}: {e}"))
        })?;

        // Extract config values before moving new_demod
        let af_rate = new_demod.config().af_sample_rate;
        let if_rate = new_demod.config().if_sample_rate;
        let deemp_allowed = new_demod.config().deemp_allowed;
        let fm_if_nr_allowed = new_demod.config().fm_if_nr_allowed;
        let nb_allowed = new_demod.config().nb_allowed;
        let squelch_allowed = new_demod.config().squelch_allowed;
        let high_pass_allowed = new_demod.config().high_pass_allowed;
        let if_agc_allowed = new_demod.config().if_agc_allowed;

        // Reconfigure AF chain for the new AF sample rate
        let new_af_chain = AfChain::new(af_rate, self.audio_sample_rate)
            .map_err(|e| RadioError::ModeSwitchFailed(format!("failed to create AF chain: {e}")))?;

        // Reapply persisted AF chain settings to the new chain
        let mut af_chain = new_af_chain;
        if deemp_allowed && self.deemp_mode != DeemphasisMode::None {
            af_chain
                .set_deemp_enabled(true, self.deemp_mode.tau())
                .map_err(|e| {
                    RadioError::ModeSwitchFailed(format!("failed to set deemphasis: {e}"))
                })?;
        }
        if self.high_pass_enabled && high_pass_allowed {
            af_chain.set_high_pass_enabled(true);
        }
        // Always restore notch frequency (even when disabled) so it's
        // correct when the user re-enables after a mode switch.
        af_chain.set_notch_frequency(self.notch_frequency);
        af_chain.set_notch_enabled(self.notch_enabled);
        // Restore CTCSS threshold FIRST so the detector built by
        // set_ctcss_mode picks it up instead of the default.
        // Sustained-gate state intentionally resets to closed on
        // mode switch — a new mode means the user retuned or
        // changed decode, and holding an old "tone confirmed"
        // latch across that transition would let stray audio
        // through before the detector re-confirmed on the new
        // signal. `set_ctcss_mode` rebuilds the detector from
        // scratch so this is the natural behavior.
        af_chain
            .set_ctcss_threshold(self.ctcss_threshold)
            .map_err(|e| {
                RadioError::ModeSwitchFailed(format!("failed to set CTCSS threshold: {e}"))
            })?;
        af_chain
            .set_ctcss_mode(self.ctcss_mode)
            .map_err(|e| RadioError::ModeSwitchFailed(format!("failed to set CTCSS mode: {e}")))?;
        // Voice squelch: apply LIVE only when the new mode is
        // NFM. Syllabic and Snr detectors are calibrated around
        // human speech shape — on WFM broadcast audio they'd
        // mangle music and non-speech content, on AM/SSB the
        // signal characteristics are different, and on DSB/CW/
        // RAW the concept doesn't apply at all. Rather than let
        // a stale cached Syllabic or Snr mode silently gate the
        // speaker on WFM, we force the live AF chain to Off for
        // any non-NFM mode.
        //
        // **Important**: we do NOT clear `self.voice_squelch_mode`
        // here — the cached setting is preserved across the
        // mode switch. When the user returns to NFM later, the
        // cached mode is reapplied live and their tuning
        // survives. This is a cleaner model than the
        // force-clear-on-leave pattern CTCSS uses: the user's
        // voice squelch configuration is "armed" and will
        // automatically re-engage on NFM without requiring
        // manual reselection.
        //
        // The UI layer's `apply_demod_visibility` hides the
        // voice squelch rows on non-NFM modes (same as CTCSS),
        // so the user never sees controls for a feature that
        // isn't currently live. Their cached selection is
        // preserved in the combo row as well so the roundtrip
        // is seamless.
        let live_voice_squelch_mode = if mode == DemodMode::Nfm {
            self.voice_squelch_mode
        } else {
            VoiceSquelchMode::Off
        };
        af_chain
            .set_voice_squelch_mode(live_voice_squelch_mode)
            .map_err(|e| {
                RadioError::ModeSwitchFailed(format!("failed to set voice squelch mode: {e}"))
            })?;

        // Update IF chain feature flags based on new mode capabilities
        if !fm_if_nr_allowed {
            self.if_chain.set_fm_if_nr_enabled(false);
        }
        if !nb_allowed {
            self.if_chain.set_nb_enabled(false);
        }
        if !squelch_allowed {
            self.if_chain.set_squelch_enabled(false);
        }
        // The software IF AGC follows the user's preference only in
        // modes that allow it — Raw / LRPT / CW hand the IQ through
        // with its amplitude intact (#738).
        self.if_chain
            .set_software_agc_enabled(self.software_agc_enabled && if_agc_allowed);
        // Reset the AF squelch envelope so stale gain state from
        // the previous mode's audio pipeline doesn't bleed into
        // the first block on the new mode. Envelope coefficients
        // stay valid — audio output rate is fixed at construction
        // and the AfChain resampler normalizes all modes to it.
        self.squelch_envelope.reset();

        self.mode = mode;
        self.demod = new_demod;
        self.af_chain = af_chain;

        // Rebuild input resampler for the new demod's IF rate
        if self.input_sample_rate > 0.0 {
            if (self.input_sample_rate - if_rate).abs() < RATE_TOLERANCE {
                self.input_resampler = None;
            } else {
                self.input_resampler = Some(
                    RationalResampler::new(self.input_sample_rate, if_rate).map_err(|e| {
                        RadioError::ModeSwitchFailed(format!("input resampler: {e}"))
                    })?,
                );
            }
        }

        tracing::debug!("switched to mode {:?}", mode);
        Ok(())
    }

    /// Estimate the maximum output sample count for a given input count.
    ///
    /// Use this to size the `output` buffer before calling [`process()`](Self::process).
    /// Accounts for the AF chain resampling ratio (e.g., CW 3kHz → 48kHz = 16x).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn max_output_samples(&self, input_count: usize) -> usize {
        let cfg = self.demod.config();
        // Account for input resampling (input_rate → IF rate) + AF resampling (AF rate → audio rate)
        let input_ratio = if self.input_sample_rate > 0.0 {
            (cfg.if_sample_rate / self.input_sample_rate).max(1.0)
        } else {
            1.0
        };
        let af_ratio = (self.audio_sample_rate / cfg.af_sample_rate).ceil() as usize;
        #[allow(clippy::cast_precision_loss)]
        let resampled_input = ((input_count as f64) * input_ratio).ceil() as usize + 16;
        resampled_input * af_ratio.max(1) + 16
    }

    /// Process complex IQ samples through the full radio chain.
    ///
    /// Returns the number of stereo audio samples written to `output`.
    /// The output count may differ from `input.len()` due to AF resampling.
    ///
    /// Callers must size `output` using [`max_output_samples()`](Self::max_output_samples)
    /// to accommodate upsampling (e.g., CW 3kHz → 48kHz produces ~16x more samples).
    ///
    /// # Errors
    ///
    /// Returns `RadioError` on processing errors.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn process(
        &mut self,
        input: &[Complex],
        output: &mut [Stereo],
    ) -> Result<usize, RadioError> {
        if input.is_empty() {
            // Nothing produced: `pre_gate_audio` must not hand back the
            // previous block as if it were this call's output.
            self.af_chain.clear_ungated_output();
            return Ok(0);
        }

        let n = input.len();

        // Stage 1: IF chain
        self.if_buf.resize(n, Complex::default());
        self.if_chain.process(input, &mut self.if_buf)?;

        // Stage 1.5: Resample from input rate to demod IF rate (if needed)
        let demod_input = if let Some(resampler) = &mut self.input_resampler {
            // Estimate output size: input * (if_rate / input_rate) + padding
            let if_rate = self.demod.config().if_sample_rate;
            let ratio = if_rate / self.input_sample_rate;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let est_out = ((n as f64) * ratio).ceil() as usize + 16;
            self.resamp_buf.resize(est_out, Complex::default());
            resampler.process(&self.if_buf[..n], &mut self.resamp_buf)?
        } else {
            n
        };

        let demod_src = if self.input_resampler.is_some() {
            &self.resamp_buf[..demod_input]
        } else {
            &self.if_buf[..n]
        };

        // Stage 2: Demodulation
        self.demod_buf.resize(demod_input, Stereo::default());
        let demod_count = self.demod.process(demod_src, &mut self.demod_buf)?;

        // Stage 3: AF chain (deemphasis + resampling). It snapshots the
        // block before its own CTCSS / voice-squelch mute; with the
        // power-squelch mute moved here (below) that snapshot is fully
        // ungated — see `pre_gate_audio` (#734).
        let af_count = self
            .af_chain
            .process(&self.demod_buf[..demod_count], output)?;

        // Diagnostic dump preparation: snapshot the pre-envelope AF
        // amplitude before Stage 4 mutates `output`. The envelope can
        // mute audio during a closed squelch (deliberately), so a dump
        // taken AFTER Stage 4 alone would mispoint the failure stage
        // for silent-chain debugging. We keep both: pre-envelope tells
        // us what the demod produced; post-envelope tells us what the
        // listener actually heard. Per CR round 1 finding on this PR.
        // Pure observational; arithmetic only fires on log ticks.
        self.samples_since_last_amp_log = self.samples_since_last_amp_log.saturating_add(n as u64);
        // Threshold derived from the active input sample rate (fallback
        // to demod IF rate) so the cadence is wall-clock consistent
        // across modes. Without this, a fixed 100k-sample threshold
        // fires every ~0.5 sec at NFM and every ~0.7 sec at LRPT —
        // close enough to be confusing.
        let log_rate_hz = if self.input_sample_rate > 0.0 {
            self.input_sample_rate
        } else {
            self.demod.config().if_sample_rate
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let log_threshold = (log_rate_hz * STAGE_AMP_LOG_PERIOD_SECS).max(1.0).round() as u64;
        let should_dump = self.samples_since_last_amp_log >= log_threshold;
        // Snapshot pre-envelope AF here (output is still demod-as-written).
        let pre_envelope_af = if should_dump {
            mean_abs_stereo(&output[..af_count])
        } else {
            0.0
        };

        // Stage 4: Audio squelch envelope — only when the user
        // has actually enabled squelch (manual or auto). Running
        // the envelope unconditionally would mute the first
        // ~2 ms of every block while the envelope ramps up from
        // 0, even when there's no gate to open in the first
        // place — a regression on modes like Raw that disable
        // squelch entirely.
        //
        // When active: drives target gain from the IF-chain's
        // block-level gate state and ramps the stereo output
        // per-sample toward that target, smoothing the step
        // discontinuity that otherwise produces loud gate-
        // transition pops (particularly on macOS CoreAudio's
        // internal resampler — see #331).
        //
        // Why here and not at IF: FM discriminators compute
        // `atan2(phase delta)` which is amplitude-invariant, so
        // scaling IQ magnitude has zero effect on FM audio.
        // Applying the envelope to post-demod audio works for
        // all modulation types uniformly.
        //
        // Close edge (#738): the envelope runs on the real demod output
        // so the close is a release ramp rather than a step — the old
        // order hard-zeroed the block first, leaving nothing to ramp,
        // so only the open edge was ever smoothed. Once the release has
        // settled (checked at the *start* of a block, so the block that
        // crosses the threshold keeps its ramp) the block is hard-muted
        // to exact silence instead of multiplied by a vanishing gain.
        // This used to be the IF chain zeroing IQ before the demod
        // (SDR++ behaviour); it moved after the demod so the imaging
        // taps get real audio (#734). FM discriminators are
        // amplitude-invariant, so the result at the speaker is
        // otherwise identical.
        if self.if_chain.squelch_active() {
            let open = self.if_chain.squelch_open();
            self.squelch_envelope.set_gate_open(open);
            if !open && self.squelch_envelope.settle_if_closed() {
                output[..af_count].fill(Stereo::default());
            } else {
                self.squelch_envelope
                    .process_stereo(&mut output[..af_count]);
            }
        } else {
            // Keep the envelope state coherent for when the user
            // re-enables squelch mid-session — force it to fully-
            // open so the first block post-enable doesn't fade in
            // from silence.
            self.squelch_envelope.reset_to_open();
        }

        // Diagnostic dump emission. Now that Stage 4 has run, we can
        // measure the true post-envelope AF that the listener actually
        // hears. Both `af_out_pre_envelope` and `af_out_post_envelope`
        // are emitted: a divergence between them isolates the squelch
        // envelope as the silencer (vs. a real demod-chain failure).
        // Subtract `log_threshold` instead of zeroing so cadence drift
        // doesn't compound over long-running passes. Per CR round 1.
        if should_dump {
            self.samples_since_last_amp_log = self
                .samples_since_last_amp_log
                .saturating_sub(log_threshold);
            let input_amp = mean_abs_complex(&input[..n]);
            let if_amp = mean_abs_complex(&self.if_buf[..n]);
            let demod_input_amp = mean_abs_complex(demod_src);
            let demod_output_amp = mean_abs_stereo(&self.demod_buf[..demod_count]);
            let post_envelope_af = mean_abs_stereo(&output[..af_count]);
            tracing::info!(
                target: "stage_amp",
                input_iq = format!("{input_amp:.5}"),
                if_chain_out = format!("{if_amp:.5}"),
                demod_in = format!("{demod_input_amp:.5}"),
                demod_out_audio = format!("{demod_output_amp:.5}"),
                af_out_pre_envelope = format!("{pre_envelope_af:.5}"),
                af_out_post_envelope = format!("{post_envelope_af:.5}"),
                "STAGE_AMP_DUMP"
            );
        }

        Ok(af_count)
    }

    /// Set the input sample rate from the IQ frontend.
    ///
    /// This configures an internal resampler to convert from the actual
    /// input rate to the demod's expected IF sample rate. Call this whenever
    /// the frontend's effective sample rate changes (decimation, sample rate).
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if the resampler cannot be created.
    pub fn set_input_sample_rate(&mut self, rate: f64) -> Result<(), RadioError> {
        let if_rate = self.demod.config().if_sample_rate;
        let resampler = if (rate - if_rate).abs() < RATE_TOLERANCE {
            None
        } else {
            Some(RationalResampler::new(rate, if_rate).map_err(RadioError::Dsp)?)
        };
        // Commit state only after the resampler is successfully built.
        self.input_sample_rate = rate;
        self.input_resampler = resampler;
        Ok(())
    }

    /// Set the channel bandwidth.
    pub fn set_bandwidth(&mut self, bw: f64) {
        self.demod.set_bandwidth(bw);
    }

    /// Set the squelch threshold in dB.
    pub fn set_squelch(&mut self, level_db: f32) {
        self.if_chain.set_squelch_level(level_db);
    }

    /// Enable or disable the squelch.
    pub fn set_squelch_enabled(&mut self, enabled: bool) {
        self.if_chain.set_squelch_enabled(enabled);
    }

    /// Enable or disable auto-squelch (noise floor tracking).
    ///
    /// When enabled, the squelch threshold is automatically derived from
    /// the tracked noise floor with hysteresis. The manual squelch level
    /// is ignored while auto-squelch is active.
    pub fn set_auto_squelch_enabled(&mut self, enabled: bool) {
        self.if_chain.set_auto_squelch_enabled(enabled);
    }

    /// Re-arm auto-squelch noise-floor tracking so it re-converges
    /// at the current tuning context. No-op when auto-squelch is
    /// disabled. Call on every engine-side retune — frequency
    /// change, demod switch, bandwidth change — so the tracker
    /// doesn't carry a prior channel's settled floor into a new
    /// noise environment. Per issue #374.
    pub fn rearm_auto_squelch(&mut self) {
        self.if_chain.rearm_auto_squelch();
    }

    /// Set the deemphasis mode.
    ///
    /// # Errors
    ///
    /// Returns `RadioError` if the deemphasis filter cannot be created.
    pub fn set_deemp_mode(&mut self, mode: DeemphasisMode) -> Result<(), RadioError> {
        self.deemp_mode = mode;
        let cfg = self.demod.config();
        if cfg.deemp_allowed && mode != DeemphasisMode::None {
            self.af_chain.set_deemp_enabled(true, mode.tau())?;
        } else {
            self.af_chain.set_deemp_enabled(false, 0.0)?;
        }
        Ok(())
    }

    /// Enable or disable the audio high-pass filter.
    ///
    /// Persists across mode changes — reapplied when the AF chain is rebuilt.
    pub fn set_high_pass_enabled(&mut self, enabled: bool) {
        self.high_pass_enabled = enabled;
        self.af_chain.set_high_pass_enabled(enabled);
    }

    /// Enable or disable the audio notch filter.
    ///
    /// Persists across mode changes — reapplied when the AF chain is rebuilt.
    pub fn set_notch_enabled(&mut self, enabled: bool) {
        self.notch_enabled = enabled;
        self.af_chain.set_notch_enabled(enabled);
    }

    /// Set the audio notch filter frequency in Hz.
    ///
    /// Persists across mode changes — reapplied when the AF chain is rebuilt.
    pub fn set_notch_frequency(&mut self, freq: f32) {
        self.notch_frequency = freq;
        self.af_chain.set_notch_frequency(freq);
    }

    /// Set the CTCSS sub-audible tone squelch mode.
    ///
    /// `CtcssMode::Off` disables the detector and restores the
    /// user's explicit high-pass preference. `CtcssMode::Tone(hz)`
    /// validates `hz` against the standard 51-entry CTCSS table,
    /// constructs a fresh detector at the current audio rate, and
    /// force-enables the 300 Hz speaker-path high-pass filter so
    /// the user doesn't hear the sub-audible tone as a low buzz.
    ///
    /// Persists across mode changes — reapplied when the AF chain
    /// is rebuilt. See [`AfChain::set_ctcss_mode`] for details on
    /// the detector's window / hysteresis behavior.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the frequency isn't a known
    /// CTCSS tone or the detector constructor rejects it.
    pub fn set_ctcss_mode(&mut self, mode: CtcssMode) -> Result<(), RadioError> {
        self.af_chain.set_ctcss_mode(mode)?;
        self.ctcss_mode = mode;
        Ok(())
    }

    /// Returns the current CTCSS squelch mode.
    pub fn ctcss_mode(&self) -> CtcssMode {
        self.ctcss_mode
    }

    /// Returns the CTCSS sustained-gate state: `true` when the
    /// target tone has been confirmed present for at least
    /// [`sdr_dsp::tone_detect::CTCSS_MIN_HITS`] consecutive
    /// windows. Always `false` when CTCSS is `Off`.
    pub fn ctcss_sustained(&self) -> bool {
        self.af_chain.ctcss_sustained()
    }

    /// Set the CTCSS detection threshold (normalized magnitude
    /// ratio, `(0, 1]`). Default is
    /// [`sdr_dsp::tone_detect::CTCSS_DEFAULT_THRESHOLD`] (0.1).
    /// Persists across mode changes.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the value is non-finite or
    /// out of range.
    pub fn set_ctcss_threshold(&mut self, threshold: f32) -> Result<(), RadioError> {
        self.af_chain.set_ctcss_threshold(threshold)?;
        self.ctcss_threshold = threshold;
        Ok(())
    }

    /// Returns the current CTCSS detection threshold.
    pub fn ctcss_threshold(&self) -> f32 {
        self.ctcss_threshold
    }

    /// Set the voice-activity squelch mode. `Off` is the default
    /// (audio passes through unchanged). `Syllabic(threshold)` runs
    /// a ~4 Hz envelope-modulation detector for speech-cadence
    /// detection. `Snr(threshold_db)` runs a voice-band vs out-of-
    /// voice-band power ratio detector. Persists across mode
    /// changes.
    ///
    /// See [`sdr_dsp::voice_squelch`] for the underlying DSP.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the mode carries a non-
    /// finite or otherwise invalid threshold.
    pub fn set_voice_squelch_mode(&mut self, mode: VoiceSquelchMode) -> Result<(), RadioError> {
        // Same NFM-only invariant `set_mode` enforces: cache the
        // user's choice unconditionally, but only arm the detector
        // live on NFM. Bookmark recall dispatches the demod mode and
        // then the voice-squelch mode, so applying it live on WFM
        // muted broadcast audio with the control hidden (#737).
        //
        // Validate first: forcing `Off` live on non-NFM would otherwise
        // let an invalid threshold skip the DSP check, enter the cache
        // and fail on NFM re-entry (CR round 1 on PR #790).
        mode.validate()?;
        let live_mode = if self.mode == DemodMode::Nfm {
            mode
        } else {
            VoiceSquelchMode::Off
        };
        self.af_chain.set_voice_squelch_mode(live_mode)?;
        self.voice_squelch_mode = mode;
        Ok(())
    }

    /// Returns the current voice-squelch mode.
    pub fn voice_squelch_mode(&self) -> VoiceSquelchMode {
        self.voice_squelch_mode
    }

    /// Returns the voice-squelch gate state: `true` when the
    /// detector has opened (speech-like content present) or when
    /// the mode is `Off` (gate permanently open). `false` when
    /// an active detector has the gate closed.
    pub fn voice_squelch_open(&self) -> bool {
        self.af_chain.voice_squelch_open()
    }

    /// Update the voice-squelch threshold. The interpretation of
    /// `threshold` depends on the currently active mode: for
    /// `Syllabic` it's a normalized envelope-ratio value
    /// (positive, unitless), for `Snr` it's dB. No-op when the
    /// mode is `Off`.
    ///
    /// Updates the persisted mode's inline threshold so
    /// subsequent mode reloads (e.g. on `set_mode`) carry the
    /// tuned value forward.
    ///
    /// # Errors
    ///
    /// Returns [`RadioError::Dsp`] if the threshold is non-finite
    /// or (for syllabic) non-positive.
    pub fn set_voice_squelch_threshold(&mut self, threshold: f32) -> Result<(), RadioError> {
        // Build and validate the candidate cached mode FIRST: while the
        // live chain is forced `Off` (non-NFM, #737) the AF-chain
        // setter below is a no-op and would not reject a bad value,
        // which would then be replayed on NFM re-entry (CR round 2 on
        // PR #790). `Off` carries no threshold — nothing to update.
        let candidate = match self.voice_squelch_mode {
            VoiceSquelchMode::Off => VoiceSquelchMode::Off,
            VoiceSquelchMode::Syllabic { .. } => VoiceSquelchMode::Syllabic { threshold },
            VoiceSquelchMode::Snr { .. } => VoiceSquelchMode::Snr {
                threshold_db: threshold,
            },
        };
        candidate.validate()?;
        self.af_chain.set_voice_squelch_threshold(threshold)?;
        // Mirror the update into the cached mode so set_mode's
        // reapply picks up the tuned value.
        self.voice_squelch_mode = candidate;
        Ok(())
    }

    /// Enable or disable WFM stereo decode.
    ///
    /// Only has an effect when the current mode is WFM. For other modes this
    /// is a no-op via the default trait implementation.
    pub fn set_wfm_stereo(&mut self, enabled: bool) {
        self.demod.set_stereo(enabled);
    }

    /// Audio of the last [`Self::process`] call *before* any speaker
    /// gate (power squelch, CTCSS, voice squelch) was applied — for
    /// decoders such as APT / SSTV whose subcarriers have no speech
    /// cadence and would otherwise be zeroed by the gates (#734). Same
    /// length as that call's return value.
    pub fn pre_gate_audio(&self) -> &[Stereo] {
        self.af_chain.ungated_output()
    }

    /// Get the current demodulation mode.
    pub fn current_mode(&self) -> DemodMode {
        self.mode
    }

    /// Get the audio output sample rate (Hz). Stable across mode
    /// switches — the AF chain resamples each demod's native rate
    /// to this single output rate. Used by downstream consumers
    /// that need to know the sample rate of `process()`'s output
    /// buffer (e.g. the NOAA APT decoder tap, which expects an
    /// input rate strictly greater than 4800 Hz).
    #[must_use]
    pub fn audio_sample_rate(&self) -> f64 {
        self.audio_sample_rate
    }

    /// Get the current demodulator's configuration.
    pub fn demod_config(&self) -> &DemodConfig {
        self.demod.config()
    }

    /// Get a reference to the IF chain for direct configuration.
    pub fn if_chain(&self) -> &IfChain {
        &self.if_chain
    }

    /// Enable or disable the software IF AGC. The preference is kept
    /// across mode switches and only applied in modes whose config
    /// allows it — Raw / LRPT / CW hand the IQ through untouched (#738).
    pub fn set_software_agc_enabled(&mut self, enabled: bool) {
        self.software_agc_enabled = enabled;
        let allowed = self.demod.config().if_agc_allowed;
        self.if_chain.set_software_agc_enabled(enabled && allowed);
    }

    /// Get a mutable reference to the IF chain for direct configuration.
    pub fn if_chain_mut(&mut self) -> &mut IfChain {
        &mut self.if_chain
    }

    /// Get a reference to the AF chain.
    pub fn af_chain(&self) -> &AfChain {
        &self.af_chain
    }

    /// Get a mutable reference to the AF chain.
    pub fn af_chain_mut(&mut self) -> &mut AfChain {
        &mut self.af_chain
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss)]
mod tests;
