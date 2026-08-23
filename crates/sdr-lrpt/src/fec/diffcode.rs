//! Differential QPSK decoder for legacy Meteor-M2 (NORAD 40069)
//! and any LRPT downlink that uses differential precoding.
//!
//! The old Meteor-M2 satellite differentially-encodes its QPSK
//! symbols so the link is immune to the receiver's absolute carrier
//! phase: information rides in the *transition* between consecutive
//! symbols rather than the absolute constellation point. The decoder
//! therefore multiplies each soft symbol component by the previous
//! one (so a phase rotation common to both cancels), then square-root
//! companding (`signsqrt`) keeps the product back inside the `i8`
//! soft range.
//!
//! Streaming, stateful: feed one `[i8; 2]` soft pair per call via
//! [`DiffDecoder::decode_pair`]; the previous pair is carried across
//! calls (and across CADUs — the satellite's differential stream is
//! continuous), exactly like dbdexter's static `_prev_i` / `_prev_q`.
//!
//! This runs on the **raw** soft stream, *before* sync correlation
//! and Viterbi — matching dbdexter `decode.c` (`if (_diffcoded)
//! diff_decode(...)` precedes `correlate`). The asymmetric sign on
//! the second component (the in-phase channel is negated) means the
//! component order is load-bearing: feed pairs in the same `[0, 1]`
//! order the demod / `.s` file produces, the way dbdexter does.
//!
//! Reference (read-only): dbdexter
//! `meteor_decode/diffcode/diffcode.c` (`diff_decode`, `signsqrt`)
//! and `meteor_decode/math/int.c` (`int_sqrt`).

/// Soft-symbol saturation magnitude — the largest value the slicer
/// produces (`i8::MAX`). `signsqrt` clamps to this so the single
/// edge case `-128 × -128 → 128` fits `i8`.
const SOFT_SAT: i32 = 127;

/// Sign-preserving integer square root used to compand the product
/// of two soft components back into the `i8` range.
///
/// `signsqrt(x) = sign(x) · floor(√|x|)`, saturated to `±127`. The
/// product of two `i8`s spans `[-16384, 16129]`, whose root is at
/// most 128 — the single value 128 (only reachable from
/// `-128 × -128`) is clamped to 127 so the result always fits `i8`.
/// dbdexter casts straight to `int8_t` and lets 128 wrap to `-128`;
/// clamping is both safer and a no-op for every real soft sample
/// (the demod never emits `-128`).
#[must_use]
fn signsqrt(x: i32) -> i8 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "isqrt of |x| <= 16384 is <= 128; clamped to SOFT_SAT below"
    )]
    let root = (x.unsigned_abs().isqrt() as i32).min(SOFT_SAT) as i8;
    if x < 0 { -root } else { root }
}

/// Streaming differential QPSK decoder. One instance per LRPT pass;
/// state persists across pairs and CADUs.
pub struct DiffDecoder {
    /// Previous pair's component 0 (dbdexter `_prev_q`).
    prev0: i32,
    /// Previous pair's component 1 (dbdexter `_prev_i`).
    prev1: i32,
}

impl Default for DiffDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffDecoder {
    /// New decoder. Initial previous components are zero (matching
    /// dbdexter's zero-initialised static state), so the very first
    /// decoded pair is `[0, 0]` — a negligible one-symbol warmup.
    #[must_use]
    pub fn new() -> Self {
        Self { prev0: 0, prev1: 0 }
    }

    /// Reset to the warmup state. Called by [`super::FecChain::reset`]
    /// between passes.
    pub fn reset(&mut self) {
        self.prev0 = 0;
        self.prev1 = 0;
    }

    /// Differentially decode one soft pair, returning the decoded
    /// pair. Component 0 is `signsqrt(cur0 · prev0)`; component 1 is
    /// `signsqrt(-cur1 · prev1)` (the in-phase channel's extra sign
    /// flip mirrors dbdexter `diff_decode`). The current pair becomes
    /// the "previous" for the next call.
    pub fn decode_pair(&mut self, soft: [i8; 2]) -> [i8; 2] {
        let cur0 = i32::from(soft[0]);
        let cur1 = i32::from(soft[1]);
        let out = [signsqrt(cur0 * self.prev0), signsqrt(-cur1 * self.prev1)];
        self.prev0 = cur0;
        self.prev1 = cur1;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signsqrt_basic_values() {
        assert_eq!(signsqrt(0), 0);
        assert_eq!(signsqrt(16129), 127); // 127*127
        assert_eq!(signsqrt(-16129), -127);
        assert_eq!(signsqrt(16384), 127); // 128*128 -> clamped from 128
        assert_eq!(signsqrt(-16384), -127);
        assert_eq!(signsqrt(100), 10);
        assert_eq!(signsqrt(-100), -10);
        assert_eq!(signsqrt(2), 1);
    }

    #[test]
    fn first_pair_is_warmup_zero() {
        let mut d = DiffDecoder::new();
        // Initial prev = 0, so the first decoded pair is [0, 0]
        // regardless of input.
        assert_eq!(d.decode_pair([127, -127]), [0, 0]);
    }

    /// Differentially *encode* a target sign stream, decode it back,
    /// and confirm the decoder recovers the intended signs. This is
    /// the load-bearing property: a differential decoder must invert
    /// a matching differential encoder.
    ///
    /// Encoder (sign domain, the inverse of `decode_pair`):
    ///   tx0[n] = want0[n] · tx0[n-1]
    ///   tx1[n] = -want1[n] · tx1[n-1]
    /// with tx[-1] seeded to the decoder's prev (here +1 after the
    /// first transmitted symbol). Decoding then yields:
    ///   dec0[n] = sign(tx0[n] · tx0[n-1]) = want0[n]
    ///   dec1[n] = sign(-tx1[n] · tx1[n-1]) = want1[n]
    #[test]
    fn round_trips_a_matching_differential_encoder() {
        // Target sign stream we want to recover (skip index 0, which
        // is the encoder's seed symbol).
        let want: [[i8; 2]; 12] = [
            [1, 1],
            [1, -1],
            [-1, 1],
            [-1, -1],
            [1, 1],
            [-1, 1],
            [1, -1],
            [-1, -1],
            [1, -1],
            [-1, -1],
            [1, 1],
            [-1, 1],
        ];
        // Seed transmitted symbol (any non-zero sign pair). The
        // decoder's prev starts at 0, so the first *transmitted*
        // symbol decodes to 0 and is discarded; thereafter prev is
        // this seed's signs.
        let mut tx0: i32 = 1;
        let mut tx1: i32 = 1;
        let mut d = DiffDecoder::new();
        // Push the seed symbol through (warmup; output ignored).
        let _ = d.decode_pair([127, 127]);
        for (n, w) in want.iter().enumerate() {
            // Differentially encode in the sign domain.
            tx0 *= i32::from(w[0]);
            tx1 *= -i32::from(w[1]);
            let sym = [
                if tx0 > 0 { 127 } else { -127 },
                if tx1 > 0 { 127 } else { -127 },
            ];
            let dec = d.decode_pair(sym);
            assert_eq!(
                [dec[0].signum(), dec[1].signum()],
                *w,
                "differential round-trip mismatch at symbol {n}",
            );
        }
    }

    #[test]
    fn reset_clears_state() {
        let mut d = DiffDecoder::new();
        let _ = d.decode_pair([127, 127]);
        let _ = d.decode_pair([100, -50]);
        d.reset();
        // After reset, first pair is warmup zero again.
        assert_eq!(d.decode_pair([127, -127]), [0, 0]);
    }
}
