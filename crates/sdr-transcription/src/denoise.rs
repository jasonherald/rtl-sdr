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
mod tests {
    use super::*;

    #[test]
    fn silence_stays_silent() {
        let mut buf = vec![0.0_f32; TEST_SILENCE_LEN];
        spectral_denoise(&mut buf, GATE_RATIO);
        for s in &buf {
            assert!(s.abs() < 1e-6, "expected silence, got {s}");
        }
    }

    #[test]
    fn short_buffer_is_noop() {
        let mut buf = vec![0.5_f32; MIN_FFT_LEN - 1];
        let original = buf.clone();
        spectral_denoise(&mut buf, GATE_RATIO);
        assert_eq!(buf, original);
    }

    #[test]
    fn strong_tone_survives_gate() {
        // Generate a strong 1 kHz tone at 16 kHz sample rate.
        let n = TEST_SIGNAL_LEN;
        let mut buf: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                (2.0 * std::f32::consts::PI * 1000.0 * t).sin()
            })
            .collect();

        // Add weak noise.
        for (i, s) in buf.iter_mut().enumerate() {
            *s += 0.01 * ((i * 7 % 13) as f32 / 13.0 - 0.5);
        }

        let pre_energy: f32 = buf.iter().map(|s| s * s).sum();

        spectral_denoise(&mut buf, GATE_RATIO);

        let post_energy: f32 = buf.iter().map(|s| s * s).sum();

        // The tone should retain most of its energy (at least 80%).
        assert!(
            post_energy > pre_energy * 0.8,
            "tone lost too much energy: pre={pre_energy}, post={post_energy}"
        );
    }

    // --- Voice-band weight + enhance_speech tests (issue #274) ---

    // Assertion thresholds for the enhance_speech test suite. Centralized
    // here so tuning the weight shape or the gate ratio only requires
    // touching one set of numbers. The thresholds themselves encode
    // invariants of the voice-band algorithm, not implementation details,
    // so they should change only when the algorithm's guarantees change.

    /// Minimum fraction of input energy an in-band tone must retain
    /// after `enhance_speech`. At weight 1.0 and a survivable noise
    /// floor the output should be nearly the full input; 0.8 gives
    /// slack for FFT numerical bleed and the 1% random noise added in
    /// the test to avoid an all-peak-one-bin pathological case.
    const IN_BAND_ENERGY_RETENTION_MIN: f32 = 0.8;

    /// Maximum fraction of input energy an out-of-band tone may leave
    /// in the output. Out-of-band weights are 0 so the ideal is
    /// exactly zero; 0.01 tolerates numerical FFT bleed from the
    /// nominal zeroing.
    const OUT_OF_BAND_ENERGY_MAX_FRACTION: f32 = 0.01;

    /// Minimum pre-enhancement energy required for a test input to
    /// count as "real signal" rather than numerical dust. Used as a
    /// setup sanity check in the kill tests.
    const MIN_SETUP_INPUT_ENERGY: f32 = 0.1;

    /// In the masking regression, the 1 kHz formant must retain at
    /// least this fraction of its pre-enhancement power. With weight
    /// 1.0 in the formant band and the voice-prior noise floor
    /// excluding the rumble, the formant should survive largely
    /// unattenuated; 0.5 is the generous lower bound.
    const FORMANT_POWER_RETENTION_MIN: f32 = 0.5;

    /// In the masking regression, the post-enhancement 1 kHz formant
    /// must dominate residual 50 Hz rumble by at least this factor.
    /// 5× is well above the 1:1 crossover and comfortably below the
    /// theoretical ∞:1 (rumble at weight 0 should be fully zeroed).
    const FORMANT_TO_RUMBLE_DOMINANCE_MIN: f32 = 5.0;

    /// Pre-check: the masking test's input buffer must have rumble
    /// genuinely dominating the formant component so the test's
    /// "masked" premise is real. Input is rumble amp 1.0 + formant
    /// amp 0.1, so power ratio is 100× — 50× gives slack for the
    /// Goertzel projection's numerical precision at a specific
    /// frequency vs. a general FFT bin.
    const SETUP_RUMBLE_DOMINANCE_MIN: f32 = 50.0;

    /// Equality tolerance for `voice_band_weight` breakpoint tests.
    /// The function is piecewise linear with f32 arithmetic so a
    /// strict `==` comparison would be brittle under compiler
    /// reordering; 1e-6 is several orders of magnitude below any
    /// weight the function produces.
    const WEIGHT_EQ_EPS: f32 = 1e-6;

    /// Sub-Hz offset used to probe the "just below a breakpoint"
    /// side of each piecewise boundary. The `voice_band_weight`
    /// function has no internal snap-to-zero behavior so any
    /// offset smaller than the width of the narrowest region
    /// works; 0.1 Hz is visually obvious in assertion messages.
    const BREAKPOINT_OFFSET_HZ: f32 = 0.1;

    /// Default test signal length in samples — 1600 at 16 kHz =
    /// 100 ms. Long enough to put the FFT bin spacing at ~10 Hz
    /// (16000/1600), which resolves every voice-band breakpoint
    /// cleanly while keeping the test suite fast.
    const TEST_SIGNAL_LEN: usize = 1600;

    /// Test buffer length for the pure-silence pass-through test.
    /// Just needs to exceed `MIN_FFT_LEN` so `spectral_denoise` /
    /// `enhance_speech` take the FFT path instead of the short-
    /// buffer early return.
    const TEST_SILENCE_LEN: usize = 256;

    /// Window below 1.0 for the non-unity weight regression test.
    /// Output power at a weighted bin is `w² × input_power`, so a
    /// weight of 0.5 should produce ~0.25× input power. The window
    /// is tolerant of FFT numerical bleed.
    const NON_UNITY_POWER_RATIO_MIN: f32 = 0.20;
    /// Upper bound on the non-unity power ratio — weight × weight
    /// plus headroom for the gate's percentile-based floor to not
    /// over-gate a bin that should pass.
    const NON_UNITY_POWER_RATIO_MAX: f32 = 0.30;

    /// Generate `n` samples of a unit-amplitude sine at `freq_hz` at 16 kHz.
    fn tone(freq_hz: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE_HZ;
                (2.0 * std::f32::consts::PI * freq_hz * t).sin()
            })
            .collect()
    }

    /// Sum-of-squares energy of a signal buffer.
    fn energy(buf: &[f32]) -> f32 {
        buf.iter().map(|s| s * s).sum()
    }

    /// Goertzel-style power projection onto a single frequency.
    ///
    /// Computes `|Σ x[i] * exp(-j 2π f i / fs)|²` without a full FFT,
    /// so a test can measure the exact power at a specific frequency
    /// rather than relying on total-energy heuristics. Output is
    /// proportional to the squared magnitude of the FFT bin nearest
    /// `freq_hz` — the same physical quantity `enhance_speech` and
    /// `spectral_denoise` gate against.
    fn power_at(buf: &[f32], freq_hz: f32) -> f32 {
        let mut re = 0.0_f32;
        let mut im = 0.0_f32;
        for (i, &x) in buf.iter().enumerate() {
            let phase = 2.0 * std::f32::consts::PI * freq_hz * (i as f32) / SAMPLE_RATE_HZ;
            re += x * phase.cos();
            im -= x * phase.sin();
        }
        re * re + im * im
    }

    #[test]
    fn voice_band_weight_at_breakpoints() {
        // All breakpoint tests reference the production `VOICE_F_*`
        // constants so a future retune can't leave this test
        // validating a stale policy. Fixed literals (20.0, 200.0,
        // 1_000.0, 8_000.0) are interior probes that stay valid
        // regardless of where the boundaries move, as long as the
        // band structure keeps at least: one sub-cut interior, one
        // fundamentals interior, one formant interior, one above-
        // sibilance interior.

        // Below sub-cut: hard zero.
        assert!((voice_band_weight(20.0) - 0.0).abs() < WEIGHT_EQ_EPS);
        assert!(
            (voice_band_weight(VOICE_F_SUB_HZ - BREAKPOINT_OFFSET_HZ) - 0.0).abs() < WEIGHT_EQ_EPS
        );

        // Fundamentals region: constant VOICE_W_FUND.
        assert!((voice_band_weight(VOICE_F_SUB_HZ) - VOICE_W_FUND).abs() < WEIGHT_EQ_EPS);
        assert!((voice_band_weight(200.0) - VOICE_W_FUND).abs() < WEIGHT_EQ_EPS);
        assert!(
            (voice_band_weight(VOICE_F_FUND_HZ - BREAKPOINT_OFFSET_HZ) - VOICE_W_FUND).abs()
                < WEIGHT_EQ_EPS
        );

        // Formant band: full weight.
        assert!((voice_band_weight(VOICE_F_FUND_HZ) - 1.0).abs() < WEIGHT_EQ_EPS);
        assert!((voice_band_weight(1_000.0) - 1.0).abs() < WEIGHT_EQ_EPS);
        assert!(
            (voice_band_weight(VOICE_F_FORMANT_HI_HZ - BREAKPOINT_OFFSET_HZ) - 1.0).abs()
                < WEIGHT_EQ_EPS
        );

        // Sibilance ramp: linear 1.0 → VOICE_W_SIB_END.
        assert!((voice_band_weight(VOICE_F_FORMANT_HI_HZ) - 1.0).abs() < WEIGHT_EQ_EPS);
        let midpoint = 0.5_f32.mul_add(VOICE_W_SIB_END - 1.0, 1.0);
        let mid_freq = (VOICE_F_FORMANT_HI_HZ + VOICE_F_SIB_HI_HZ) * 0.5;
        assert!(
            (voice_band_weight(mid_freq) - midpoint).abs() < WEIGHT_EQ_EPS,
            "mid-ramp should be halfway between 1.0 and VOICE_W_SIB_END"
        );

        // Above sibilance cutoff: hard zero.
        assert!((voice_band_weight(VOICE_F_SIB_HI_HZ) - 0.0).abs() < WEIGHT_EQ_EPS);
        assert!((voice_band_weight(8_000.0) - 0.0).abs() < WEIGHT_EQ_EPS);
    }

    #[test]
    fn enhance_speech_preserves_formant_band_tone() {
        // A 1 kHz tone is smack in the middle of the formant band —
        // weight 1.0, threshold should let it pass, and the output
        // weight is also 1.0 so the magnitude is preserved.
        let n = TEST_SIGNAL_LEN;
        let mut buf = tone(1_000.0, n);
        // Add weak noise.
        for (i, s) in buf.iter_mut().enumerate() {
            *s += 0.01 * ((i * 7 % 13) as f32 / 13.0 - 0.5);
        }

        let pre = energy(&buf);
        enhance_speech(&mut buf, GATE_RATIO);
        let post = energy(&buf);

        assert!(
            post > pre * IN_BAND_ENERGY_RETENTION_MIN,
            "formant-band tone lost too much energy: pre={pre}, post={post}"
        );
    }

    #[test]
    fn enhance_speech_kills_sub_80hz_rumble() {
        // A 50 Hz tone (AC hum, HVAC rumble, CTCSS leakage) — weight
        // is zero, should be gated to silence.
        let n = TEST_SIGNAL_LEN;
        let mut buf = tone(50.0, n);
        let pre = energy(&buf);
        assert!(
            pre > MIN_SETUP_INPUT_ENERGY,
            "setup: input should have real energy"
        );

        enhance_speech(&mut buf, GATE_RATIO);
        let post = energy(&buf);

        // Output should be near-zero. Use a tolerant bound to allow
        // FFT numerical bleed.
        assert!(
            post < pre * OUT_OF_BAND_ENERGY_MAX_FRACTION,
            "sub-80Hz rumble should be killed: pre={pre}, post={post}"
        );
    }

    #[test]
    fn enhance_speech_kills_above_7500hz_hiss() {
        // A 7.8 kHz tone — above VOICE_F_SIB_HI_HZ, weight is zero,
        // should be gated to silence regardless of amplitude.
        let n = TEST_SIGNAL_LEN;
        let mut buf = tone(7_800.0, n);
        let pre = energy(&buf);
        assert!(
            pre > MIN_SETUP_INPUT_ENERGY,
            "setup: input should have real energy"
        );

        enhance_speech(&mut buf, GATE_RATIO);
        let post = energy(&buf);

        assert!(
            post < pre * OUT_OF_BAND_ENERGY_MAX_FRACTION,
            "above-7500Hz hiss should be killed: pre={pre}, post={post}"
        );
    }

    #[test]
    fn enhance_speech_silence_stays_silent() {
        let mut buf = vec![0.0_f32; TEST_SILENCE_LEN];
        enhance_speech(&mut buf, GATE_RATIO);
        for s in &buf {
            assert!(s.abs() < 1e-6, "expected silence, got {s}");
        }
    }

    #[test]
    fn enhance_speech_short_buffer_is_noop() {
        let mut buf = vec![0.5_f32; MIN_FFT_LEN - 1];
        let original = buf.clone();
        enhance_speech(&mut buf, GATE_RATIO);
        assert_eq!(buf, original);
    }

    #[test]
    fn enhance_speech_in_band_wins_over_louder_out_of_band() {
        // Regression test for the voice-prior noise floor: a quiet
        // 1 kHz tone in the formant band should survive even when a
        // much louder 50 Hz rumble is present. The broadband gate
        // (spectral_denoise) would let the rumble drag the noise
        // floor up and could gate the quieter formant tone out.
        let n = TEST_SIGNAL_LEN;
        let rumble = tone(50.0, n);
        let formant = tone(1_000.0, n);

        // Build: rumble at amplitude 1.0 + formant at amplitude 0.1.
        let mut buf: Vec<f32> = rumble
            .iter()
            .zip(formant.iter())
            .map(|(&r, &f)| r + 0.1 * f)
            .collect();

        // Record the pre-enhancement input power at each frequency
        // so the post-enhancement assertion can compare against a
        // real baseline, not just absolute thresholds.
        let p_rumble_pre = power_at(&buf, 50.0);
        let p_formant_pre = power_at(&buf, 1_000.0);
        assert!(
            p_rumble_pre > p_formant_pre * SETUP_RUMBLE_DOMINANCE_MIN,
            "setup: rumble should initially dominate formant by >{SETUP_RUMBLE_DOMINANCE_MIN}x (rumble amp=1.0 vs formant amp=0.1 → 100x power)"
        );

        enhance_speech(&mut buf, GATE_RATIO);

        // Post-enhancement: the 1 kHz formant must survive AND
        // dominate the residual 50 Hz rumble. Goertzel projection
        // gives us the exact power at each frequency, so we can
        // assert both that the formant survived and that the
        // spectral dominance flipped.
        let p_rumble_post = power_at(&buf, 50.0);
        let p_formant_post = power_at(&buf, 1_000.0);

        assert!(
            p_formant_post > p_formant_pre * FORMANT_POWER_RETENTION_MIN,
            "1 kHz formant should retain >{}% of its input power after voice-band gating: pre={p_formant_pre}, post={p_formant_post}",
            FORMANT_POWER_RETENTION_MIN * 100.0
        );
        assert!(
            p_formant_post > p_rumble_post * FORMANT_TO_RUMBLE_DOMINANCE_MIN,
            "1 kHz formant should dominate residual 50 Hz rumble by >{FORMANT_TO_RUMBLE_DOMINANCE_MIN}x after enhancement: p_formant={p_formant_post}, p_rumble={p_rumble_post}"
        );
    }

    #[test]
    fn enhance_speech_scales_surviving_fundamental_band_tone_by_weight() {
        // Regression coverage for the survivor-scaling path at
        // non-unity weights. A 200 Hz tone is in the fundamentals
        // band (weight VOICE_W_FUND = 0.5). If it survives the
        // gate — it should, because it's the only bin in the
        // buffer and therefore trivially above the percentile
        // floor — the spectrum bin gets multiplied by the weight.
        //
        // Output power at 200 Hz should be approximately
        // `(VOICE_W_FUND)² * input_power` because:
        //   - Forward FFT bin magnitude scales linearly with input
        //     amplitude.
        //   - We multiply the bin by `weight` before the inverse
        //     FFT.
        //   - Output amplitude scales linearly with the scaled
        //     bin.
        //   - Output *power* is amplitude squared.
        //
        // With `VOICE_W_FUND = 0.5`, the expected ratio is 0.25 ±
        // tolerance for FFT numerical bleed and percentile-based
        // threshold interactions.
        let n = TEST_SIGNAL_LEN;
        let mut buf = tone(200.0, n);
        let p_input = power_at(&buf, 200.0);
        assert!(
            p_input > MIN_SETUP_INPUT_ENERGY,
            "setup: input should have real energy at 200 Hz"
        );

        enhance_speech(&mut buf, GATE_RATIO);

        let p_output = power_at(&buf, 200.0);
        let ratio = p_output / p_input;

        // Sanity: the tone must not have been gated to zero —
        // that would be a separate regression, not a scaling one.
        assert!(
            p_output > 0.0,
            "fundamentals-band tone should survive the gate, not be zeroed: p_output={p_output}"
        );

        // The interesting part: the survivor must have been
        // scaled by `VOICE_W_FUND` (not left unscaled). A future
        // change that drops the `spectrum[i] *= *weight` line
        // would push this ratio to ~1.0 and fail the upper
        // bound.
        assert!(
            (NON_UNITY_POWER_RATIO_MIN..=NON_UNITY_POWER_RATIO_MAX).contains(&ratio),
            "200 Hz survivor should be scaled to approximately VOICE_W_FUND² = {}× input power, got ratio={ratio} (p_input={p_input}, p_output={p_output})",
            VOICE_W_FUND * VOICE_W_FUND
        );
    }

    // ─── AudioEnhancement dispatcher tests ──────────────────────
    //
    // The dispatcher is a thin routing function, but pinning its
    // behavior matters: a future refactor that accidentally swaps
    // the VoiceBand / Broadband branches would be caught here, and
    // the config-string round-trip is load-bearing for persistence.

    /// Helper: build a test signal that all three modes can process
    /// and compare — a sum of three tones at 100 Hz (fundamental
    /// band), 1000 Hz (formant band), and 6000 Hz (sibilance band).
    /// 4096 samples at 16 kHz = 256 ms, comfortably above
    /// `MIN_FFT_LEN`.
    fn three_tone_signal() -> Vec<f32> {
        let n = 4096;
        let mut buf = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / SAMPLE_RATE_HZ;
            let sample = 0.3 * (2.0 * std::f32::consts::PI * 100.0 * t).sin()
                + 0.3 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin()
                + 0.3 * (2.0 * std::f32::consts::PI * 6000.0 * t).sin();
            buf.push(sample);
        }
        buf
    }

    #[test]
    fn audio_enhancement_off_is_identity() {
        // Off mode must leave the buffer byte-identical. This is
        // load-bearing for users who want to feed pre-cleaned audio
        // directly to the recognizer without any spectral gate.
        let input = three_tone_signal();
        let mut buf = input.clone();
        apply(&mut buf, AudioEnhancement::Off, GATE_RATIO);
        assert_eq!(
            buf, input,
            "Off mode must not mutate the input buffer in any way"
        );
    }

    #[test]
    fn audio_enhancement_voice_band_matches_enhance_speech() {
        // VoiceBand must route to enhance_speech. A bit-exact
        // comparison against a side-by-side enhance_speech call on
        // an identical input buffer pins the dispatch — a future
        // refactor that silently swapped the VoiceBand branch to a
        // different function would produce different output and
        // fail this assertion.
        let input = three_tone_signal();
        let mut via_apply = input.clone();
        let mut via_direct = input.clone();
        apply(&mut via_apply, AudioEnhancement::VoiceBand, GATE_RATIO);
        enhance_speech(&mut via_direct, GATE_RATIO);
        assert_eq!(
            via_apply, via_direct,
            "VoiceBand dispatcher must produce bit-identical output to enhance_speech"
        );
    }

    #[test]
    fn audio_enhancement_broadband_matches_spectral_denoise() {
        // Same contract for Broadband → spectral_denoise.
        let input = three_tone_signal();
        let mut via_apply = input.clone();
        let mut via_direct = input.clone();
        apply(&mut via_apply, AudioEnhancement::Broadband, GATE_RATIO);
        spectral_denoise(&mut via_direct, GATE_RATIO);
        assert_eq!(
            via_apply, via_direct,
            "Broadband dispatcher must produce bit-identical output to spectral_denoise"
        );
    }

    #[test]
    fn audio_enhancement_modes_produce_different_outputs() {
        // Sanity check that the three modes actually differ on the
        // same input — if this ever asserts `==` the tests above
        // are comparing against themselves and would silently pass
        // even with a busted dispatcher.
        let input = three_tone_signal();
        let mut off = input.clone();
        let mut voice = input.clone();
        let mut broad = input.clone();
        apply(&mut off, AudioEnhancement::Off, GATE_RATIO);
        apply(&mut voice, AudioEnhancement::VoiceBand, GATE_RATIO);
        apply(&mut broad, AudioEnhancement::Broadband, GATE_RATIO);
        assert_ne!(
            off, voice,
            "Off and VoiceBand should differ on a noisy signal"
        );
        assert_ne!(
            off, broad,
            "Off and Broadband should differ on a noisy signal"
        );
        assert_ne!(
            voice, broad,
            "VoiceBand and Broadband should differ on a signal with out-of-voice content"
        );
    }

    #[test]
    fn audio_enhancement_config_str_round_trip() {
        // as_config_str ↔ from_config_str must round-trip for all
        // three variants.
        for mode in [
            AudioEnhancement::VoiceBand,
            AudioEnhancement::Broadband,
            AudioEnhancement::Off,
        ] {
            let s = mode.as_config_str();
            let parsed = AudioEnhancement::from_config_str(s);
            assert_eq!(parsed, mode, "round-trip failed for {mode:?} via {s:?}");
        }
    }

    #[test]
    fn audio_enhancement_config_str_unknown_falls_back_to_default() {
        // Unknown / stale / typo config values must fall back to
        // the default, not error. This matters because a missing
        // audio-enhancement key in an old config file (from before
        // this feature shipped) will deserialize to an empty
        // string which should land on VoiceBand, not some noisy
        // error.
        assert_eq!(
            AudioEnhancement::from_config_str(""),
            AudioEnhancement::default()
        );
        assert_eq!(
            AudioEnhancement::from_config_str("nonsense"),
            AudioEnhancement::default()
        );
        assert_eq!(
            AudioEnhancement::from_config_str("VoiceBand"), // wrong case
            AudioEnhancement::default()
        );
        assert_eq!(AudioEnhancement::default(), AudioEnhancement::VoiceBand);
    }
}
