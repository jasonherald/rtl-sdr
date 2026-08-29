//! SDPSK demodulator for the Orbcomm subscriber downlink.
//!
//! Physical layer (see `docs/superpowers/specs/2026-08-29-orbcomm-decoder-design.md`):
//! information bits are NRZ-M differentially encoded (`coded[n] = info[n] ^ coded[n-1]`),
//! each *coded* bit selects a carrier phase **shift** (`1` ⇒ +90°, `0` ⇒ −90°), and the
//! resulting symbol stream is pulse-shaped with an α = 0.4 root-raised-cosine filter at
//! [`crate::SYMBOL_RATE_HZ`].
//!
//! The receive chain implemented here, in block order:
//!
//! 1. **RRC matched filter** — the same 33-tap α = 0.4 root-raised-cosine as the
//!    transmitter, so the cascade is a Nyquist raised cosine (zero ISI at the symbol
//!    instants). State: a `taps.len() - 1` sample delay line.
//! 2. **Gardner timing recovery** — a second-order loop driving a fractional read cursor
//!    across the 4 samples/symbol grid, with a 4-point cubic-Lagrange interpolator.
//!    State: the fractional cursor, the tracked samples-per-symbol, the previous symbol
//!    sample and an EWMA symbol-power estimate used to normalise the error.
//! 3. **Delay-conjugate detector** — `d[n] = s[n] · conj(s[n-1])`; `im(d) > 0` means the
//!    carrier advanced ⇒ a +90° shift.
//! 4. **NRZ-M decode** — `info[n] = coded[n] ^ coded[n-1]`.
//!
//! The demodulator assumes the channelizer has already removed coarse Doppler: the
//! contract is a **residual carrier offset of at most ±800 Hz**. The delay-conjugate
//! detector turns a residual offset `Δf` into a fixed rotation of `2π·Δf / 4800` per
//! symbol — 60° at 800 Hz, which still leaves a 30° decision margin against the ±90°
//! constellation. At the full ±3.5 kHz Doppler seen at 137 MHz the rotation would exceed
//! 90° and the detector would invert, so per-channel coarse frequency correction upstream
//! is mandatory, not optional.
//!
//! Pure DSP: no I/O, no threads, no allocation in `process` once the internal buffers
//! have reached their steady-state capacity.

use sdr_types::Complex;

use crate::SAMPLES_PER_SYMBOL;

/// Root-raised-cosine roll-off factor of the Orbcomm downlink pulse shape.
pub const RRC_ALPHA: f64 = 0.4;
/// Matched-filter span in symbols; `SAMPLES_PER_SYMBOL * RRC_SPAN_SYMBOLS + 1` = 33 taps.
pub const RRC_SPAN_SYMBOLS: usize = 8;

/// Proportional gain of the timing loop: samples of cursor correction per unit of
/// normalised timing error.
const TIMING_LOOP_KP: f64 = 0.08;
/// Integral gain of the timing loop: samples-per-symbol correction per unit error.
/// Roughly `KP² / 4` for a critically damped second-order loop.
const TIMING_LOOP_KI: f64 = 0.0016;
/// Half-width of the samples-per-symbol clamp. ±2 % is ~40× the ±500 ppm of clock error
/// a real receiver sees, while still forbidding the loop from running away.
const SPS_TRACK_HALF_RANGE: f64 = 0.08;
/// Smoothing factor of the symbol-power EWMA used to normalise the timing error.
const POWER_EWMA_ALPHA: f32 = 0.02;
/// Floor added to the power estimate so the error normalisation cannot divide by zero.
const POWER_FLOOR: f32 = 1e-20;
/// Samples kept behind the read cursor when the pending buffer is compacted. Must cover
/// the interpolator's look-behind (`sps / 2 + 1` samples, ~3 at 4 sps).
const PENDING_KEEP_BACK: usize = 8;
/// Initial cursor position: far enough into the buffer that the half-symbol look-behind
/// of the Gardner mid-sample interpolator is in range from the very first symbol.
const INITIAL_CURSOR: f64 = 5.0;
/// Nominal samples per symbol as a float; mirrors [`SAMPLES_PER_SYMBOL`].
#[allow(clippy::cast_precision_loss)]
const NOMINAL_SPS: f64 = SAMPLES_PER_SYMBOL as f64;

/// `im(d) > 0` (carrier advanced, i.e. a +90° shift) is coded bit `1`.
///
/// See [`bit_convention`] — flip there, never here.
const SHIFT_ADVANCE_IS_ONE: bool = true;
/// Apply the NRZ-M differential decode (`info = coded ^ prev_coded`).
///
/// See [`bit_convention`] — flip there, never here.
const NRZ_M_DECODE: bool = true;

/// Map a detected phase-shift bit to an information bit.
///
/// **This is the single place where the two ambiguous conventions of the Orbcomm
/// physical layer live.** The protocol description fixes the modulation but not, with
/// certainty, (a) whether a *positive* phase shift is coded `1` or coded `0`, nor
/// (b) whether the deframer downstream wants NRZ-M-decoded information bits or the raw
/// coded bits. **The real-capture fixture in Task 9 is the arbiter.**
///
/// While `NRZ_M_DECODE` is on, flipping the sign is a provable *no-op* — inverting every
/// coded bit leaves `coded[n] ^ coded[n-1]` unchanged, which is the differential encoding
/// doing its job (it also makes the demodulator immune to spectral inversion in the front
/// end). That is asserted by `sign_flip_is_a_noop_while_nrz_m_is_enabled` in this
/// module's tests. So there are only **three distinguishable configurations**, and the
/// fixture should be tried against them in this order:
///
/// 1. `NRZ_M_DECODE = true` — the shipped setting; the sign is irrelevant here.
/// 2. `NRZ_M_DECODE = false`, `SHIFT_ADVANCE_IS_ONE = true` — raw coded bits.
/// 3. `NRZ_M_DECODE = false`, `SHIFT_ADVANCE_IS_ONE = false` — raw coded bits, inverted.
///
/// Turning `NRZ_M_DECODE` off also means updating the `const` assertion in
/// `sign_flip_is_a_noop_while_nrz_m_is_enabled`, which exists precisely so the no-op
/// claim above cannot silently outlive the setting it depends on: that test guards the
/// documentation, it does not forbid the flip.
///
/// Never scatter convention changes across the detector, the deframer or the descrambler.
#[must_use]
const fn bit_convention(coded: bool, prev_coded: bool) -> bool {
    // Both bits go through the same sign convention so the XOR stays consistent.
    let coded = coded == SHIFT_ADVANCE_IS_ONE;
    let prev_coded = prev_coded == SHIFT_ADVANCE_IS_ONE;
    if NRZ_M_DECODE {
        coded != prev_coded
    } else {
        coded
    }
}

/// Root-raised-cosine impulse response at time `t`, in symbol periods (`Ts = 1`).
///
/// Ports `rrcosfilter` from the reference receiver
/// (`original/ORBCOMM-receiver/helpers.py`), including its two removable
/// singularities at `t = 0` and `t = ±1 / (4α)`.
fn rrc_impulse(alpha: f64, t: f64) -> f64 {
    /// Tolerance for hitting a removable singularity of the closed form.
    const SINGULARITY_EPS: f64 = 1e-9;

    if t.abs() < SINGULARITY_EPS {
        return 1.0 - alpha + 4.0 * alpha / std::f64::consts::PI;
    }
    if alpha > 0.0 && (t.abs() - 1.0 / (4.0 * alpha)).abs() < SINGULARITY_EPS {
        let quarter = std::f64::consts::PI / (4.0 * alpha);
        return (alpha / std::f64::consts::SQRT_2)
            * ((1.0 + 2.0 / std::f64::consts::PI) * quarter.sin()
                + (1.0 - 2.0 / std::f64::consts::PI) * quarter.cos());
    }
    let num = (std::f64::consts::PI * t * (1.0 - alpha)).sin()
        + 4.0 * alpha * t * (std::f64::consts::PI * t * (1.0 + alpha)).cos();
    let den = std::f64::consts::PI * t * (1.0 - (4.0 * alpha * t) * (4.0 * alpha * t));
    num / den
}

/// Root-raised-cosine filter taps: `sps * span_symbols + 1` coefficients.
///
/// Ported from the reference receiver's `rrcosfilter`, including its
/// `t = (x − N/2) / sps` time axis: for an odd tap count that centres the response
/// half a sample off the grid. Transmit and receive filters share this offset, so the
/// cascade peaks on an integer sample and the timing loop has a valid lock point.
///
/// Taps are normalised to **unit energy** (`Σ h² = 1`). The cascade of the transmit
/// filter and this matched filter therefore has unity peak gain, so a unit-magnitude
/// transmitted symbol arrives at the detector with unit magnitude.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn rrc_taps(alpha: f64, sps: usize, span_symbols: usize) -> Vec<f32> {
    let count = sps * span_symbols + 1;
    if count == 0 || sps == 0 {
        return Vec::new();
    }
    let half = count as f64 / 2.0;
    let mut taps: Vec<f64> = (0..count)
        .map(|x| rrc_impulse(alpha, (x as f64 - half) / sps as f64))
        .collect();
    let energy = taps.iter().map(|t| t * t).sum::<f64>().sqrt();
    if energy > 0.0 {
        for t in &mut taps {
            *t /= energy;
        }
    }
    taps.iter().map(|&t| t as f32).collect()
}

/// Four-point cubic Lagrange interpolation of `buf` at fractional index `x`.
///
/// Indices are clamped to the buffer, so the function is total; callers keep `x` inside
/// `1 ..= len - 3` where the interpolation is exact.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn interpolate(buf: &[Complex], x: f64) -> Complex {
    let Some(last) = buf.len().checked_sub(1) else {
        return Complex::default();
    };
    let base = x.floor();
    let mu = (x - base) as f32;
    let i = base.clamp(0.0, last as f64) as usize;

    // Samples straddling the fractional position, at relative offsets −1, 0, +1, +2.
    let before = buf[i.saturating_sub(1)];
    let at = buf[i];
    let after = buf[(i + 1).min(last)];
    let beyond = buf[(i + 2).min(last)];

    let weight_before = -mu * (mu - 1.0) * (mu - 2.0) / 6.0;
    let weight_at = (mu + 1.0) * (mu - 1.0) * (mu - 2.0) / 2.0;
    let weight_after = -mu * (mu + 1.0) * (mu - 2.0) / 2.0;
    let weight_beyond = mu * (mu + 1.0) * (mu - 1.0) / 6.0;

    before * weight_before + at * weight_at + after * weight_after + beyond * weight_beyond
}

/// Streaming SDPSK demodulator: complex samples at [`crate::CHANNEL_SAMPLE_RATE_HZ`] in,
/// information bits out.
///
/// One instance per channel; call [`SdpskDemod::process`] with successive blocks. All
/// state (matched-filter delay line, timing loop, differential detector) carries across
/// calls, so block boundaries are invisible to the output.
pub struct SdpskDemod {
    /// Matched-filter coefficients.
    taps: Vec<f32>,
    /// The last `taps.len() - 1` input samples, oldest first.
    mf_history: Vec<Complex>,
    /// Scratch holding `mf_history` followed by the current input block.
    work: Vec<Complex>,
    /// Matched-filter output not yet consumed by the timing loop.
    pending: Vec<Complex>,
    /// Fractional read cursor into `pending`, in samples.
    cursor: f64,
    /// Tracked samples per symbol (nominally [`SAMPLES_PER_SYMBOL`]).
    sps: f64,
    /// Previous symbol sample — the delay-conjugate reference and the Gardner `y[k-1]`.
    prev_symbol: Option<Complex>,
    /// Previous raw (pre-convention) coded bit, for the NRZ-M XOR.
    prev_coded: Option<bool>,
    /// EWMA of the symbol power; normalises the timing error to be amplitude-independent.
    power: f32,
}

impl SdpskDemod {
    /// Create a demodulator with the Orbcomm matched filter and a timing loop parked at
    /// the nominal 4 samples/symbol.
    #[must_use]
    pub fn new() -> Self {
        let taps = rrc_taps(RRC_ALPHA, SAMPLES_PER_SYMBOL, RRC_SPAN_SYMBOLS);
        let history = taps.len().saturating_sub(1);
        Self {
            taps,
            mf_history: vec![Complex::default(); history],
            work: Vec::new(),
            pending: Vec::new(),
            cursor: INITIAL_CURSOR,
            sps: NOMINAL_SPS,
            prev_symbol: None,
            prev_coded: None,
            power: 0.0,
        }
    }

    /// Drop all filter and loop state, as if freshly constructed.
    pub fn reset(&mut self) {
        self.mf_history.fill(Complex::default());
        self.work.clear();
        self.pending.clear();
        self.cursor = INITIAL_CURSOR;
        self.sps = NOMINAL_SPS;
        self.prev_symbol = None;
        self.prev_coded = None;
        self.power = 0.0;
    }

    /// Demodulate a block of channel samples, appending recovered information bits.
    ///
    /// Bits are appended, never cleared — the caller owns `bits_out`.
    pub fn process(&mut self, samples: &[Complex], bits_out: &mut Vec<bool>) {
        if samples.is_empty() || self.taps.is_empty() {
            return;
        }
        self.matched_filter(samples);
        self.recover_symbols(bits_out);
        self.compact_pending();
    }

    /// Convolve the input block with the RRC taps into `pending`, one output per input.
    fn matched_filter(&mut self, samples: &[Complex]) {
        let history = self.mf_history.len();
        self.work.clear();
        self.work.extend_from_slice(&self.mf_history);
        self.work.extend_from_slice(samples);

        self.pending.reserve(samples.len());
        for i in 0..samples.len() {
            let mut acc_re = 0.0_f32;
            let mut acc_im = 0.0_f32;
            for (j, &tap) in self.taps.iter().enumerate() {
                let s = self.work[i + history - j];
                acc_re += s.re * tap;
                acc_im += s.im * tap;
            }
            self.pending.push(Complex::new(acc_re, acc_im));
        }

        if history > 0 {
            let tail = self.work.len() - history;
            self.mf_history.copy_from_slice(&self.work[tail..]);
        }
    }

    /// Walk the timing loop across `pending`, emitting one bit per recovered symbol.
    #[allow(clippy::cast_precision_loss)]
    fn recover_symbols(&mut self, bits_out: &mut Vec<bool>) {
        let len = self.pending.len() as f64;
        while self.cursor + 2.0 < len {
            let mid_cursor = self.cursor - self.sps / 2.0;
            if mid_cursor < 1.0 {
                // Not enough look-behind yet (only reachable on a pathological rewind).
                self.cursor += 1.0;
                continue;
            }
            let curr = interpolate(&self.pending, self.cursor);
            if !curr.re.is_finite() || !curr.im.is_finite() {
                // Keep non-finite samples out of the detector and out of `prev_symbol`.
                // This is not the boundedness guarantee — the guard on the loop-filter
                // output below is; `curr` is only one of the several ways a NaN can
                // reach `error`.
                self.cursor += self.sps;
                continue;
            }

            let Some(prev) = self.prev_symbol else {
                self.prev_symbol = Some(curr);
                self.power = curr.re * curr.re + curr.im * curr.im;
                self.cursor += self.sps;
                continue;
            };

            let mid = interpolate(&self.pending, mid_cursor);
            match self.timing_error(curr, mid, curr - prev) {
                Some(error) => {
                    // A positive error means we sampled late, so retard the cursor and
                    // shorten the symbol period.
                    self.sps = (self.sps - TIMING_LOOP_KI * error).clamp(
                        NOMINAL_SPS - SPS_TRACK_HALF_RANGE,
                        NOMINAL_SPS + SPS_TRACK_HALF_RANGE,
                    );
                    self.cursor += self.sps - TIMING_LOOP_KP * error;
                }
                // Never let a non-finite error reach the loop filter: `cursor` and `sps`
                // would go NaN, the symbol loop would stop advancing, `compact_pending`'s
                // `max(0.0)` would swallow the NaN and `pending` would then grow by a
                // whole block on every call, forever. Coast at the tracked rate instead;
                // the loop picks up again as soon as the samples are finite.
                None => self.cursor += self.sps,
            }

            // Delay-conjugate detection of the ±90° phase shift.
            let d = curr * prev.conj();
            let coded = d.im > 0.0;
            if let Some(prev_coded) = self.prev_coded {
                bits_out.push(bit_convention(coded, prev_coded));
            }
            self.prev_coded = Some(coded);
            self.prev_symbol = Some(curr);
        }
    }

    /// Gardner timing-error detector — `Re{ conj(y_mid) · (y[k] − y[k−1]) }`, normalised
    /// by the tracked symbol power so the loop gain is amplitude-independent.
    ///
    /// Insensitive to carrier *phase* (the rotation cancels between the two factors),
    /// which is what lets it run ahead of any carrier recovery.
    ///
    /// Also owns the symbol-power EWMA, because the two cannot be separated safely:
    /// `mid` is drawn from a window of `pending` that the caller's guard on `curr` does
    /// not cover, and a finite but enormous `curr` overflows `power` to infinity and then
    /// to NaN on the next EWMA step. A poisoned estimate is re-seeded rather than carried
    /// forward, and `None` is returned whenever the error is not finite.
    fn timing_error(&mut self, curr: Complex, mid: Complex, diff: Complex) -> Option<f64> {
        let raw_error = mid.re * diff.re + mid.im * diff.im;

        let sample_power = curr.re * curr.re + curr.im * curr.im;
        self.power = if self.power.is_finite() && sample_power.is_finite() {
            self.power + POWER_EWMA_ALPHA * (sample_power - self.power)
        } else if sample_power.is_finite() {
            sample_power
        } else {
            0.0
        };

        let error = f64::from(raw_error / (2.0 * self.power + POWER_FLOOR)).clamp(-1.0, 1.0);
        error.is_finite().then_some(error)
    }

    /// Drop the consumed prefix of `pending`, keeping the interpolator's look-behind.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn compact_pending(&mut self) {
        let consumed = self.cursor.floor().max(0.0) as usize;
        let drop = consumed
            .saturating_sub(PENDING_KEEP_BACK)
            .min(self.pending.len());
        if drop > 0 {
            self.pending.drain(..drop);
            #[allow(clippy::cast_precision_loss)]
            {
                self.cursor -= drop as f64;
            }
        }
    }
}

impl Default for SdpskDemod {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
mod tests {
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

    /// Spec-literal SDPSK transmitter: NRZ-M encode → ±90° phase steps → 4 sps impulse
    /// train → RRC pulse shaping with the receiver's taps.
    ///
    /// This is the *transmitter the specification describes*; whether the air interface
    /// agrees is what the Task 9 real-capture fixture decides (see [`bit_convention`]).
    fn modulate_sdpsk(bits: &[bool]) -> Vec<Complex> {
        let taps = rrc_taps(RRC_ALPHA, SAMPLES_PER_SYMBOL, RRC_SPAN_SYMBOLS);
        let mut phase = 0.0_f64;
        let mut prev_coded = false;
        let mut impulses = vec![Complex::default(); bits.len() * SAMPLES_PER_SYMBOL];
        for (n, &info) in bits.iter().enumerate() {
            let coded = info != prev_coded;
            prev_coded = coded;
            phase += if coded {
                std::f64::consts::FRAC_PI_2
            } else {
                -std::f64::consts::FRAC_PI_2
            };
            impulses[n * SAMPLES_PER_SYMBOL] = Complex::new(phase.cos() as f32, phase.sin() as f32);
        }

        let mut out = vec![Complex::default(); impulses.len() + taps.len() - 1];
        for (i, &s) in impulses.iter().enumerate() {
            for (j, &tap) in taps.iter().enumerate() {
                out[i + j] += s * tap;
            }
        }
        out
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
    fn sign_flip_is_a_noop_while_nrz_m_is_enabled() {
        // Documents the claim made in `bit_convention`'s doc comment: inverting every
        // coded bit leaves the NRZ-M-decoded information bits untouched.
        const { assert!(NRZ_M_DECODE) };
        for &coded in &[false, true] {
            for &prev in &[false, true] {
                assert_eq!(bit_convention(coded, prev), bit_convention(!coded, !prev));
            }
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
}
