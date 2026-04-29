//! Multi-channel synthetic IQ test for `ChannelBank`.
//!
//! Aspirational target: build a 2.5 `MSps` IQ buffer carrying two
//! MSK transmissions on distinct ACARS frequencies and assert
//! that both decode into messages on the right channels with no
//! cross-talk. That's deferred (see the IMPLEMENTER comment at
//! the bottom): faithfully synthesizing ACARS-grade MSK in test
//! code requires building a proper frame (parity + CRC) and an
//! MSK-modulated 12.5 kHz audio waveform, then upsampling +
//! mixing onto the IQ buffer. The acarsdec reference doesn't
//! ship a synthesizer, so we'd be writing one from spec.
//!
//! What this file actually does today: a noise-only sanity
//! check. Pure white noise at the source rate must NOT produce
//! decoded messages — if it does, the `ChannelBank` is
//! false-positiving and that's a bug worth chasing. This is
//! supplementary; the e2e test against `acarsdec`'s `test.wav`
//! (see `e2e_acarsdec_compat.rs`) is the definitive correctness
//! oracle.

use num_complex::Complex32;
use sdr_acars::ChannelBank;

// Corrected airband geometry: 2.5 MSps source rate, centered on
// 130.3375 MHz (the midpoint of the US-6 ACARS channel extremes).
const SOURCE_RATE_HZ: f64 = 2_500_000.0;
const CENTER_HZ: f64 = 130_337_500.0;

/// Synthesize `seconds` of complex IQ at `SOURCE_RATE_HZ`
/// containing nothing but low-level white noise. Used as a
/// baseline: noise alone must not produce decoded messages.
fn synth_noise(seconds: f64) -> Vec<Complex32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = (seconds * SOURCE_RATE_HZ) as usize;
    let mut out = Vec::with_capacity(n);
    // Deterministic LCG (Knuth's MMIX constants) so the test is
    // reproducible across platforms and CI runs.
    let mut s: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        #[allow(clippy::cast_precision_loss)]
        let i = (s as f32) / (u64::MAX as f32) - 0.5;
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        #[allow(clippy::cast_precision_loss)]
        let q = (s as f32) / (u64::MAX as f32) - 0.5;
        out.push(Complex32::new(i * 0.01, q * 0.01)); // ~-40 dBFS noise
    }
    out
}

#[test]
#[allow(clippy::panic)]
fn pure_noise_produces_no_messages() {
    let mut bank = ChannelBank::new(SOURCE_RATE_HZ, CENTER_HZ, &[131_550_000.0, 131_525_000.0])
        .expect("valid 2-channel config");
    let noise = synth_noise(2.0);
    bank.process(&noise, |msg| {
        panic!("noise should not decode: {msg:?}");
    });
}

// IMPLEMENTER NOTE: a proper "decode a synthesized MSK signal"
// test would build a 2400-baud MSK waveform on top of one of the
// channel offsets, confirm decode happens on that channel, and
// confirm the OTHER channel stays silent. Synthesis takes:
//
//   1. Build a proper ACARS frame: SYN+SYN+SOH+Mode+Addr+ACK+
//      Label+BlockID+STX+text+ETX+CRC (with odd parity per
//      character and a frame-CRC at the end).
//   2. MSK-encode each bit at 1200/2400 Hz tones, 12.5 kHz audio
//      sample rate.
//   3. Upsample to SOURCE_RATE_HZ via zero-stuff + LPF.
//   4. Mix to channel offset (multiply by complex exp at offset).
//   5. Sum onto the IQ buffer.
//
// Step 2 is the intricate part. The acarsdec ref doesn't ship a
// synthesizer, so we'd be writing one from spec. Deferred — the
// e2e test against acarsdec's test.wav is sufficient correctness
// coverage; build this only if a future bug requires it.
