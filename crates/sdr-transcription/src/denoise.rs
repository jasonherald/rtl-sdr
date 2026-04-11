//! FFT-based spectral noise gate for cleaning radio audio before transcription.
//!
//! Estimates the noise floor from the quietest frequency bins, then zeros out
//! bins that fall below `noise_floor + margin`. This removes broadband static,
//! squelch tail hiss, and low-level interference while preserving speech.
//!
//! Based on the FFT → identify noise → zero bins → IFFT approach from
//! Tariq & Khan (2023), "Mathematical Approach for Enhancing Audio Signal
//! Quality: Theory, Insights, and Applications."

use rustfft::{num_complex::Complex, FftPlanner};

/// Margin above the estimated noise floor in linear magnitude.
/// Bins below `noise_floor * GATE_RATIO` are zeroed.
/// A ratio of 3.0 means bins must be 3x the noise floor to survive (~9.5 dB).
const GATE_RATIO: f32 = 3.0;

/// Percentile of magnitude-sorted bins used to estimate the noise floor.
/// 0.2 means the bottom 20% of bins define the noise level.
const NOISE_FLOOR_PERCENTILE: f32 = 0.20;

/// Apply spectral noise gating to a mono f32 audio buffer in-place.
///
/// The buffer is FFT'd, noise floor is estimated from the quietest bins,
/// bins below the threshold are zeroed, then IFFT'd back to time domain.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn spectral_denoise(samples: &mut [f32]) {
    let n = samples.len();
    if n < 64 {
        return; // too short for meaningful FFT
    }

    let mut planner = FftPlanner::new();
    let fft_fwd = planner.plan_fft_forward(n);
    let fft_inv = planner.plan_fft_inverse(n);

    // Convert to complex for FFT.
    let mut spectrum: Vec<Complex<f32>> = samples
        .iter()
        .map(|&s| Complex::new(s, 0.0))
        .collect();

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
    let threshold = noise_floor * GATE_RATIO;

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

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    #[test]
    fn silence_stays_silent() {
        let mut buf = vec![0.0_f32; 256];
        spectral_denoise(&mut buf);
        for s in &buf {
            assert!(s.abs() < 1e-6, "expected silence, got {s}");
        }
    }

    #[test]
    fn short_buffer_is_noop() {
        let mut buf = vec![0.5_f32; 32];
        let original = buf.clone();
        spectral_denoise(&mut buf);
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

        spectral_denoise(&mut buf);

        let post_energy: f32 = buf.iter().map(|s| s * s).sum();

        // The tone should retain most of its energy (at least 80%).
        assert!(
            post_energy > pre_energy * 0.8,
            "tone lost too much energy: pre={pre_energy}, post={post_energy}"
        );
    }
}
