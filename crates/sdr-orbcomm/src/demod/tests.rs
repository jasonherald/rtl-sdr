use super::*;
use crate::CHANNEL_SAMPLE_RATE_HZ;
use sdr_dsp::multirate::RationalResampler;

/// Bits skipped at the head of the recovered stream to let the timing loop settle.
const SETTLE_BITS: usize = 128;
/// Alignment offsets searched when matching recovered bits to transmitted bits.
const MAX_ALIGN_SEARCH: usize = SETTLE_BITS + 32;
/// Bit-error rate a passing loopback must stay under.
const MAX_BER: f64 = 0.005;
/// Oversampling factor used by the clock-offset helper.
const PPM_OVERSAMPLE: usize = 10;

/// Deterministic xorshift64* PRNG — tests must never be flaky.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `(0, 1)`.
    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0
    }

    /// Standard normal via Box–Muller.
    fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// The spec-literal transmitter at the channel rate's 4 samples per symbol.
fn modulate_sdpsk(bits: &[bool]) -> Vec<Complex> {
    modulate_sdpsk_at_sps(bits, SAMPLES_PER_SYMBOL)
}

fn apply_cfo(samples: &[Complex], cfo_hz: f64) -> Vec<Complex> {
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

/// Add complex AWGN at the requested per-sample SNR (in-band, at the channel rate).
fn add_awgn(samples: &[Complex], snr_db: f64, seed: u64) -> Vec<Complex> {
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

/// Simulate a sample-clock error of `ppm` parts per million.
///
/// Sign: output sample `m` is drawn from input time `m · (1 + ppm·1e-6)`, so a
/// *positive* `ppm` stretches the sampling grid — the receiver clock runs slow and
/// the demodulator sees slightly *fewer* than 4 samples per symbol (its tracked
/// `sps` settles below nominal). A negative `ppm` does the opposite. Both signs are
/// exercised, so this only matters when reading a single case.
///
/// An exact 1 + 50e-6 ratio through [`RationalResampler`] alone would need
/// `interp = 19_201` polyphase branches over a multi-million-tap prototype, so the
/// resampler does the tractable part — a clean 10× oversample — and the fractional
/// clock step is taken by linear interpolation on that dense grid, where the signal
/// occupies under 4 % of Nyquist (±3.4 kHz of ±96 kHz) and the interpolation error is
/// far below the noise floor of the tests that use it.
fn resample_ppm(samples: &[Complex], ppm: f64) -> Vec<Complex> {
    let mut resampler = RationalResampler::new(
        CHANNEL_SAMPLE_RATE_HZ,
        CHANNEL_SAMPLE_RATE_HZ * PPM_OVERSAMPLE as f64,
    )
    .unwrap();
    let mut dense = vec![Complex::default(); samples.len() * PPM_OVERSAMPLE + 16];
    let count = resampler.process(samples, &mut dense).unwrap();

    let step = PPM_OVERSAMPLE as f64 * (1.0 + ppm * 1e-6);
    let mut out = Vec::with_capacity(samples.len() + 4);
    let mut x = 0.0_f64;
    while x + 1.0 < count as f64 {
        let i = x as usize;
        let mu = (x - i as f64) as f32;
        out.push(dense[i] * (1.0 - mu) + dense[i + 1] * mu);
        x += step;
    }
    out
}

/// Bit error rate of `got` against `expected` at the best alignment, after skipping
/// [`SETTLE_BITS`] of loop acquisition.
fn bit_error_rate(expected: &[bool], got: &[bool]) -> f64 {
    assert!(
        got.len() > SETTLE_BITS + 512,
        "demodulator produced only {} bits",
        got.len()
    );
    let tail = &got[SETTLE_BITS..];
    let mut best = 1.0_f64;
    for offset in 0..=MAX_ALIGN_SEARCH {
        if offset + 512 > expected.len() {
            break;
        }
        let compared = tail.len().min(expected.len() - offset);
        let errors = (0..compared)
            .filter(|&i| tail[i] != expected[i + offset])
            .count();
        best = best.min(errors as f64 / compared as f64);
    }
    best
}

fn assert_recovered(expected: &[bool], got: &[bool]) {
    // A timing loop that slips a symbol shows up as a bit-count mismatch, so bind
    // the count as well as the error rate.
    let delta = got.len().abs_diff(expected.len());
    assert!(
        delta <= 16,
        "recovered {} bits from {} transmitted",
        got.len(),
        expected.len()
    );
    let ber = bit_error_rate(expected, got);
    assert!(ber <= MAX_BER, "bit error rate {ber} exceeds {MAX_BER}");
}

fn demod_all(iq: &[Complex]) -> Vec<bool> {
    let mut out = Vec::new();
    SdpskDemod::new().process(iq, &mut out);
    out
}

#[test]
fn rrc_taps_have_expected_shape() {
    let taps = rrc_taps(RRC_ALPHA, SAMPLES_PER_SYMBOL, RRC_SPAN_SYMBOLS);
    assert_eq!(taps.len(), SAMPLES_PER_SYMBOL * RRC_SPAN_SYMBOLS + 1);
    let energy: f32 = taps.iter().map(|t| t * t).sum();
    assert!((energy - 1.0).abs() < 1e-5, "energy {energy}");
    // The reference's `t = (x − N/2)/sps` axis centres the response on index
    // N/2 = 16.5, so taps mirror as `h[k] == h[N − k]` for k ≥ 1; index 0 is the
    // unpaired sample that the half-sample offset leaves over.
    for k in 1..taps.len() {
        let mirror = taps.len() - k;
        assert!(
            (taps[k] - taps[mirror]).abs() < 1e-6,
            "taps {k} and {mirror} differ"
        );
    }
    // Peak sits either side of the half-sample centre.
    let peak = taps.iter().copied().fold(f32::MIN, f32::max);
    assert!((taps[16] - peak).abs() < 1e-6 && (taps[17] - peak).abs() < 1e-6);
}

#[test]
fn bit_convention_is_the_bare_shift_bit() {
    // Guards the two claims `bit_convention`'s doc comment makes about the
    // arbitrated setting, so neither can silently outlive the constants.
    const { assert!(!NRZ_M_DECODE) };

    // (1) With the NRZ-M decode off, the predecessor is ignored: the detected
    // shift bit *is* the information bit.
    for &shift in &[false, true] {
        assert_eq!(bit_convention(shift, false), bit_convention(shift, true));
    }
    // (2) ... and the sign is therefore no longer a no-op. Inverting the detected
    // bit inverts the information bit, whichever way `SHIFT_ADVANCE_IS_ONE` points
    // — which is why the capture decodes under one sign and not the other.
    for &prev in &[false, true] {
        assert_ne!(bit_convention(false, prev), bit_convention(true, prev));
    }
}

#[test]
fn loopback_clean() {
    let bits: Vec<bool> = (0..2048).map(|i| (i * 7 + 3) % 5 < 2).collect();
    let iq = modulate_sdpsk(&bits);
    assert_recovered(&bits, &demod_all(&iq));
}

#[test]
fn loopback_clean_random_bits() {
    let mut rng = Rng::new(0x5EED_0001);
    let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
    let iq = modulate_sdpsk(&bits);
    assert_recovered(&bits, &demod_all(&iq));
}

#[test]
fn loopback_survives_block_fragmentation() {
    // State must carry across process() calls: fragmenting the input into ragged
    // blocks may not change the recovered bits.
    let mut rng = Rng::new(0x5EED_0002);
    let bits: Vec<bool> = (0..2048).map(|_| rng.next_u64() & 1 == 1).collect();
    let iq = modulate_sdpsk(&bits);

    let whole = demod_all(&iq);

    let mut demod = SdpskDemod::new();
    let mut fragmented = Vec::new();
    let mut start = 0;
    let mut size = 1;
    while start < iq.len() {
        let end = (start + size).min(iq.len());
        demod.process(&iq[start..end], &mut fragmented);
        start = end;
        size = size % 97 + 1;
    }
    // Exact equality, not just an equivalent BER: the matched-filter delay line,
    // the timing loop and the differential detector must all carry state across
    // calls, so block boundaries have to be bit-for-bit invisible.
    assert_eq!(whole, fragmented);
    assert_recovered(&bits, &fragmented);
}

#[test]
fn loopback_with_cfo_and_noise() {
    // ±3.5 kHz CFO is worst-case Doppler at 137 MHz, but the delay-conjugate
    // detector sees a constant per-symbol phase bias of 2π·Δf/4800 — 262° at
    // 3.5 kHz, far past the ±90° decision boundary. Coarse CFO correction is the
    // channelizer's job; the demod contract is a residual of at most ±800 Hz
    // (60° bias, 30° of margin left).
    for (seed, cfo_hz) in [(0x5EED_0010_u64, -800.0_f64), (0x5EED_0011, 800.0)] {
        let mut rng = Rng::new(seed);
        let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
        let iq = add_awgn(&apply_cfo(&modulate_sdpsk(&bits), cfo_hz), 15.0, seed);
        assert_recovered(&bits, &demod_all(&iq));
    }
}

#[test]
fn cfo_beyond_the_contract_breaks_detection() {
    // The other half of the contract, and the proof that the loopback harness is
    // actually sensitive: at 1200 Hz the per-symbol bias is exactly 90°, the
    // decision boundary, and detection collapses. Anything past ±800 Hz belongs to
    // the channelizer's coarse frequency correction, not here.
    let mut rng = Rng::new(0x5EED_0012);
    let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
    let iq = add_awgn(
        &apply_cfo(&modulate_sdpsk(&bits), 1200.0),
        15.0,
        0x5EED_0012,
    );
    let ber = bit_error_rate(&bits, &demod_all(&iq));
    assert!(ber > 0.2, "expected detection to collapse, got ber {ber}");
}

#[test]
fn loopback_with_sample_clock_offset() {
    // ±50 ppm of symbol-clock error, the datasheet-grade spread of a TCXO pair.
    for (seed, ppm) in [(0x5EED_0013_u64, -50.0_f64), (0x5EED_0014, 50.0)] {
        let mut rng = Rng::new(seed);
        let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
        let iq = resample_ppm(&modulate_sdpsk(&bits), ppm);
        assert_recovered(&bits, &demod_all(&iq));
    }
}

#[test]
fn loopback_with_gross_sample_clock_offset() {
    // ±500 ppm is 10× the contract: over 4096 symbols it walks the sampling instant
    // by two whole symbols, so this genuinely binds the timing loop's rate term
    // rather than just its phase term.
    for (seed, ppm) in [(0x5EED_0020_u64, -500.0_f64), (0x5EED_0021, 500.0)] {
        let mut rng = Rng::new(seed);
        let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
        let iq = resample_ppm(&modulate_sdpsk(&bits), ppm);
        assert_recovered(&bits, &demod_all(&iq));
    }
}

#[test]
fn noise_margin_at_10_db_snr() {
    // Soft ceiling so the noise numbers in the task report are a guard, not a note.
    // `add_awgn`'s SNR is *per sample* at 4 samples/symbol, so 10 dB here reads as
    // roughly 16 dB of Es/N0 — the interesting part is that this stays put.
    let mut rng = Rng::new(0x5EED_0050);
    let bits: Vec<bool> = (0..4096).map(|_| rng.next_u64() & 1 == 1).collect();
    let iq = add_awgn(&apply_cfo(&modulate_sdpsk(&bits), 800.0), 10.0, 0x5EED_0051);
    let ber = bit_error_rate(&bits, &demod_all(&iq));
    assert!(
        ber < 0.05,
        "bit error rate {ber} regressed past 5 % at 10 dB SNR"
    );
}

#[test]
fn non_finite_samples_cannot_stall_the_demodulator() {
    // Regression: a non-finite value reaching the loop filter used to poison `cursor`
    // and `sps`, after which the symbol loop stopped advancing, `compact_pending`'s
    // `max(0.0)` swallowed the NaN, and `pending` grew by a whole block on every
    // call — forever. Guarding `curr` alone is not enough: `mid` is interpolated from
    // a different window, and a finite-but-enormous sample overflows the power
    // estimate to infinity and then to NaN. Hence the guard on the error itself.
    let mut rng = Rng::new(0x5EED_0040);
    let bits: Vec<bool> = (0..6144).map(|_| rng.next_u64() & 1 == 1).collect();
    // 500 ppm of clock offset, poisoned *after* resampling (poisoning before would
    // smear the NaN through the resampler's own delay line). The offset is what makes
    // the tail assertion bite: a loop that merely coasts — because a poisoned power
    // estimate froze it — slips more than a symbol over the remaining record.
    let mut iq = resample_ppm(&modulate_sdpsk(&bits), 500.0);
    // Each poisoned sample smears over the 33-tap matched filter, and the timing
    // cursor walks that run in ~4-sample steps — so whether `mid` lands inside the
    // run while `curr` is already outside it depends on the run's phase. Spread the
    // NaNs across all four residues mod `SAMPLES_PER_SYMBOL` so that case is hit
    // deterministically, then add the infinities and the finite-but-enormous values
    // that overflow the power estimate.
    let poisons = [
        (8000_usize, f32::NAN),
        (8401, f32::NAN),
        (8802, f32::NAN),
        (9203, f32::NAN),
        (9600, f32::INFINITY),
        (9813, f32::NEG_INFINITY),
        (10_000, 1e30),
        (10_213, -1e30),
    ];
    for (index, poison) in poisons {
        iq[index] = Complex::new(poison, poison);
    }

    let mut demod = SdpskDemod::new();
    let mut out = Vec::new();
    for chunk in iq.chunks(512) {
        demod.process(chunk, &mut out);
        assert!(
            demod.cursor.is_finite() && demod.sps.is_finite(),
            "loop state went non-finite: cursor {} sps {}",
            demod.cursor,
            demod.sps
        );
        assert!(
            demod.pending.len() <= 32,
            "pending grew to {} — the drain has stalled",
            demod.pending.len()
        );
    }

    // Symbols are still being produced at the symbol rate (each poisoned sample
    // costs the ~8 symbols its matched-filter response touches, nothing more) ...
    assert!(
        out.len() + 64 >= bits.len(),
        "recovered only {} bits from {}",
        out.len(),
        bits.len()
    );
    // ... and the stream after the disturbance decodes cleanly again.
    let ber = bit_error_rate(&bits[bits.len() - 2048..], &out[out.len() - 2048..]);
    assert!(ber <= MAX_BER, "post-disturbance bit error rate {ber}");
}

#[test]
fn empty_input_is_a_no_op() {
    let mut demod = SdpskDemod::new();
    let mut out = Vec::new();
    demod.process(&[], &mut out);
    assert!(out.is_empty());
}

#[test]
fn reset_restores_initial_state() {
    let mut rng = Rng::new(0x5EED_0030);
    let bits: Vec<bool> = (0..2048).map(|_| rng.next_u64() & 1 == 1).collect();
    let iq = modulate_sdpsk(&bits);

    let mut demod = SdpskDemod::new();
    let mut first = Vec::new();
    demod.process(&iq, &mut first);
    demod.reset();
    let mut second = Vec::new();
    demod.process(&iq, &mut second);
    assert_eq!(first, second);
}
