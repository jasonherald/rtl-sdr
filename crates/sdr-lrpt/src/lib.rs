//! Meteor-M LRPT post-demod decoder (epic #469).
//!
//! Stages 2-4 of the LRPT receive pipeline; stage 1 (QPSK demod)
//! lives in [`sdr_dsp::lrpt`].
//!
//! Layers shipped in this crate:
//!
//! - [`fec`] — Viterbi rate-1/2 + frame sync + de-randomize +
//!   Reed-Solomon (RS lands in PR 3; this PR ships the first three).
//!
//! Stage 3 (CCSDS framing, [`ccsds`]) and stage 4 (image
//! assembly, [`image`]) ship in subsequent PRs.
//!
//! Pure data crate — no DSP (those live in [`sdr_dsp::lrpt`]),
//! no GTK (UI lives in `sdr-ui`). Each layer's public surface is a
//! small struct with a `process` / `step` / `push` method matching
//! the project-wide DSP convention; internals stay private.
//!
//! Reference codebases (read-only, not linked):
//! `original/medet/`, `original/meteordemod/`, `original/SatDump/`.

#![forbid(unsafe_code)]

pub mod ccsds;
pub mod fec;
pub mod image;

use crate::ccsds::{Demux, ImagePacket};
use crate::image::{ImageAssembler, JpegDecoder, MCUS_PER_LINE, fill_dqt};

/// MCUs encoded per Meteor LRPT image packet. Per medet's
/// `mcu_per_packet`. Drives the per-packet decode loop and the
/// row-group denominator below.
const MCUS_PER_PACKET: u16 = 14;

/// Packets per Meteor LRPT scan-line group (3 imaging channels ×
/// 14 packets each + 1 onboard-time packet = 43). Used as the
/// row-index denominator: `mcu_row = (pkt - first_pkt) / 43`.
/// Per medet's `progress_image`.
const PACKETS_PER_ROW_GROUP: i32 = 43;

/// Modulus for the 14-bit CCSDS packet sequence counter.
/// `2^14 = 16384` — the counter wraps back to 0 after this value,
/// so anchor counts are walked back by this amount on detection
/// of a non-monotonic step.
const SEQUENCE_COUNT_MODULUS: i32 = 1 << 14;

/// Minimum backward delta that distinguishes a real
/// `SEQUENCE_COUNT_MODULUS` wraparound from a small reordering
/// or a corrupted sequence-count byte. Set to half the modulus:
/// any backward step ≥ 8192 must be a wrap (the next packet
/// can't legitimately be that far behind), while a smaller
/// reversal is more likely a glitch the wrap fix would massively
/// over-correct. Per CR round 7 — the prior unconditional
/// `pkt < last` walked `first_pkt` back 16384 on any reversal,
/// shifting later MCU placements hundreds of rows.
const WRAP_MIN_BACKWARD_DELTA: i32 = SEQUENCE_COUNT_MODULUS / 2;

/// Image-packet payload header length in bytes: 1 byte MCU id +
/// 2 bytes `scan_hdr` + 2 bytes `seg_hdr` + 1 byte quality.
/// JPEG-coded MCU stream begins immediately after.
const IMAGE_PACKET_HEADER_LEN: usize = 6;

/// APID of the on-board-time / housekeeping packet. Carries no
/// MCU stream; dropped on entry to `consume_packet`.
const APID_ONBOARD_TIME: u16 = 70;

/// Per-channel JPEG-decode + image-assembly state. The DC
/// predictor is per-channel because CCSDS MCU streams are
/// independent across APIDs.
struct ChannelDecoder {
    jpeg: JpegDecoder,
    /// First packet count we saw on this channel — anchors the
    /// per-MCU row index calculation. Per medet's `progress_image`.
    first_pkt: Option<i32>,
    last_pkt: Option<i32>,
}

impl ChannelDecoder {
    fn new() -> Self {
        Self {
            jpeg: JpegDecoder::new(),
            first_pkt: None,
            last_pkt: None,
        }
    }
}

/// Top-level LRPT decoder pipeline. Consumes whole VCDUs (post-RS
/// 892-byte buffers from the FEC stage) and accumulates imagery
/// into the per-channel image assembler.
///
/// Caller pulls images out via [`Self::assembler`] (live snapshot)
/// or saves PNGs at LOS via [`crate::image::save_channel`] /
/// [`crate::image::save_composite`].
pub struct LrptPipeline {
    demux: Demux,
    decoders: std::collections::HashMap<u16, ChannelDecoder>,
    assembler: ImageAssembler,
}

impl Default for LrptPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl LrptPipeline {
    #[must_use]
    pub fn new() -> Self {
        Self {
            demux: Demux::new(),
            decoders: std::collections::HashMap::new(),
            assembler: ImageAssembler::new(),
        }
    }

    /// Push one [`VCDU_TOTAL_LEN`]-byte VCDU. Drives demux →
    /// per-channel JPEG decode → image-assembler placement.
    pub fn push_vcdu(&mut self, vcdu_bytes: &[u8]) {
        for packet in self.demux.push(vcdu_bytes) {
            self.consume_packet(&packet);
        }
    }

    /// Decode one image packet: parse the per-MCU header bytes,
    /// run the JPEG decoder for each MCU in the packet, and
    /// place each decoded block in the channel buffer.
    ///
    /// Per medet's `mj_dec_mcus` layout: image packet payload
    /// starts with 6 metadata bytes (MCU id, scan headers,
    /// quality byte) followed by the JPEG-coded MCU stream.
    fn consume_packet(&mut self, packet: &ImagePacket) {
        if packet.apid == APID_ONBOARD_TIME {
            return;
        }
        if packet.payload.len() < IMAGE_PACKET_HEADER_LEN {
            return;
        }
        let mcu_id = u16::from(packet.payload[0]);
        // bytes 1-2 = scan_hdr, bytes 3-4 = seg_hdr (unused
        // here; medet only displays them in debug logs).
        let quality = packet.payload[5];
        let jpeg_bytes = &packet.payload[IMAGE_PACKET_HEADER_LEN..];

        let decoder = self
            .decoders
            .entry(packet.apid)
            .or_insert_with(ChannelDecoder::new);

        // Anchor the row-index calculation on first sight of
        // this channel (per medet `progress_image`). Different
        // APIDs start their packet counter at different offsets
        // within a row group; subtract the channel-specific
        // offset so all channels align to row 0.
        let pkt = i32::from(packet.sequence_count);
        if decoder.first_pkt.is_none() {
            let offset = match packet.apid {
                65 => i32::from(MCUS_PER_PACKET),
                66 | 68 => i32::from(2 * MCUS_PER_PACKET),
                _ => 0,
            };
            decoder.first_pkt = Some(pkt - offset);
            decoder.last_pkt = Some(pkt);
        }
        // 14-bit sequence count wraps at SEQUENCE_COUNT_MODULUS.
        // Only treat large backward steps (≥ half the modulus)
        // as a wrap — a small reversal is more likely a corrupted
        // sequence-count byte (post-RS miscorrection or demux
        // desync), and the prior unconditional wrap fix would
        // shift first_pkt back 16384 in those cases, jumping
        // future MCU placements by hundreds of rows. Per CR
        // round 7. Smaller reversals are trace-logged for
        // visibility but do not move first_pkt.
        if let (Some(last), Some(first)) = (decoder.last_pkt, decoder.first_pkt)
            && pkt < last
        {
            if last - pkt >= WRAP_MIN_BACKWARD_DELTA {
                decoder.first_pkt = Some(first - SEQUENCE_COUNT_MODULUS);
            } else {
                tracing::trace!(
                    "non-wrap sequence-count reversal on APID {apid}: last={last} pkt={pkt}",
                    apid = packet.apid,
                );
            }
        }
        decoder.last_pkt = Some(pkt);

        let Some(first) = decoder.first_pkt else {
            return;
        };
        let row_pkt = pkt - first;
        // Per CR round 8: drop pre-anchor packets entirely
        // instead of clamping them to row 0. The previous
        // `.max(0)` would silently snap a corrupted-but-not-
        // wrap reversal (caught by WRAP_MIN_BACKWARD_DELTA
        // above) onto the first image row, overwriting real
        // data. Trace-log the drop so the corruption stays
        // visible during debug runs.
        if row_pkt < 0 {
            tracing::trace!(
                "dropping pre-anchor packet on APID {apid}: first={first} pkt={pkt}",
                apid = packet.apid,
            );
            return;
        }
        #[allow(
            clippy::cast_sign_loss,
            reason = "row_pkt is non-negative after the row_pkt < 0 early return above"
        )]
        let mcu_row = (row_pkt / PACKETS_PER_ROW_GROUP) as usize;

        // Reset DC predictor per packet — Meteor packets are
        // independently coded. Compute DQT once for the packet
        // (it depends only on the per-packet quality byte) and
        // pass the same reference into every per-MCU decode so
        // the inner loop doesn't recompute it 14 times. Per CR
        // round 6.
        decoder.jpeg.reset_dc();
        let dqt = fill_dqt(quality);
        let mut bit_offset = 0_usize;
        for m in 0..MCUS_PER_PACKET {
            let Ok(block) = decoder.jpeg.decode_mcu(jpeg_bytes, &mut bit_offset, &dqt) else {
                tracing::trace!(
                    "JPEG decode failed at MCU {m} of APID {apid} packet {pkt}",
                    apid = packet.apid,
                    pkt = packet.sequence_count,
                );
                break;
            };
            #[allow(
                clippy::cast_possible_truncation,
                reason = "mcu_id + m fits in usize on every supported target"
            )]
            let mcu_col = (mcu_id + m) as usize;
            // mcu_id is a single payload byte (0-255) and m is
            // 0..14, so mcu_col can reach 268 — past the 196
            // MCUS_PER_LINE bound. Skip the place_mcu call
            // entirely on out-of-range columns: place_mcu's
            // internal guard would drop the block, but only
            // AFTER growing the channel buffer's row count
            // (composite.rs: needed_lines extension runs before
            // the column check), so a corrupt mcu_id would
            // permanently inflate channel height with blank
            // rows. Per CR round 3.
            //
            // Trace surfaces the corruption so it's visible in
            // debug logs without breaking real-time flow.
            // Causes upstream: post-RS miscorrection of the
            // packet header byte, or demux desync.
            if mcu_col >= MCUS_PER_LINE {
                tracing::trace!(
                    "out-of-range mcu_col {mcu_col} (max {max}) on APID {apid} packet {pkt}",
                    max = MCUS_PER_LINE - 1,
                    apid = packet.apid,
                    pkt = packet.sequence_count,
                );
                continue;
            }
            self.assembler
                .place_mcu(packet.apid, mcu_row, mcu_col, &block);
        }
    }

    /// Borrow the image assembler — used by the live viewer to
    /// pull updated scan lines, and at LOS to save PNGs.
    #[must_use]
    pub fn assembler(&self) -> &ImageAssembler {
        &self.assembler
    }

    /// Reset the pipeline (clear all state). Called between
    /// passes when the recorder fires `RestoreTune`.
    pub fn reset(&mut self) {
        self.demux = Demux::new();
        self.decoders.clear();
        self.assembler.clear();
    }
}

#[cfg(test)]
mod tests {
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
        let mut payload = vec![0_u8; HEADER_LEN];
        payload[0] = 0; // mcu_id starts at column 0
        payload[5] = TEST_QUALITY; // per-packet quality byte
        // Append 14 minimal MCUs back-to-back as one bit stream.
        // Each MCU is 6 bits (DC code "00" = cat 0, delta=0;
        // then AC EOB code "1010"). 14 × 6 = 84 bits = 10 full
        // bytes + 4 trailing pad bits. See MCU_PATTERN_3B for
        // the cycle derivation.
        for _ in 0..MCU_PATTERN_CYCLES {
            payload.extend_from_slice(&MCU_PATTERN_3B);
        }
        payload.extend_from_slice(&MCU_TAIL_2B);
        debug_assert_eq!(payload.len() - HEADER_LEN, SYNTHETIC_TAIL_LEN);
        ImagePacket {
            vcid: TEST_VCID,
            apid,
            sequence_count,
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
        assert_eq!(dec.first_pkt, Some(100), "no offset for apid 64");
        assert_eq!(dec.last_pkt, Some(100));
    }

    #[test]
    fn consume_packet_anchors_apid_65_with_minus_14_offset() {
        // medet's per-channel anchoring: APID 65 = -14 (one
        // packet group of MCUS_PER_PACKET).
        let mut p = LrptPipeline::new();
        let pkt = synthetic_image_packet(65, 100);
        p.consume_packet(&pkt);
        let dec = p.decoders.get(&65).expect("apid 65 channel created");
        assert_eq!(dec.first_pkt, Some(100 - i32::from(MCUS_PER_PACKET)));
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
        let dec66 = p.decoders.get(&66).expect("apid 66 channel created");
        assert_eq!(dec66.first_pkt, Some(200 - 2 * i32::from(MCUS_PER_PACKET)));

        let pkt68 = synthetic_image_packet(68, 300);
        p.consume_packet(&pkt68);
        let dec68 = p.decoders.get(&68).expect("apid 68 channel created");
        assert_eq!(dec68.first_pkt, Some(300 - 2 * i32::from(MCUS_PER_PACKET)));
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
        let first_before = p
            .decoders
            .get(&64)
            .expect("apid 64 channel created")
            .first_pkt
            .expect("first_pkt set on initial packet");
        // Push a second packet whose sequence_count has wrapped.
        let after_wrap = synthetic_image_packet(64, 2);
        p.consume_packet(&after_wrap);
        let dec = p.decoders.get(&64).expect("apid 64 still present");
        assert_eq!(
            dec.first_pkt,
            Some(first_before - SEQUENCE_COUNT_MODULUS),
            "first_pkt must walk back by one modulus on wrap"
        );
        assert_eq!(dec.last_pkt, Some(2));
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
            payload: {
                let mut payload = vec![0_u8; HEADER_LEN];
                payload[5] = TEST_QUALITY;
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
        let first_before = p
            .decoders
            .get(&64)
            .expect("apid 64 channel created")
            .first_pkt
            .expect("first_pkt set on initial packet");
        let pkt_b = synthetic_image_packet(64, 99);
        p.consume_packet(&pkt_b);
        let dec = p.decoders.get(&64).expect("apid 64 still present");
        assert_eq!(
            dec.first_pkt,
            Some(first_before),
            "1-step reversal must NOT trigger wrap correction"
        );
        assert_eq!(dec.last_pkt, Some(99));
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
        pkt.payload[0] = 200; // overwrite the mcu_id byte
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
}
