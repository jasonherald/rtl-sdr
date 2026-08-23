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
    let dqt = fill_dqt(QUALITY_LOWER_BRANCH);
    let block = decoder
        .decode_mcu(&bytes, &mut bit_offset, &dqt)
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
    // The DC predictor (`decoder.last_dc`) carries across
    // consecutive MCUs of one packet and `reset_dc` zeros it
    // between packets. A zero-delta stream therefore reproduces
    // whatever the predictor currently holds: seed it with a
    // non-zero value and the decoded block must differ from the
    // from-zero baseline, stay identical on the next call (carry),
    // and return to the baseline after `reset_dc`.
    const SEEDED_DC: f32 = 42.0;
    /// Minimal encoded MCU: DC category 0 (delta 0) followed by the
    /// AC end-of-block code, so every coefficient is the predictor.
    const ZERO_DELTA_MCU: [u8; 1] = [0x28];
    let bytes = ZERO_DELTA_MCU;
    let mut decoder = JpegDecoder::new();
    let mut bit_offset = 0_usize;
    let dqt = fill_dqt(QUALITY_LOWER_BRANCH);
    let baseline = decoder
        .decode_mcu(&bytes, &mut bit_offset, &dqt)
        .expect("baseline MCU");

    decoder.last_dc = SEEDED_DC;
    bit_offset = 0;
    let seeded_a = decoder
        .decode_mcu(&bytes, &mut bit_offset, &dqt)
        .expect("seeded MCU");
    assert_ne!(
        seeded_a, baseline,
        "a non-zero predictor must shift the block"
    );
    bit_offset = 0;
    let seeded_b = decoder
        .decode_mcu(&bytes, &mut bit_offset, &dqt)
        .expect("carried MCU");
    assert_eq!(
        seeded_a, seeded_b,
        "the predictor carries across decode_mcu calls"
    );

    decoder.reset_dc();
    bit_offset = 0;
    let after_reset = decoder
        .decode_mcu(&bytes, &mut bit_offset, &dqt)
        .expect("post-reset MCU");
    assert_eq!(
        after_reset, baseline,
        "post-reset MCU must match the from-zero baseline"
    );
}

#[test]
fn decode_mcu_eos_on_empty_input() {
    // Zero-length payload: the very first peek should EOF.
    let mut decoder = JpegDecoder::new();
    let mut bit_offset = 0_usize;
    let dqt = fill_dqt(QUALITY_LOWER_BRANCH);
    let result = decoder.decode_mcu(&[], &mut bit_offset, &dqt);
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
    let dqt = fill_dqt(QUALITY_LOWER_BRANCH);
    let result = dec.decode_mcu(&bytes, &mut bit_offset, &dqt);
    assert!(
        matches!(result, Err(JpegError::BadAcCode)),
        "4x ZRL overshoots coefficient 63; expected BadAcCode, got {result:?}"
    );
}

#[test]
fn ensure_n_bits_available_validates_bounds() {
    // Direct test of the helper's contract: returns Ok when
    // `bit_offset + n` is in bounds, EndOfStream otherwise,
    // including the checked_add overflow path.
    let bytes = [0xFF_u8; 2]; // 16 bits
    assert!(ensure_n_bits_available(&bytes, 0, 16).is_ok());
    assert!(ensure_n_bits_available(&bytes, 8, 8).is_ok());
    assert!(matches!(
        ensure_n_bits_available(&bytes, 8, 9),
        Err(JpegError::EndOfStream)
    ));
    assert!(matches!(
        ensure_n_bits_available(&bytes, usize::MAX, 1),
        Err(JpegError::EndOfStream)
    ));
}

#[test]
fn decode_mcu_rejects_truncated_dc_code() {
    // CR round 7: peek_n_bits zero-pads short windows so a
    // truncated tail can spuriously match a Huffman LUT
    // entry. The AFTER-match availability check must catch
    // that and return EndOfStream rather than advance
    // bit_offset past the end of the payload.
    //
    // Trigger: empty payload with bit_offset already past
    // the start. peek returns EndOfStream directly here.
    // For the matched-code-but-not-enough-bits path, build
    // a 1-byte payload whose last 4 bits look like a valid
    // DC code prefix but where there isn't room for the
    // category's value suffix.
    //
    // DC cat 6 has code 0b1110 (4 bits) + 6 value bits
    // (10 bits total). Pack DC "1110" left-aligned in one
    // byte = 0b1110_0000 = 0xE0. After matching cat 6, the
    // pre-CR code would advance bit_offset by 4 then try to
    // fetch 6 value bits — but only 4 bits remain in the
    // payload after the code, so fetch_n_bits would EOF
    // anyway. The new pre-advance guard makes the failure
    // mode crisper: ensure_n_bits_available catches the
    // missing code bits BEFORE advancing.
    //
    // To exercise the ensure_n_bits_available branch
    // specifically, construct a payload where the LUT
    // matches via padding bits. 1-byte payload `0b1110_0000`
    // peeked at bit_offset=4 sees only 4 valid bits
    // (`0000`) followed by 12 zero pads — the all-zeros
    // window matches DC cat 0 (code "00"). With cat 0
    // requiring 2 code bits and only 4 actual bits left
    // (bit_offset=4 in an 8-bit payload), the pre-advance
    // guard accepts the 2-bit consumption. To force the
    // failure, peek at bit_offset=7: only 1 valid bit
    // remains, but the LUT will still match cat 0 (the
    // all-zero window). The pre-advance guard rejects
    // because 7 + 2 = 9 > 8.
    let bytes = [0xE0_u8]; // 8 valid bits
    let mut decoder = JpegDecoder::new();
    let mut bit_offset = 7_usize;
    let dqt = fill_dqt(QUALITY_LOWER_BRANCH);
    let result = decoder.decode_mcu(&bytes, &mut bit_offset, &dqt);
    assert!(
        matches!(result, Err(JpegError::EndOfStream)),
        "matched DC cat 0 (2 bits) at bit_offset=7 of 8-bit payload must EOF, got {result:?}"
    );
}

#[test]
fn decode_mcu_is_transactional_on_error() {
    // CR round 8: on any intermediate Err, the caller's
    // bit_offset and the decoder's last_dc must stay at
    // their pre-call values. Otherwise a streaming caller
    // is left with a poisoned predictor / half-advanced
    // offset that desyncs the next MCU.
    //
    // Trigger: bit_offset=7 in an 8-bit payload (same as
    // the truncated-DC test above) — `ensure_n_bits_available`
    // returns EndOfStream AFTER the LUT match. Confirm
    // both bit_offset and last_dc are unchanged after
    // the call returns Err.
    // Pre-set last_dc to a non-zero sentinel so an
    // accidental commit-on-error would clobber it visibly.
    const PRE_DC: f32 = 99.0;
    let bytes = [0xE0_u8];
    let mut decoder = JpegDecoder::new();
    decoder.last_dc = PRE_DC;
    let mut bit_offset = 7_usize;
    let dqt = fill_dqt(QUALITY_LOWER_BRANCH);
    let result = decoder.decode_mcu(&bytes, &mut bit_offset, &dqt);
    assert!(matches!(result, Err(JpegError::EndOfStream)));
    assert_eq!(
        bit_offset, 7,
        "caller's bit_offset must not advance on error"
    );
    assert_eq!(
        decoder.last_dc, PRE_DC,
        "decoder's last_dc must not change on error"
    );
}
