//! SDPSK demodulator for the Orbcomm subscriber downlink.
//!
//! Physical layer (see `docs/superpowers/specs/2026-08-29-orbcomm-decoder-design.md`):
//! each information bit selects a carrier phase **shift** (`1` ⇒ +90°, `0` ⇒ −90°), and
//! the resulting symbol stream is pulse-shaped with an α = 0.4 root-raised-cosine filter
//! at [`crate::SYMBOL_RATE_HZ`]. The ±90° phase-shift keying *is* the differential
//! encoding — there is no separate NRZ-M layer stacked on top of it, which is what the
//! Task 9 real-capture arbitration settled; see [`bit_convention`].
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
//!    carrier advanced ⇒ a +90° shift ⇒ an information bit of `1`.
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

/// `im(d) > 0` (carrier advanced, i.e. a +90° shift) is information bit `1`.
///
/// **Confirmed** against real captures — see [`bit_convention`]. Flip there, never here.
const SHIFT_ADVANCE_IS_ONE: bool = true;
/// Apply a second, NRZ-M differential decode (`info = coded ^ prev_coded`) on top of the
/// delay-conjugate detector.
///
/// **Confirmed off** against real captures — see [`bit_convention`]. Flip there, never
/// here.
const NRZ_M_DECODE: bool = false;

/// Map a detected phase-shift bit to an information bit.
///
/// **This is the single place where the conventions of the Orbcomm physical layer
/// live.** The protocol description fixes the modulation but did not, on its own, settle
/// (a) whether a *positive* phase shift carries `1` or `0`, nor (b) whether the deframer
/// downstream wants a further NRZ-M decode of the detected shift bits or the shift bits
/// themselves.
///
/// # Arbitrated against real captures, Task 9
///
/// `crates/sdr-orbcomm/tests/real_capture.rs` decided it on the two off-air recordings
/// shipped with the reference receiver (`original/ORBCOMM-receiver/data/*.mat`, 2 s of
/// 1.2288 Msps at 137.5 MHz, ORBCOMM FM114 overhead). Checksum-valid packets decoded:
///
/// | `NRZ_M_DECODE` | `SHIFT_ADVANCE_IS_ONE` | `1552071892p6` | `1552072122p64` |
/// |----------------|------------------------|----------------|-----------------|
/// | `true`         | either (a no-op)       | 0              | 0               |
/// | `false`        | `true` — **shipped**   | **46**         | **133**         |
/// | `false`        | `false`                | 0              | 0               |
///
/// Both settings are therefore **confirmed**, not merely plausible: the winning row is
/// the only one that decodes anything at all, and what it decodes is externally
/// checkable — Sync code `65A8F9` / `sat_id 2C` and an ephemeris at 715.3 km doing
/// 7151.4 m/s, matching the reference decoder's published output for the same file.
///
/// So the ±90° phase-shift keying *is* the differential encoding: the shift bit the
/// delay-conjugate detector recovers is already the information bit, and XOR-ing it
/// against its predecessor would encode the data a second time. This matches the
/// reference decoder, which likewise takes `arg(s[n]) − arg(s[n−1]) > 0` straight to a
/// packet bit (`original/ORBCOMM-receiver/file_decoder.py`, "Differential
/// demodulation").
///
/// The sign is only a no-op while `NRZ_M_DECODE` is on, where inverting every detected
/// bit leaves `coded[n] ^ coded[n-1]` unchanged. With the XOR off it is load-bearing —
/// flipping it inverts every information bit, which is exactly why the third row above
/// decodes nothing. `bit_convention_is_the_bare_shift_bit` in this module's tests pins
/// both halves of that.
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
    /// Previous raw (pre-convention) detected shift bit. Only [`NRZ_M_DECODE`] consumes
    /// it; it is kept unconditionally so the convention stays a single call site.
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
            // The first detected shift bit is withheld because [`bit_convention`] takes a
            // predecessor. With `NRZ_M_DECODE` off that predecessor is ignored, so this
            // costs one bit at the very start of a channel's stream — immaterial next to
            // the deframer's own acquisition, and the price of keeping the convention a
            // single call site that can be flipped back without restructuring the loop.
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

/// SDPSK transmitter at `sps` samples per symbol: ±90° phase steps → impulse
/// train → RRC pulse shaping with the receiver's taps.
///
/// The inverse of the receive chain under the **confirmed** convention (see
/// [`bit_convention`]): bit `1` advances the carrier by 90°, bit `0` retards it,
/// and there is no NRZ-M layer. It has to track [`SHIFT_ADVANCE_IS_ONE`] and
/// [`NRZ_M_DECODE`], or the loopback tests would only ever prove the demodulator
/// self-consistent with a transmitter nobody flies.
///
/// Test-only, but `pub(crate)` — the same pattern as
/// `packet::encode_ephemeris_for_test` — so the channelizer's tests can
/// synthesise a wideband multi-channel stream by modulating directly at the
/// source rate (`sps = source_rate / SYMBOL_RATE_HZ`) instead of interpolating
/// a 4 sps waveform up to it.
///
/// Returns an empty vector for `sps == 0`.
#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn modulate_sdpsk_at_sps(bits: &[bool], sps: usize) -> Vec<Complex> {
    let taps = rrc_taps(RRC_ALPHA, sps, RRC_SPAN_SYMBOLS);
    if taps.is_empty() {
        return Vec::new();
    }
    let mut phase = 0.0_f64;
    // Mirror image of `bit_convention`: `signed` is the detected bit after the sign
    // convention has been applied, `advance` the raw "the carrier moved forwards" flag
    // that the delay-conjugate detector reads back off the air.
    let mut prev_signed = false;
    let mut out = vec![Complex::default(); bits.len() * sps + taps.len() - 1];
    for (n, &info) in bits.iter().enumerate() {
        let signed = if NRZ_M_DECODE {
            info != prev_signed
        } else {
            info
        };
        prev_signed = signed;
        let advance = signed == SHIFT_ADVANCE_IS_ONE;
        phase += if advance {
            std::f64::consts::FRAC_PI_2
        } else {
            -std::f64::consts::FRAC_PI_2
        };
        let symbol = Complex::new(phase.cos() as f32, phase.sin() as f32);
        // Only the impulse positions carry energy, so skip the `sps - 1`
        // zeros between them rather than convolving them.
        for (j, &tap) in taps.iter().enumerate() {
            out[n * sps + j] += symbol * tap;
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
mod tests;
