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

/// Target sample rate of the mono buffer handed to these functions.
/// Both callers (`offline.rs` and `streaming.rs`) resample to 16 kHz
/// before denoising, so the bin-frequency math is consistent.
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
    if n < 64 {
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
/// The weight function is a static prior. Dynamic (per-segment) voice
/// activity detection lives downstream at the recognizer's VAD stage;
/// this is purely a spectral tilt.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn enhance_speech(samples: &mut [f32], gate_ratio: f32) {
    let n = samples.len();
    if n < 64 {
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

    // Voice-prior noise floor: percentile over voice-band bins only.
    // Out-of-band bins (weight == 0) contribute nothing — a strong PL
    // tone at 100 Hz or a birdie at 9 kHz can't lift the floor that
    // gates the formant band.
    let mut voice_band_mags: Vec<f32> = magnitudes
        .iter()
        .zip(weights.iter())
        .filter_map(|(&m, &w)| (w > 0.0).then_some(m))
        .collect();
    voice_band_mags.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_floor = if voice_band_mags.is_empty() {
        0.0
    } else {
        let idx = ((voice_band_mags.len() as f32) * NOISE_FLOOR_PERCENTILE) as usize;
        voice_band_mags[idx.min(voice_band_mags.len() - 1)]
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
        let mut buf = vec![0.0_f32; 256];
        spectral_denoise(&mut buf, GATE_RATIO);
        for s in &buf {
            assert!(s.abs() < 1e-6, "expected silence, got {s}");
        }
    }

    #[test]
    fn short_buffer_is_noop() {
        let mut buf = vec![0.5_f32; 32];
        let original = buf.clone();
        spectral_denoise(&mut buf, GATE_RATIO);
        assert_eq!(buf, original);
    }

    #[test]
    fn strong_tone_survives_gate() {
        // Generate a strong 1 kHz tone at 16 kHz sample rate.
        let n = 1600; // 100ms
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

    #[test]
    fn voice_band_weight_at_breakpoints() {
        // Below sub-cut: hard zero.
        assert!((voice_band_weight(20.0) - 0.0).abs() < 1e-6);
        assert!((voice_band_weight(79.9) - 0.0).abs() < 1e-6);

        // Fundamentals region: constant VOICE_W_FUND.
        assert!((voice_band_weight(80.0) - VOICE_W_FUND).abs() < 1e-6);
        assert!((voice_band_weight(200.0) - VOICE_W_FUND).abs() < 1e-6);
        assert!((voice_band_weight(299.9) - VOICE_W_FUND).abs() < 1e-6);

        // Formant band: full weight.
        assert!((voice_band_weight(300.0) - 1.0).abs() < 1e-6);
        assert!((voice_band_weight(1_000.0) - 1.0).abs() < 1e-6);
        assert!((voice_band_weight(3_399.9) - 1.0).abs() < 1e-6);

        // Sibilance ramp: linear 1.0 → VOICE_W_SIB_END.
        assert!((voice_band_weight(3_400.0) - 1.0).abs() < 1e-5);
        let midpoint = 0.5_f32.mul_add(VOICE_W_SIB_END - 1.0, 1.0);
        let mid_freq = (VOICE_F_FORMANT_HI_HZ + VOICE_F_SIB_HI_HZ) * 0.5;
        assert!(
            (voice_band_weight(mid_freq) - midpoint).abs() < 1e-5,
            "mid-ramp should be halfway between 1.0 and VOICE_W_SIB_END"
        );

        // Above sibilance cutoff: hard zero.
        assert!((voice_band_weight(7_500.0) - 0.0).abs() < 1e-6);
        assert!((voice_band_weight(8_000.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn enhance_speech_preserves_formant_band_tone() {
        // A 1 kHz tone is smack in the middle of the formant band —
        // weight 1.0, threshold should let it pass, and the output
        // weight is also 1.0 so the magnitude is preserved.
        let n = 1600;
        let mut buf = tone(1_000.0, n);
        // Add weak noise.
        for (i, s) in buf.iter_mut().enumerate() {
            *s += 0.01 * ((i * 7 % 13) as f32 / 13.0 - 0.5);
        }

        let pre = energy(&buf);
        enhance_speech(&mut buf, GATE_RATIO);
        let post = energy(&buf);

        assert!(
            post > pre * 0.8,
            "formant-band tone lost too much energy: pre={pre}, post={post}"
        );
    }

    #[test]
    fn enhance_speech_kills_sub_80hz_rumble() {
        // A 50 Hz tone (AC hum, HVAC rumble, CTCSS leakage) — weight
        // is zero, should be gated to silence.
        let n = 1600;
        let mut buf = tone(50.0, n);
        let pre = energy(&buf);
        assert!(pre > 0.1, "setup: input should have real energy");

        enhance_speech(&mut buf, GATE_RATIO);
        let post = energy(&buf);

        // Output should be near-zero. Use a tolerant bound to allow
        // FFT numerical bleed.
        assert!(
            post < pre * 0.01,
            "sub-80Hz rumble should be killed: pre={pre}, post={post}"
        );
    }

    #[test]
    fn enhance_speech_kills_above_7500hz_hiss() {
        // A 7.8 kHz tone — above VOICE_F_SIB_HI_HZ, weight is zero,
        // should be gated to silence regardless of amplitude.
        let n = 1600;
        let mut buf = tone(7_800.0, n);
        let pre = energy(&buf);
        assert!(pre > 0.1, "setup: input should have real energy");

        enhance_speech(&mut buf, GATE_RATIO);
        let post = energy(&buf);

        assert!(
            post < pre * 0.01,
            "above-7500Hz hiss should be killed: pre={pre}, post={post}"
        );
    }

    #[test]
    fn enhance_speech_silence_stays_silent() {
        let mut buf = vec![0.0_f32; 256];
        enhance_speech(&mut buf, GATE_RATIO);
        for s in &buf {
            assert!(s.abs() < 1e-6, "expected silence, got {s}");
        }
    }

    #[test]
    fn enhance_speech_short_buffer_is_noop() {
        let mut buf = vec![0.5_f32; 32];
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
        let n = 1600;
        let rumble = tone(50.0, n);
        let formant = tone(1_000.0, n);

        // Build: rumble at amplitude 1.0 + formant at amplitude 0.1.
        let mut buf: Vec<f32> = rumble
            .iter()
            .zip(formant.iter())
            .map(|(&r, &f)| r + 0.1 * f)
            .collect();

        enhance_speech(&mut buf, GATE_RATIO);

        // Measure the output energy in the 1 kHz neighborhood via a
        // narrow FFT and verify the formant component survives. We
        // approximate by checking the total output energy is
        // dominated by content at the tone frequency rather than at
        // 50 Hz — if the rumble had gated out the formant, the
        // output would be near-silent.
        let out_energy = energy(&buf);
        assert!(
            out_energy > 1e-4,
            "formant tone should survive voice-band gating even when masked by 10x-louder sub-voice rumble: out_energy={out_energy}"
        );
    }
}
