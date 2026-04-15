//! Voice-activity squelch — speech-shape detectors that gate the
//! speaker path after the RF-level [`PowerSquelch`] and any
//! CTCSS tone gate have already opened.
//!
//! Two detection strategies, selectable at runtime:
//!
//! - **Syllabic** (#270) — detects the 2–10 Hz envelope modulation
//!   characteristic of human speech. Audio is full-wave rectified
//!   to extract its amplitude envelope, bandpass filtered to the
//!   syllabic rate band, and the short-term RMS of that filtered
//!   envelope is compared against a threshold. Speech produces
//!   strong envelope modulation in the ~4 Hz region that music,
//!   continuous tones, hiss, and most data modes lack.
//! - **Snr** (#271) — detects voice-band signal energy above
//!   out-of-voice-band noise energy. Parallel bandpass filters
//!   carve the audio into an in-voice-band region (telephone
//!   voice ~300–3000 Hz) and an out-of-voice-band reference
//!   (below 200 Hz + above 5000 Hz); the ratio of their RMS
//!   values, expressed in dB, is compared against a threshold.
//!   Scale-invariant across fading.
//!
//! Both run on the post-demod mono 48 kHz signal in
//! [`sdr_radio::af_chain::AfChain`] and produce a boolean gate
//! state via [`VoiceSquelch::is_open`]. The gate state is
//! sampled per block; block-level AND-gating with other squelch
//! stages lives in the AF chain, not here.
//!
//! [`PowerSquelch`]: crate::noise::PowerSquelch

use sdr_types::DspError;

/// Canonical audio sample rate the detectors are calibrated for.
/// Matches `sdr-radio::af_chain::DEFAULT_AUDIO_RATE`. Both biquad
/// coefficient sets and the RMS window length depend on this value
/// — a different sample rate would land the filters on different
/// physical frequencies and break the detection bands.
pub const VOICE_SQUELCH_SAMPLE_RATE_HZ: f32 = 48_000.0;

/// Short-window RMS integration length in milliseconds.
///
/// 100 ms is long enough to smooth out normal syllabic structure
/// (one syllable is ~150–300 ms so the RMS window sees at least a
/// few cycles of envelope at the 4 Hz syllabic rate) and short
/// enough that the gate opens/closes within one utterance boundary
/// rather than lagging by a full second.
pub const VOICE_SQUELCH_RMS_WINDOW_MS: f32 = 100.0;

/// Short-window RMS length in samples, derived from
/// [`VOICE_SQUELCH_RMS_WINDOW_MS`] at
/// [`VOICE_SQUELCH_SAMPLE_RATE_HZ`].
///
/// Integer literal because the const-eval float → usize cast
/// trips clippy's `cast_*` lints in const context. The compile-
/// time assertion below catches drift if either input constant
/// changes.
pub const VOICE_SQUELCH_RMS_WINDOW_SAMPLES: usize = 4_800;

// Sanity check: if anyone touches the ms or sample rate constants
// without also updating VOICE_SQUELCH_RMS_WINDOW_SAMPLES, this
// fails to compile.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]
const _: () = {
    let derived = (VOICE_SQUELCH_RMS_WINDOW_MS * VOICE_SQUELCH_SAMPLE_RATE_HZ / 1000.0) as usize;
    assert!(
        derived == VOICE_SQUELCH_RMS_WINDOW_SAMPLES,
        "VOICE_SQUELCH_RMS_WINDOW_SAMPLES out of sync with MS / SAMPLE_RATE"
    );
};

/// Tolerance for matching a caller-supplied sample rate against
/// [`VOICE_SQUELCH_SAMPLE_RATE_HZ`]. The biquad coefficients
/// encode the physical sample rate at construction time, so
/// running the detector at a different rate would land the
/// filter bands on the wrong frequencies and break detection.
/// The constructor rejects any rate outside this tolerance.
const SAMPLE_RATE_MATCH_EPSILON_HZ: f32 = 0.5;

// ─── Syllabic detector parameters ────────────────────────────

/// Center frequency of the syllabic-envelope bandpass (Hz).
///
/// Human speech has a characteristic ~4 Hz syllable rate; the
/// BPF centered here with moderate Q captures the 2–10 Hz
/// region where syllable-rate modulation lives while rejecting
/// both DC (slow level changes) and higher-frequency envelope
/// components (which would come from speech formants, not
/// syllabic structure).
const SYLLABIC_BPF_CENTER_HZ: f32 = 4.0;

/// Q factor for the syllabic-envelope bandpass. `Q = 1.5` gives
/// a ~2.7 Hz half-power bandwidth centered at 4 Hz, which
/// approximately covers the 2–6 Hz core of natural speech
/// syllable rate without bleeding into 10+ Hz phonetic detail.
const SYLLABIC_BPF_Q: f32 = 1.5;

/// Default detection threshold for the syllabic detector —
/// unitless because the syllabic envelope RMS is normalized by
/// the full-signal RMS to make it scale-invariant. See
/// [`SyllabicDetector::process`] for the exact ratio definition.
///
/// 0.15 is an empirically reasonable starting point; the value
/// will likely want tuning once real-world scanner audio is
/// available.
pub const VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD: f32 = 0.15;

// ─── SNR detector parameters ─────────────────────────────────

/// In-voice-band BPF center frequency (Hz) — roughly the
/// geometric midpoint of the telephone voice band 300–3400 Hz.
const SNR_IN_BAND_CENTER_HZ: f32 = 1_000.0;

/// In-voice-band BPF Q. `Q = 0.7` gives a ~1400 Hz bandwidth
/// centered at 1 kHz, roughly spanning 300–1700 Hz — covers
/// the fundamental + first two formants of typical speech
/// where the bulk of voice energy lives.
const SNR_IN_BAND_Q: f32 = 0.7;

/// Out-of-voice-band BPF center frequency (Hz) — high enough
/// to miss sibilance (which has voice energy up to ~7.5 kHz),
/// low enough to catch broadband hiss and adjacent-channel bleed
/// that sits well above the voice band.
const SNR_OUT_OF_BAND_CENTER_HZ: f32 = 12_000.0;

/// Out-of-band BPF Q. Matched to the in-band Q so the two
/// filters have comparable bandwidth; makes the ratio easier
/// to reason about as "power in a typical-voice-bandwidth
/// slice of one region vs. the other."
const SNR_OUT_OF_BAND_Q: f32 = 0.7;

/// Default SNR threshold in dB. `+6 dB` means the in-voice-band
/// RMS has to be at least 2× the out-of-voice-band RMS for the
/// gate to open — comfortable for intelligible voice, tight
/// enough to reject music and pure noise.
pub const VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB: f32 = 6.0;

/// Noise floor clamp (linear) for the SNR ratio divisor. Without
/// this, a silent out-of-voice-band slice produces a division by
/// near-zero and the SNR blows up to +∞, opening the gate on
/// pure silence. Clamping the denominator at a small positive
/// value caps the maximum reported SNR at a sane finite number.
const SNR_OUT_OF_BAND_FLOOR: f32 = 1e-6;

/// User-selectable voice-activity squelch mode.
///
/// `Off` is the default; no gate is applied and audio passes
/// through unchanged (the detectors are not even constructed).
/// `Syllabic(threshold)` and `Snr(threshold_db)` carry their
/// per-variant threshold inline so the DSP layer only has to
/// look at the variant to know what comparison to apply.
///
/// Serialized form: `{"kind":"off"}`,
/// `{"kind":"syllabic","threshold":0.15}`, or
/// `{"kind":"snr","threshold_db":6.0}` — tagged representation
/// so bookmark JSON is self-describing and round-trips cleanly
/// through serde.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum VoiceSquelchMode {
    /// No voice squelch — audio passes through unchanged.
    Off,
    /// Syllabic detector: speech-cadence envelope modulation.
    /// Threshold is unitless (normalized envelope ratio).
    Syllabic {
        /// Detection threshold; compare against
        /// `VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD`.
        threshold: f32,
    },
    /// SNR detector: voice-band vs out-of-voice-band energy ratio.
    /// Threshold is in dB.
    Snr {
        /// Detection threshold in dB; compare against
        /// `VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB`.
        threshold_db: f32,
    },
}

impl VoiceSquelchMode {
    /// Returns `true` if the mode is anything other than
    /// [`Self::Off`] — convenient for the UI's "should I show
    /// a threshold slider" check.
    #[must_use]
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }
}

// ─── Biquad BPF ──────────────────────────────────────────────

/// Second-order IIR bandpass biquad, constant-skirt-gain form.
///
/// Coefficients follow the Audio EQ Cookbook (Robert Bristow-
/// Johnson, 2005-09-04):
/// ```text
/// w0    = 2π · f_center / f_s
/// alpha = sin(w0) / (2·Q)
/// b0    =  sin(w0) / 2   (= Q · alpha)
/// b1    =  0
/// b2    = -sin(w0) / 2
/// a0    = 1 + alpha
/// a1    = -2·cos(w0)
/// a2    = 1 - alpha
/// ```
/// Coefficients are normalized by `a0` at construction time so
/// the per-sample loop is six multiplies and four adds with no
/// divisions.
///
/// Private to this module — the syllabic and SNR detectors are
/// the only callers. The existing `sdr-dsp::filter::NotchFilter`
/// has very similar shape but is a band-REJECT filter, not band-
/// PASS, and the cookbook formulas differ in the numerator.
struct BandpassBiquad {
    b0: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BandpassBiquad {
    fn new(center_hz: f32, q: f32, sample_rate_hz: f32) -> Self {
        let w0 = core::f32::consts::TAU * center_hz / sample_rate_hz;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        // Constant-skirt-gain bandpass (b0 = sin(w0)/2 = Q·alpha).
        let b0_raw = sin_w0 / 2.0;
        let b2_raw = -sin_w0 / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0_raw / a0,
            b2: b2_raw / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Reset delay elements to zero — used on mode change so
    /// stale history doesn't leak across sessions.
    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    /// Process a single sample through the BPF, Direct Form I.
    /// Hot path — marked `#[inline]` so the enclosing loop can
    /// flatten the call. Note that `b1 = 0` for the constant-
    /// skirt-gain bandpass so that term is omitted entirely.
    #[inline]
    fn process_sample(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

// ─── Rolling RMS ─────────────────────────────────────────────

/// Rolling sum-of-squares over the last
/// [`VOICE_SQUELCH_RMS_WINDOW_SAMPLES`] samples, implemented as
/// a ring buffer over squared inputs. RMS is recovered on demand
/// by dividing by the window length and taking the square root.
///
/// Constant-time per sample regardless of window length — each
/// update adds one squared sample, subtracts the outgoing one,
/// and advances the ring pointer.
struct RollingRms {
    squares: Vec<f32>,
    head: usize,
    sum_sq: f32,
}

impl RollingRms {
    fn new() -> Self {
        Self {
            squares: vec![0.0; VOICE_SQUELCH_RMS_WINDOW_SAMPLES],
            head: 0,
            sum_sq: 0.0,
        }
    }

    fn reset(&mut self) {
        for v in &mut self.squares {
            *v = 0.0;
        }
        self.head = 0;
        self.sum_sq = 0.0;
    }

    /// Push one sample and update the rolling sum.
    #[inline]
    fn push(&mut self, x: f32) {
        let sq = x * x;
        // Subtract the oldest squared sample being overwritten,
        // add the new one. Clamp to 0 on the lower bound so f32
        // rounding can't drive sum_sq negative over long runs.
        self.sum_sq = (self.sum_sq - self.squares[self.head] + sq).max(0.0);
        self.squares[self.head] = sq;
        self.head = (self.head + 1) % VOICE_SQUELCH_RMS_WINDOW_SAMPLES;
    }

    /// Current RMS over the window.
    #[inline]
    fn rms(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let n = VOICE_SQUELCH_RMS_WINDOW_SAMPLES as f32;
        (self.sum_sq / n).sqrt()
    }
}

// ─── Syllabic detector ───────────────────────────────────────

/// Syllabic-envelope voice squelch implementation.
///
/// Pipeline per sample:
/// 1. Full-wave rectify: `|x|`
/// 2. Bandpass the rectified envelope at ~4 Hz (syllable rate)
/// 3. Update rolling RMS of the BPF output (envelope RMS)
/// 4. Update rolling RMS of the raw input (signal RMS) for
///    normalization — the gate decision compares the envelope
///    RMS against a fraction of the signal RMS, which makes
///    the threshold scale-invariant to audio level
///
/// Gate opens when `envelope_rms > threshold * signal_rms` AND
/// `signal_rms` is above a tiny floor (pure-silence rejection).
struct SyllabicDetector {
    envelope_bpf: BandpassBiquad,
    envelope_rms: RollingRms,
    signal_rms: RollingRms,
    threshold: f32,
}

impl SyllabicDetector {
    fn new(threshold: f32, sample_rate_hz: f32) -> Self {
        Self {
            envelope_bpf: BandpassBiquad::new(
                SYLLABIC_BPF_CENTER_HZ,
                SYLLABIC_BPF_Q,
                sample_rate_hz,
            ),
            envelope_rms: RollingRms::new(),
            signal_rms: RollingRms::new(),
            threshold,
        }
    }

    fn reset(&mut self) {
        self.envelope_bpf.reset();
        self.envelope_rms.reset();
        self.signal_rms.reset();
    }

    fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
    }

    /// Feed a block of mono audio samples and return the gate
    /// state at the END of the block. Caller gets one bool per
    /// process call regardless of block size.
    fn process(&mut self, samples: &[f32]) -> bool {
        for &x in samples {
            // Full-wave rectify for envelope extraction. Using
            // abs() rather than x² + sqrt because the BPF that
            // follows is linear and the spectral content we care
            // about (syllabic rate) sits in the rectified
            // envelope regardless of whether we take |x| or x².
            let rectified = x.abs();
            let envelope = self.envelope_bpf.process_sample(rectified);
            self.envelope_rms.push(envelope);
            self.signal_rms.push(x);
        }

        let env = self.envelope_rms.rms();
        let sig = self.signal_rms.rms();
        // Pure-silence guard: if the whole signal is effectively
        // zero, don't let the ratio blow up. The raw RMS must
        // also clear a tiny floor before we even consider the
        // envelope ratio.
        if sig < SNR_OUT_OF_BAND_FLOOR {
            return false;
        }
        env > self.threshold * sig
    }
}

// ─── SNR detector ────────────────────────────────────────────

/// Voice-band vs out-of-voice-band SNR detector.
///
/// Pipeline per sample:
/// 1. Feed the in-band BPF (300–1700 Hz) and update its RMS
/// 2. Feed the out-of-band BPF (high-pass reference region) and
///    update its RMS
/// 3. At end of block, compute `20·log10(in_rms / max(out_rms, floor))`
/// 4. Gate opens when SNR (dB) > threshold
struct SnrDetector {
    in_band_bpf: BandpassBiquad,
    out_of_band_bpf: BandpassBiquad,
    in_band_rms: RollingRms,
    out_of_band_rms: RollingRms,
    threshold_db: f32,
}

impl SnrDetector {
    fn new(threshold_db: f32, sample_rate_hz: f32) -> Self {
        Self {
            in_band_bpf: BandpassBiquad::new(SNR_IN_BAND_CENTER_HZ, SNR_IN_BAND_Q, sample_rate_hz),
            out_of_band_bpf: BandpassBiquad::new(
                SNR_OUT_OF_BAND_CENTER_HZ,
                SNR_OUT_OF_BAND_Q,
                sample_rate_hz,
            ),
            in_band_rms: RollingRms::new(),
            out_of_band_rms: RollingRms::new(),
            threshold_db,
        }
    }

    fn reset(&mut self) {
        self.in_band_bpf.reset();
        self.out_of_band_bpf.reset();
        self.in_band_rms.reset();
        self.out_of_band_rms.reset();
    }

    fn set_threshold_db(&mut self, threshold_db: f32) {
        self.threshold_db = threshold_db;
    }

    fn process(&mut self, samples: &[f32]) -> bool {
        for &x in samples {
            let in_band = self.in_band_bpf.process_sample(x);
            let out_band = self.out_of_band_bpf.process_sample(x);
            self.in_band_rms.push(in_band);
            self.out_of_band_rms.push(out_band);
        }

        let in_rms = self.in_band_rms.rms();
        let out_rms = self.out_of_band_rms.rms().max(SNR_OUT_OF_BAND_FLOOR);
        // Pure-silence guard: if the in-band is at or below the
        // floor too, skip the ratio entirely — nothing to gate.
        if in_rms < SNR_OUT_OF_BAND_FLOOR {
            return false;
        }
        let snr_db = 20.0 * (in_rms / out_rms).log10();
        snr_db > self.threshold_db
    }
}

// ─── Public VoiceSquelch ─────────────────────────────────────

/// Top-level voice-activity squelch. Owns the active detector
/// (if any) and the current gate state. Construct once per
/// session, feed blocks of mono audio via
/// [`Self::accept_samples`], and consume the boolean returned
/// by [`Self::is_open`].
///
/// Off mode is cheap: no detector is constructed, `accept_samples`
/// is a no-op, and `is_open` always returns `true` (gate is
/// permanently open). Caller can treat `is_open` uniformly.
pub struct VoiceSquelch {
    mode: VoiceSquelchMode,
    sample_rate_hz: f32,
    syllabic: Option<SyllabicDetector>,
    snr: Option<SnrDetector>,
    open: bool,
}

impl VoiceSquelch {
    /// Construct a new voice squelch in the given mode at the
    /// canonical sample rate.
    ///
    /// # Errors
    ///
    /// Returns [`DspError::InvalidParameter`] if `sample_rate_hz`
    /// differs from [`VOICE_SQUELCH_SAMPLE_RATE_HZ`] by more than
    /// [`SAMPLE_RATE_MATCH_EPSILON_HZ`], or if the mode carries a
    /// non-finite threshold.
    pub fn new(mode: VoiceSquelchMode, sample_rate_hz: f32) -> Result<Self, DspError> {
        if !sample_rate_hz.is_finite() {
            return Err(DspError::InvalidParameter(format!(
                "voice squelch sample rate must be finite, got {sample_rate_hz}"
            )));
        }
        if (sample_rate_hz - VOICE_SQUELCH_SAMPLE_RATE_HZ).abs() > SAMPLE_RATE_MATCH_EPSILON_HZ {
            return Err(DspError::InvalidParameter(format!(
                "voice squelch is calibrated for {VOICE_SQUELCH_SAMPLE_RATE_HZ} Hz, got {sample_rate_hz}"
            )));
        }

        let (syllabic, snr) = match mode {
            VoiceSquelchMode::Off => (None, None),
            VoiceSquelchMode::Syllabic { threshold } => {
                if !threshold.is_finite() || threshold <= 0.0 {
                    return Err(DspError::InvalidParameter(format!(
                        "syllabic threshold must be finite and positive, got {threshold}"
                    )));
                }
                (Some(SyllabicDetector::new(threshold, sample_rate_hz)), None)
            }
            VoiceSquelchMode::Snr { threshold_db } => {
                if !threshold_db.is_finite() {
                    return Err(DspError::InvalidParameter(format!(
                        "SNR threshold must be finite, got {threshold_db}"
                    )));
                }
                (None, Some(SnrDetector::new(threshold_db, sample_rate_hz)))
            }
        };

        // Off mode starts open (gate is permanently open); other
        // modes start closed and have to warm up the detector
        // before opening. This matches the CTCSS behavior — gate
        // is closed at start and has to confirm before it opens.
        let open = matches!(mode, VoiceSquelchMode::Off);

        Ok(Self {
            mode,
            sample_rate_hz,
            syllabic,
            snr,
            open,
        })
    }

    /// Current squelch mode.
    pub fn mode(&self) -> VoiceSquelchMode {
        self.mode
    }

    /// Current gate state. `true` means audio should flow through
    /// to the speaker; `false` means mute. Always `true` in Off
    /// mode.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Reset detector state to "closed" and drop any history
    /// that survived from the previous block. Called on mode
    /// change or session boundary.
    pub fn reset(&mut self) {
        if let Some(s) = &mut self.syllabic {
            s.reset();
        }
        if let Some(s) = &mut self.snr {
            s.reset();
        }
        self.open = matches!(self.mode, VoiceSquelchMode::Off);
    }

    /// Swap the mode in place. Drops the previous detector (if
    /// any) and constructs a new one. Cheap — the detectors
    /// allocate one ring buffer each, which is ~19 KiB at the
    /// current window length, per detector.
    ///
    /// # Errors
    ///
    /// Propagates validation errors from [`Self::new`] with the
    /// previous state preserved unchanged on failure.
    pub fn set_mode(&mut self, mode: VoiceSquelchMode) -> Result<(), DspError> {
        // Build the replacement first. If it errors we leave
        // `self` unchanged so a bad setter call doesn't leave
        // the caller with a half-constructed squelch.
        let replacement = Self::new(mode, self.sample_rate_hz)?;
        *self = replacement;
        Ok(())
    }

    /// Update the threshold of the active detector. Returns
    /// [`DspError::InvalidParameter`] if the threshold is non-
    /// finite; the setter is a silent no-op in Off mode.
    pub fn set_threshold(&mut self, threshold: f32) -> Result<(), DspError> {
        if !threshold.is_finite() {
            return Err(DspError::InvalidParameter(format!(
                "voice squelch threshold must be finite, got {threshold}"
            )));
        }
        match &mut self.mode {
            VoiceSquelchMode::Off => {}
            VoiceSquelchMode::Syllabic { threshold: t } => {
                if threshold <= 0.0 {
                    return Err(DspError::InvalidParameter(format!(
                        "syllabic threshold must be positive, got {threshold}"
                    )));
                }
                *t = threshold;
                if let Some(s) = &mut self.syllabic {
                    s.set_threshold(threshold);
                }
            }
            VoiceSquelchMode::Snr { threshold_db } => {
                *threshold_db = threshold;
                if let Some(s) = &mut self.snr {
                    s.set_threshold_db(threshold);
                }
            }
        }
        Ok(())
    }

    /// Feed a block of mono audio samples and update the gate
    /// state. Returns the post-update `is_open` value so callers
    /// that only care about the latest state don't have to follow
    /// up with a separate call.
    pub fn accept_samples(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return self.open;
        }
        self.open = match self.mode {
            VoiceSquelchMode::Off => true,
            VoiceSquelchMode::Syllabic { .. } => {
                self.syllabic.as_mut().is_some_and(|d| d.process(samples))
            }
            VoiceSquelchMode::Snr { .. } => self.snr.as_mut().is_some_and(|d| d.process(samples)),
        };
        self.open
    }
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;

    /// Build a 48 kHz mono tone at the given frequency + amplitude
    /// for `ms` milliseconds.
    fn tone(freq_hz: f32, amplitude: f32, ms: usize) -> Vec<f32> {
        let n = (VOICE_SQUELCH_SAMPLE_RATE_HZ * (ms as f32) / 1000.0) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = (i as f32) / VOICE_SQUELCH_SAMPLE_RATE_HZ;
            out.push(amplitude * (core::f32::consts::TAU * freq_hz * t).sin());
        }
        out
    }

    /// Build a 48 kHz mono "syllable-modulated" signal: a 1 kHz
    /// carrier whose amplitude is itself modulated by a slow
    /// sine at `syllable_hz`. Approximates speech envelope
    /// structure closely enough to exercise the syllabic
    /// detector.
    fn syllable_modulated(carrier_hz: f32, syllable_hz: f32, ms: usize) -> Vec<f32> {
        let n = (VOICE_SQUELCH_SAMPLE_RATE_HZ * (ms as f32) / 1000.0) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = (i as f32) / VOICE_SQUELCH_SAMPLE_RATE_HZ;
            // Envelope is raised cosine so it stays non-negative
            // and looks like speech loudness structure. 0.5 + 0.5·cos
            // oscillates between 0 and 1.
            let envelope = 0.5 + 0.5 * (core::f32::consts::TAU * syllable_hz * t).cos();
            let carrier = (core::f32::consts::TAU * carrier_hz * t).sin();
            out.push(0.5 * envelope * carrier);
        }
        out
    }

    /// Pseudo-random white noise with bounded peak amplitude.
    /// Uses a cheap LCG; no need for a real PRNG just for tests.
    fn white_noise(amplitude: f32, ms: usize, seed: u64) -> Vec<f32> {
        let n = (VOICE_SQUELCH_SAMPLE_RATE_HZ * (ms as f32) / 1000.0) as usize;
        let mut out = Vec::with_capacity(n);
        let mut state: u64 = seed;
        for _ in 0..n {
            // LCG constants from Numerical Recipes.
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Top 24 bits → unit float → [-1, 1]
            let u = ((state >> 40) as f32) / ((1u64 << 24) as f32);
            out.push(amplitude * (u * 2.0 - 1.0));
        }
        out
    }

    #[test]
    fn mode_is_active_helper() {
        assert!(!VoiceSquelchMode::Off.is_active());
        assert!(VoiceSquelchMode::Syllabic { threshold: 0.15 }.is_active());
        assert!(VoiceSquelchMode::Snr { threshold_db: 6.0 }.is_active());
    }

    #[test]
    fn off_mode_always_open_and_passes_samples_through() {
        let mut vs =
            VoiceSquelch::new(VoiceSquelchMode::Off, VOICE_SQUELCH_SAMPLE_RATE_HZ).unwrap();
        assert!(vs.is_open(), "Off mode should start open");
        let samples = tone(1000.0, 0.5, 50);
        assert!(vs.accept_samples(&samples));
        // An empty block after any previous state shouldn't
        // toggle the gate.
        assert!(vs.accept_samples(&[]));
    }

    #[test]
    fn constructor_rejects_non_canonical_sample_rate() {
        let err = VoiceSquelch::new(VoiceSquelchMode::Off, 44_100.0);
        assert!(err.is_err());
        let err = VoiceSquelch::new(VoiceSquelchMode::Off, f32::NAN);
        assert!(err.is_err());
    }

    #[test]
    fn constructor_rejects_non_finite_threshold() {
        for t in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let err = VoiceSquelch::new(
                VoiceSquelchMode::Syllabic { threshold: t },
                VOICE_SQUELCH_SAMPLE_RATE_HZ,
            );
            assert!(err.is_err(), "syllabic threshold {t} should be rejected");
            let err = VoiceSquelch::new(
                VoiceSquelchMode::Snr { threshold_db: t },
                VOICE_SQUELCH_SAMPLE_RATE_HZ,
            );
            assert!(err.is_err(), "snr threshold {t} should be rejected");
        }
    }

    #[test]
    fn syllabic_constructor_rejects_zero_or_negative_threshold() {
        for t in [0.0_f32, -0.1, -1.0] {
            let err = VoiceSquelch::new(
                VoiceSquelchMode::Syllabic { threshold: t },
                VOICE_SQUELCH_SAMPLE_RATE_HZ,
            );
            assert!(err.is_err(), "threshold {t} should be rejected");
        }
    }

    #[test]
    fn syllabic_detects_syllable_rate_modulation() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic {
                threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();

        // Feed 2 seconds of syllable-modulated audio at 4 Hz.
        // That's plenty of time for the BPF to ring up and the
        // RMS window to saturate.
        let signal = syllable_modulated(1_000.0, 4.0, 2_000);
        vs.accept_samples(&signal);
        assert!(
            vs.is_open(),
            "syllabic detector should open on 4 Hz-modulated 1 kHz carrier"
        );
    }

    #[test]
    fn syllabic_rejects_continuous_tone() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic {
                threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();

        // Pure 1 kHz tone with constant amplitude — no syllabic
        // modulation. Envelope is flat after the rectifier so
        // the 4 Hz BPF sees ~0 energy.
        let signal = tone(1_000.0, 0.5, 2_000);
        vs.accept_samples(&signal);
        assert!(
            !vs.is_open(),
            "syllabic detector should reject a continuous unmodulated tone"
        );
    }

    #[test]
    fn syllabic_rejects_silence() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic {
                threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let silence = vec![0.0_f32; 48_000];
        vs.accept_samples(&silence);
        assert!(!vs.is_open(), "silence should not open the gate");
    }

    #[test]
    fn snr_detects_strong_in_band_signal() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Snr {
                threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        // Strong 1 kHz tone — well inside the in-voice-band BPF
        // center — and no out-of-voice-band content beyond what
        // the biquad's finite Q leaks. SNR should be very high.
        let signal = tone(1_000.0, 0.8, 2_000);
        vs.accept_samples(&signal);
        assert!(
            vs.is_open(),
            "SNR detector should open on strong in-voice-band tone"
        );
    }

    #[test]
    fn snr_rejects_broadband_noise() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Snr {
                threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        // White noise — equal energy in every bin — so the
        // in-band BPF and out-of-band BPF pick up the same
        // amount once bandwidth-normalized. SNR ~ 0 dB.
        let signal = white_noise(0.5, 2_000, 0xDEAD_BEEF);
        vs.accept_samples(&signal);
        assert!(!vs.is_open(), "SNR detector should reject broadband noise");
    }

    #[test]
    fn snr_rejects_silence() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Snr {
                threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let silence = vec![0.0_f32; 48_000];
        vs.accept_samples(&silence);
        assert!(!vs.is_open(), "silence should not open the gate");
    }

    #[test]
    fn mode_change_resets_gate_state() {
        // Open the gate with syllabic, then switch to SNR on
        // fresh content — gate should start closed again.
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic {
                threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let signal = syllable_modulated(1_000.0, 4.0, 2_000);
        vs.accept_samples(&signal);
        assert!(vs.is_open());

        vs.set_mode(VoiceSquelchMode::Snr {
            threshold_db: VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB,
        })
        .unwrap();
        assert!(!vs.is_open(), "mode change should reset gate to closed");
    }

    #[test]
    fn mode_change_to_off_opens_gate() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic {
                threshold: VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
            },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        assert!(!vs.is_open());
        vs.set_mode(VoiceSquelchMode::Off).unwrap();
        assert!(
            vs.is_open(),
            "Off mode should leave the gate permanently open"
        );
    }

    #[test]
    fn set_threshold_rejects_non_finite() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic { threshold: 0.15 },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        assert!(vs.set_threshold(f32::NAN).is_err());
        assert!(vs.set_threshold(f32::INFINITY).is_err());
        assert!(vs.set_threshold(f32::NEG_INFINITY).is_err());
    }

    #[test]
    fn set_threshold_rejects_non_positive_for_syllabic() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic { threshold: 0.15 },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        assert!(vs.set_threshold(0.0).is_err());
        assert!(vs.set_threshold(-0.1).is_err());
    }

    #[test]
    fn mode_serde_round_trip() {
        let off = VoiceSquelchMode::Off;
        let syl = VoiceSquelchMode::Syllabic { threshold: 0.15 };
        let snr = VoiceSquelchMode::Snr { threshold_db: 6.0 };
        for m in [off, syl, snr] {
            let json = serde_json::to_string(&m).unwrap();
            let back: VoiceSquelchMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m, "serde round-trip failed for {m:?}");
        }
    }

    #[test]
    fn empty_block_does_not_flip_state() {
        let mut vs = VoiceSquelch::new(
            VoiceSquelchMode::Syllabic { threshold: 0.15 },
            VOICE_SQUELCH_SAMPLE_RATE_HZ,
        )
        .unwrap();
        assert!(!vs.is_open());
        // Feeding an empty slice should be a no-op regardless
        // of state.
        assert!(!vs.accept_samples(&[]));
        assert!(!vs.is_open());
    }
}
