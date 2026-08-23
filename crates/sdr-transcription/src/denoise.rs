//! FFT-based spectral noise gate for cleaning radio audio before transcription.
//!
//! Two entry points:
//!
//! - [`spectral_denoise`] — the original broadband gate. Estimates the
//!   noise floor from the quietest frequency bins and zeros everything
//!   below `noise_floor * gate_ratio`. Treats a 10 kHz whistle the same
//!   as a 1 kHz vowel formant. Still exported for A/B testing.
//! - [`enhance_speech`] — voice-band shaped gate. Same FFT, but every
//!   bin gets multiplied by a voice-prior weight `w(f)` that peaks in
//!   the formant band (300–3400 Hz), tapers across the fundamentals
//!   (80–300 Hz) and sibilance (3400–7500 Hz) regions, and zeroes
//!   everything outside. The weight governs both the gate decision
//!   (so in-band speech wins against out-of-band rumble even when the
//!   out-of-band signal is louder) and the output magnitude (so the
//!   function acts as a true soft bandpass). Noise-floor estimation
//!   uses only bins inside the voice band so a strong PL tone or
//!   ultrasonic birdie can't drag the floor up.
//!
//! Both paths are based on the FFT → identify noise → zero bins → IFFT
//! approach from Tariq & Khan (2023), "Mathematical Approach for
//! Enhancing Audio Signal Quality: Theory, Insights, and Applications."
//!
//! Voice-band shaping is issue #274 — the bin weights are a static
//! voice prior, intentionally simple so we can A/B test against the
//! broadband gate and iterate on the weight shape in follow-ups.

use rustfft::{FftPlanner, num_complex::Complex};

/// Required sample rate of the mono buffer handed to these functions.
/// The voice-band weight function keys off absolute frequencies (80 Hz,
/// 300 Hz, 3.4 kHz, 7.5 kHz), so callers must resample to 16 kHz mono
/// before invoking the gate — otherwise the bin→frequency math is wrong
/// and the weights land on the wrong physical bands.
const SAMPLE_RATE_HZ: f32 = 16_000.0;

// --- Voice-band weight function breakpoints (issue #274) ---
//
// Piecewise linear `w(f)`:
//
//     0             f < 80 Hz         (hard cut: rumble, CTCSS leakage, AC hum)
//     0.5           80  ≤ f < 300     (fundamentals; present but de-emphasized)
//     1.0           300 ≤ f < 3400    (formant / telephony band; full passthrough)
//     ramp → 0.3    3400 ≤ f < 7500   (sibilance; linearly tapered)
//     0             f ≥ 7500 Hz       (hard cut; near Nyquist at 16 kHz)
//
// The breakpoints are named constants so follow-up exploration can
// tune them without hunting through the function body.
const VOICE_F_SUB_HZ: f32 = 80.0;
const VOICE_F_FUND_HZ: f32 = 300.0;
const VOICE_F_FORMANT_HI_HZ: f32 = 3_400.0;
const VOICE_F_SIB_HI_HZ: f32 = 7_500.0;

/// Weight applied to the fundamental-frequency band (80–300 Hz).
///
/// Voice fundamentals carry pitch information but very little
/// intelligibility — the recognizer keys on formants, not F0. Keeping
/// the fundamental band at full weight means low-frequency noise
/// sitting in 100–200 Hz survives the gate just because it's
/// "in-band". Half-weight splits the difference: we don't throw away
/// speaker pitch outright, but we stop treating it as equal to the
/// formant band.
const VOICE_W_FUND: f32 = 0.5;

/// Weight at the top end of the sibilance ramp (3400–7500 Hz). The
/// band above the telephony cutoff carries /s/, /ʃ/, /t/ and other
/// fricatives — useful for speech intelligibility but also where
/// most radio noise (static, heterodynes) sits. Linear taper from
/// 1.0 down to this value.
const VOICE_W_SIB_END: f32 = 0.3;

/// Default gate ratio — bins must exceed `noise_floor * GATE_RATIO` to survive.
/// A ratio of 3.0 means bins must be 3x the noise floor (~9.5 dB above).
/// Used as the default in tests; the runtime value is user-configurable.
#[cfg(test)]
const GATE_RATIO: f32 = 3.0;

/// Percentile of magnitude-sorted bins used to estimate the noise floor.
/// 0.2 means the bottom 20% of bins define the noise level.
const NOISE_FLOOR_PERCENTILE: f32 = 0.20;

/// Minimum buffer length required for a meaningful FFT-based gate.
/// Buffers shorter than this (typical at session start when only a
/// few milliseconds of audio have arrived) are passed through
/// unchanged — the FFT would have so few bins that the noise-floor
/// estimate would be pure noise itself. Shared between
/// [`spectral_denoise`] and [`enhance_speech`] so the policy lives
/// in one place.
const MIN_FFT_LEN: usize = 64;

/// User-selectable audio enhancement mode applied to mono
/// transcription audio before it reaches the recognizer.
///
/// Every call site in the transcription pipeline (sherpa offline
/// VAD, sherpa offline Auto Break, sherpa streaming, whisper)
/// dispatches through [`apply`] using the mode configured on the
/// session. Switching modes takes effect at the next session
/// start — the session I/O threads read the config once and use
/// it for the session's lifetime.
///
/// # Issue #281 context
///
/// The default [`VoiceBand`] path shaves audio outside ~80–7500 Hz
/// with a voice-prior weight function. Some recognizers — notably
/// Moonshine Tiny/Base in the sherpa-onnx int8 releases — have a
/// convolutional frontend that appears to be more sensitive to
/// these hard cutoffs than `Parakeet`'s `NeMo` fbank frontend, and
/// produce empty text on the same NFM audio where `Parakeet`
/// transcribes correctly. Switching the affected session to
/// [`Broadband`] (flat noise-floor gate, no voice-prior) restores
/// Moonshine's output. See issue #281 for the investigation and
/// trace data.
///
/// # Variants
///
/// - [`VoiceBand`] — [`enhance_speech`], bandpass-shaped gate with
///   voice-prior weights. Default for most users on most audio.
/// - [`Broadband`] — [`spectral_denoise`], flat noise-floor gate
///   without voice-prior weights. Use when [`VoiceBand`] is
///   suppressing recognizer output (e.g. Moonshine on NFM).
/// - [`Off`] — no enhancement. Pass the audio straight to the
///   recognizer. Useful as a baseline for troubleshooting or when
///   the source is already clean (file playback of pre-cleaned
///   audio, etc.).
///
/// [`VoiceBand`]: AudioEnhancement::VoiceBand
/// [`Broadband`]: AudioEnhancement::Broadband
/// [`Off`]: AudioEnhancement::Off
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum AudioEnhancement {
    /// Voice-prior weighted spectral gate — default.
    /// See [`enhance_speech`] for the algorithm.
    #[default]
    VoiceBand,
    /// Flat-weight spectral gate — the original PR #227 broadband
    /// path. See [`spectral_denoise`].
    Broadband,
    /// No enhancement. Pass audio through unchanged.
    Off,
}

impl AudioEnhancement {
    /// Stable string identifier for config persistence. Paired
    /// with [`Self::from_config_str`].
    ///
    /// Snake-case so it looks natural in the JSON config file
    /// alongside other transcription keys like `display_mode`.
    #[must_use]
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::VoiceBand => "voice_band",
            Self::Broadband => "broadband",
            Self::Off => "off",
        }
    }

    /// Parse a config-string identifier produced by
    /// [`Self::as_config_str`]. Unknown values (old configs, typos,
    /// future-reserved names) fall back to the default
    /// [`AudioEnhancement::VoiceBand`] rather than erroring — a
    /// missing or invalid audio-enhancement config key should
    /// never fail a session start.
    #[must_use]
    pub fn from_config_str(s: &str) -> Self {
        match s {
            "broadband" => Self::Broadband,
            "off" => Self::Off,
            // "voice_band" and unknown both fall through to the
            // default, per the lenient-parsing contract documented
            // on the function.
            _ => Self::VoiceBand,
        }
    }
}

/// Apply the selected audio enhancement to `samples` in place.
///
/// Central dispatcher for all transcription call sites. Every
/// recognizer path should route its mono buffer through here
/// instead of calling [`enhance_speech`] / [`spectral_denoise`]
/// directly so the user's mode selection is honored.
///
/// `gate_ratio` is passed through to whichever FFT-based path
/// runs; it has no effect in [`AudioEnhancement::Off`] mode.
/// Buffers shorter than [`MIN_FFT_LEN`] are left unchanged by the
/// underlying functions (same as the existing short-buffer
/// behavior) so very short segments at session boundaries still
/// reach the recognizer without being gated by a degenerate FFT.
pub fn apply(samples: &mut [f32], enhancement: AudioEnhancement, gate_ratio: f32) {
    match enhancement {
        AudioEnhancement::VoiceBand => enhance_speech(samples, gate_ratio),
        AudioEnhancement::Broadband => spectral_denoise(samples, gate_ratio),
        AudioEnhancement::Off => {
            // No-op — leave samples untouched. This is the
            // escape hatch for users whose audio is already
            // clean or whose recognizer behaves badly with any
            // spectral gate.
        }
    }
}

/// Apply spectral noise gating to a mono f32 audio buffer in-place.
///
/// The buffer is FFT'd, noise floor is estimated from the quietest bins,
/// bins below the threshold are zeroed, then IFFT'd back to time domain.
///
/// `gate_ratio` controls how aggressive the gate is — bins must exceed
/// `noise_floor * gate_ratio` to survive. Higher values remove more noise
/// but may clip speech transients.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn spectral_denoise(samples: &mut [f32], gate_ratio: f32) {
    let n = samples.len();
    if n < MIN_FFT_LEN {
        return; // too short for meaningful FFT
    }

    let mut planner = FftPlanner::new();
    let fft_fwd = planner.plan_fft_forward(n);
    let fft_inv = planner.plan_fft_inverse(n);

    // Convert to complex for FFT.
    let mut spectrum: Vec<Complex<f32>> = samples.iter().map(|&s| Complex::new(s, 0.0)).collect();

    // Forward FFT.
    fft_fwd.process(&mut spectrum);

    // Compute magnitudes for noise floor estimation.
    let magnitudes: Vec<f32> = spectrum.iter().map(|c| c.norm()).collect();

    // Estimate noise floor from the quietest percentile of bins.
    let mut sorted_mags = magnitudes.clone();
    sorted_mags.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let percentile_idx = ((n as f32) * NOISE_FLOOR_PERCENTILE) as usize;
    let percentile_idx = percentile_idx.min(n.saturating_sub(1));
    let noise_floor = sorted_mags[percentile_idx];

    // Gate threshold: bins must exceed noise_floor * ratio to survive.
    let threshold = noise_floor * gate_ratio;

    // Zero out bins below threshold (spectral gate).
    for (i, mag) in magnitudes.iter().enumerate() {
        if *mag < threshold {
            spectrum[i] = Complex::new(0.0, 0.0);
        }
    }

    // Inverse FFT.
    fft_inv.process(&mut spectrum);

    // Normalize (rustfft doesn't normalize) and write back.
    let scale = 1.0 / n as f32;
    for (i, s) in samples.iter_mut().enumerate() {
        *s = spectrum[i].re * scale;
    }
}

/// Piecewise-linear voice-band weight for a given bin frequency in Hz.
///
/// See the module-level docstring and the `VOICE_*` constants for the
/// shape. Used by [`enhance_speech`] to weight each FFT bin by a
/// voice prior that peaks in the 300–3400 Hz formant band.
fn voice_band_weight(freq_hz: f32) -> f32 {
    if !(VOICE_F_SUB_HZ..VOICE_F_SIB_HI_HZ).contains(&freq_hz) {
        0.0
    } else if freq_hz < VOICE_F_FUND_HZ {
        VOICE_W_FUND
    } else if freq_hz < VOICE_F_FORMANT_HI_HZ {
        1.0
    } else {
        // 3400 ≤ f < 7500: linear ramp from 1.0 to VOICE_W_SIB_END.
        let t = (freq_hz - VOICE_F_FORMANT_HI_HZ) / (VOICE_F_SIB_HI_HZ - VOICE_F_FORMANT_HI_HZ);
        1.0 + t * (VOICE_W_SIB_END - 1.0)
    }
}

/// Voice-band shaped spectral gate (issue #274).
///
/// Same FFT-based noise gate as [`spectral_denoise`], but with three
/// additions:
///
/// 1. Each bin is multiplied by a voice-prior weight `w(f)` before the
///    gate decision, so in-band speech wins against out-of-band
///    interference even when the interference is louder.
/// 2. The noise floor is estimated from voice-band bins only, not the
///    full spectrum. A strong PL tone or ultrasonic birdie can't drag
///    the floor up and cause the gate to chew into speech.
/// 3. Surviving bins are scaled by the same weight, so the function
///    doubles as a true soft bandpass — out-of-band bins are zeroed,
///    fundamental-band bins are halved, sibilance rolls off linearly.
///
/// The weight function is a static prior — purely spectral shaping.
/// Any dynamic (per-segment) voice-activity / endpoint detection is
/// the caller's problem and happens downstream, at whatever stage
/// makes sense for the specific recognizer backend. This function
/// guarantees nothing about segmentation.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn enhance_speech(samples: &mut [f32], gate_ratio: f32) {
    let n = samples.len();
    if n < MIN_FFT_LEN {
        return;
    }

    let mut planner = FftPlanner::new();
    let fft_fwd = planner.plan_fft_forward(n);
    let fft_inv = planner.plan_fft_inverse(n);

    let mut spectrum: Vec<Complex<f32>> = samples.iter().map(|&s| Complex::new(s, 0.0)).collect();
    fft_fwd.process(&mut spectrum);

    // Precompute each bin's frequency and voice-band weight.
    //
    // FFT of a real signal is conjugate-symmetric: bins k and n-k carry
    // the same magnitude. Using `min(k, n-k) * sample_rate / n` gives
    // the correct physical frequency for both halves of the spectrum so
    // the same weight applies to each mirrored pair and the inverse
    // transform stays real.
    let n_f = n as f32;
    let magnitudes: Vec<f32> = spectrum.iter().map(|c| c.norm()).collect();
    let weights: Vec<f32> = (0..n)
        .map(|k| {
            let k_f = k as f32;
            let bin_freq = k_f.min(n_f - k_f) * SAMPLE_RATE_HZ / n_f;
            voice_band_weight(bin_freq)
        })
        .collect();

    // Voice-prior noise floor: percentile over voice-band bins only,
    // in *weighted* units (`m * w`). The gate decision below compares
    // `effective = mag * weight` against `threshold = floor * gate_ratio`,
    // so the floor MUST be computed in the same units — otherwise a
    // loud 100-250 Hz PL/hum tone (weight 0.5) contributes its full
    // raw magnitude to the percentile but is only half-weighted at
    // gate time, creating a mismatch that can suppress weaker formants
    // unnecessarily. Out-of-band bins (weight == 0) still contribute
    // nothing because their weighted magnitude is zero.
    let mut voice_band_mags: Vec<f32> = magnitudes
        .iter()
        .zip(weights.iter())
        .filter_map(|(&m, &w)| (w > 0.0).then_some(m * w))
        .collect();
    let noise_floor = if voice_band_mags.is_empty() {
        0.0
    } else {
        // `select_nth_unstable_by` partitions the slice in O(n) average
        // time so the element at `idx` ends up in its final sorted
        // position — strictly cheaper than a full O(n log n) sort when
        // we only need one percentile. `enhance_speech` runs on every
        // decoded segment so the hot path matters.
        let idx = ((voice_band_mags.len() as f32) * NOISE_FLOOR_PERCENTILE) as usize;
        let idx = idx.min(voice_band_mags.len() - 1);
        let (_, nth, _) = voice_band_mags.select_nth_unstable_by(idx, |a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        *nth
    };

    let threshold = noise_floor * gate_ratio;

    // Gate + shape in one pass. Effective magnitude = raw * weight.
    // Out-of-band bins (weight == 0) gate out automatically because
    // effective_mag = 0 < threshold. Surviving bins get shaped by the
    // weight so the output is a true soft bandpass.
    for (i, (mag, weight)) in magnitudes.iter().zip(weights.iter()).enumerate() {
        let effective = mag * weight;
        if effective < threshold {
            spectrum[i] = Complex::new(0.0, 0.0);
        } else {
            spectrum[i] *= *weight;
        }
    }

    fft_inv.process(&mut spectrum);

    let scale = 1.0 / n_f;
    for (i, s) in samples.iter_mut().enumerate() {
        *s = spectrum[i].re * scale;
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests;
