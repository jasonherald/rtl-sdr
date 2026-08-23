use super::*;
use crate::ccsds::VCDU_TOTAL_LEN;

#[test]
fn pipeline_constructible_and_resets() {
    let mut p = LrptPipeline::new();
    assert!(p.assembler.channels().next().is_none());
    // Push some bytes (all zeros — empty version, no
    // imagery emerges) and confirm reset clears state.
    p.push_vcdu(&vec![0_u8; VCDU_TOTAL_LEN]);
    p.reset();
    assert!(p.assembler.channels().next().is_none());
}

#[test]
fn empty_vcdu_doesnt_crash() {
    let mut p = LrptPipeline::new();
    // Wrong-length input — demux silently drops.
    p.push_vcdu(&[0_u8; 100]);
    assert!(p.assembler.channels().next().is_none());
}

// ─── consume_packet path tests ──────────────────────────────
//
// The full IQ→VCDU FEC chain isn't wired into LrptPipeline
// yet (deferred to a follow-up PR), so we exercise
// `consume_packet` directly with hand-built `ImagePacket`s.
// These tests pin the per-channel anchoring math (medet's
// `progress_image` rules), the 14-bit sequence-count
// wraparound, the APID 70 timestamp drop, and the short-
// payload guard — none of which the demux-driven
// `pipeline_constructible_and_resets` test reaches with its
// all-zeros input.

/// Quality byte that selects the lower branch of `fill_dqt`.
/// 60 sits comfortably inside `qf >= 50`.
const TEST_QUALITY: u8 = 60;
/// Per-MCU header length the `consume_packet` path expects:
/// 1 byte `mcu_id` + 2 bytes `scan_hdr` + 2 bytes `seg_hdr`
/// + 1 byte quality.
const HEADER_LEN: usize = IMAGE_PACKET_HEADER_LEN;
/// VCID for the AVHRR imaging stream — propagated through
/// the demux. `consume_packet` doesn't actually inspect it
/// (decisions are by APID), but the struct requires the
/// field.
const TEST_VCID: u8 = 5;
/// Bit pattern for one minimal-MCU encoded as a back-to-back
/// 6-bit code stream.
/// Every 4 MCUs (= 24 bits = 3 bytes) cycle through this
/// pattern; with 14 MCUs we get 3 full cycles + a 2-byte
/// tail. See [`MCU_TAIL_2B`] for the partial-cycle remainder.
const MCU_PATTERN_3B: [u8; 3] = [0x28, 0xA2, 0x8A];
/// Trailing 2 bytes after 3 full [`MCU_PATTERN_3B`] cycles —
/// encodes MCUs 13 + 14, with the last 4 bits zero-padded.
const MCU_TAIL_2B: [u8; 2] = [0x28, 0xA0];
/// Number of full [`MCU_PATTERN_3B`] cycles in the
/// 14-MCU synthetic packet payload.
const MCU_PATTERN_CYCLES: usize = 3;
/// Total post-header bytes the synthetic packet appends.
/// Pinned so a future change to the 14-MCU layout fails
/// `synthetic_image_packet`'s `debug_assert_eq!`.
const SYNTHETIC_TAIL_LEN: usize = MCU_PATTERN_CYCLES * 3 + 2;

/// Build an [`ImagePacket`] whose payload is a valid header +
/// 14 minimal-MCU bitstreams stitched together. The decoder
/// loop will succeed on every MCU and place 14 blocks into
/// the assembler.
fn synthetic_image_packet(apid: u16, sequence_count: u16) -> ImagePacket {
    // 8-byte timestamp secondary header (zeros here) precedes
    // the AVHRR header on every real image MPDU.
    let mut payload = vec![0_u8; MPDU_TIME_HEADER_LEN + HEADER_LEN];
    payload[MPDU_TIME_HEADER_LEN] = 0; // mcu_id starts at column 0
    payload[MPDU_TIME_HEADER_LEN + 5] = TEST_QUALITY; // per-packet quality byte
    // Append 14 minimal MCUs back-to-back as one bit stream.
    // Each MCU is 6 bits (DC code "00" = cat 0, delta=0;
    // then AC EOB code "1010"). 14 × 6 = 84 bits = 10 full
    // bytes + 4 trailing pad bits. See MCU_PATTERN_3B for
    // the cycle derivation.
    for _ in 0..MCU_PATTERN_CYCLES {
        payload.extend_from_slice(&MCU_PATTERN_3B);
    }
    payload.extend_from_slice(&MCU_TAIL_2B);
    debug_assert_eq!(
        payload.len() - MPDU_TIME_HEADER_LEN - HEADER_LEN,
        SYNTHETIC_TAIL_LEN
    );
    ImagePacket {
        vcid: TEST_VCID,
        apid,
        sequence_count,
        has_secondary_header: true,
        payload,
    }
}

#[test]
fn consume_packet_drops_apid_70_timestamp() {
    // APID 70 carries the on-board timestamp packet, not
    // imagery. consume_packet must drop it before any
    // channel state is touched.
    let mut p = LrptPipeline::new();
    let pkt = synthetic_image_packet(APID_ONBOARD_TIME, 0);
    p.consume_packet(&pkt);
    assert!(p.assembler.channels().next().is_none());
    assert!(
        p.decoders.is_empty(),
        "no channel state allocated for apid 70"
    );
}

#[test]
fn consume_packet_drops_short_payload() {
    // Payload too short to even hold the 6-byte header —
    // must early-return without touching any state.
    let mut p = LrptPipeline::new();
    let pkt = ImagePacket {
        vcid: TEST_VCID,
        apid: 64,
        sequence_count: 100,
        has_secondary_header: true,
        payload: vec![0_u8; HEADER_LEN - 1],
    };
    p.consume_packet(&pkt);
    assert!(p.assembler.channels().next().is_none());
}

#[test]
fn consume_packet_anchors_apid_64_at_zero_offset() {
    // APID 64 is the "first" channel; medet anchors it with
    // offset = 0, so first_pkt = sequence_count.
    let mut p = LrptPipeline::new();
    let pkt = synthetic_image_packet(64, 100);
    p.consume_packet(&pkt);
    let dec = p.decoders.get(&64).expect("apid 64 channel created");
    assert_eq!(p.anchor, Some(100), "no offset for apid 64");
    assert_eq!(dec.last_pkt, Some(100));
}

#[test]
fn consume_packet_anchors_apid_65_with_minus_14_offset() {
    // medet's per-channel anchoring: APID 65 = -14 (one
    // packet group of MCUS_PER_PACKET).
    let mut p = LrptPipeline::new();
    let pkt = synthetic_image_packet(65, 100);
    p.consume_packet(&pkt);
    assert!(p.decoders.contains_key(&65), "apid 65 channel created");
    assert_eq!(p.anchor, Some(100 - i32::from(MCUS_PER_PACKET)));
}

#[test]
fn consume_packet_anchors_apid_66_with_minus_28_offset() {
    // medet's per-channel anchoring: APID 66 / 68 = -28
    // (two MCUS_PER_PACKET groups). Pin both APIDs so a
    // future per-APID rewrite that breaks 68 doesn't slip
    // through.
    let mut p = LrptPipeline::new();
    let pkt66 = synthetic_image_packet(66, 200);
    p.consume_packet(&pkt66);
    assert!(p.decoders.contains_key(&66), "apid 66 channel created");
    assert_eq!(p.anchor, Some(200 - 2 * i32::from(MCUS_PER_PACKET)));

    // A second channel does not re-anchor (#726).
    let pkt68 = synthetic_image_packet(68, 300);
    p.consume_packet(&pkt68);
    assert!(p.decoders.contains_key(&68), "apid 68 channel created");
    assert_eq!(p.anchor, Some(200 - 2 * i32::from(MCUS_PER_PACKET)));
}

#[test]
fn consume_packet_handles_sequence_count_wraparound() {
    // The 14-bit sequence counter wraps at SEQUENCE_COUNT_MODULUS
    // (16384). When we observe a backward step ≥ half the
    // modulus, walk first_pkt back by one modulus so the
    // row-index calc keeps producing monotonically increasing
    // rows across the wrap boundary. (Smaller backward steps
    // are treated as corruption — see
    // `consume_packet_ignores_small_sequence_count_reversal`.)
    let mut p = LrptPipeline::new();
    // Establish anchor at a high sequence count near the
    // wrap boundary. SEQUENCE_COUNT_MODULUS - 4 = 16380,
    // well within u16 range.
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "SEQUENCE_COUNT_MODULUS = 16384 fits in u16; -4 stays positive"
    )]
    let near = (SEQUENCE_COUNT_MODULUS - 4) as u16;
    let near_wrap = synthetic_image_packet(64, near);
    p.consume_packet(&near_wrap);
    let anchor_before = p.anchor.expect("anchor set on initial packet");
    // Push a second packet whose sequence_count has wrapped — it is
    // provisional until the third corroborates it (#728).
    let after_wrap = synthetic_image_packet(64, 2);
    p.consume_packet(&after_wrap);
    p.consume_packet(&synthetic_image_packet(64, 3));
    let dec = p.decoders.get(&64).expect("apid 64 still present");
    assert_eq!(dec.wraps, 1, "one wrap counted on the channel");
    assert_eq!(p.anchor, Some(anchor_before), "the anchor never moves");
    assert_eq!(dec.last_pkt, Some(3));
}

#[test]
fn consume_packet_decodes_mcus_into_assembler() {
    // End-to-end smoke test: a synthetic packet with a
    // valid header + 14 minimal MCUs decodes successfully
    // and writes 14 MCUs (= one packet's worth of one row)
    // into the assembler under the packet's APID.
    let mut p = LrptPipeline::new();
    let pkt = synthetic_image_packet(64, 100);
    p.consume_packet(&pkt);
    // The assembler now has channel 64 with at least one
    // row's worth of pixels (8 lines × IMAGE_WIDTH).
    let ch = p.assembler.channel(64).expect("channel 64 populated");
    assert!(
        ch.lines >= 8,
        "at least 8 lines should be present, got {}",
        ch.lines
    );
    // Every placed MCU is a uniform 128-valued block (the
    // minimal-stream output). The first 14 MCUs occupy
    // columns 0..14 of row 0; verify a sample pixel inside
    // the first MCU.
    assert_eq!(
        ch.pixels[0], 128,
        "first MCU pixel should be level-shifted 128"
    );
}

#[test]
fn consume_packet_breaks_loop_on_jpeg_error() {
    // Header is valid but the MCU bitstream is empty —
    // first decode_mcu call returns EndOfStream, which
    // triggers the `else` branch and breaks the loop.
    // No MCUs land in the assembler.
    let mut p = LrptPipeline::new();
    let pkt = ImagePacket {
        vcid: TEST_VCID,
        apid: 64,
        sequence_count: 100,
        has_secondary_header: true,
        payload: {
            // Timestamp + AVHRR header, but no JPEG bytes after
            // it — first decode_mcu hits EndOfStream.
            let mut payload = vec![0_u8; MPDU_TIME_HEADER_LEN + HEADER_LEN];
            payload[MPDU_TIME_HEADER_LEN + 5] = TEST_QUALITY;
            payload
        },
    };
    p.consume_packet(&pkt);
    // Channel state was created (we passed the early
    // returns) but the assembler has no actual MCU pixels —
    // place_mcu was never called.
    assert!(p.decoders.contains_key(&64));
    assert!(
        p.assembler.channel(64).is_none(),
        "no pixels on JPEG decode failure"
    );
}

#[test]
fn push_vcdu_drives_demux_into_consume_packet() {
    // The exposed entry point. We don't have a synthetic
    // VCDU helper at this layer (that lives in ccsds), so
    // just confirm that pushing the all-zero VCDU (which
    // the demux silently drops because APID is 0 / IDLE)
    // doesn't allocate channel state.
    let mut p = LrptPipeline::new();
    p.push_vcdu(&vec![0_u8; VCDU_TOTAL_LEN]);
    assert!(
        p.decoders.is_empty(),
        "all-zero VCDU yields no image packets"
    );
}

#[test]
fn consume_packet_ignores_small_sequence_count_reversal() {
    // CR round 7: a small backward step in sequence_count is
    // more likely a corrupted header (post-RS miscorrection,
    // upstream demux desync) than a real wrap. The prior
    // unconditional `pkt < last` would walk first_pkt back
    // 16384 in those cases, jumping later MCU placements by
    // hundreds of rows. The fixed gate requires the backward
    // delta to be at least WRAP_MIN_BACKWARD_DELTA
    // (= SEQUENCE_COUNT_MODULUS / 2 = 8192) before treating
    // it as a wrap.
    //
    // Push pkt=100, then pkt=99 (one step back, NOT a wrap).
    // first_pkt must NOT be moved by SEQUENCE_COUNT_MODULUS.
    let mut p = LrptPipeline::new();
    let pkt_a = synthetic_image_packet(64, 100);
    p.consume_packet(&pkt_a);
    let anchor_before = p.anchor.expect("anchor set on initial packet");
    let pkt_b = synthetic_image_packet(64, 99);
    p.consume_packet(&pkt_b);
    let dec = p.decoders.get(&64).expect("apid 64 still present");
    assert_eq!(dec.wraps, 0, "1-step reversal must NOT count as a wrap");
    assert_eq!(p.anchor, Some(anchor_before));
    // 99 lands one packet before the anchor and is dropped; a
    // dropped packet does not move the channel's position (#728).
    assert_eq!(dec.last_pkt, Some(100));
}

#[test]
fn consume_packet_drops_pre_anchor_packet() {
    // CR round 8: a packet whose pkt < first_pkt (after
    // wrap handling) used to be silently snapped to row 0
    // by `.max(0)`, overwriting real first-row data. The
    // fix returns early instead.
    //
    // Anchor APID 65 (whose offset = -14 from the first
    // sequence_count). With initial pkt = 100, first_pkt
    // becomes 86. Then push pkt = 80 — small backward
    // step (≤ WRAP_MIN_BACKWARD_DELTA), so the wrap fix
    // does NOT move first_pkt; row_pkt = 80 - 86 = -6,
    // which would have clamped to 0 before the fix.
    //
    // First push lands at row 0 (writes assembler data).
    // Second push must NOT add more pixels — the drop
    // happens before `place_mcu` is called.
    let mut p = LrptPipeline::new();
    let pkt_a = synthetic_image_packet(65, 100);
    p.consume_packet(&pkt_a);
    let pixels_after_first = p.assembler.channel(65).map_or(0, |c| c.pixels.len());
    assert!(
        pixels_after_first > 0,
        "first packet must populate assembler",
    );

    let pkt_b = synthetic_image_packet(65, 80);
    p.consume_packet(&pkt_b);
    let pixels_after_second = p.assembler.channel(65).map_or(0, |c| c.pixels.len());
    assert_eq!(
        pixels_after_second, pixels_after_first,
        "pre-anchor packet must be dropped, not overwrite row 0",
    );
}

#[test]
fn consume_packet_skips_place_mcu_on_out_of_range_column() {
    // CR round 3: when mcu_id pushes mcu_col past the
    // MCUS_PER_LINE bound (e.g. corrupt packet header
    // post-RS miscorrection), the place_mcu call must be
    // skipped entirely — not just have its block silently
    // dropped after the channel buffer's row count was
    // already grown. Otherwise corrupt packets would
    // permanently inflate channel height with blank rows.
    //
    // mcu_id = 200 + the loop's m ∈ 0..14 → mcu_col ranges
    // from 200 to 213, all past MCUS_PER_LINE = 196.
    let mut p = LrptPipeline::new();
    let mut pkt = synthetic_image_packet(64, 100);
    pkt.payload[MPDU_TIME_HEADER_LEN] = 200; // overwrite the mcu_id byte
    p.consume_packet(&pkt);
    // Channel state was created (we passed the early
    // returns), and the JPEG decode loop ran successfully
    // for all 14 MCUs — but every placement was skipped
    // because the columns were out of range. The assembler
    // must NOT have inflated the channel buffer.
    assert!(p.decoders.contains_key(&64));
    assert!(
        p.assembler.channel(64).is_none(),
        "out-of-range columns must skip place_mcu entirely",
    );
}

// --- #726 (Aug 2026 deep review) ---

use crate::image::MCU_SIDE;

fn packet_with_mcu_id(apid: u16, sequence_count: u16, mcu_id: u8) -> ImagePacket {
    let mut pkt = synthetic_image_packet(apid, sequence_count);
    pkt.payload[MPDU_TIME_HEADER_LEN] = mcu_id;
    pkt
}

/// medet / `MeteorDemod` refuse to anchor unless `mcu_id == 0`: a
/// first packet from mid-segment must be dropped, not used as the
/// row-0 anchor (which sheared the leftmost `8·mcu_id` px of every
/// later row up by one MCU row).
#[test]
fn anchor_requires_a_segment_start_packet() {
    let mut p = LrptPipeline::new();
    // Third packet of APID 64's segment: mcu_id = 2 · 14 = 28.
    p.consume_packet(&packet_with_mcu_id(64, 102, 28));
    assert!(p.anchor.is_none(), "mid-segment packet must not anchor");
    assert!(
        p.assembler.channel(64).is_none(),
        "pre-anchor packet is dropped, not placed"
    );
    // The next row group's segment start anchors at its own count.
    p.consume_packet(&packet_with_mcu_id(64, 143, 0));
    assert_eq!(p.anchor, Some(143));
    let ch = p.assembler.channel(64).expect("placed");
    assert_eq!(ch.lines, MCU_SIDE, "segment start lands on row 0");
}

/// One anchor is shared across channels: APID 65 first seen in row
/// group 1 must land on row 1, not be re-anchored to row 0 (which
/// misregistered channels in the RGB composite).
#[test]
fn anchor_is_shared_across_channels() {
    let mut p = LrptPipeline::new();
    p.consume_packet(&packet_with_mcu_id(64, 100, 0));
    assert_eq!(p.anchor, Some(100));
    // Row group 1: APID 65's segment starts 14 packets after 64's.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let pkt65 = (100 + PACKETS_PER_ROW_GROUP + i32::from(MCUS_PER_PACKET)) as u16;
    p.consume_packet(&packet_with_mcu_id(65, pkt65, 0));
    let ch65 = p.assembler.channel(65).expect("placed");
    assert_eq!(
        ch65.lines,
        2 * MCU_SIDE,
        "APID 65 lands on row 1 under the shared anchor"
    );
    assert_eq!(
        p.anchor,
        Some(100),
        "anchor unchanged by the second channel"
    );
}

/// A channel that first appears after the sequence counter wrapped
/// (relative to the shared anchor) still resolves to a sane row.
#[test]
fn channel_first_seen_after_a_wrap_resolves_relative_to_the_anchor() {
    let mut p = LrptPipeline::new();
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let near = (SEQUENCE_COUNT_MODULUS - PACKETS_PER_ROW_GROUP) as u16; // row 0 anchor
    p.consume_packet(&packet_with_mcu_id(64, near, 0));
    // APID 66's segment of row group 1 starts 28 packets into the
    // group, which is past the wrap.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let pkt66 = ((i32::from(near) + PACKETS_PER_ROW_GROUP + 2 * i32::from(MCUS_PER_PACKET))
        % SEQUENCE_COUNT_MODULUS) as u16;
    p.consume_packet(&packet_with_mcu_id(66, pkt66, 0));
    let ch66 = p.assembler.channel(66).expect("placed");
    assert_eq!(ch66.lines, 2 * MCU_SIDE, "row 1 despite the wrapped count");
}

/// CR round 1 on PR #802 — a channel whose first packet sits just
/// *before* the anchor (previous row group) must be dropped at the
/// pre-anchor guard, not treated as post-wrap: the old condition
/// shifted it by a whole modulus (+381 rows) for the rest of the
/// pass and grew the channel buffer to thousands of blank lines.
#[test]
fn small_negative_first_sighting_is_dropped_not_wrapped() {
    let mut p = LrptPipeline::new();
    // APID 66 anchors at C − 28.
    p.consume_packet(&packet_with_mcu_id(66, 1_028, 0));
    assert_eq!(p.anchor, Some(1_000));
    // APID 64's first packet belongs to the previous row group
    // (row_pkt = −43).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let previous_group = (1_000 - PACKETS_PER_ROW_GROUP) as u16;
    p.consume_packet(&packet_with_mcu_id(64, previous_group, 0));
    assert!(
        p.assembler.channel(64).is_none(),
        "dropped, not placed on row 380"
    );
    assert_eq!(p.decoders[&64].wraps, 0, "no phantom wrap");
    // Its next packet, in the anchor's row group, lands on row 0.
    p.consume_packet(&packet_with_mcu_id(64, 1_000, 0));
    assert_eq!(p.assembler.channel(64).expect("placed").lines, MCU_SIDE);
}

// --- #728 (Aug 2026 deep review) ---

/// Only the AVHRR image APIDs (64–69) get a channel decoder; any
/// other APID is a miscorrection (or telemetry) and must not
/// allocate two 64 K-entry JPEG LUTs or a viewer channel.
#[test]
fn image_packet_without_secondary_header_is_rejected() {
    let mut p = LrptPipeline::new();
    let mut pkt = synthetic_image_packet(64, 10);
    pkt.has_secondary_header = false;
    p.consume_packet(&pkt);
    assert!(p.decoders.is_empty(), "unflagged packet is not decoded");
    assert!(p.anchor.is_none());
}

#[test]
fn unknown_apids_are_ignored() {
    let mut p = LrptPipeline::new();
    p.consume_packet(&synthetic_image_packet(100, 10));
    p.consume_packet(&synthetic_image_packet(1_000, 10));
    assert!(p.decoders.is_empty(), "no decoder for a non-image APID");
    assert!(p.assembler.channels().next().is_none());
}

/// A forward miscorrection (`pkt = last + 9000`) used to allocate
/// ~1600 blank lines and make the next good packet look like a
/// 14-bit wrap. A discontinuity is now provisional: the jumped
/// packet is dropped and `last_pkt` is left alone until the next
/// packet corroborates the new position.
#[test]
fn forward_miscorrection_is_dropped_and_does_not_poison_the_wrap_state() {
    let mut p = LrptPipeline::new();
    p.consume_packet(&packet_with_mcu_id(64, 100, 0));
    let lines_before = p.assembler.channel(64).map_or(0, |c| c.lines);
    p.consume_packet(&packet_with_mcu_id(64, 9_100, 0));
    assert_eq!(
        p.assembler.channel(64).map_or(0, |c| c.lines),
        lines_before,
        "no blank rows allocated for a lone jump"
    );
    // The next good packet (row group 1) is not a wrap.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let next = (100 + PACKETS_PER_ROW_GROUP) as u16;
    p.consume_packet(&packet_with_mcu_id(64, next, 0));
    assert_eq!(p.decoders[&64].wraps, 0, "no phantom wrap");
    assert_eq!(p.assembler.channel(64).expect("placed").lines, 2 * MCU_SIDE);
}

/// A genuine wrap is corroborated by the following packet and
/// then counted once; a lone backward miscorrection is not.
#[test]
fn sequence_wrap_is_counted_only_when_corroborated() {
    let mut p = LrptPipeline::new();
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let near = (SEQUENCE_COUNT_MODULUS - PACKETS_PER_ROW_GROUP) as u16;
    p.consume_packet(&packet_with_mcu_id(64, near, 0));
    // Lone backward "wrap" followed by the real continuation: not a wrap.
    p.consume_packet(&packet_with_mcu_id(64, 7, 0));
    assert_eq!(p.decoders[&64].wraps, 0, "uncorroborated");
    p.consume_packet(&packet_with_mcu_id(64, near, 14));
    assert_eq!(p.decoders[&64].wraps, 0);
    // Real wrap: two consecutive post-wrap packets.
    p.consume_packet(&packet_with_mcu_id(64, 0, 0)); // provisional
    p.consume_packet(&packet_with_mcu_id(64, 1, 14)); // corroborates
    assert_eq!(p.decoders[&64].wraps, 1);
    assert_eq!(p.assembler.channel(64).expect("placed").lines, 2 * MCU_SIDE);
}

/// Rows are bounded per pass so corrupt input cannot drive a
/// multi-GB channel allocation. The 14-bit count spans 381 rows per
/// cycle, so the bound is only reachable through corroborated wraps
/// — which is how the test drives it. Row-bound rejections still
/// commit the channel's position (`wraps`, `last_pkt`) so the
/// detector is not poisoned for the rest of the pass.
#[test]
fn mcu_rows_are_bounded_per_pass() {
    /// A count late in the 14-bit cycle, past the forward-gap limit.
    const LATE_IN_CYCLE: u16 = 16_000;
    let mut p = LrptPipeline::new();
    p.consume_packet(&packet_with_mcu_id(64, 0, 0));
    // Each cycle: a corroborated forward jump late into the cycle,
    // then a corroborated wrap. Four wraps put the channel at
    // ~1500 rows, past MAX_MCU_ROWS_PER_PASS.
    for _ in 0..4 {
        p.consume_packet(&packet_with_mcu_id(64, LATE_IN_CYCLE, 0));
        p.consume_packet(&packet_with_mcu_id(64, LATE_IN_CYCLE + 1, 14));
        p.consume_packet(&packet_with_mcu_id(64, 10, 0));
        p.consume_packet(&packet_with_mcu_id(64, 11, 14));
    }
    let dec = &p.decoders[&64];
    assert_eq!(dec.wraps, 4, "wraps are committed even past the bound");
    assert_eq!(
        dec.last_pkt,
        Some(11),
        "position follows the rejected packet"
    );
    let lines = p.assembler.channel(64).expect("exists").lines;
    assert!(lines < MAX_MCU_ROWS_PER_PASS * MCU_SIDE, "bounded: {lines}");
    // The last placed row is the third cycle's post-wrap packet 11:
    // (11 + 3·16384) / 43 = row 1143.
    assert_eq!(lines, 1144 * MCU_SIDE);
}

/// Past the row bound the wrap count saturates, so an endless
/// corrupt wrap cycle can never overflow the unwrapped position
/// and fold back onto row 0.
#[test]
fn wrap_count_saturates_past_the_row_bound() {
    const LATE_IN_CYCLE: u16 = 16_000;
    let mut p = LrptPipeline::new();
    p.consume_packet(&packet_with_mcu_id(64, 0, 0));
    for _ in 0..(MAX_WRAPS_PER_PASS + 4) {
        p.consume_packet(&packet_with_mcu_id(64, LATE_IN_CYCLE, 0));
        p.consume_packet(&packet_with_mcu_id(64, LATE_IN_CYCLE + 1, 14));
        p.consume_packet(&packet_with_mcu_id(64, 10, 0));
        p.consume_packet(&packet_with_mcu_id(64, 11, 14));
    }
    assert_eq!(p.decoders[&64].wraps, MAX_WRAPS_PER_PASS);
    let lines = p.assembler.channel(64).expect("exists").lines;
    assert!(lines < MAX_MCU_ROWS_PER_PASS * MCU_SIDE, "bounded: {lines}");
}
