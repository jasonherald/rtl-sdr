//! Meteor reduced-JPEG decoder.
//!
//! Decodes the JPEG-compressed scan-line groups carried in
//! Meteor LRPT image packets. Meteor uses a *reduced* JPEG: the
//! standard JPEG AC Huffman table, the standard zigzag pattern,
//! and the standard quantization template — but no per-frame
//! tables on the wire. The receiver must hardcode all three.
//!
//! Output: one [`Block8x8`] per call to [`JpegDecoder::decode_mcu`],
//! representing one 8×8 MCU (minimum coded unit) of the AVHRR
//! scan-line group.
//!
//! Pipeline math (per medet's `met_jpg.pas`):
//! 1. Huffman-decode DC delta, reconstruct DC coefficient
//! 2. Huffman-decode AC run-length pairs until end-of-block
//! 3. Zigzag-unscramble the 64 coefficients
//! 4. Dequantize: `coeffs[i] *= dqt[i]` (dqt scaled per packet
//!    quality byte)
//! 5. Inverse DCT (8×8 naive O(N²) — adequate at Meteor's
//!    ~400 IDCTs/second budget)
//! 6. Level-shift +128 + clamp to 0-255
//!
//! References (read-only):
//! - `original/medet/met_jpg.pas`
//! - `original/medet/dct.pas`
//! - `original/medet/huffman.pas`
//! - `original/MeteorDemod/decoder/protocol/lrpt/msumr/image.cpp`

use std::f32::consts::{FRAC_1_SQRT_2, PI};

/// Side length of one MCU in pixels.
pub const MCU_SIDE: usize = 8;

/// Total samples per MCU.
pub const MCU_SAMPLES: usize = MCU_SIDE * MCU_SIDE; // 64

/// Pixel values in a decoded 8×8 MCU.
pub type Block8x8 = [[u8; MCU_SIDE]; MCU_SIDE];

/// Standard JPEG luminance quantization template (see JPEG ISO/
/// IEC 10918-1 Annex K). Meteor scales this per-packet by a
/// quality byte; `fill_dqt` does the scaling.
const QUANT_TEMPLATE: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Standard JPEG zigzag pattern (Annex F.1.1.5). Maps a 64-entry
/// run-length-decoded coefficient array back to its 8×8 spatial
/// position.
const ZIGZAG: [u8; 64] = [
    0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42, 3, 8, 12, 17, 25, 30, 41, 43, 9, 11,
    18, 24, 31, 40, 44, 53, 10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51, 55, 60, 21, 34,
    37, 47, 50, 56, 59, 61, 35, 36, 48, 49, 57, 58, 62, 63,
];

/// Per-category bit-offset for the standard JPEG DC Huffman
/// table. Index = DC category (0-11); value = total Huffman code
/// length in bits (length includes the variable-length suffix
/// for category > 0).
const DC_CAT_BIT_LEN: [u8; 12] = [2, 3, 3, 3, 3, 3, 4, 5, 6, 7, 8, 9];

/// Standard JPEG AC Huffman table preamble + symbols. First 16
/// bytes = "BITS" array (number of codes of each length 1..=16);
/// remaining bytes = symbol table. Verbatim from JPEG spec /
/// medet's `t_ac_0`.
const T_AC_0: [u8; 16 + 162] = [
    0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125, 1, 2, 3, 0, 4, 17, 5, 18, 33, 49, 65, 6, 19,
    81, 97, 7, 34, 113, 20, 50, 129, 145, 161, 8, 35, 66, 177, 193, 21, 82, 209, 240, 36, 51, 98,
    114, 130, 9, 10, 22, 23, 24, 25, 26, 37, 38, 39, 40, 41, 42, 52, 53, 54, 55, 56, 57, 58, 67,
    68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105,
    106, 115, 116, 117, 118, 119, 120, 121, 122, 131, 132, 133, 134, 135, 136, 137, 138, 146, 147,
    148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178, 179, 180,
    181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211, 212, 213,
    214, 215, 216, 217, 218, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 241, 242, 243, 244,
    245, 246, 247, 248, 249, 250,
];

/// Decoded AC table entry (post-table-build).
#[derive(Debug, Clone, Copy, Default)]
struct AcEntry {
    /// Run length of zero coefficients before this AC value.
    run: u8,
    /// Bit-size of the AC value's variable-length suffix.
    size: u8,
    /// Total Huffman-code length for this entry (bits).
    len: u8,
    /// Huffman code value, right-aligned.
    code: u16,
}

/// Decode error.
#[derive(Debug, thiserror::Error)]
pub enum JpegError {
    #[error("invalid DC Huffman code")]
    BadDcCode,
    #[error("invalid AC Huffman code")]
    BadAcCode,
    #[error("ran out of bits mid-MCU")]
    EndOfStream,
}

/// Streaming Meteor JPEG decoder. Holds the precomputed AC / DC
/// lookup tables (built once at construction); per-MCU state is
/// the running DC predictor.
pub struct JpegDecoder {
    /// 16-bit-window → AC table index. -1 = no match.
    ac_lookup: Box<[i16; 65536]>,
    /// 16-bit-window → DC category. -1 = no match.
    dc_lookup: Box<[i16; 65536]>,
    ac_table: Vec<AcEntry>,
    /// Precomputed 8×8 cosine table for the IDCT inner loop.
    /// Hoisted from a global `OnceLock` to a per-decoder field
    /// (per CR round 2) so the hot path doesn't pay the
    /// `OnceLock::get_or_init` atomic load per IDCT call.
    cosine: [[f32; 8]; 8],
    /// Running DC predictor (across MCUs in the same packet).
    last_dc: f32,
}

impl Default for JpegDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegDecoder {
    #[must_use]
    #[allow(
        clippy::large_stack_arrays,
        reason = "two 65536-entry i16 LUTs are heap-allocated via Box::new — the stack pressure is the boxed initializer, not the final storage; smaller LUTs would defeat the O(1) Huffman lookup"
    )]
    pub fn new() -> Self {
        let ac_table = build_ac_table();
        let mut ac_lookup = Box::new([-1_i16; 65536]);
        let mut dc_lookup = Box::new([-1_i16; 65536]);
        for w in 0_u32..65536 {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "loop bound 0..65536 fits in u16"
            )]
            let w16 = w as u16;
            ac_lookup[w as usize] = lookup_ac(w16, &ac_table);
            dc_lookup[w as usize] = lookup_dc(w16);
        }
        Self {
            ac_lookup,
            dc_lookup,
            ac_table,
            cosine: build_cosine_table(),
            last_dc: 0.0,
        }
    }

    /// Reset the DC predictor (call at the start of a new
    /// per-channel packet group).
    pub fn reset_dc(&mut self) {
        self.last_dc = 0.0;
    }

    /// Decode one 8×8 MCU from the bit stream `bytes`. `quality`
    /// is the per-packet quantization-table scaling byte (Meteor
    /// transmits this in the image-packet payload header).
    /// Returns the decoded pixel block on success.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::BadDcCode`] / [`BadAcCode`] when the
    /// Huffman lookup misses, or [`EndOfStream`] when the bit
    /// stream runs out mid-decode.
    pub fn decode_mcu(
        &mut self,
        bytes: &[u8],
        bit_offset: &mut usize,
        quality: u8,
    ) -> Result<Block8x8, JpegError> {
        let dqt = fill_dqt(quality);
        let mut zdct = [0_f32; MCU_SAMPLES];

        // Step 1: DC delta.
        let dc_window = peek_n_bits(bytes, *bit_offset, 16)?;
        let dc_cat_signed = self.dc_lookup[dc_window as usize];
        if dc_cat_signed < 0 {
            return Err(JpegError::BadDcCode);
        }
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "guarded by the dc_cat_signed < 0 branch above; DC category ∈ [0, 11] always fits in u8"
        )]
        let dc_cat = dc_cat_signed as u8;
        *bit_offset += DC_CAT_BIT_LEN[dc_cat as usize] as usize;
        let dc_value_bits = if dc_cat > 0 {
            fetch_n_bits(bytes, bit_offset, dc_cat as usize)?
        } else {
            0
        };
        #[allow(
            clippy::cast_precision_loss,
            reason = "DC delta is bounded by ±2^11 (category ≤ 11), well within f32 mantissa"
        )]
        let dc_delta = map_range(dc_cat, dc_value_bits) as f32;
        zdct[0] = dc_delta + self.last_dc;
        self.last_dc = zdct[0];

        // Step 2: AC run-length pairs until end-of-block.
        let mut k: usize = 1;
        while k < 64 {
            let ac_window = peek_n_bits(bytes, *bit_offset, 16)?;
            let ac_idx_signed = self.ac_lookup[ac_window as usize];
            if ac_idx_signed < 0 {
                return Err(JpegError::BadAcCode);
            }
            #[allow(
                clippy::cast_sign_loss,
                reason = "guarded by the ac_idx_signed < 0 branch above"
            )]
            let ac = self.ac_table[ac_idx_signed as usize];
            *bit_offset += ac.len as usize;
            // EOB marker: run=0 size=0.
            if ac.run == 0 && ac.size == 0 {
                for slot in zdct.iter_mut().take(64).skip(k) {
                    *slot = 0.0;
                }
                break;
            }
            // Pre-validate that the AC symbol won't run past
            // coefficient 63 BEFORE writing any zeros or fetching
            // any value bits. Without this, an AC symbol whose
            // run + value would land on k > 63 used to break the
            // loop mid-symbol, leaving `bit_offset` part-way
            // through the value bits and desyncing the next MCU.
            // Per CR round 3.
            //
            // `needed` slots:
            //   size > 0          : run zeros + 1 coefficient
            //   run == 15, size 0 : ZRL writes 16 zeros total
            //   anything else with size == 0 is invalid AC code
            let needed = if ac.size > 0 {
                usize::from(ac.run) + 1
            } else if ac.run == 15 {
                16
            } else {
                return Err(JpegError::BadAcCode);
            };
            if k + needed > MCU_SAMPLES {
                return Err(JpegError::BadAcCode);
            }
            // Skip `run` zeros then place the next coefficient.
            for _ in 0..ac.run {
                zdct[k] = 0.0;
                k += 1;
            }
            if ac.size > 0 {
                let n = fetch_n_bits(bytes, bit_offset, ac.size as usize)?;
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "AC coefficient is bounded by ±2^10 (size ≤ 10 in practice), within f32 mantissa"
                )]
                let coeff = map_range(ac.size, n) as f32;
                zdct[k] = coeff;
                k += 1;
            } else {
                // ZRL — guaranteed by the `needed` branch above
                // (run == 15, size == 0). Writes one extra zero
                // on top of the 15 the run loop already wrote.
                zdct[k] = 0.0;
                k += 1;
            }
        }

        // Step 3: zigzag-unscramble + dequantize (single pass).
        let mut dct = [0_f32; MCU_SAMPLES];
        for i in 0..MCU_SAMPLES {
            dct[i] = zdct[ZIGZAG[i] as usize] * f32::from(dqt[i]);
        }

        // Step 4: inverse DCT.
        let mut img = [0_f32; MCU_SAMPLES];
        idct_8x8(&dct, &mut img, &self.cosine);

        // Step 5: level-shift + clamp + pack into 8×8 block.
        let mut block: Block8x8 = [[0_u8; MCU_SIDE]; MCU_SIDE];
        for y in 0..MCU_SIDE {
            for x in 0..MCU_SIDE {
                let v = img[y * MCU_SIDE + x] + 128.0;
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamp to [0, 255] before cast keeps the conversion lossless"
                )]
                let clamped = v.clamp(0.0, 255.0) as u8;
                block[y][x] = clamped;
            }
        }
        Ok(block)
    }
}

/// Per-packet quantization table — derived from the standard
/// template scaled by the packet's quality byte.
fn fill_dqt(q: u8) -> [u16; 64] {
    let qf = f32::from(q);
    let f = if qf > 20.0 && qf < 50.0 {
        5000.0 / qf
    } else {
        200.0 - 2.0 * qf
    };
    let mut dqt = [0_u16; 64];
    for (i, slot) in dqt.iter_mut().enumerate() {
        let scaled = (f * f32::from(QUANT_TEMPLATE[i]) / 100.0).round();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "max value is QUANT_TEMPLATE max (121) × max f (≈10) = 1210, fits in u16"
        )]
        let raw = scaled.max(1.0) as u16;
        *slot = raw;
    }
    dqt
}

/// JPEG `EXTEND` operation — convert variable-length bit pattern
/// to signed integer (Annex F.1.2.1.1).
fn map_range(cat: u8, value: u16) -> i32 {
    if cat == 0 {
        return 0;
    }
    let max_val = (1_u32 << cat) - 1;
    let sign_bit = 1_u32 << (cat - 1);
    if (u32::from(value) & sign_bit) != 0 {
        i32::from(value)
    } else {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "max_val = 2^cat - 1 with cat ≤ 11, well within i32 range"
        )]
        let signed = max_val as i32;
        i32::from(value) - signed
    }
}

/// Peek the next `n` bits from `bytes` starting at `bit_offset`,
/// MSB-first. Returns the bits right-aligned in a u16.
///
/// Zero-pads any portion of the window that runs off the end of
/// `bytes` rather than erroring — `decode_mcu` always asks for a
/// full 16-bit window into the LUT, but the actual Huffman code
/// inside that window may be much shorter (a final EOB can be a
/// 4-bit code at the very end of the payload). Erroring on a
/// partial peek would reject those valid trailing codes.
/// `EndOfStream` is reserved for [`fetch_n_bits`] — the actual
/// consume operation that asks for bits we don't have.
fn peek_n_bits(bytes: &[u8], bit_offset: usize, n: usize) -> Result<u16, JpegError> {
    debug_assert!(n <= 16);
    let total_bits = bytes.len() * 8;
    if bit_offset >= total_bits {
        return Err(JpegError::EndOfStream);
    }
    let mut result: u32 = 0;
    for i in 0..n {
        let bit_pos = bit_offset + i;
        let bit = if bit_pos < total_bits {
            let byte_idx = bit_pos / 8;
            let bit_in_byte = 7 - (bit_pos % 8);
            (bytes[byte_idx] >> bit_in_byte) & 1
        } else {
            0
        };
        result = (result << 1) | u32::from(bit);
    }
    // Left-pad to 16 bits so callers can index a 65k LUT
    // directly off the value (matches medet's bio_peek_n_bits
    // convention).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "result < 2^n ≤ 2^16, fits in u16 after the shift below"
    )]
    let padded = (result << (16 - n)) as u16;
    Ok(padded)
}

/// Fetch the next `n` bits from `bytes`, advancing `bit_offset`.
fn fetch_n_bits(bytes: &[u8], bit_offset: &mut usize, n: usize) -> Result<u16, JpegError> {
    debug_assert!(n <= 16);
    let mut result: u16 = 0;
    for _ in 0..n {
        let byte_idx = *bit_offset / 8;
        if byte_idx >= bytes.len() {
            return Err(JpegError::EndOfStream);
        }
        let bit_in_byte = 7 - (*bit_offset % 8);
        let bit = (bytes[byte_idx] >> bit_in_byte) & 1;
        result = (result << 1) | u16::from(bit);
        *bit_offset += 1;
    }
    Ok(result)
}

/// DC Huffman lookup (matches medet's `get_dc_real`).
fn lookup_dc(w: u16) -> i16 {
    // Decision tree over the high bits of `w`.
    if w >> 14 == 0 {
        return 0;
    }
    match w >> 13 {
        2 => return 1,
        3 => return 2,
        4 => return 3,
        5 => return 4,
        6 => return 5,
        _ => {}
    }
    if w >> 12 == 0x00E {
        return 6;
    }
    if w >> 11 == 0x01E {
        return 7;
    }
    if w >> 10 == 0x03E {
        return 8;
    }
    if w >> 9 == 0x07E {
        return 9;
    }
    if w >> 8 == 0x0FE {
        return 10;
    }
    if w >> 7 == 0x1FE {
        return 11;
    }
    -1
}

/// AC Huffman lookup — match `w` against the precomputed
/// `ac_table` linearly (slow real lookup; cached into a 65k LUT
/// at construction time).
fn lookup_ac(w: u16, table: &[AcEntry]) -> i16 {
    for (i, e) in table.iter().enumerate() {
        // Right-shift to align `w` with the entry's code length,
        // then mask to the entry's bit width. Mask uses u32
        // so e.len = 16 doesn't overflow `1_u16 << 16`.
        let shifted = u32::from(w) >> (16 - e.len);
        let mask = (1_u32 << e.len) - 1;
        if (shifted & mask) == u32::from(e.code) {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_possible_wrap,
                reason = "ac table size ≤ 256 fits in i16"
            )]
            let idx = i as i16;
            return idx;
        }
    }
    -1
}

/// Build the AC Huffman table from the canonical JPEG bits +
/// values arrays. Direct port of medet's `default_huffman_table`.
#[allow(
    clippy::large_stack_arrays,
    reason = "the 65536-entry slot table is the canonical (length, position) addressing space matching medet's port; called once per JpegDecoder construction"
)]
fn build_ac_table() -> Vec<AcEntry> {
    let bits = &T_AC_0[0..16];
    let values = &T_AC_0[16..];
    // Distribute symbols into per-length slots.
    let mut v = vec![0_u8; 65536];
    let mut p = 0_usize;
    for k in 1..=16 {
        for i in 0..bits[k - 1] as usize {
            v[(k << 8) + i] = values[p];
            p += 1;
        }
    }
    // Compute min/max code per length.
    let mut min_code = [0_u16; 17];
    let mut maj_code = [0_u16; 17];
    let mut code = 0_u16;
    for k in 1..=16 {
        min_code[k] = code;
        for _ in 1..=bits[k - 1] {
            code = code.wrapping_add(1);
        }
        maj_code[k] = code.saturating_sub(u16::from(code != 0));
        code = code.wrapping_mul(2);
        if bits[k - 1] == 0 {
            min_code[k] = 0xFFFF;
            maj_code[k] = 0;
        }
    }
    // Walk the (length, code) space and emit one AcEntry per
    // valid Huffman code. Iteration counter is u32 because
    // (1 << 16) doesn't fit in u16; AcEntry.code stays u16.
    let mut table = Vec::with_capacity(256);
    for k in 1..=16 {
        let min_val = u32::from(min_code[k]);
        let max_val = u32::from(maj_code[k]);
        for i in 0_u32..(1_u32 << k) {
            if i <= max_val && i >= min_val {
                let size_val = v[(k << 8) + (i - min_val) as usize];
                let run = size_val >> 4;
                let size = size_val & 0xF;
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "k ∈ [1, 16] fits in u8; i < 1<<16 fits in u16"
                )]
                let len = k as u8;
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "i < 2^16 = 65536, fits in u16"
                )]
                let code = i as u16;
                table.push(AcEntry {
                    run,
                    size,
                    len,
                    code,
                });
            }
        }
    }
    table
}

// ─── DCT ────────────────────────────────────────────────────────

/// Inverse 8×8 DCT (naive O(N⁴) — adequate at Meteor's
/// ~400 IDCTs/second budget). Direct port of medet's
/// `flt_idct_8x8`.
///
/// `cosine` is the precomputed 8×8 cosine table; the caller
/// owns it (typically a [`JpegDecoder`] field) so this hot
/// function doesn't pay a per-call atomic load.
fn idct_8x8(input: &[f32; MCU_SAMPLES], output: &mut [f32; MCU_SAMPLES], cosine: &[[f32; 8]; 8]) {
    let alpha = alpha_table();
    for y in 0..MCU_SIDE {
        for x in 0..MCU_SIDE {
            let mut s = 0_f32;
            for u in 0..MCU_SIDE {
                let cxu = alpha[u] * cosine[x][u];
                // Inner sum unrolled per medet's optimization.
                let mut inner = 0_f32;
                for v in 0..MCU_SIDE {
                    inner += input[v * MCU_SIDE + u] * alpha[v] * cosine[y][v];
                }
                s += cxu * inner;
            }
            output[y * MCU_SIDE + x] = s / 4.0;
        }
    }
}

/// Build the 8×8 cosine table used by [`idct_8x8`]. Called
/// once per [`JpegDecoder::new`]; the result is stored in the
/// decoder so the IDCT inner loop can read it without any
/// synchronization.
fn build_cosine_table() -> [[f32; 8]; 8] {
    let mut t = [[0.0_f32; 8]; 8];
    #[allow(
        clippy::cast_precision_loss,
        reason = "loop indices 0..8 fit exactly in f32"
    )]
    for (y, row) in t.iter_mut().enumerate() {
        for (x, slot) in row.iter_mut().enumerate() {
            *slot = (PI / 16.0 * (2.0 * y as f32 + 1.0) * x as f32).cos();
        }
    }
    t
}

/// Precomputed alpha vector — `alpha[0] = 1/√2`, rest = 1.
fn alpha_table() -> [f32; 8] {
    [FRAC_1_SQRT_2, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    #[test]
    fn quant_template_starts_with_documented_value() {
        assert_eq!(QUANT_TEMPLATE[0], 16);
        assert_eq!(QUANT_TEMPLATE[63], 99);
    }

    #[test]
    fn zigzag_is_a_permutation_of_0_to_63() {
        let mut seen = [false; 64];
        for &v in &ZIGZAG {
            assert!(!seen[v as usize], "duplicate ZIGZAG entry {v}");
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn fill_dqt_clamps_to_minimum_one() {
        // Quality value that produces tiny coefficients. The
        // minimum of 1 prevents divide-by-zero downstream.
        let dqt = fill_dqt(100);
        for &v in &dqt {
            assert!(v >= 1, "dqt entry {v} below minimum");
        }
    }

    #[test]
    fn map_range_decodes_jpeg_extend() {
        // JPEG Annex F.1.2.1.1 Table F.1: cat=0 → 0; cat=N
        // values in [0, 2^(N-1)-1] are negative.
        assert_eq!(map_range(0, 0), 0);
        // cat=1: 0 → -1, 1 → 1
        assert_eq!(map_range(1, 0), -1);
        assert_eq!(map_range(1, 1), 1);
        // cat=3: max_val=7. Values 0-3 negative, 4-7 positive.
        assert_eq!(map_range(3, 0), -7);
        assert_eq!(map_range(3, 4), 4);
        assert_eq!(map_range(3, 7), 7);
    }

    #[test]
    fn peek_and_fetch_round_trip() {
        // Bit stream: [0b1010_1010, 0b1100_0011]
        let bytes = [0xAA, 0xC3];
        let mut ofs = 0_usize;
        // peek then fetch for 4 bits — should match high nibble.
        let peeked = peek_n_bits(&bytes, ofs, 4).unwrap();
        // peek returns value left-aligned in u16, so 0b1010 << 12.
        assert_eq!(peeked, 0b1010 << 12);
        let fetched = fetch_n_bits(&bytes, &mut ofs, 4).unwrap();
        assert_eq!(fetched, 0b1010);
        assert_eq!(ofs, 4);
    }

    #[test]
    fn idct_zero_block_returns_zero() {
        let zeros = [0_f32; MCU_SAMPLES];
        let mut out = [0_f32; MCU_SAMPLES];
        let cosine = build_cosine_table();
        idct_8x8(&zeros, &mut out, &cosine);
        for &v in &out {
            assert!(v.abs() < 1e-5, "IDCT of zeros should be zero, got {v}");
        }
    }

    #[test]
    fn idct_dc_only_block_is_uniform() {
        // A pure DC coefficient (cat=0 position) should produce a
        // uniform 8×8 block with value DC × alpha_0² / 4 = DC / 8
        // (since alpha_0 = 1/√2, alpha_0² = 1/2, then /4).
        let mut input = [0_f32; MCU_SAMPLES];
        input[0] = 800.0;
        let mut out = [0_f32; MCU_SAMPLES];
        let cosine = build_cosine_table();
        idct_8x8(&input, &mut out, &cosine);
        let expected = 800.0 / 8.0;
        for &v in &out {
            assert!(
                (v - expected).abs() < 1e-3,
                "DC-only IDCT not uniform: got {v}, expected {expected}",
            );
        }
    }

    #[test]
    fn ac_table_has_expected_canonical_jpeg_entries() {
        // The table is built by walking (length, code) in
        // increasing order. JPEG Annex K Table K.5 ordering:
        //   - length 2: 2 codes (symbols 1, 2 → run/size pairs
        //     (0,1) and (0,2))
        //   - length 3: 1 code (symbol 3 → (0,3))
        //   - length 4: 3 codes (symbols 0, 4, 17 → EOB (0,0),
        //     (0,4), (1,1))
        // We pin the first entry + the EOB position so a future
        // table-build refactor can't silently scramble the order.
        let table = build_ac_table();
        assert!(!table.is_empty(), "ac_table must be non-empty");
        let first = &table[0];
        assert_eq!((first.run, first.size, first.len), (0, 1, 2));
        // EOB lives at index 3 (after 2 length-2 + 1 length-3
        // entries). Symbol value 0 → (run=0, size=0).
        let eob = &table[3];
        assert_eq!((eob.run, eob.size, eob.len), (0, 0, 4));
    }

    #[test]
    fn decoder_constructible() {
        let dec = JpegDecoder::new();
        // Pin that tables are populated.
        assert!(!dec.ac_table.is_empty());
        assert_eq!(dec.last_dc, 0.0);
    }

    #[test]
    fn decoder_resets_dc() {
        let mut dec = JpegDecoder::new();
        dec.last_dc = 42.0;
        dec.reset_dc();
        assert_eq!(dec.last_dc, 0.0);
    }

    /// Quality byte that selects the upper branch of `fill_dqt`'s
    /// piecewise function (`f = 5000 / qf`, valid range 20 < q < 50).
    const QUALITY_UPPER_BRANCH: u8 = 30;
    /// Quality byte that selects the lower branch (`f = 200 - 2 * qf`).
    /// 60 sits comfortably inside `qf >= 50`.
    const QUALITY_LOWER_BRANCH: u8 = 60;
    /// Quality byte that drives `f` very small so the per-slot
    /// `max(1.0)` clamp actually fires — exercises the "minimum 1"
    /// guard that prevents divide-by-zero downstream.
    const QUALITY_MAX: u8 = 100;
    /// Expected level-shift output for an all-zero DCT block:
    /// IDCT(0) = 0, then `+128` level shift. Pin this so a future
    /// refactor that drops the level shift fails a test.
    const LEVEL_SHIFT_OFFSET: u8 = 128;

    #[test]
    fn peek_n_bits_zero_pads_partial_window_at_end_of_stream() {
        // CR round 1: peek into a 16-bit LUT must succeed even
        // when fewer than 16 bits remain. Construct a 1-byte
        // payload (8 bits available) and ask for 16 bits at
        // offset 0 — the high 8 bits should be the byte's
        // contents and the low 8 bits should be zero-padded.
        let bytes = [0xA5_u8]; // 1010 0101
        let peeked = peek_n_bits(&bytes, 0, 16).expect("partial peek must succeed");
        assert_eq!(
            peeked, 0xA500,
            "high 8 bits = byte, low 8 bits zero-padded; got {peeked:#06x}"
        );
    }

    #[test]
    fn peek_n_bits_returns_eof_when_offset_past_end() {
        // Reserved-EOF case: when bit_offset itself is past the
        // available bits, peek must return EndOfStream so the
        // decoder can break the AC loop instead of looping
        // forever on a zero-padded code.
        let bytes = [0xA5_u8];
        // 8 bits available, ask for 16 starting at bit 8 — that's
        // exactly at the boundary. Peeking from bit 8 should EOF
        // because 8 >= total_bits (= 8).
        let result = peek_n_bits(&bytes, 8, 16);
        assert!(
            matches!(result, Err(JpegError::EndOfStream)),
            "got {result:?}"
        );
    }

    #[test]
    fn fetch_n_bits_advances_offset_and_returns_eof_past_end() {
        // Fetch is the actual consume operation — it MUST
        // surface EOF when the requested bits run past the
        // available payload, since the decoder relies on that
        // signal to abort mid-MCU.
        let bytes = [0xFF_u8];
        let mut ofs = 4_usize;
        let four = fetch_n_bits(&bytes, &mut ofs, 4).expect("4 bits available");
        assert_eq!(four, 0b1111);
        assert_eq!(ofs, 8);
        // Now ask for one more bit — the byte is exhausted.
        let result = fetch_n_bits(&bytes, &mut ofs, 1);
        assert!(
            matches!(result, Err(JpegError::EndOfStream)),
            "got {result:?}"
        );
    }

    #[test]
    fn lookup_dc_returns_negative_for_invalid_window() {
        // The DC table covers categories 0-11, all of which
        // start with a 1-bit prefix that lookup_dc handles. The
        // only unmapped windows are those whose top 7 bits are
        // 0b1111111 followed by a non-canonical continuation —
        // those should return -1 so decode_mcu can surface
        // BadDcCode.
        let invalid = 0xFFFE_u16; // top 7 bits all 1 + non-canonical
        assert_eq!(lookup_dc(invalid), -1);
    }

    #[test]
    fn lookup_dc_decodes_each_known_category() {
        // Walk the DC code table — code "00" + cat-specific
        // suffixes — and verify each maps to the right category.
        // (Bits beyond the code length don't matter; the helper
        // only inspects the prefix.)
        assert_eq!(lookup_dc(0b00 << 14), 0); // cat 0: code 00
        assert_eq!(lookup_dc(0b010 << 13), 1); // cat 1: code 010
        assert_eq!(lookup_dc(0b011 << 13), 2);
        assert_eq!(lookup_dc(0b100 << 13), 3);
        assert_eq!(lookup_dc(0b101 << 13), 4);
        assert_eq!(lookup_dc(0b110 << 13), 5);
        assert_eq!(lookup_dc(0b1110 << 12), 6); // cat 6: code 1110
        assert_eq!(lookup_dc(0b11110 << 11), 7);
        assert_eq!(lookup_dc(0b11_1110 << 10), 8);
        assert_eq!(lookup_dc(0b111_1110 << 9), 9);
        assert_eq!(lookup_dc(0b1111_1110 << 8), 10);
        assert_eq!(lookup_dc(0b1_1111_1110 << 7), 11);
    }

    #[test]
    fn fill_dqt_branches_on_quality_band() {
        // Coverage gate: exercise both arms of the piecewise
        // `f` formula. Different quality bands give different
        // dqt magnitudes — pin "different" rather than exact
        // values so QUANT_TEMPLATE refactors don't break this.
        let lo = fill_dqt(QUALITY_UPPER_BRANCH);
        let hi = fill_dqt(QUALITY_LOWER_BRANCH);
        assert_ne!(lo, hi, "different quality bands must produce different dqt");
        // Both must satisfy the `max(1.0)` floor.
        assert!(lo.iter().all(|&v| v >= 1));
        assert!(hi.iter().all(|&v| v >= 1));
        // Highest quality should produce dqt ≈ 0 in the formula
        // but the floor saturates everything to 1.
        let max = fill_dqt(QUALITY_MAX);
        assert!(
            max.iter().all(|&v| v == 1),
            "max-quality dqt must be all 1s"
        );
    }

    #[test]
    fn decode_mcu_minimal_stream_produces_uniform_block() {
        // End-to-end smoke test of `decode_mcu`'s success path —
        // the only path that exercises zigzag-unscramble + IDCT +
        // level-shift in one call. Largely uncovered by the
        // construction-only tests above.
        //
        // Bitstream: DC code "00" (cat 0, delta=0) then AC EOB
        // code "1010" (run=0, size=0). Total 6 bits, packed
        // MSB-first into one byte. Trailing zero bits don't
        // matter — decode_mcu hits EOB and stops.
        //   bits:  0 0 1 0 1 0 _ _
        //          ────── ─────── ──
        //           DC      EOB    pad
        //   byte:  0b0010_1000 = 0x28
        //
        // Result: zdct = [0; 64] → IDCT zeros → +128 level shift
        // → every pixel = 128.
        let bytes = [0x28_u8];
        let mut decoder = JpegDecoder::new();
        let mut bit_offset = 0_usize;
        let block = decoder
            .decode_mcu(&bytes, &mut bit_offset, QUALITY_LOWER_BRANCH)
            .expect("minimal MCU should decode");
        assert_eq!(bit_offset, 6, "consumed exactly 6 bits");
        for (y, row) in block.iter().enumerate() {
            for (x, &p) in row.iter().enumerate() {
                assert_eq!(
                    p, LEVEL_SHIFT_OFFSET,
                    "pixel ({y}, {x}) should be {LEVEL_SHIFT_OFFSET} after level shift"
                );
            }
        }
    }

    #[test]
    fn decode_mcu_dc_predictor_carries_across_calls() {
        // The DC predictor accumulates across consecutive MCUs
        // in the same packet (decoder.last_dc), then `reset_dc`
        // zeros it between packets. Verify that the second
        // identical "delta=0" MCU stream produces the same
        // pixels as the first — the predictor stays at 0 because
        // both deltas are 0.
        let bytes = [0x28_u8]; // same minimal stream
        let mut decoder = JpegDecoder::new();
        let mut bit_offset = 0_usize;
        let block_a = decoder
            .decode_mcu(&bytes, &mut bit_offset, QUALITY_LOWER_BRANCH)
            .expect("first MCU");
        bit_offset = 0;
        let block_b = decoder
            .decode_mcu(&bytes, &mut bit_offset, QUALITY_LOWER_BRANCH)
            .expect("second MCU");
        assert_eq!(block_a, block_b, "DC=0 streams must match exactly");
        // Now write a non-zero DC and confirm reset clears the
        // predictor for the third call.
        decoder.last_dc = 42.0;
        decoder.reset_dc();
        bit_offset = 0;
        let block_c = decoder
            .decode_mcu(&bytes, &mut bit_offset, QUALITY_LOWER_BRANCH)
            .expect("post-reset MCU");
        assert_eq!(
            block_c, block_a,
            "post-reset MCU must match the from-zero baseline"
        );
    }

    #[test]
    fn decode_mcu_eos_on_empty_input() {
        // Zero-length payload: the very first peek should EOF.
        let mut decoder = JpegDecoder::new();
        let mut bit_offset = 0_usize;
        let result = decoder.decode_mcu(&[], &mut bit_offset, QUALITY_LOWER_BRANCH);
        assert!(
            matches!(result, Err(JpegError::EndOfStream)),
            "got {result:?}"
        );
    }

    #[test]
    fn decode_mcu_rejects_ac_run_past_coefficient_63() {
        // CR round 3: an AC symbol whose run + value would land
        // past coefficient 63 must be rejected as a malformed
        // code rather than silently breaking the AC loop and
        // leaving bit_offset mid-symbol.
        //
        // Trigger: DC=0 (cat 0, code "00", 2 bits) then 4 × ZRL.
        // Each ZRL writes 16 zeros — after 3 ZRLs k = 1 + 48 = 49,
        // and the 4th ZRL needs 16 more slots which would land at
        // k = 65, tripping the `k + needed > MCU_SAMPLES` guard.
        //
        // ZRL's actual code value depends on the AC table walk
        // order, so look it up rather than hardcoding the bits.
        let decoder = JpegDecoder::new();
        let zrl = decoder
            .ac_table
            .iter()
            .find(|e| e.run == 15 && e.size == 0)
            .expect("ZRL must exist in AC table");

        // Pack DC "00" (2 zero bits — already in the bit
        // accumulator) + 4 × ZRL code, MSB-first.
        let mut bits: u64 = 0;
        let mut nbits: u32 = 2;
        for _ in 0..4 {
            bits = (bits << zrl.len) | u64::from(zrl.code);
            nbits += u32::from(zrl.len);
        }
        let pad = (8 - (nbits % 8)) % 8;
        bits <<= pad;
        let total_bytes = (nbits + pad) as usize / 8;
        let mut bytes = vec![0_u8; total_bytes];
        for i in (0..total_bytes).rev() {
            bytes[i] = (bits & 0xFF) as u8;
            bits >>= 8;
        }

        let mut dec = JpegDecoder::new();
        let mut bit_offset = 0_usize;
        let result = dec.decode_mcu(&bytes, &mut bit_offset, QUALITY_LOWER_BRANCH);
        assert!(
            matches!(result, Err(JpegError::BadAcCode)),
            "4x ZRL overshoots coefficient 63; expected BadAcCode, got {result:?}"
        );
    }
}
