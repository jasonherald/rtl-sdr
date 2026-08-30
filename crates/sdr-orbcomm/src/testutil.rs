//! Shared synthesis helpers for the crate's loopback/decode tests.
//!
//! `demod::tests` and `channelizer::tests` both build synthetic SDPSK
//! waveforms and need the same deterministic noise/CFO impairments applied
//! to them; this module is the single source of truth for that so the two
//! test suites can't drift apart on seed handling or noise convention.

use crate::CHANNEL_SAMPLE_RATE_HZ;
use sdr_types::Complex;

/// Deterministic xorshift64* PRNG — tests must never be flaky.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `(0, 1)`.
    pub(crate) fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0
    }

    /// Standard normal via Box–Muller.
    pub(crate) fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Add complex AWGN at the requested per-sample SNR (in-band, at
/// [`CHANNEL_SAMPLE_RATE_HZ`]). Non-mutating: returns a new signal, leaving
/// `samples` untouched.
pub(crate) fn add_awgn(samples: &[Complex], snr_db: f64, seed: u64) -> Vec<Complex> {
    if samples.is_empty() {
        return Vec::new();
    }
    let signal_power = samples
        .iter()
        .map(|s| f64::from(s.re) * f64::from(s.re) + f64::from(s.im) * f64::from(s.im))
        .sum::<f64>()
        / samples.len() as f64;
    let noise_power = signal_power / 10.0_f64.powf(snr_db / 10.0);
    let sigma = (noise_power / 2.0).sqrt();
    let mut rng = Rng::new(seed);
    samples
        .iter()
        .map(|s| {
            Complex::new(
                s.re + (sigma * rng.next_normal()) as f32,
                s.im + (sigma * rng.next_normal()) as f32,
            )
        })
        .collect()
}

/// Apply a constant carrier-frequency offset at [`CHANNEL_SAMPLE_RATE_HZ`].
/// Non-mutating: returns a new signal, leaving `samples` untouched.
pub(crate) fn apply_cfo(samples: &[Complex], cfo_hz: f64) -> Vec<Complex> {
    let w = 2.0 * std::f64::consts::PI * cfo_hz / CHANNEL_SAMPLE_RATE_HZ;
    samples
        .iter()
        .enumerate()
        .map(|(n, s)| {
            let phase = w * n as f64;
            let rot = Complex::new(phase.cos() as f32, phase.sin() as f32);
            *s * rot
        })
        .collect()
}
