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
use crate::fec::FecChain;
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

/// Meteor AVHRR image APIDs (channels 1–6 map to 64–69). Anything else
/// on the imaging VC is telemetry or a miscorrection and must not get a
/// channel decoder (two 64 K-entry JPEG LUTs) or a viewer entry (#728).
const IMAGE_APIDS: std::ops::RangeInclusive<u16> = 64..=69;

/// Upper bound on MCU rows per pass (~20 min at one row group per
/// second). Corrupt sequence counts cannot grow a channel buffer past
/// `MAX_MCU_ROWS_PER_PASS · 8 · 1568` bytes (~15 MB) (#728).
const MAX_MCU_ROWS_PER_PASS: usize = 1_200;

/// A forward jump in a channel's sequence count larger than this is
/// treated like a wrap: provisional until the next packet lands within
/// [`CORROBORATION_WINDOW_PACKETS`] of it. Real fades lose packets, but
/// a lone miscorrected count (`last + 9000`) must not allocate 1600
/// blank rows and then make the next good packet look like a wrap.
const MAX_FORWARD_GAP_PACKETS: i32 = 8 * PACKETS_PER_ROW_GROUP;

/// How close the packet after a provisional discontinuity must be for
/// the discontinuity to be believed (two row groups).
const CORROBORATION_WINDOW_PACKETS: i32 = 2 * PACKETS_PER_ROW_GROUP;

/// Wrap count saturates here: `8 · 16384 / 43` ≈ 3048 rows, already
/// past [`MAX_MCU_ROWS_PER_PASS`], so every further packet is rejected
/// by the row bound regardless, and `wraps · SEQUENCE_COUNT_MODULUS`
/// can never overflow `i32` (CR on PR #803).
const MAX_WRAPS_PER_PASS: i32 = 8;

/// Does `pkt` corroborate `pending` — land within the window after it?
fn corroborates(pending: PendingJump, pkt: i32) -> bool {
    pkt >= pending.pkt && pkt - pending.pkt <= CORROBORATION_WINDOW_PACKETS
}

/// Row-group index for a non-negative anchor-relative packet position,
/// or `None` beyond [`MAX_MCU_ROWS_PER_PASS`].
fn bounded_mcu_row(apid: u16, row_pkt: i32) -> Option<usize> {
    let mcu_row = usize::try_from(row_pkt / PACKETS_PER_ROW_GROUP).ok()?;
    if mcu_row >= MAX_MCU_ROWS_PER_PASS {
        tracing::trace!("dropping packet beyond the row bound on APID {apid}: row {mcu_row}");
        return None;
    }
    Some(mcu_row)
}

/// Position of a channel's 14-packet segment within the 43-packet
/// row group, in packets (per medet `progress_image`): APID 64 leads,
/// 65 follows one segment later, 66 / 68 two segments later.
fn channel_group_offset(apid: u16) -> i32 {
    match apid {
        65 => i32::from(MCUS_PER_PACKET),
        66 | 68 => i32::from(2 * MCUS_PER_PACKET),
        _ => 0,
    }
}

/// CCSDS secondary-header length in bytes for Meteor image MPDUs:
/// an 8-byte on-board timestamp (`day[2] + ms[4] + us[2]`) that
/// sits between the 6-byte CCSDS primary header and the AVHRR MCU
/// segment. The demux already strips the primary header, so each
/// image packet's `payload` *starts* with this timestamp — the
/// AVHRR header (and the JPEG stream) begin
/// [`MPDU_TIME_HEADER_LEN`] bytes in.
///
/// Missing this offset is catastrophic: `payload[0]` is then the
/// timestamp's day byte (constant across a pass), so every MCU
/// lands in the same image column and the JPEG stream is read 8
/// bytes early (pure garbage). Per dbdexter `protocol/mpdu.h`
/// (`Mpdu.data = { Timestamp time; McuSegment mcu; }`).
const MPDU_TIME_HEADER_LEN: usize = 8;

/// AVHRR MCU-segment header length in bytes: 1 byte MCU id +
/// 2 bytes `scan_hdr` + 3 bytes `segment_hdr` (the last of which
/// is the quality byte). The JPEG-coded MCU stream begins
/// immediately after, i.e. at payload offset
/// [`MPDU_TIME_HEADER_LEN`] + [`IMAGE_PACKET_HEADER_LEN`].
/// Per dbdexter `protocol/mcu.h` (`AVHRR` struct).
const IMAGE_PACKET_HEADER_LEN: usize = 6;

/// APID of the on-board-time / housekeeping packet. Carries no
/// MCU stream; dropped on entry to `consume_packet`.
/// Test-only: it is outside [`IMAGE_APIDS`], which is the production gate.
#[cfg(test)]
const APID_ONBOARD_TIME: u16 = 70;

/// Per-channel JPEG-decode + image-assembly state. The DC
/// predictor is per-channel because CCSDS MCU streams are
/// independent across APIDs.
struct ChannelDecoder {
    jpeg: JpegDecoder,
    /// Last raw 14-bit sequence count seen on this channel — for
    /// wrap detection.
    last_pkt: Option<i32>,
    /// Number of sequence-count wraps observed on this channel; the
    /// unwrapped count is `pkt + wraps · SEQUENCE_COUNT_MODULUS` and
    /// is what the row index is computed from, against the
    /// pipeline-wide anchor (#726).
    wraps: i32,
    /// A sequence-count discontinuity (wrap or large forward jump)
    /// seen on the previous packet, awaiting corroboration (#728).
    pending: Option<PendingJump>,
}

impl ChannelDecoder {
    /// Sequence-count discontinuity handling against the channel's
    /// last placed packet. Returns `false` when `pkt` must be dropped:
    /// a wrap or a large forward jump is provisional (`pending`) until
    /// the next packet corroborates it. A small reversal is a
    /// corrupted count byte (post-RS miscorrection or demux desync)
    /// and is placed without touching the wrap count.
    fn accept_sequence(&mut self, apid: u16, pkt: i32) -> bool {
        let Some(last) = self.last_pkt else {
            return true;
        };
        // 14-bit sequence count wraps at SEQUENCE_COUNT_MODULUS; only a
        // backward step of at least half the modulus can be a wrap.
        let candidate_wrap = pkt < last && last - pkt >= WRAP_MIN_BACKWARD_DELTA;
        let forward_jump = pkt > last && pkt - last > MAX_FORWARD_GAP_PACKETS;
        match self.pending.take() {
            Some(pending) if corroborates(pending, pkt) => {
                if pending.wrap {
                    self.wraps = (self.wraps + 1).min(MAX_WRAPS_PER_PASS);
                }
            }
            _ if candidate_wrap || forward_jump => {
                self.pending = Some(PendingJump {
                    pkt,
                    wrap: candidate_wrap,
                });
                tracing::trace!(
                    "provisional sequence-count discontinuity on APID {apid}: last={last} pkt={pkt}"
                );
                return false;
            }
            _ if pkt < last => {
                tracing::trace!(
                    "non-wrap sequence-count reversal on APID {apid}: last={last} pkt={pkt}"
                );
            }
            _ => {}
        }
        true
    }
}

/// A discontinuity that is not believed until the next packet on the
/// channel lands within [`CORROBORATION_WINDOW_PACKETS`] of it.
#[derive(Clone, Copy)]
struct PendingJump {
    pkt: i32,
    wrap: bool,
}

impl ChannelDecoder {
    fn new() -> Self {
        Self {
            jpeg: JpegDecoder::new(),
            last_pkt: None,
            wraps: 0,
            pending: None,
        }
    }
}

/// Top-level LRPT decoder pipeline. Consumes whole VCDUs (post-RS
/// 892-byte buffers from the FEC stage) and accumulates imagery
/// into the per-channel image assembler.
///
/// Callers pull imagery out via [`Self::assembler`] (live snapshot):
/// the app's `LrptDecoder` harvests scan lines into the shared
/// viewer image and the UI writes its own PNGs at LOS, while the
/// replay CLI saves per-channel / composite PNGs through
/// [`crate::image::save_channel`] / [`crate::image::save_composite`].
pub struct LrptPipeline {
    fec: FecChain,
    demux: Demux,
    decoders: std::collections::HashMap<u16, ChannelDecoder>,
    assembler: ImageAssembler,
    /// Row-0 anchor shared by every channel: the unwrapped sequence
    /// count of the first segment-start packet (`mcu_id == 0`) seen,
    /// minus that channel's in-group offset. medet (`met_jpg.pas`)
    /// and `MeteorDemod` keep one such anchor and refuse to set it from
    /// a mid-segment packet; anchoring per channel on whatever packet
    /// came first sheared rows and misregistered the RGB composite
    /// (#726).
    anchor: Option<i32>,
}

impl Default for LrptPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl LrptPipeline {
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_differential(false)
    }

    /// Build a pipeline, choosing whether the downlink is
    /// differentially precoded. Legacy Meteor-M2 (NORAD 40069) and
    /// any `.s` captured from it use `true`; the current
    /// Meteor-M2-3 / M2-4 birds use `false`.
    #[must_use]
    pub fn new_with_differential(differential: bool) -> Self {
        Self {
            fec: FecChain::new_with_differential(differential),
            demux: Demux::new(),
            decoders: std::collections::HashMap::new(),
            assembler: ImageAssembler::new(),
            anchor: None,
        }
    }

    /// Snapshot the FEC chain's decode statistics (rotation locks,
    /// CADUs decoded / failed). For diagnostics + status reporting.
    #[must_use]
    pub fn fec_stats(&self) -> crate::fec::FecStats {
        self.fec.stats()
    }

    /// Whether the FEC chain runs the differential pre-decoder
    /// (see [`Self::new_with_differential`]).
    #[must_use]
    pub fn is_differential(&self) -> bool {
        self.fec.is_differential()
    }

    /// Push one soft-symbol pair from the QPSK demod through the
    /// full FEC chain. When the chain emits a complete VCDU
    /// (Viterbi → ASM sync → derand → RS-decode), it's
    /// immediately routed through demux + image assembly.
    /// Caller-friendly: one call per demod output, no buffering
    /// required at the call site.
    pub fn push_symbol(&mut self, soft: [i8; 2]) {
        if let Some(vcdu) = self.fec.push_symbol(soft) {
            self.push_vcdu(&vcdu);
        }
    }

    /// Push one [`VCDU_TOTAL_LEN`]-byte VCDU. Drives demux →
    /// per-channel JPEG decode → image-assembler placement.
    /// Public so the [`crate::fec::FecChain`] CLI replay path
    /// (and tests with synthetic VCDUs) can skip the FEC stage
    /// entirely.
    pub fn push_vcdu(&mut self, vcdu_bytes: &[u8]) {
        for packet in self.demux.push(vcdu_bytes) {
            self.consume_packet(&packet);
        }
    }

    /// Resolve the MCU row an image packet belongs to, or `None` when
    /// the packet precedes the shared anchor and must be dropped.
    /// Handles per-channel sequence-count wrap tracking and the
    /// segment-start anchoring rule (#726).
    fn mcu_row_for(&mut self, packet: &ImagePacket, mcu_id: u16) -> Option<usize> {
        // The caller allocated this channel's decoder just before.
        let decoder = self.decoders.get_mut(&packet.apid)?;
        let offset = channel_group_offset(packet.apid);
        let pkt = i32::from(packet.sequence_count);

        if !decoder.accept_sequence(packet.apid, pkt) {
            return None;
        }
        let first_on_channel = decoder.last_pkt.is_none();
        let unwrapped = pkt + decoder.wraps * SEQUENCE_COUNT_MODULUS;

        // Anchor only on a segment-start packet (`mcu_id == 0`), as
        // medet and `MeteorDemod` do. A mid-segment first packet would
        // put row 0 at the wrong count and shear every later row's
        // leading MCUs up by one MCU row; drop such packets until a
        // segment start arrives (#726).
        if self.anchor.is_none() {
            if mcu_id != 0 {
                tracing::trace!(
                    "dropping pre-anchor mid-segment packet on APID {apid} (mcu_id {mcu_id})",
                    apid = packet.apid,
                );
                return None;
            }
            self.anchor = Some(unwrapped - offset);
        }
        let anchor = self.anchor?;
        let mut row_pkt = unwrapped - offset - anchor;
        // A channel first seen after the counter wrapped (relative to
        // the anchor) has no wrap of its own to count yet: resolve it
        // onto the anchor's cycle. Only a wrap-sized deficit counts —
        // a packet from just before the anchor (previous row group)
        // is simply pre-anchor and is dropped below (CR on PR #802).
        if first_on_channel && row_pkt <= -WRAP_MIN_BACKWARD_DELTA {
            decoder.wraps = (decoder.wraps + 1).min(MAX_WRAPS_PER_PASS);
            row_pkt += SEQUENCE_COUNT_MODULUS;
        }
        // Pre-anchor packets are dropped, not clamped onto row 0, and
        // do not move the channel's position (#728).
        if row_pkt < 0 {
            tracing::trace!(
                "dropping pre-anchor packet on APID {apid}: anchor={anchor} pkt={pkt}",
                apid = packet.apid,
            );
            return None;
        }
        // `accept_sequence` has committed this packet's wrap / jump
        // state, so the position must follow even when the row bound
        // rejects it — a frozen `last_pkt` would turn every later
        // packet into a phantom wrap (CR on PR #803).
        decoder.last_pkt = Some(pkt);
        bounded_mcu_row(packet.apid, row_pkt)
    }

    /// Decode one image packet: parse the per-MCU header bytes,
    /// run the JPEG decoder for each MCU in the packet, and
    /// place each decoded block in the channel buffer.
    ///
    /// Image-packet payload layout (after the demux strips the
    /// 6-byte CCSDS primary header): an 8-byte MPDU timestamp
    /// secondary header ([`MPDU_TIME_HEADER_LEN`]), then the 6-byte
    /// AVHRR MCU-segment header ([`IMAGE_PACKET_HEADER_LEN`]: MCU id,
    /// scan headers, quality byte), then the JPEG-coded MCU stream.
    fn consume_packet(&mut self, packet: &ImagePacket) {
        // [`APID_ONBOARD_TIME`] (70) is outside the image range.
        if !IMAGE_APIDS.contains(&packet.apid) {
            return;
        }
        // Meteor image packets always carry the 8-byte on-board
        // timestamp secondary header, so the header flag is a validity
        // gate: a clear flag is a miscorrected header bit, and reading
        // the AVHRR header from offset 0 would place 14 garbage MCUs
        // at the timestamp's day byte (#729, CR on PR #803).
        if !packet.has_secondary_header {
            tracing::trace!(
                "dropping image packet without a secondary header on APID {apid}",
                apid = packet.apid,
            );
            return;
        }
        // Skip the timestamp to reach the AVHRR MCU segment. Without
        // this, `payload[0]` is the timestamp's (near-constant) day
        // byte, pinning every MCU to one column and shifting the JPEG
        // stream 8 bytes early into garbage.
        if packet.payload.len() < MPDU_TIME_HEADER_LEN + IMAGE_PACKET_HEADER_LEN {
            return;
        }
        let avhrr = &packet.payload[MPDU_TIME_HEADER_LEN..];
        let mcu_id = u16::from(avhrr[0]);
        // bytes 1-2 = scan_hdr, bytes 3-5 = segment_hdr (byte 5 of
        // the AVHRR header = segment_hdr[2] = the quality factor).
        let quality = avhrr[5];
        // A zero quality factor would divide by zero in the
        // hyperbolic dequant rule; dbdexter's `avhrr_decode` skips
        // the packet outright in that case, so do the same.
        if quality == 0 {
            return;
        }
        let jpeg_bytes = &avhrr[IMAGE_PACKET_HEADER_LEN..];

        self.decoders
            .entry(packet.apid)
            .or_insert_with(ChannelDecoder::new);

        let Some(mcu_row) = self.mcu_row_for(packet, mcu_id) else {
            return;
        };
        self.place_packet_mcus(packet, mcu_row, mcu_id, quality, jpeg_bytes);
    }

    /// JPEG-decode the packet's 14 MCUs and place each in the channel
    /// buffer at `mcu_row`, columns `mcu_id..mcu_id + 14`.
    fn place_packet_mcus(
        &mut self,
        packet: &ImagePacket,
        mcu_row: usize,
        mcu_id: u16,
        quality: u8,
        jpeg_bytes: &[u8],
    ) {
        let Some(decoder) = self.decoders.get_mut(&packet.apid) else {
            return;
        };

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

    /// Mutable access to the image assembler — for harvest tests
    /// and tools that place blocks directly.
    pub fn assembler_mut(&mut self) -> &mut ImageAssembler {
        &mut self.assembler
    }

    /// Reset the pipeline (clear all state). Called between
    /// passes when the recorder fires `RestoreTune`.
    pub fn reset(&mut self) {
        self.anchor = None;
        self.fec.reset();
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
}
