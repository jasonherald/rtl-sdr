//! QPSK hard slicer → soft i8 symbol pairs for FEC input.
//!
//! Each QPSK symbol carries 2 bits, mapped from the sign of
//! `(I, Q)`. The Viterbi decoder downstream wants soft information
//! rather than hard bits — we produce signed bytes scaled to ±127,
//! with magnitude proportional to constellation-point distance
//! from the decision boundary. Saturating clamp prevents overflow
//! on outliers.

use sdr_types::Complex;

/// Maximum soft-bit magnitude. Symmetric around zero so the
/// downstream FEC can index it as a signed Euclidean distance.
const SOFT_BIT_MAX: f32 = 127.0;

/// Slice one recovered QPSK symbol to two soft i8 bits. Output
/// `[i_bit, q_bit]` order matches CCSDS 131.0-B-3 convention: the
/// I-axis carries the high (G1) bit, Q-axis the low (G2).
#[must_use]
pub fn slice_soft(sample: Complex) -> [i8; 2] {
    [scale(sample.re), scale(sample.im)]
}

/// Scale an axis component to i8 range. Saturates beyond ±127.
#[allow(
    clippy::cast_possible_truncation,
    reason = "explicit clamp to [-127, 127] keeps the cast lossless"
)]
fn scale(x: f32) -> i8 {
    (x * SOFT_BIT_MAX)
        .round()
        .clamp(-SOFT_BIT_MAX, SOFT_BIT_MAX) as i8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_qpsk_constellation_to_signed_bytes() {
        // Standard QPSK constellation, normalized to unit
        // magnitude. After scaling, each axis lands near ±90
        // (= round(0.707 · 127)).
        let cases = [
            (Complex::new(0.707, 0.707), 90, 90),
            (Complex::new(-0.707, 0.707), -90, 90),
            (Complex::new(0.707, -0.707), 90, -90),
            (Complex::new(-0.707, -0.707), -90, -90),
        ];
        for (sample, expected_i, expected_q) in cases {
            let [i, q] = slice_soft(sample);
            assert!(
                (i32::from(i) - expected_i).abs() <= 1,
                "I: expected ~{expected_i}, got {i}",
            );
            assert!(
                (i32::from(q) - expected_q).abs() <= 1,
                "Q: expected ~{expected_q}, got {q}",
            );
        }
    }

    #[test]
    fn saturates_beyond_unit_magnitude() {
        let huge = Complex::new(5.0, -5.0);
        assert_eq!(slice_soft(huge), [127, -127]);
    }

    #[test]
    fn zero_maps_to_zero() {
        assert_eq!(slice_soft(Complex::new(0.0, 0.0)), [0, 0]);
    }
}
