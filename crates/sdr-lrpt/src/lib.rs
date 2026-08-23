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
mod tests;
