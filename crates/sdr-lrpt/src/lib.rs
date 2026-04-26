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
use crate::image::{ImageAssembler, JpegDecoder};

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
        // If the new pkt is less than the previous one (allowing
        // for jitter), assume a wrap.
        if let (Some(last), Some(first)) = (decoder.last_pkt, decoder.first_pkt)
            && pkt < last
        {
            decoder.first_pkt = Some(first - SEQUENCE_COUNT_MODULUS);
        }
        decoder.last_pkt = Some(pkt);

        let Some(first) = decoder.first_pkt else {
            return;
        };
        let row_pkt = (pkt - first).max(0);
        #[allow(
            clippy::cast_sign_loss,
            reason = "row_pkt is non-negative after the .max(0)"
        )]
        let mcu_row = (row_pkt / PACKETS_PER_ROW_GROUP) as usize;

        // Reset DC predictor per packet — Meteor packets are
        // independently coded.
        decoder.jpeg.reset_dc();
        let mut bit_offset = 0_usize;
        for m in 0..MCUS_PER_PACKET {
            let Ok(block) = decoder
                .jpeg
                .decode_mcu(jpeg_bytes, &mut bit_offset, quality)
            else {
                tracing::trace!(
                    "JPEG decode failed at MCU {m} of APID {apid} packet {pkt}",
                    apid = packet.apid,
                    pkt = packet.sequence_count,
                );
                break;
            };
            #[allow(
                clippy::cast_possible_truncation,
                reason = "mcu_id + m is bounded by mcu_per_line = 196"
            )]
            let mcu_col = (mcu_id + m) as usize;
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
}
