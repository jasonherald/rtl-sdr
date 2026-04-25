//! Reed-Solomon (255, 223) decoder for CCSDS-formatted frames.
//!
//! Direct Rust port of `medet/alib/ecc.pas` (RS encode/decode for
//! the CCSDS dual-basis convention). 32 parity bytes per 255-byte
//! codeword, corrects up to T = 16 byte errors per codeword.
//!
//! **Dual basis.** CCSDS 101.0-B-3 §A2.2 specifies a particular
//! polynomial basis (dual basis) for byte representation that
//! differs from the conventional "standard basis" used by most
//! off-the-shelf RS implementations. medet's [`ALPHA`] / [`IDX`]
//! lookup tables already encode this — they are the dual-basis
//! log / antilog tables, so the decode loop operates directly on
//! the bytes coming off the wire without an explicit basis
//! conversion.
//!
//! **Magic constants.**
//! - `(112 + i) * 11` in the syndrome computation: CCSDS uses
//!   first-consecutive-root α^112 and code-generator α^11
//!   (rather than the textbook α^1 / α). These are the spec's
//!   characteristic constants.
//! - `root[j] * 111` in Forney's evaluation: the inverse of
//!   the first-root offset under modular arithmetic mod 255.
//!
//! Reference (read-only): `original/medet/alib/ecc.pas`.

/// RS codeword length.
pub const N: usize = 255;
/// Message length per codeword.
pub const K: usize = 223;
/// Number of parity bytes per codeword.
pub const PARITY: usize = N - K; // 32
/// Maximum correctable byte errors per codeword.
pub const T: usize = PARITY / 2; // 16

// --- CCSDS / GF(256) characteristic constants ---
//
// These embed the CCSDS-RS spec choices that distinguish this
// decoder from a textbook RS(255, 223). Lifted to named constants
// so the algorithm reads as a series of named primitives instead
// of bare numerals.

/// Number of nonzero elements in GF(256). All exponent arithmetic
/// is done modulo this value.
const GF_NONZERO: u32 = 255;
/// Length of the error-locator polynomial register (`λ` and `B`).
/// `PARITY + 1` because λ has degree at most `PARITY` (= 2T),
/// hence `PARITY + 1` coefficients.
const LAMBDA_LEN: usize = PARITY + 1;
/// CCSDS-RS first consecutive root of the code generator. Per
/// CCSDS 101.0-B-3 the first root is α^112 (rather than the
/// textbook α^1).
const FIRST_ROOT_INDEX: u32 = 112;
/// CCSDS-RS code generator power. The code generator polynomial
/// has roots at α^(`FIRST_ROOT_INDEX` + i · `CODE_GENERATOR_POWER`),
/// i = 0..2T-1. For CCSDS that's α^(112 + 11·i).
const CODE_GENERATOR_POWER: u32 = 11;
/// Per-step increment for the dual-basis location index in
/// Chien search. `LOCATION_STEP` and `LOCATION_INIT` together
/// implement medet's location-walk: `k = (k + LOCATION_STEP) mod
/// GF_NONZERO` advances the search through dual-basis positions.
const LOCATION_STEP: u32 = 116;
/// Initial dual-basis location index for Chien search (one less
/// than `LOCATION_STEP` so the first iteration lands on the
/// correct first position).
const LOCATION_INIT: u32 = 115;
/// CCSDS-RS Forney-evaluation pre-factor power. The Forney error
/// magnitude for root r includes a factor α^(r · `FORNEY_NUM2_POWER`)
/// — characteristic of the dual-basis representation.
const FORNEY_NUM2_POWER: u32 = 111;
/// Maximum index when computing the Forney denominator. Loop
/// walks `λ` indices in steps of 2 starting from
/// `min(deg_lambda, FORNEY_DEN_MAX_IDX)`; the cap exists because
/// `λ` has only [`LAMBDA_LEN`] = `PARITY + 1` coefficients and we
/// index `λ[i+1]`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "PARITY = 32, well within i32 range"
)]
const FORNEY_DEN_MAX_IDX: i32 = (PARITY - 1) as i32;

/// GF(256) antilog table — `ALPHA[i] = α^i mod field-poly` under
/// the CCSDS dual-basis primitive polynomial. Verbatim from
/// `medet/alib/ecc.pas`.
const ALPHA: [u8; 256] = [
    0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x87, 0x89, 0x95, 0xad, 0xdd, 0x3d, 0x7a, 0xf4,
    0x6f, 0xde, 0x3b, 0x76, 0xec, 0x5f, 0xbe, 0xfb, 0x71, 0xe2, 0x43, 0x86, 0x8b, 0x91, 0xa5, 0xcd,
    0x1d, 0x3a, 0x74, 0xe8, 0x57, 0xae, 0xdb, 0x31, 0x62, 0xc4, 0x0f, 0x1e, 0x3c, 0x78, 0xf0, 0x67,
    0xce, 0x1b, 0x36, 0x6c, 0xd8, 0x37, 0x6e, 0xdc, 0x3f, 0x7e, 0xfc, 0x7f, 0xfe, 0x7b, 0xf6, 0x6b,
    0xd6, 0x2b, 0x56, 0xac, 0xdf, 0x39, 0x72, 0xe4, 0x4f, 0x9e, 0xbb, 0xf1, 0x65, 0xca, 0x13, 0x26,
    0x4c, 0x98, 0xb7, 0xe9, 0x55, 0xaa, 0xd3, 0x21, 0x42, 0x84, 0x8f, 0x99, 0xb5, 0xed, 0x5d, 0xba,
    0xf3, 0x61, 0xc2, 0x03, 0x06, 0x0c, 0x18, 0x30, 0x60, 0xc0, 0x07, 0x0e, 0x1c, 0x38, 0x70, 0xe0,
    0x47, 0x8e, 0x9b, 0xb1, 0xe5, 0x4d, 0x9a, 0xb3, 0xe1, 0x45, 0x8a, 0x93, 0xa1, 0xc5, 0x0d, 0x1a,
    0x34, 0x68, 0xd0, 0x27, 0x4e, 0x9c, 0xbf, 0xf9, 0x75, 0xea, 0x53, 0xa6, 0xcb, 0x11, 0x22, 0x44,
    0x88, 0x97, 0xa9, 0xd5, 0x2d, 0x5a, 0xb4, 0xef, 0x59, 0xb2, 0xe3, 0x41, 0x82, 0x83, 0x81, 0x85,
    0x8d, 0x9d, 0xbd, 0xfd, 0x7d, 0xfa, 0x73, 0xe6, 0x4b, 0x96, 0xab, 0xd1, 0x25, 0x4a, 0x94, 0xaf,
    0xd9, 0x35, 0x6a, 0xd4, 0x2f, 0x5e, 0xbc, 0xff, 0x79, 0xf2, 0x63, 0xc6, 0x0b, 0x16, 0x2c, 0x58,
    0xb0, 0xe7, 0x49, 0x92, 0xa3, 0xc1, 0x05, 0x0a, 0x14, 0x28, 0x50, 0xa0, 0xc7, 0x09, 0x12, 0x24,
    0x48, 0x90, 0xa7, 0xc9, 0x15, 0x2a, 0x54, 0xa8, 0xd7, 0x29, 0x52, 0xa4, 0xcf, 0x19, 0x32, 0x64,
    0xc8, 0x17, 0x2e, 0x5c, 0xb8, 0xf7, 0x69, 0xd2, 0x23, 0x46, 0x8c, 0x9f, 0xb9, 0xf5, 0x6d, 0xda,
    0x33, 0x66, 0xcc, 0x1f, 0x3e, 0x7c, 0xf8, 0x77, 0xee, 0x5b, 0xb6, 0xeb, 0x51, 0xa2, 0xc3, 0x00,
];

/// GF(256) log table — `IDX[ALPHA[i]] = i`. `IDX[0] = 255` is
/// the canonical sentinel for log(0) = ∞ (medet convention).
/// Verbatim from `medet/alib/ecc.pas`.
const IDX: [u8; 256] = [
    255, 0, 1, 99, 2, 198, 100, 106, 3, 205, 199, 188, 101, 126, 107, 42, 4, 141, 206, 78, 200,
    212, 189, 225, 102, 221, 127, 49, 108, 32, 43, 243, 5, 87, 142, 232, 207, 172, 79, 131, 201,
    217, 213, 65, 190, 148, 226, 180, 103, 39, 222, 240, 128, 177, 50, 53, 109, 69, 33, 18, 44, 13,
    244, 56, 6, 155, 88, 26, 143, 121, 233, 112, 208, 194, 173, 168, 80, 117, 132, 72, 202, 252,
    218, 138, 214, 84, 66, 36, 191, 152, 149, 249, 227, 94, 181, 21, 104, 97, 40, 186, 223, 76,
    241, 47, 129, 230, 178, 63, 51, 238, 54, 16, 110, 24, 70, 166, 34, 136, 19, 247, 45, 184, 14,
    61, 245, 164, 57, 59, 7, 158, 156, 157, 89, 159, 27, 8, 144, 9, 122, 28, 234, 160, 113, 90,
    209, 29, 195, 123, 174, 10, 169, 145, 81, 91, 118, 114, 133, 161, 73, 235, 203, 124, 253, 196,
    219, 30, 139, 210, 215, 146, 85, 170, 67, 11, 37, 175, 192, 115, 153, 119, 150, 92, 250, 82,
    228, 236, 95, 74, 182, 162, 22, 134, 105, 197, 98, 254, 41, 125, 187, 204, 224, 211, 77, 140,
    242, 31, 48, 220, 130, 171, 231, 86, 179, 147, 64, 216, 52, 176, 239, 38, 55, 12, 17, 68, 111,
    120, 25, 154, 71, 116, 167, 193, 35, 83, 137, 251, 20, 93, 248, 151, 46, 75, 185, 96, 15, 237,
    62, 229, 246, 135, 165, 23, 58, 163, 60, 183,
];

/// RS generator polynomial coefficients (33 entries). Verbatim
/// from `medet/alib/ecc.pas`. Used by [`encode`] only.
const POLY: [u8; 33] = [
    0, 249, 59, 66, 4, 43, 126, 251, 97, 30, 3, 213, 50, 66, 170, 5, 24, 5, 170, 66, 50, 213, 3,
    30, 97, 251, 126, 43, 4, 66, 59, 249, 0,
];

/// Sentinel returned by [`IDX`] for log(0). Standard medet
/// convention.
const LOG_ZERO: u8 = 255;

/// Decode failure modes.
#[derive(Debug, thiserror::Error)]
pub enum RsError {
    /// More than `T` byte errors — beyond correction capacity.
    #[error("uncorrectable: more than T={T} byte errors")]
    Uncorrectable,
}

/// CCSDS Reed-Solomon (255, 223) encoder / decoder. Stateless;
/// the struct is a marker for API symmetry with the other FEC
/// types in this module ([`super::ViterbiDecoder`] etc.).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReedSolomon;

impl ReedSolomon {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Encode `K` message bytes into a 255-byte RS codeword.
    /// Systematic: bytes 0..K of the output equal `message`,
    /// bytes K..N hold the parity. Used in tests + by anyone
    /// wanting to round-trip data through the FEC.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_lossless,
        clippy::many_single_char_names,
        clippy::if_not_else,
        reason = "GF(256) arithmetic is bounded to u8 by construction; \
                  single-char names match the medet pascal source for \
                  side-by-side readability"
    )]
    pub fn encode(&self, message: &[u8; K]) -> [u8; N] {
        let mut codeword = [0_u8; N];
        codeword[..K].copy_from_slice(message);
        // Parity goes in bytes K..N (final 32 bytes). Loop over
        // message bytes shifting through a 32-byte feedback shift
        // register; final state IS the parity. Pad = 0 (no
        // shortened codeword) for the standard 255-byte form.
        let mut bb = [0_u8; PARITY];
        for &m in message {
            let feedback = IDX[(m ^ bb[0]) as usize];
            if feedback != LOG_ZERO {
                for j in 1..PARITY {
                    let exponent = (u32::from(feedback) + u32::from(POLY[PARITY - j])) % GF_NONZERO;
                    bb[j] ^= ALPHA[exponent as usize];
                }
            }
            bb.copy_within(1..PARITY, 0);
            bb[PARITY - 1] = if feedback != LOG_ZERO {
                ALPHA[((u32::from(feedback) + u32::from(POLY[0])) % GF_NONZERO) as usize]
            } else {
                0
            };
        }
        codeword[K..].copy_from_slice(&bb);
        codeword
    }

    /// Decode one 255-byte RS codeword. Returns the corrected
    /// codeword and the number of byte errors that were
    /// corrected. Returns `Err(RsError::Uncorrectable)` if more
    /// than `T = 16` byte errors are present.
    ///
    /// # Errors
    ///
    /// Returns `RsError::Uncorrectable` when the error count
    /// exceeds `T` and the decoder cannot recover the message.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::cast_lossless,
        clippy::many_single_char_names,
        clippy::range_plus_one,
        clippy::needless_range_loop,
        clippy::if_not_else,
        clippy::too_many_lines,
        reason = "GF(256) arithmetic is bounded to u8 by construction; \
                  single-char names + index aliases match the medet \
                  pascal source for side-by-side readability; the long \
                  body is a faithful port of medet's ecc_decode and \
                  splitting it would obscure the BM/Chien/Forney flow"
    )]
    pub fn decode(&self, codeword: &[u8; N]) -> Result<([u8; N], u8), RsError> {
        let mut data = *codeword;

        // Compute syndromes S_0 .. S_31 over GF(256).
        // CCSDS RS uses first-consecutive-root α^112 and code-
        // generator α^11, so the per-symbol multiplier is α^((112 +
        // i) * 11) — see medet/alib/ecc.pas line 136.
        let mut s = [0_u8; PARITY];
        for (i, slot) in s.iter_mut().enumerate() {
            *slot = data[0];
            for j in 1..N {
                if *slot == 0 {
                    *slot = data[j];
                } else {
                    let exponent = (u32::from(IDX[*slot as usize])
                        + (FIRST_ROOT_INDEX + i as u32) * CODE_GENERATOR_POWER)
                        % GF_NONZERO;
                    *slot = data[j] ^ ALPHA[exponent as usize];
                }
            }
        }

        // Quick exit: if every syndrome is zero the codeword is
        // already valid — no errors to correct.
        let syn_error: u8 = s.iter().fold(0_u8, |acc, &b| acc | b);
        if syn_error == 0 {
            return Ok((data, 0));
        }

        // Convert syndromes from value-domain to log-domain for
        // the Berlekamp-Massey loop.
        let mut s_log = [0_u8; PARITY];
        for (i, slot) in s_log.iter_mut().enumerate() {
            *slot = IDX[s[i] as usize];
        }

        // Berlekamp-Massey to find the error-locator polynomial
        // λ(x). Both `lambda` and `b` are 33-element registers
        // (degree-32 polynomials).
        let mut lambda = [0_u8; LAMBDA_LEN];
        lambda[0] = 1;
        let mut b = [0_u8; LAMBDA_LEN];
        for i in 0..LAMBDA_LEN {
            b[i] = IDX[lambda[i] as usize];
        }
        let mut t = [0_u8; LAMBDA_LEN];
        let mut el: i32 = 0;
        for r in 1..=PARITY as i32 {
            // Discrepancy: Δ = Σ λ_i · S_{r-i-1}
            let mut discr_r = 0_u8;
            for i in 0..r as usize {
                if lambda[i] != 0 && s_log[r as usize - i - 1] != LOG_ZERO {
                    let exponent = (u32::from(IDX[lambda[i] as usize])
                        + u32::from(s_log[r as usize - i - 1]))
                        % GF_NONZERO;
                    discr_r ^= ALPHA[exponent as usize];
                }
            }
            let discr_r_log = IDX[discr_r as usize];
            if discr_r_log == LOG_ZERO {
                // Δ = 0: shift b right by one, prepend the log-
                // zero sentinel.
                b.copy_within(0..PARITY, 1);
                b[0] = LOG_ZERO;
            } else {
                // T(x) = λ(x) − (Δ/b) · x · B(x)
                t[0] = lambda[0];
                for i in 0..PARITY {
                    if b[i] != LOG_ZERO {
                        let exponent = (u32::from(discr_r_log) + u32::from(b[i])) % GF_NONZERO;
                        t[i + 1] = lambda[i + 1] ^ ALPHA[exponent as usize];
                    } else {
                        t[i + 1] = lambda[i + 1];
                    }
                }
                if 2 * el < r {
                    el = r - el;
                    for i in 0..PARITY {
                        b[i] = if lambda[i] == 0 {
                            LOG_ZERO
                        } else {
                            let v: i32 = i32::from(IDX[lambda[i] as usize])
                                - i32::from(discr_r_log)
                                + GF_NONZERO as i32;
                            (v % GF_NONZERO as i32) as u8
                        };
                    }
                } else {
                    b.copy_within(0..PARITY, 1);
                    b[0] = LOG_ZERO;
                }
                lambda.copy_from_slice(&t);
            }
        }

        // Determine deg(λ) and convert lambda to log-domain.
        let mut deg_lambda: usize = 0;
        for i in 0..LAMBDA_LEN {
            lambda[i] = IDX[lambda[i] as usize];
            if lambda[i] != LOG_ZERO {
                deg_lambda = i;
            }
        }

        // Bound check: a non-positive degree means BM didn't
        // find a valid locator (degenerate trellis), and a degree
        // above T means more errors than we can correct (Chien
        // would otherwise push past `roots[T]` and panic). Both
        // fail closed.
        if deg_lambda == 0 || deg_lambda > T {
            return Err(RsError::Uncorrectable);
        }

        // Chien search — try every i in 1..=GF_NONZERO as a
        // candidate root, walking the location index by
        // LOCATION_STEP per iteration (dual-basis indexing).
        let mut reg = [0_u8; LAMBDA_LEN];
        reg[1..].copy_from_slice(&lambda[1..]);
        let mut roots = [0_u8; T];
        let mut locs = [0_u8; T];
        let mut found: usize = 0;
        let mut i: u32 = 1;
        let mut k: u32 = LOCATION_INIT;
        while i <= GF_NONZERO {
            let mut q: u8 = 1;
            for j in (1..=deg_lambda).rev() {
                if reg[j] != LOG_ZERO {
                    let new_reg = (u32::from(reg[j]) + j as u32) % GF_NONZERO;
                    let new_reg_u8 = new_reg as u8;
                    reg[j] = new_reg_u8;
                    q ^= ALPHA[new_reg as usize];
                }
            }
            if q == 0 {
                // Belt-and-braces guard against the array overrun
                // that the deg_lambda > T check above also
                // prevents — if either bound check is ever
                // weakened we still fail closed instead of
                // panicking.
                if found >= T {
                    return Err(RsError::Uncorrectable);
                }
                let i_u8 = i as u8;
                let k_u8 = k as u8;
                roots[found] = i_u8;
                locs[found] = k_u8;
                found += 1;
                if found == deg_lambda {
                    break;
                }
            }
            i += 1;
            k = (k + LOCATION_STEP) % GF_NONZERO;
        }

        if deg_lambda != found {
            return Err(RsError::Uncorrectable);
        }

        // Compute Ω(x) = λ(x) · S(x) mod x^(2T) for Forney.
        // deg_omega = deg_lambda - 1 is well-defined here
        // because the bound check above guaranteed deg_lambda > 0.
        let deg_omega = deg_lambda - 1;
        let mut omega = [0_u8; LAMBDA_LEN];
        for i in 0..=deg_omega {
            let mut tmp = 0_u8;
            for j in (0..=i).rev() {
                if s_log[i - j] != LOG_ZERO && lambda[j] != LOG_ZERO {
                    let exponent = (u32::from(s_log[i - j]) + u32::from(lambda[j])) % GF_NONZERO;
                    tmp ^= ALPHA[exponent as usize];
                }
            }
            omega[i] = IDX[tmp as usize];
        }

        // Forney: error magnitudes from
        //   e_j = num1 / (num2 · den)
        // with num2 = α^(root_j · 111) (the dual-basis Forney
        // pre-factor).
        let pad: usize = 0; // standard 255-byte codeword, no shortening
        for j in (0..found).rev() {
            let mut num1: u8 = 0;
            let root_j = u32::from(roots[j]);
            for ii in (0..=deg_omega).rev() {
                if omega[ii] != LOG_ZERO {
                    let exponent = (u32::from(omega[ii]) + ii as u32 * root_j) % GF_NONZERO;
                    num1 ^= ALPHA[exponent as usize];
                }
            }
            let num2 = ALPHA[((root_j * FORNEY_NUM2_POWER + GF_NONZERO) % GF_NONZERO) as usize];
            let mut den: u8 = 0;
            let mut ii: i32 = if (deg_lambda as i32) < FORNEY_DEN_MAX_IDX {
                deg_lambda as i32
            } else {
                FORNEY_DEN_MAX_IDX
            };
            ii &= !1;
            while ii >= 0 {
                if lambda[ii as usize + 1] != LOG_ZERO {
                    let exponent =
                        (u32::from(lambda[ii as usize + 1]) + ii as u32 * root_j) % GF_NONZERO;
                    den ^= ALPHA[exponent as usize];
                }
                ii -= 2;
            }
            if num1 != 0 && (locs[j] as usize) >= pad {
                let target = locs[j] as usize - pad;
                let exponent =
                    (u32::from(IDX[num1 as usize]) + u32::from(IDX[num2 as usize]) + GF_NONZERO
                        - u32::from(IDX[den as usize]))
                        % GF_NONZERO;
                data[target] ^= ALPHA[exponent as usize];
            }
        }

        #[allow(clippy::cast_possible_truncation, reason = "found ≤ T = 16 fits in u8")]
        let n_corrected = found as u8;
        Ok((data, n_corrected))
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
mod tests {
    use super::*;

    #[test]
    fn alpha_idx_round_trip_for_nonzero_values() {
        // α^IDX[v] = v for every nonzero v ∈ GF(256). IDX[0] is
        // the log(0) sentinel (LOG_ZERO = 255), so we exclude it.
        for v in 1..=255_u8 {
            let log = IDX[v as usize];
            assert_eq!(
                ALPHA[log as usize], v,
                "α^IDX[{v}] = α^{log} = {} ≠ {v}",
                ALPHA[log as usize],
            );
        }
    }

    #[test]
    fn idx_zero_is_log_zero_sentinel() {
        assert_eq!(IDX[0], LOG_ZERO, "IDX[0] must be the log(0) sentinel = 255");
    }

    #[test]
    fn alpha_table_is_a_permutation_excluding_zero() {
        // ALPHA[0..=254] should be a permutation of 1..=255.
        // ALPHA[255] = 0 by medet convention (the "extra" entry
        // that simplifies inner-loop handling).
        let mut seen = [false; 256];
        for &v in &ALPHA[..255] {
            assert_ne!(v, 0, "no zero in α^0..α^254");
            assert!(!seen[v as usize], "duplicate ALPHA entry {v}");
            seen[v as usize] = true;
        }
        assert_eq!(ALPHA[255], 0, "ALPHA[255] must be 0 by medet convention");
    }

    #[test]
    fn poly_starts_and_ends_with_zero() {
        // Per medet's RS generator polynomial layout.
        assert_eq!(POLY[0], 0);
        assert_eq!(POLY[32], 0);
    }

    #[test]
    fn round_trip_clean_codeword() {
        let rs = ReedSolomon::new();
        let message: [u8; K] = std::array::from_fn(|i| (i * 17 + 31) as u8);
        let codeword = rs.encode(&message);
        let (decoded, n_corrected) = rs.decode(&codeword).expect("clean codeword");
        assert_eq!(n_corrected, 0);
        assert_eq!(&decoded[..K], &message);
    }

    #[test]
    fn corrects_single_byte_error() {
        let rs = ReedSolomon::new();
        let message: [u8; K] = std::array::from_fn(|i| (i * 7 + 13) as u8);
        let mut codeword = rs.encode(&message);
        codeword[42] ^= 0xA5; // flip arbitrary bits in one byte
        let (decoded, n_corrected) = rs.decode(&codeword).expect("correctable");
        assert_eq!(n_corrected, 1);
        assert_eq!(&decoded[..K], &message);
    }

    #[test]
    fn corrects_t_byte_errors() {
        let rs = ReedSolomon::new();
        let message: [u8; K] = std::array::from_fn(|i| (i * 3 + 5) as u8);
        let mut codeword = rs.encode(&message);
        // Inject T = 16 byte errors at non-overlapping positions.
        for k in 0..T {
            codeword[k * 13] ^= 0x5A;
        }
        let (decoded, n_corrected) = rs.decode(&codeword).expect("at-limit correction");
        assert_eq!(n_corrected as usize, T);
        assert_eq!(&decoded[..K], &message);
    }

    #[test]
    fn rejects_t_plus_one_errors() {
        let rs = ReedSolomon::new();
        let message: [u8; K] = std::array::from_fn(|i| (i * 23 + 11) as u8);
        let mut codeword = rs.encode(&message);
        // Inject T + 1 = 17 byte errors.
        for k in 0..=T {
            codeword[k * 11] ^= 0x3C;
        }
        let result = rs.decode(&codeword);
        assert!(matches!(result, Err(RsError::Uncorrectable)));
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Random message + random ≤T-error pattern. Decoder
            /// must recover the original message bytes exactly.
            #[test]
            fn rs_corrects_random_error_pattern_within_capacity(
            seed in any::<u64>(),
            error_count in 0_usize..=T,
            error_positions in proptest::collection::vec(0_usize..N, T),
            error_values in proptest::collection::vec(1_u8..=255, T),
        ) {
            let rs = ReedSolomon::new();
            // Deterministic message derived from seed (avoids the
            // shrinker-blow-up of generating 223 random bytes).
            let mut message = [0_u8; K];
            for (i, m) in message.iter_mut().enumerate() {
                *m = ((seed.wrapping_mul(31).wrapping_add(i as u64 * 17)) & 0xFF) as u8;
            }
            let mut codeword = rs.encode(&message);
            // Apply `error_count` distinct error positions.
            let mut applied = 0_usize;
            let mut used = std::collections::HashSet::new();
            for (&pos, &val) in error_positions.iter().zip(error_values.iter()) {
                if applied >= error_count {
                    break;
                }
                if used.insert(pos) {
                    codeword[pos] ^= val;
                    applied += 1;
                }
            }
            let (decoded, n_corrected) = rs.decode(&codeword)
                .expect("≤T errors must be correctable");
            prop_assert_eq!(usize::from(n_corrected), applied);
            prop_assert_eq!(&decoded[..K], &message);
            }
        }
    }
}
