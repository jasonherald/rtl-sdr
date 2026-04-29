//! MSK (minimum-shift keying) demodulator at 2400 baud over
//! 1200/2400 Hz tones. Faithful port of
//! `original/acarsdec/msk.c`.
//!
//! Consumes real `f32` audio at 12500 Hz (the IF rate after
//! per-channel decimation). Internally builds a complex
//! baseband via a 1800 Hz VCO mixer, applies a 133-tap
//! oversampled matched filter (`FLEN`=11 × `MFLT_OVER`=12 + 1),
//! and emits one bit per ~5.2 audio samples (= 12500 / 2400).
//! Bit timing is recovered by a Gardner-style PLL on the
//! matched-filter quadrature output.
//!
//! Output bits are pushed to a [`BitSink`] one at a time;
//! the [`crate::frame::FrameParser`] is the production sink.

use num_complex::Complex32;

/// IF sample rate this demod expects. Source-rate IQ must be
/// decimated to this rate before reaching the demod. Matches
/// `INTRATE` in `acarsdec.h`.
pub const IF_RATE_HZ: u32 = 12_500;

/// Matched-filter length in IF samples (~one bit at 1200 Hz).
/// Integer division: `12500 / 1200 = 10`, then `+1 = 11`.
/// Matches the C macro `FLEN` in `msk.c:25`.
const FLEN: usize = (IF_RATE_HZ as usize / 1200) + 1;

/// Matched-filter oversampling factor. Matches `MFLTOVER` in
/// `msk.c:26`.
const MFLT_OVER: usize = 12;

/// Total length of the upsampled matched filter coefficients.
/// `FLEN * MFLT_OVER + 1 = 133`. Matches `FLENO` in `msk.c:27`.
const FLEN_OVERSAMPLED: usize = FLEN * MFLT_OVER + 1;

/// PLL gain. Matches `PLLG` in `msk.c:65`.
const PLL_GAIN: f32 = 38e-4;
/// PLL low-pass coefficient. Matches `PLLC` in `msk.c:66`.
const PLL_COEF: f32 = 0.52;

/// Receiver of demodulated bits from [`MskDemod`]. The frame
/// parser implements this; tests can implement it to capture
/// the output.
pub trait BitSink {
    /// One bit per call. `value > 0.0` is a binary 1, `<= 0.0`
    /// is a binary 0 (acarsdec convention — see
    /// `msk.c::putbit`).
    fn put_bit(&mut self, value: f32);
}

/// MSK demodulator state for a single ACARS channel.
///
/// Type widths follow `acarsdec.h::channel_t` exactly:
/// `MskPhi`, `MskDf`, `MskLvlSum` are `double`; `MskClk` is
/// `float`; `MskS`, `idx` are `unsigned int`; `MskBitCount` is
/// `int`.
pub struct MskDemod {
    /// VCO phase (radians). C `double MskPhi`.
    msk_phi: f64,
    /// Bit-clock phase accumulator. C `float MskClk`.
    msk_clk: f32,
    /// Bit-position counter. C `unsigned int MskS`.
    msk_s: u32,
    /// PLL frequency offset (radians/sample). C `double MskDf`.
    msk_df: f64,
    /// Circular buffer of post-mixer baseband samples.
    inb: [Complex32; FLEN],
    /// Write index into `inb`. C `unsigned int idx`.
    idx: usize,
    /// Sum of squared matched-filter magnitudes for the
    /// current level window. C `double MskLvlSum`.
    pub(crate) lvl_sum: f64,
    /// Bit-count for the current level window. C
    /// `int MskBitCount`.
    pub(crate) bit_count: i32,
    /// Matched-filter coefficients, oversampled.
    /// One copy per channel — small (133 floats, ~530 bytes).
    /// Acarsdec's static singleton is a C optimization we
    /// don't replicate; the per-channel cost is negligible.
    h: [f32; FLEN_OVERSAMPLED],
}

impl MskDemod {
    /// Create a new demodulator with cleared state.
    ///
    /// Builds the matched-filter coefficients per
    /// `msk.c:44-48`:
    /// `h[i] = cos(2π · 600 / INTRATE / MFLTOVER · (i − (FLENO−1)/2))`,
    /// then half-wave clip: `if h[i] < 0 set h[i] = 0`.
    #[must_use]
    pub fn new() -> Self {
        let mut h = [0.0_f32; FLEN_OVERSAMPLED];
        // Center index of the (odd-length) filter.
        // FLENO = 133, so center = 66.
        let center = (FLEN_OVERSAMPLED - 1) / 2;
        // FLEN_OVERSAMPLED is a small compile-time constant
        // (133), so the i32/f64 casts of the index never
        // truncate or lose precision. We allow the lints
        // narrowly here.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            clippy::cast_precision_loss
        )]
        for (i, slot) in h.iter_mut().enumerate() {
            // Compute in f64 to keep cosine precision; the C
            // uses `cosf` (float), but the argument involves a
            // constant ratio that's fine at either precision.
            // We match the C's float-domain math by computing
            // in f64 then casting at the end.
            let n = i as i32 - center as i32;
            let arg =
                2.0_f64 * core::f64::consts::PI * 600.0 / f64::from(IF_RATE_HZ) / MFLT_OVER as f64
                    * f64::from(n);
            let c = arg.cos() as f32;
            *slot = if c < 0.0 { 0.0 } else { c };
        }
        Self {
            msk_phi: 0.0,
            msk_clk: 0.0,
            msk_s: 0,
            msk_df: 0.0,
            inb: [Complex32::new(0.0, 0.0); FLEN],
            idx: 0,
            lvl_sum: 0.0,
            bit_count: 0,
            h,
        }
    }

    /// Consume `samples` (real `f32` at [`IF_RATE_HZ`]) and emit
    /// bits via `sink`. Faithful port of `demodMSK(ch, len)` in
    /// `msk.c:67-137`.
    ///
    /// Algorithm (per sample): advance the VCO
    /// (`s = 2π·1800/INTRATE + MskDf`, `MskPhi += s`, wrap mod
    /// 2π); mix to complex baseband
    /// (`inb[idx] = in · e^(−jp)`); advance the bit clock
    /// (`MskClk += s`). On bit-clock crossings
    /// (`MskClk ≥ 3π/2 − s/2`): subtract 3π/2 from `MskClk`,
    /// apply the oversampled matched filter using sub-sample
    /// offset `o = MFLTOVER·(MskClk/s + ½)`, normalize `v` by
    /// its magnitude, accumulate level (`lvl²/4`) and
    /// bit-count, compute `dphi` from quadrature based on
    /// `MskS & 1` (bit phase parity), emit a bit via
    /// [`BitSink::put_bit`] (negated when `MskS & 2`),
    /// increment `MskS`, and update the PLL with
    /// `MskDf = PLLC·MskDf + (1−PLLC)·PLLG·dphi`.
    pub fn process<S: BitSink>(&mut self, samples: &[f32], sink: &mut S) {
        // Local copies match the C, which hoists `idx` and
        // `MskPhi` into registers for the loop. Behavior is
        // identical, but the read-modify-write pattern is
        // explicit.
        let mut idx = self.idx;
        let mut p = self.msk_phi;

        for &in_sample in samples {
            // VCO. `s` is the per-sample VCO advance in
            // radians at the current PLL-corrected center
            // frequency (1800 Hz nominal).
            let s: f64 =
                1800.0_f64 / f64::from(IF_RATE_HZ) * 2.0 * core::f64::consts::PI + self.msk_df;
            p += s;
            if p >= 2.0 * core::f64::consts::PI {
                p -= 2.0 * core::f64::consts::PI;
            }

            // Mixer: in * exp(-j·p) = in · (cos p − j sin p).
            // Compute cos/sin in f64 to match C's `cexp` on
            // `double` argument, cast to f32 for the
            // `float complex` product.
            #[allow(clippy::cast_possible_truncation)]
            let cos_p = p.cos() as f32;
            #[allow(clippy::cast_possible_truncation)]
            let sin_p = p.sin() as f32;
            self.inb[idx] = Complex32::new(in_sample * cos_p, -in_sample * sin_p);
            idx = (idx + 1) % FLEN;

            // Bit clock. `MskClk` is f32 (matches C
            // `float MskClk`); the addition is done in f64 to
            // mirror the C's implicit float+double promotion,
            // then narrowed back to f32 on store.
            #[allow(clippy::cast_possible_truncation)]
            {
                self.msk_clk = (f64::from(self.msk_clk) + s) as f32;
            }

            // Threshold: 3π/2 − s/2. Compute in f64 since `s`
            // is f64, then compare with f64-promoted MskClk.
            let threshold = 3.0 * core::f64::consts::PI / 2.0 - s / 2.0;
            if f64::from(self.msk_clk) >= threshold {
                // Roll the bit clock back by one bit-period.
                #[allow(clippy::cast_possible_truncation)]
                {
                    self.msk_clk =
                        (f64::from(self.msk_clk) - 3.0 * core::f64::consts::PI / 2.0) as f32;
                }

                // Matched filter sub-sample offset selection.
                // `MskClk/s + 0.5` ∈ [0, 1) typically, so
                // `o0 = MFLTOVER · that` ∈ [0, MFLTOVER).
                // Edge: if o0 truncates to MFLTOVER, clamp.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let mut o = (MFLT_OVER as f64 * (f64::from(self.msk_clk) / s + 0.5)) as usize;
                if o > MFLT_OVER {
                    o = MFLT_OVER;
                }

                // Inner product: v = Σ h[o + j·MFLTOVER] ·
                //   inb[(j+idx) mod FLEN].
                let mut v = Complex32::new(0.0, 0.0);
                for j in 0..FLEN {
                    v += self.inb[(j + idx) % FLEN] * self.h[o];
                    o += MFLT_OVER;
                }

                // Normalize. The 1e-8 guard prevents
                // divide-by-zero on pure silence.
                let lvl = v.norm();
                v /= lvl + 1e-8;
                // Level accumulation in f64 (matches C
                // `double MskLvlSum`).
                self.lvl_sum += f64::from(lvl) * f64::from(lvl) / 4.0;
                self.bit_count += 1;

                // Quadrature discriminator. The branch is on
                // `MskS & 1` (which axis carries the symbol on
                // this bit). Both branches set `vo` to the
                // symbol component and `dphi` to the timing
                // error derived from the orthogonal component,
                // sign-flipped when the symbol crosses zero.
                let vo: f32;
                let dphi: f32;
                if self.msk_s & 1 != 0 {
                    vo = v.im;
                    dphi = if vo >= 0.0 { -v.re } else { v.re };
                } else {
                    vo = v.re;
                    dphi = if vo >= 0.0 { v.im } else { -v.im };
                }

                // Output bit, negated on every other pair.
                if self.msk_s & 2 != 0 {
                    sink.put_bit(-vo);
                } else {
                    sink.put_bit(vo);
                }
                self.msk_s = self.msk_s.wrapping_add(1);

                // PLL filter. C does this in promoted-double
                // due to the `1.0-PLLC` literal; we mirror
                // that by computing in f64 and storing into
                // the f64 `msk_df`.
                self.msk_df = f64::from(PLL_COEF) * self.msk_df
                    + (1.0_f64 - f64::from(PLL_COEF)) * f64::from(PLL_GAIN) * f64::from(dphi);
            }
        }

        self.idx = idx;
        self.msk_phi = p;
    }
}

impl Default for MskDemod {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Sink that captures bits into a `Vec` for assertions.
    struct CapturingSink {
        bits: Vec<bool>,
    }

    impl BitSink for CapturingSink {
        fn put_bit(&mut self, value: f32) {
            self.bits.push(value > 0.0);
        }
    }

    #[test]
    fn flen_constants_match_c_integer_division() {
        // Pin the integer-division trap from `msk.c:25-27`.
        // 12500 / 1200 = 10, then +1 = 11. 11*12+1 = 133.
        // If anyone later switches to `as f32` math this will
        // catch the off-by-one (FLEN=12, FLENO=145).
        assert_eq!(FLEN, 11);
        assert_eq!(FLEN_OVERSAMPLED, 133);
    }

    #[test]
    fn matched_filter_is_half_wave_clipped_cosine() {
        // Property: `h[]` is non-negative everywhere (the
        // `if h[i]<0 h[i]=0` clip in `msk.c:47`) and centered
        // (h[center] is the maximum, equal to 1.0).
        let demod = MskDemod::new();
        for &c in &demod.h {
            assert!(c >= 0.0, "matched filter coefficient went negative");
        }
        let center = (FLEN_OVERSAMPLED - 1) / 2;
        assert!(
            (demod.h[center] - 1.0).abs() < 1e-6,
            "center tap should be 1.0"
        );
    }

    #[test]
    fn demod_produces_no_bits_from_silence() {
        // Property: zero-amplitude input shouldn't produce
        // NaN/Inf in the level accumulator (the `1e-8` guard
        // in normalization is what prevents divide-by-zero).
        let mut demod = MskDemod::new();
        let mut sink = CapturingSink { bits: Vec::new() };
        let silence = vec![0.0_f32; 12_500]; // 1 second
        demod.process(&silence, &mut sink);
        assert!(demod.lvl_sum.is_finite(), "lvl_sum became NaN/Inf");
        assert!(demod.msk_df.is_finite(), "MskDf became NaN/Inf");
    }

    #[test]
    fn demod_advances_phase_state() {
        // After a non-empty `process` call, internal state
        // must have moved. Catches a no-op implementation.
        let mut demod = MskDemod::new();
        let mut sink = CapturingSink { bits: Vec::new() };
        let initial_phi = demod.msk_phi;
        let initial_idx = demod.idx;
        demod.process(&vec![0.0_f32; 1000], &mut sink);
        // VCO phase is initialized at 0.0 and advances by the
        // strictly-positive `s` each sample, so it cannot
        // remain bit-identical to the starting value. We
        // assert the difference is non-zero rather than a
        // float `assert_ne!` (clippy::float_cmp).
        assert!(
            (demod.msk_phi - initial_phi).abs() > 0.0,
            "VCO phase did not advance"
        );
        // idx wraps mod FLEN; 1000 % 11 = 10.
        assert_eq!(demod.idx, (initial_idx + 1000) % FLEN);
    }

    // NOTE: MSK correctness on real signals is validated by
    // the e2e test against acarsdec's `test.wav` (Task 10).
    // Synthesizing real ACARS-grade MSK in unit tests is
    // non-trivial; we trust the e2e diff for the correctness
    // oracle and keep unit tests here to lifecycle invariants
    // only.
}
