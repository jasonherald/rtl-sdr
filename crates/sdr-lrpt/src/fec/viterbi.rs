//! Rate-1/2 K=7 Viterbi decoder for Meteor-M LRPT.
//!
//! CCSDS 131.0-B-3 standard convolutional code:
//! - Constraint length K = 7 (64 trellis states, since input bit
//!   is the decision variable)
//! - Soft-decision input: i8 ±127 (Euclidean-style branch metric)
//! - Output: 1 hard bit per pair of input soft symbols
//!
//! Polynomial constants are named after [medet's
//! `viterbi27.pas`](original/medet/viterbi27.pas) (POLYA = 79,
//! POLYB = 109) — these are the bit-reversed forms of the CCSDS
//! spec's G1 = 0o171 / G2 = 0o133. Either convention is correct
//! as long as encoder and decoder agree; matching medet means
//! future differential tests against medet / `SatDump` output line
//! up bit-exactly.
//!
//! Reference (read-only): `original/medet/viterbi27.pas`.

use std::collections::VecDeque;

/// Generator polynomial A (medet convention; bit-reversed CCSDS G1).
pub const POLYA: u8 = 79; // 0b1001111
/// Generator polynomial B (medet convention; bit-reversed CCSDS G2).
pub const POLYB: u8 = 109; // 0b1101101

/// Constraint length.
pub const K: usize = 7;

/// Number of trellis states (decision variable is the input bit;
/// state is the previous K-1 input bits, hence 2^(K-1)).
pub const NUM_STATES: usize = 1 << (K - 1);

/// Traceback depth in trellis steps. 5 × K is the conventional
/// safe minimum; 32 × K is overkill-safe for noisy input and
/// memory cost is trivial (32 × K × [`NUM_STATES`] bytes ≈ 14 KB).
pub const TRACEBACK_DEPTH: usize = 32 * K;

/// Streaming Viterbi decoder. Caller pushes pairs of soft symbols
/// (`[i8; 2]` per encoded bit), decoder emits decoded bits as the
/// traceback completes (after the first `TRACEBACK_DEPTH` pushes).
pub struct ViterbiDecoder {
    metrics: [i32; NUM_STATES],
    history: VecDeque<[u8; NUM_STATES]>,
}

impl Default for ViterbiDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ViterbiDecoder {
    #[must_use]
    pub fn new() -> Self {
        let mut metrics = [i32::MIN / 2; NUM_STATES];
        // Encoder starts in state 0 by CCSDS convention; biasing
        // the initial metric here gives the trellis a consistent
        // anchor for the first traceback.
        metrics[0] = 0;
        Self {
            metrics,
            history: VecDeque::with_capacity(TRACEBACK_DEPTH + 1),
        }
    }

    /// Push one pair of soft symbols (one encoded bit's worth).
    /// Returns `Some(bit)` when traceback emits a decoded bit
    /// (after the first [`TRACEBACK_DEPTH`] pushes).
    ///
    /// **Trellis convention.** State = `K-1 = 6` bits of the
    /// encoder shift register with bit 5 = newest. For each
    /// `(prev_state, input_bit)` pair we form the full 7-bit
    /// `combined = (input_bit << 6) | prev_state` snapshot the
    /// encoder would have at the moment `input_bit` is processed,
    /// tap with [`POLYA`] / [`POLYB`] to compute encoder output,
    /// then the successor state is `combined >> 1` (oldest bit
    /// dropped, `input_bit` slides into bit 5). This is the
    /// standard `libfec` formulation and matches what medet's
    /// encoder produces on the wire.
    pub fn step(&mut self, soft: [i8; 2]) -> Option<u8> {
        let mut new_metrics = [i32::MIN / 2; NUM_STATES];
        let mut parents = [0_u8; NUM_STATES];
        let high_bit_pos: usize = K - 1; // = 6
        for prev_state in 0..NUM_STATES {
            for input_bit in 0_u8..2 {
                let combined = (usize::from(input_bit) << high_bit_pos) | prev_state;
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "combined < 2^K = 128 fits in u8"
                )]
                let combined_u8 = combined as u8;
                let g1_out = parity_8(combined_u8 & POLYA);
                let g2_out = parity_8(combined_u8 & POLYB);
                let next_state = combined >> 1;
                // Branch metric: soft-decision correlation with
                // the encoded bits the encoder would have emitted
                // along this trellis edge. Higher metric = closer
                // match. Soft input range is ±127.
                let metric_g1 = if g1_out == 0 {
                    i32::from(soft[0])
                } else {
                    -i32::from(soft[0])
                };
                let metric_g2 = if g2_out == 0 {
                    i32::from(soft[1])
                } else {
                    -i32::from(soft[1])
                };
                let candidate = self.metrics[prev_state] + metric_g1 + metric_g2;
                if candidate > new_metrics[next_state] {
                    new_metrics[next_state] = candidate;
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "prev_state < NUM_STATES = 64 fits in u8"
                    )]
                    {
                        parents[next_state] = prev_state as u8;
                    }
                }
            }
        }
        self.metrics = new_metrics;
        self.history.push_back(parents);
        // Renormalize so accumulated metrics never overflow i32.
        // Subtract the min from every state — preserves relative
        // ordering, prevents unbounded growth over long passes.
        let min = *self.metrics.iter().min().unwrap_or(&0);
        for m in &mut self.metrics {
            *m -= min;
        }
        if self.history.len() > TRACEBACK_DEPTH {
            // Trace back from current best state through history.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "best state index < NUM_STATES = 64"
            )]
            let best = self
                .metrics
                .iter()
                .enumerate()
                .max_by_key(|(_, m)| **m)
                .map_or(0_u8, |(i, _)| i as u8);
            let bit = self.traceback(best);
            self.history.pop_front();
            Some(bit)
        } else {
            None
        }
    }

    /// Trace back through history starting at `state` and return
    /// the decoded bit corresponding to the OLDEST trellis step
    /// in the current window.
    ///
    /// The high bit of any post-transition state IS the input bit
    /// that drove that transition (because the successor is
    /// `combined >> 1` and `combined`'s bit 6 was the input). To
    /// recover step 0's input we therefore want step 1's state,
    /// reached by tracing back through every parents entry EXCEPT
    /// the oldest.
    fn traceback(&self, mut state: u8) -> u8 {
        let depth = self.history.len().saturating_sub(1);
        for parents in self.history.iter().rev().take(depth) {
            state = parents[state as usize];
        }
        // State now = step 1's state. Bit 5 (= K-2) is input_bit_0.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "K=7, shift by 5 yields a single-bit value"
        )]
        let bit_pos = (K - 2) as u8;
        (state >> bit_pos) & 1
    }
}

/// Parity (XOR-fold) of an 8-bit value.
#[must_use]
pub fn parity_8(b: u8) -> u8 {
    let mut v = b;
    v ^= v >> 4;
    v ^= v >> 2;
    v ^= v >> 1;
    v & 1
}

/// CCSDS rate-1/2 K=7 convolutional encoder (medet polynomial
/// convention). For each input bit produces 2 saturated soft
/// samples (±127) suitable for feeding [`ViterbiDecoder::step`]
/// or [`super::SoftSyncDetector::push_symbol`] in tests.
///
/// Drains the encoder with K-1 trailing zero bits so the output
/// captures every encoded bit pair for `bits.len()` input bits.
///
/// `pub(crate)` and `#[cfg(test)]` because this is a test-only
/// utility shared between the viterbi tests and the FEC-chain
/// round-trip / rotation tests; no production code path uses it
/// (the Meteor satellite is the encoder in production).
#[cfg(test)]
pub(crate) fn ccsds_encode(bits: &[u8]) -> Vec<i8> {
    /// Soft-symbol magnitude for clean-signal test fixtures.
    /// Saturated to ±127 — the largest distance the soft slicer
    /// produces, modeling a noiseless encoded stream.
    const CLEAN_SOFT_MAG: i8 = 127;
    let mut shift_reg: u8 = 0;
    let mut out = Vec::with_capacity((bits.len() + K - 1) * 2);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "K = 7, shift count fits in u8"
    )]
    let high_bit_pos: u8 = (K - 1) as u8; // = 6
    let push_pair = |out: &mut Vec<i8>, g1: u8, g2: u8| {
        out.push(if g1 == 0 {
            CLEAN_SOFT_MAG
        } else {
            -CLEAN_SOFT_MAG
        });
        out.push(if g2 == 0 {
            CLEAN_SOFT_MAG
        } else {
            -CLEAN_SOFT_MAG
        });
    };
    for &b in bits {
        shift_reg = (shift_reg >> 1) | ((b & 1) << high_bit_pos);
        push_pair(
            &mut out,
            parity_8(shift_reg & POLYA),
            parity_8(shift_reg & POLYB),
        );
    }
    // Flush — append K-1 zeros to drain the encoder.
    for _ in 0..(K - 1) {
        shift_reg >>= 1;
        push_pair(
            &mut out,
            parity_8(shift_reg & POLYA),
            parity_8(shift_reg & POLYB),
        );
    }
    out
}

#[cfg(test)]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    #[test]
    fn parity_8_matches_xor_fold() {
        for b in 0..=255_u8 {
            let want = b.count_ones() & 1;
            assert_eq!(u32::from(parity_8(b)), want, "parity mismatch at {b}");
        }
    }

    #[test]
    fn round_trip_clean_signal() {
        let input_bits: Vec<u8> = (0..512).map(|i| ((i * 31 + 17) & 1) as u8).collect();
        let encoded = ccsds_encode(&input_bits);
        let mut dec = ViterbiDecoder::new();
        let mut decoded: Vec<u8> = Vec::new();
        for chunk in encoded.chunks_exact(2) {
            if let Some(bit) = dec.step([chunk[0], chunk[1]]) {
                decoded.push(bit);
            }
        }
        // `decoded[i]` aligns with `input_bits[i]` directly (the
        // decoder only starts emitting once the traceback window
        // is full, so there's no warmup prefix in the output
        // stream — the emitted bits ARE the steady-state output
        // for input positions 0, 1, 2, …).
        assert!(
            decoded.len() >= 100,
            "expected at least 100 emitted bits for meaningful coverage, got {}",
            decoded.len(),
        );
        let mismatches = decoded
            .iter()
            .zip(input_bits.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            mismatches, 0,
            "clean-signal round-trip must have zero bit errors"
        );
    }

    #[test]
    fn medet_polynomials_match_documented_values() {
        // Pin medet's POLYA / POLYB so a future "fix" doesn't
        // silently flip the convention and break differential
        // tests against medet / SatDump captures.
        assert_eq!(POLYA, 0b100_1111);
        assert_eq!(POLYB, 0b110_1101);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Minimum proptest bitstream length. The decoder only starts
    /// emitting after `TRACEBACK_DEPTH + 1` pushes; a tighter
    /// minimum produces too few decoded bits for the property to
    /// meaningfully exercise the steady-state path. 350 input
    /// bits → ~125 emitted bits, which is plenty of coverage and
    /// still small enough to keep proptest cases fast.
    const PROPTEST_MIN_LEN: usize = TRACEBACK_DEPTH + 125;

    /// Minimum number of decoded bits we require per proptest
    /// case for the assertions to be non-vacuous.
    const PROPTEST_MIN_DECODED: usize = 100;

    proptest! {
        #[test]
        fn viterbi_recovers_random_bitstreams(
            bits in proptest::collection::vec(0..2_u8, PROPTEST_MIN_LEN..500)
        ) {
            let encoded = ccsds_encode(&bits);
            let mut dec = ViterbiDecoder::new();
            let mut decoded: Vec<u8> = Vec::new();
            for chunk in encoded.chunks_exact(2) {
                if let Some(bit) = dec.step([chunk[0], chunk[1]]) {
                    decoded.push(bit);
                }
            }
            // `decoded[i]` aligns with `bits[i]` directly — no
            // warmup prefix in the emitted stream. Pin a minimum
            // decoded count so a shrunken case can't accidentally
            // pass with a near-empty mismatch count.
            prop_assert!(
                decoded.len() >= PROPTEST_MIN_DECODED,
                "decoded {} bits, want >= {}",
                decoded.len(), PROPTEST_MIN_DECODED,
            );
            let mismatches = decoded
                .iter()
                .zip(bits.iter())
                .filter(|(a, b)| a != b)
                .count();
            prop_assert_eq!(mismatches, 0);
        }

        #[test]
        fn viterbi_corrects_single_bit_errors(
            bits in proptest::collection::vec(0..2_u8, PROPTEST_MIN_LEN..500),
            error_idx in 0_usize..200,
        ) {
            let mut encoded = ccsds_encode(&bits);
            // Flip one soft symbol's sign — equivalent to a hard
            // bit flip in the encoded stream.
            let i = error_idx % encoded.len();
            encoded[i] = -encoded[i];
            let mut dec = ViterbiDecoder::new();
            let mut decoded: Vec<u8> = Vec::new();
            for chunk in encoded.chunks_exact(2) {
                if let Some(bit) = dec.step([chunk[0], chunk[1]]) {
                    decoded.push(bit);
                }
            }
            prop_assert!(
                decoded.len() >= PROPTEST_MIN_DECODED,
                "decoded {} bits, want >= {}",
                decoded.len(), PROPTEST_MIN_DECODED,
            );
            let mismatches = decoded
                .iter()
                .zip(bits.iter())
                .filter(|(a, b)| a != b)
                .count();
            // Single-bit error in a rate-1/2 K=7 code should be
            // correctable by Viterbi within a small window —
            // accept up to 2 bit errors as the steady-state
            // post-correction tolerance.
            prop_assert!(
                mismatches <= 2,
                "single-bit error caused {mismatches} decode errors"
            );
        }
    }
}
