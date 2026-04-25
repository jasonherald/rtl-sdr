//! Virtual-channel demux for Meteor LRPT.
//!
//! Routes incoming VCDUs by VCID. v1 only handles the AVHRR
//! imaging VC ([`super::VCID_AVHRR`] = 5); non-imaging VCs
//! (housekeeping, telemetry, etc.) are dropped silently.
//! Telemetry decode is deferred to follow-up
//! [#523](https://github.com/jasonherald/rtl-sdr/issues/523).
//!
//! Each VC gets its own [`MpduReassembler`] (image VCs only need
//! one today, but the per-VC abstraction matches the CCSDS spec
//! and lines up with the future telemetry-VC work).
//!
//! Reference (read-only):
//! `original/MeteorDemod/decoder/protocol/lrpt/decoder.cpp`.

use std::collections::HashMap;

use super::VCID_AVHRR;
use super::mpdu::{MpduReassembler, PKT_HEADER_LEN};
use super::vcdu::Vcdu;

/// Mask for the 24-bit VCDU frame counter.
const VCDU_COUNTER_MASK: u32 = 0x00FF_FFFF;

/// CCSDS packet primary header APID field width (bits 0-10 of
/// the first 16-bit word). Used to mask the APID out of the
/// header word.
const APID_MASK: u16 = 0x07FF;

/// CCSDS packet primary header sequence-count field width (bits
/// 0-13 of the second 16-bit word; top 2 bits are sequence
/// flags, masked off here).
const SEQUENCE_COUNT_MASK: u16 = 0x3FFF;

/// CCSDS idle APID — reserved for fill packets per CCSDS Blue
/// Book 133.0-B-1 §4.1.2.6.7. Never carries useful data.
const APID_IDLE: u16 = APID_MASK; // 0x07FF

/// APID 0 is also filtered: not formally reserved by CCSDS, but
/// it's what the `M_PDU` reassembler emits when it walks zero-
/// padded trailing bytes in a sparse data field (synthetic test
/// fixtures, idle-period VCDUs with partial packets and unused
/// remainder). Real Meteor AVHRR packets all use non-zero APIDs.
const APID_ZERO: u16 = 0;

/// Decoded CCSDS image packet (post-M_PDU-reassembly,
/// pre-JPEG-decode). Field meanings depend on the imaging
/// virtual channel; consumed by the image-assembly stage in
/// Task 5.
#[derive(Debug, Clone)]
pub struct ImagePacket {
    pub vcid: u8,
    pub apid: u16,
    pub sequence_count: u16,
    pub payload: Vec<u8>,
}

/// Per-VC demux + reassembly state.
pub struct Demux {
    reassemblers: HashMap<u8, MpduReassembler>,
    last_counter: HashMap<u8, u32>,
}

impl Default for Demux {
    fn default() -> Self {
        Self::new()
    }
}

impl Demux {
    #[must_use]
    pub fn new() -> Self {
        Self {
            reassemblers: HashMap::new(),
            last_counter: HashMap::new(),
        }
    }

    /// Push one [`super::VCDU_TOTAL_LEN`]-byte VCDU. Routes its `M_PDU`
    /// region to the matching VC reassembler; returns image
    /// packets emitted by that VC's reassembler. Non-imaging VCs
    /// drop silently.
    pub fn push(&mut self, vcdu_bytes: &[u8]) -> Vec<ImagePacket> {
        let Ok(header) = Vcdu::parse(vcdu_bytes) else {
            return Vec::new();
        };
        // Empty / placeholder VCDUs (version 0) signal idle slots
        // — drop without affecting any VC's state.
        if header.version == 0 {
            return Vec::new();
        }
        if !is_imaging_vcid(header.virtual_channel_id) {
            return Vec::new();
        }
        // Counter-jump detection: a non-sequential counter means
        // we lost at least one VCDU on this VC; drop the partial
        // packet so we don't splice random bytes into the next
        // emission.
        if let Some(&last) = self.last_counter.get(&header.virtual_channel_id) {
            let expected = (last + 1) & VCDU_COUNTER_MASK;
            if header.counter != expected
                && let Some(r) = self.reassemblers.get_mut(&header.virtual_channel_id)
            {
                r.lose_sync();
            }
        }
        self.last_counter
            .insert(header.virtual_channel_id, header.counter);
        let r = self
            .reassemblers
            .entry(header.virtual_channel_id)
            .or_default();
        let Ok(region) = Vcdu::mpdu_region(vcdu_bytes) else {
            return Vec::new();
        };
        let raw_packets = r.push(region).unwrap_or_default();
        raw_packets
            .into_iter()
            .filter_map(|raw| parse_packet(&raw, header.virtual_channel_id))
            .collect()
    }
}

/// Whether a VCID belongs to the imaging stream. v1 only
/// recognizes [`VCID_AVHRR`]; future Meteor revisions or other
/// satellites adding imaging VCs would extend this filter.
fn is_imaging_vcid(vcid: u8) -> bool {
    vcid == VCID_AVHRR
}

fn parse_packet(raw: &[u8], vcid: u8) -> Option<ImagePacket> {
    if raw.len() < PKT_HEADER_LEN {
        return None;
    }
    let header_word = u16::from_be_bytes([raw[0], raw[1]]);
    let apid = header_word & APID_MASK;
    // Drop CCSDS idle packets and zero-APID fill — neither
    // carries imagery (see APID_IDLE / APID_ZERO doc comments).
    if apid == APID_IDLE || apid == APID_ZERO {
        return None;
    }
    let seq_word = u16::from_be_bytes([raw[2], raw[3]]);
    let sequence_count = seq_word & SEQUENCE_COUNT_MASK;
    Some(ImagePacket {
        vcid,
        apid,
        sequence_count,
        payload: raw[PKT_HEADER_LEN..].to_vec(),
    })
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test fixture builders take synthetic numeric values; \
              casts are bounded by test-data design"
)]
mod tests {
    use super::super::{FHP_NO_HEADER, MPDU_HEADER_LEN, VCDU_HEADER_LEN, VCDU_TOTAL_LEN};
    use super::*;

    /// Build a synthetic VCDU buffer carrying the given payload
    /// at FHP=`fhp` for VCID=`vcid`, counter=`counter`.
    fn synthetic_vcdu(vcid: u8, counter: u32, fhp: u16, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0_u8; VCDU_TOTAL_LEN];
        // Version=01 (≠ 0 so the demux doesn't treat it as
        // empty), SCID=140 (Meteor M2-3), VCID=vcid.
        buf[0] = (1 << 6) | ((0x008C_u16 >> 2) & 0b0011_1111) as u8;
        buf[1] = (((0x008C_u16 & 0b11) as u8) << 6) | (vcid & 0x3F);
        buf[2] = ((counter >> 16) & 0xFF) as u8;
        buf[3] = ((counter >> 8) & 0xFF) as u8;
        buf[4] = (counter & 0xFF) as u8;
        // Bytes 5-7: replay flag + insert zone — all zero.
        // M_PDU region starts at VCDU_HEADER_LEN.
        let fhp_be = (fhp & 0x07FF).to_be_bytes();
        buf[VCDU_HEADER_LEN] = fhp_be[0];
        buf[VCDU_HEADER_LEN + 1] = fhp_be[1];
        let payload_offset = VCDU_HEADER_LEN + MPDU_HEADER_LEN;
        let n = payload.len().min(VCDU_TOTAL_LEN - payload_offset);
        buf[payload_offset..payload_offset + n].copy_from_slice(&payload[..n]);
        buf
    }

    fn make_packet(apid: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&(apid & 0x07FF).to_be_bytes());
        p.extend_from_slice(&[0xC0, 0x00]);
        let len_field = (payload.len() - 1) as u16;
        p.extend_from_slice(&len_field.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn routes_avhrr_vc_packet() {
        let pkt = make_packet(0x100, b"meteor-image-pixels");
        let vcdu = synthetic_vcdu(VCID_AVHRR, 0, 0, &pkt);
        let mut d = Demux::new();
        let out = d.push(&vcdu);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].vcid, VCID_AVHRR);
        assert_eq!(out[0].apid, 0x100);
        assert_eq!(out[0].payload, b"meteor-image-pixels");
    }

    #[test]
    fn drops_non_imaging_vc() {
        let pkt = make_packet(0x100, b"telemetry");
        let vcdu = synthetic_vcdu(63, 0, 0, &pkt);
        let mut d = Demux::new();
        let out = d.push(&vcdu);
        assert!(out.is_empty(), "VCID 63 (non-imaging) should be dropped");
    }

    #[test]
    fn empty_version_zero_vcdu_is_dropped() {
        // Version=0 in byte 0 (top 2 bits) signals an empty /
        // placeholder VCDU — should be ignored without affecting
        // any VC state.
        let mut vcdu = synthetic_vcdu(VCID_AVHRR, 5, 0, &[]);
        vcdu[0] = 0; // clear version bits
        let mut d = Demux::new();
        let out = d.push(&vcdu);
        assert!(out.is_empty());
    }

    #[test]
    fn counter_jump_loses_sync_for_that_vc() {
        let mut d = Demux::new();
        let pkt = make_packet(0x100, b"first");
        let v0 = synthetic_vcdu(VCID_AVHRR, 0, 0, &pkt);
        d.push(&v0);
        // Jump from counter 0 to 5. The reassembler for AVHRR
        // should lose sync; an immediate FHP_NO_HEADER VCDU then
        // emits nothing (we discard the no-header continuation
        // until we see a real header).
        let v_jump = synthetic_vcdu(VCID_AVHRR, 5, FHP_NO_HEADER, &[]);
        let out = d.push(&v_jump);
        assert!(out.is_empty());
    }

    #[test]
    fn handles_oversize_input() {
        // Defensive: a longer-than-expected slice should still
        // parse the leading bytes correctly.
        let pkt = make_packet(0x100, b"x");
        let mut vcdu = synthetic_vcdu(VCID_AVHRR, 0, 0, &pkt);
        vcdu.extend_from_slice(&[0xAA; 16]); // junk past VCDU
        let mut d = Demux::new();
        let out = d.push(&vcdu);
        assert_eq!(out.len(), 1);
    }
}
