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

/// Per-packet quantization table — one `u16` per zigzag slot.
/// Built once per image packet by [`fill_dqt`] and threaded into
/// every [`JpegDecoder::decode_mcu`] call so the hot loop doesn't
/// recompute it 14 times per packet.
pub type Dqt = [u16; MCU_SAMPLES];

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

/// Bit-window width for Huffman code lookup. The decoder peeks
/// this many bits, indexes a flat LUT keyed by the window, and
/// reads back the Huffman entry — turning a per-symbol bit-by-bit
/// walk into a single load. 16 bits is the maximum standard JPEG
/// Huffman code length, so any valid code fits inside one window.
const HUFF_LOOKAHEAD_BITS: usize = 16;

/// Number of entries in each Huffman LUT (`2 ^ HUFF_LOOKAHEAD_BITS`
/// = 65536). Each entry is an `i16` storing either the table
/// index for a matched code or `-1` for "no match".
const HUFF_LUT_SIZE: usize = 1 << HUFF_LOOKAHEAD_BITS;

/// Per-category bit-offset for the standard JPEG DC Huffman
/// table. Index = DC category (0-11); value = Huffman code
/// length in bits (code only; the variable-length value suffix
/// of `cat` bits is fetched separately by `decode_mcu`).
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
    /// `HUFF_LOOKAHEAD_BITS`-window → AC table index. -1 = no match.
    ac_lookup: Box<[i16; HUFF_LUT_SIZE]>,
    /// `HUFF_LOOKAHEAD_BITS`-window → DC category. -1 = no match.
    dc_lookup: Box<[i16; HUFF_LUT_SIZE]>,
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
        reason = "two HUFF_LUT_SIZE-entry i16 LUTs are heap-allocated via Box::new — the stack pressure is the boxed initializer, not the final storage; smaller LUTs would defeat the O(1) Huffman lookup"
    )]
    pub fn new() -> Self {
        let ac_table = build_ac_table();
        let mut ac_lookup = Box::new([-1_i16; HUFF_LUT_SIZE]);
        let mut dc_lookup = Box::new([-1_i16; HUFF_LUT_SIZE]);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "HUFF_LUT_SIZE = 2^16 = 65536 fits exactly in u32"
        )]
        let lut_size_u32 = HUFF_LUT_SIZE as u32;
        for w in 0_u32..lut_size_u32 {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "loop bound 0..HUFF_LUT_SIZE = 2^16 fits in u16"
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

    /// Decode one 8×8 MCU from the bit stream `bytes`. `dqt` is
    /// the per-packet quantization table — caller computes it
    /// once per packet via [`fill_dqt`] and passes the same
    /// reference to every MCU in the packet so the dequant
    /// step doesn't recompute it 14 times in the inner loop.
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
        dqt: &Dqt,
    ) -> Result<Block8x8, JpegError> {
        // Per CR round 8: decode through local shadow state
        // (`next_offset` for bit_offset, `next_dc` for self.last_dc)
        // and only commit the shadows back to the caller-visible
        // fields once the MCU has fully decoded. On any
        // intermediate Err the caller's `bit_offset` and the
        // decoder's `last_dc` stay at their pre-call values, so
        // the public streaming API is transactional.
        let mut next_offset = *bit_offset;
        let mut next_dc = self.last_dc;
        let mut zdct = [0_f32; MCU_SAMPLES];

        // Step 1: DC delta.
        let dc_window = peek_n_bits(bytes, next_offset, HUFF_LOOKAHEAD_BITS)?;
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
        // The LUT can match a code via zero-padded peek bits; only
        // advance if those bits actually exist. Per CR round 7.
        ensure_n_bits_available(
            bytes,
            next_offset,
            usize::from(DC_CAT_BIT_LEN[dc_cat as usize]),
        )?;
        next_offset += DC_CAT_BIT_LEN[dc_cat as usize] as usize;
        let dc_value_bits = if dc_cat > 0 {
            fetch_n_bits(bytes, &mut next_offset, dc_cat as usize)?
        } else {
            0
        };
        #[allow(
            clippy::cast_precision_loss,
            reason = "DC delta is bounded by ±2^11 (category ≤ 11), well within f32 mantissa"
        )]
        let dc_delta = map_range(dc_cat, dc_value_bits) as f32;
        next_dc += dc_delta;
        zdct[0] = next_dc;

        // Step 2: AC run-length pairs until end-of-block.
        let mut k: usize = 1;
        while k < 64 {
            let ac_window = peek_n_bits(bytes, next_offset, HUFF_LOOKAHEAD_BITS)?;
            let ac_idx_signed = self.ac_lookup[ac_window as usize];
            if ac_idx_signed < 0 {
                return Err(JpegError::BadAcCode);
            }
            #[allow(
                clippy::cast_sign_loss,
                reason = "guarded by the ac_idx_signed < 0 branch above"
            )]
            let ac = self.ac_table[ac_idx_signed as usize];
            // Same zero-padded-peek hazard as DC above: only
            // advance if the matched code's bits actually exist.
            ensure_n_bits_available(bytes, next_offset, usize::from(ac.len))?;
            next_offset += ac.len as usize;
            // EOB marker: run=0 size=0.
            if ac.run == 0 && ac.size == 0 {
                zdct[k..].fill(0.0);
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
                let n = fetch_n_bits(bytes, &mut next_offset, ac.size as usize)?;
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
        // Commit shadow state — the MCU has decoded fully without
        // any intermediate Err. Per CR round 8: leaving these
        // commits to the very end keeps `decode_mcu` transactional
        // (caller's bit_offset and decoder's last_dc stay at the
        // pre-call values on any error path above).
        *bit_offset = next_offset;
        self.last_dc = next_dc;
        Ok(block)
    }
}

/// Quality threshold separating the two scaling rules. For
/// `qf < 50` the hyperbolic `5000 / qf` rule applies; for
/// `qf >= 50` the linear `200 - 2 * qf` rule does. Per dbdexter
/// `jpeg.c::quantization` (`if (quality < 50) ... else ...`) — note
/// there is **no** lower bound: a previous revision gated the
/// hyperbolic branch on `qf > 20` as well, which mis-scaled every
/// low-quality (qf ≤ 20) packet onto the linear rule. Quality 0 is
/// guarded upstream (`fill_dqt` is only called for non-zero
/// quality), so `5000 / qf` never divides by zero.
const QUALITY_HYPERBOLIC_MAX: f32 = 50.0;
/// Numerator of the hyperbolic quality rule: `f = HYP_NUM / qf`
/// for `qf < QUALITY_HYPERBOLIC_MAX`.
const QUALITY_HYPERBOLIC_NUM: f32 = 5000.0;
/// Constant term of the linear quality rule: `f = LIN_BASE - LIN_SLOPE * qf`.
const QUALITY_LINEAR_BASE: f32 = 200.0;
/// Slope of the linear quality rule.
const QUALITY_LINEAR_SLOPE: f32 = 2.0;
/// Divisor that scales `f * QUANT_TEMPLATE[i]` back into the JPEG
/// quantization range. Lifted directly from medet's `met_jpg.pas`.
const QUALITY_TEMPLATE_DIVISOR: f32 = 100.0;
/// Floor applied per-slot — JPEG dequantization divides by
/// `dqt[i]`, so a zero would blow up the IDCT input.
const QUALITY_MIN_DQT: f32 = 1.0;

/// Per-packet quantization table — derived from the standard
/// template scaled by the packet's quality byte. Public so the
/// LRPT pipeline can compute it once per packet and pass the
/// same `Dqt` to every MCU in that packet.
///
/// # Preconditions
///
/// `q` must be non-zero: the hyperbolic branch divides
/// [`QUALITY_HYPERBOLIC_NUM`] by `q`, so `q == 0` yields non-finite
/// coefficients. The LRPT pipeline enforces this in
/// `consume_packet` (matching dbdexter's `avhrr_decode`, which skips
/// `q == 0` packets); the `debug_assert` documents the contract.
pub fn fill_dqt(q: u8) -> Dqt {
    debug_assert!(
        q != 0,
        "fill_dqt requires non-zero quality (q == 0 divides by zero)"
    );
    let qf = f32::from(q);
    let f = if qf < QUALITY_HYPERBOLIC_MAX {
        QUALITY_HYPERBOLIC_NUM / qf
    } else {
        QUALITY_LINEAR_BASE - QUALITY_LINEAR_SLOPE * qf
    };
    let mut dqt: Dqt = [0_u16; MCU_SAMPLES];
    for (i, slot) in dqt.iter_mut().enumerate() {
        let scaled = (f * f32::from(QUANT_TEMPLATE[i]) / QUALITY_TEMPLATE_DIVISOR).round();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "q != 0 (caller-guarded), so max f is 5000 at q=1; QUANT_TEMPLATE max 121 gives max scaled ≈ 6050, which fits in u16"
        )]
        let raw = scaled.max(QUALITY_MIN_DQT) as u16;
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
    debug_assert!(n <= HUFF_LOOKAHEAD_BITS);
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
    // Left-pad to HUFF_LOOKAHEAD_BITS so callers can index the
    // HUFF_LUT_SIZE-entry LUT directly off the value (matches
    // medet's bio_peek_n_bits convention).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "result < 2^n ≤ 2^HUFF_LOOKAHEAD_BITS, fits in u16 after the shift below"
    )]
    let padded = (result << (HUFF_LOOKAHEAD_BITS - n)) as u16;
    Ok(padded)
}

/// Verify that `n` bits are actually present in `bytes` starting at
/// `bit_offset`. Used to gate Huffman-code consumption AFTER the
/// LUT lookup has matched, because [`peek_n_bits`] zero-pads short
/// windows — without this guard a payload tail of `101` could be
/// accepted as the 4-bit AC EOB code (`1010`) and `bit_offset` would
/// advance past the end of the stream. Per CR round 7.
fn ensure_n_bits_available(bytes: &[u8], bit_offset: usize, n: usize) -> Result<(), JpegError> {
    let total_bits = bytes.len() * 8;
    match bit_offset.checked_add(n) {
        Some(end) if end <= total_bits => Ok(()),
        _ => Err(JpegError::EndOfStream),
    }
}

/// Fetch the next `n` bits from `bytes`, advancing `bit_offset`.
fn fetch_n_bits(bytes: &[u8], bit_offset: &mut usize, n: usize) -> Result<u16, JpegError> {
    debug_assert!(n <= HUFF_LOOKAHEAD_BITS);
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
        // then mask to the entry's bit width. Mask uses u32 so
        // e.len = HUFF_LOOKAHEAD_BITS doesn't overflow
        // `1_u16 << HUFF_LOOKAHEAD_BITS`.
        let shifted = u32::from(w) >> (HUFF_LOOKAHEAD_BITS - usize::from(e.len));
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
    reason = "the HUFF_LUT_SIZE-entry slot table is the canonical (length, position) addressing space matching medet's port; called once per JpegDecoder construction"
)]
fn build_ac_table() -> Vec<AcEntry> {
    let bits = &T_AC_0[0..16];
    let values = &T_AC_0[16..];
    // Distribute symbols into per-length slots.
    let mut v = vec![0_u8; HUFF_LUT_SIZE];
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
mod tests;
