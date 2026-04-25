//! `M_PDU` (Multiplexed Protocol Data Unit) reassembler.
//!
//! Each VCDU's `M_PDU` region starts with a 16-bit word whose top
//! 5 bits are reserved and low 11 bits are the **first-header-
//! pointer (FHP)**: byte offset of the next CCSDS-packet header
//! within this VCDU's data field, OR [`super::FHP_NO_HEADER`] if
//! the entire data field is a continuation of the previous
//! packet.
//!
//! The reassembler buffers bytes across VCDU boundaries and
//! emits complete CCSDS packets (variable length per packet
//! primary header).
//!
//! Reference (read-only):
//! `original/MeteorDemod/decoder/protocol/lrpt/decoder.cpp`.

use super::{FHP_NO_HEADER, MPDU_DATA_LEN, MPDU_HEADER_LEN};

/// CCSDS packet primary header length.
pub const PKT_HEADER_LEN: usize = 6;

/// Minimum CCSDS packet length (header + 1-byte payload — the
/// length field is zero-based, so a 0 length field means a 1-byte
/// payload).
pub const PKT_MIN_LEN: usize = PKT_HEADER_LEN + 1;

/// Maximum plausible CCSDS packet length for Meteor AVHRR. The
/// CCSDS spec maxes out at 65 542 bytes (16-bit length + 7), but
/// realistic Meteor imagery packets fit comfortably under 4 KB —
/// anything bigger is RS miscorrection turning a length field
/// into garbage. Capping here is the integrity gate that prevents
/// the `M_PDU` layer from emitting bogus multi-kB "packets" the
/// downstream image stage would then have to defend against.
pub const PKT_MAX_LEN: usize = 4096;

/// Initial allocation for the cross-VCDU reassembly buffer.
/// One `M_PDU` data field is 882 bytes, so 8 KB holds ~9 fields
/// of buffered partial data before re-allocation. Plenty of
/// headroom for any realistic packet-spans-many-VCDUs case
/// without paying for re-allocs in steady state.
const INITIAL_BUFFER_CAPACITY: usize = 8192;

/// `M_PDU` first-header-pointer mask: low 11 bits of the 16-bit
/// header word (top 5 bits are reserved).
const FHP_MASK: u16 = 0x07FF;

/// Reassembles CCSDS packets from a stream of VCDU `M_PDU` regions.
pub struct MpduReassembler {
    buffer: Vec<u8>,
    /// Whether the buffer is currently aligned to a packet
    /// header. Cleared after a lost VCDU + restored at the next
    /// FHP that is not [`FHP_NO_HEADER`].
    in_sync: bool,
}

impl Default for MpduReassembler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MpduError {
    #[error("`M_PDU` region has wrong length: got {0}, expected {expected}", expected = MPDU_HEADER_LEN + MPDU_DATA_LEN)]
    BadRegionLength(usize),
}

impl MpduReassembler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(INITIAL_BUFFER_CAPACITY),
            in_sync: false,
        }
    }

    /// Push one VCDU's `M_PDU` region (header + data field, total
    /// `MPDU_HEADER_LEN + MPDU_DATA_LEN` bytes). Returns any
    /// complete CCSDS packets extracted.
    ///
    /// # Errors
    ///
    /// Returns `MpduError::BadRegionLength` if the input slice
    /// isn't exactly the expected size — caller should slice
    /// `Vcdu::mpdu_region(...)` and pass the result directly.
    pub fn push(&mut self, region: &[u8]) -> Result<Vec<Vec<u8>>, MpduError> {
        if region.len() != MPDU_HEADER_LEN + MPDU_DATA_LEN {
            return Err(MpduError::BadRegionLength(region.len()));
        }
        // FHP: low 11 bits of first 2 bytes (top 5 bits reserved).
        let fhp_word = u16::from_be_bytes([region[0], region[1]]);
        let fhp = fhp_word & FHP_MASK;
        let payload = &region[MPDU_HEADER_LEN..];
        if self.in_sync {
            self.buffer.extend_from_slice(payload);
        } else {
            // Drop the buffer + restart at FHP, unless FHP signals
            // "no header" (entire payload is an unsynced
            // continuation — discard) or FHP exceeds the data
            // field (malformed CADU got past RS — skip silently).
            if fhp == FHP_NO_HEADER || (fhp as usize) >= payload.len() {
                return Ok(Vec::new());
            }
            self.buffer.clear();
            self.buffer.extend_from_slice(&payload[fhp as usize..]);
            self.in_sync = true;
        }
        // Try to emit packets from the buffer. Walk a `consumed`
        // cursor instead of draining per packet — `Vec::drain`
        // shifts the remaining bytes each call, which becomes
        // O(n²) when many small packets are packed in one push.
        // Single drain at the end keeps the hot path linear.
        let mut packets = Vec::new();
        let mut consumed: usize = 0;
        loop {
            let avail = &self.buffer[consumed..];
            if avail.len() < PKT_HEADER_LEN {
                break;
            }
            // CCSDS packet primary header bytes 4-5: zero-based
            // length field (actual length = field + 1 + header).
            let pkt_len_field = u16::from_be_bytes([avail[4], avail[5]]);
            let total_len = PKT_HEADER_LEN + pkt_len_field as usize + 1;
            // Integrity gate FIRST — before checking whether the
            // buffer holds total_len bytes. RS decode returns Ok
            // for some over-T corruption patterns (silently
            // miscorrected codewords; see
            // sdr_lrpt::fec::reed_solomon::RsError docs), so a
            // claim of "60 KB packet incoming" usually means RS
            // turned a length-field byte into garbage. Without
            // checking here first we'd just sit and wait for
            // bytes that will never come, freezing the
            // reassembler. CCSDS 133.0-B-1 §4.1.2.6.1: packet
            // version is always 000 (top 3 bits of byte 0).
            let version = avail[0] >> 5;
            if version != 0 || !(PKT_MIN_LEN..=PKT_MAX_LEN).contains(&total_len) {
                tracing::warn!(
                    "M_PDU rejecting implausible packet (version={version}, total_len={total_len}); RS likely miscorrected, losing sync",
                );
                self.in_sync = false;
                self.buffer.clear();
                return Ok(packets);
            }
            if avail.len() < total_len {
                break;
            }
            packets.push(avail[..total_len].to_vec());
            consumed += total_len;
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        Ok(packets)
    }

    /// Mark the reassembler as having lost sync (call when a VCDU
    /// is dropped or arrives out of order). Buffer is cleared at
    /// the next push that has a valid FHP.
    pub fn lose_sync(&mut self) {
        self.in_sync = false;
        self.buffer.clear();
    }

    /// Whether the reassembler currently has a packet header
    /// alignment. Useful for callers that want to surface a
    /// "currently locked" indicator.
    #[must_use]
    pub fn is_in_sync(&self) -> bool {
        self.in_sync
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test fixture builders take synthetic numeric values; \
              casts are bounded by test-data design"
)]
mod tests {
    use super::*;

    /// Build a VCDU `M_PDU` region with FHP at byte `fhp` followed
    /// by `packets` concatenated starting at that offset.
    fn build_region(fhp: u16, packets: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = vec![0_u8; MPDU_HEADER_LEN + MPDU_DATA_LEN];
        let fhp_word = fhp & FHP_MASK;
        buf[0..2].copy_from_slice(&fhp_word.to_be_bytes());
        let mut offset = MPDU_HEADER_LEN + fhp as usize;
        for p in packets {
            let n = p.len().min(buf.len() - offset);
            buf[offset..offset + n].copy_from_slice(&p[..n]);
            offset += n;
        }
        buf
    }

    /// Build a synthetic CCSDS packet with the given APID and
    /// payload. Primary header layout per CCSDS 133.0-B-1.
    fn make_packet(apid: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(PKT_HEADER_LEN + payload.len());
        // Bytes 0-1: version=0 + type=0 + secondary header flag=0
        // + APID (low 11 bits).
        // 0x07FF = APID field width (low 11 bits of the first
        // 16-bit word per CCSDS 133.0-B-1). Same numeric value
        // as FHP_MASK, but a different field — kept inline here
        // since this is test fixture code with a fixed test APID
        // already inside the field's range.
        let header_word = apid & 0x07FF;
        p.extend_from_slice(&header_word.to_be_bytes());
        // Bytes 2-3: sequence flags (top 2) + sequence count (low
        // 14) — flags 0b11 = "unsegmented".
        p.extend_from_slice(&[0xC0, 0x00]);
        // Bytes 4-5: zero-based length field.
        let len_field = (payload.len() - 1) as u16;
        p.extend_from_slice(&len_field.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn reassembles_packets_packed_in_one_field() {
        // Realistic Meteor case: `M_PDU` data field is packed with
        // multiple packets back-to-back; the reassembler emits
        // each in turn. Trailing-zero fixtures (where one small
        // packet sits alone in an 882-byte field) aren't
        // representative of the wire and would parse the
        // remainder as APID-0 fill — see the demux's APID_ZERO
        // filter for that case.
        let pkt_a = make_packet(0x100, b"hello-meteor");
        let pkt_b = make_packet(0x101, b"second-packet-back-to-back");
        let region = build_region(0, &[pkt_a.clone(), pkt_b.clone()]);
        let mut r = MpduReassembler::new();
        let out = r.push(&region).expect("push");
        // First two emissions are our planted packets; further
        // emissions are 7-byte APID-0 packets parsed from the
        // trailing zero fill (filtered downstream by the demux).
        assert!(out.len() >= 2);
        assert_eq!(out[0], pkt_a);
        assert_eq!(out[1], pkt_b);
        assert!(r.is_in_sync());
    }

    #[test]
    fn reassembles_packet_spanning_two_regions() {
        // Packet large enough that it can't fit in one `M_PDU`
        // data field — needs to span two regions.
        let big_payload: Vec<u8> = (0..1500).map(|i| (i & 0xFF) as u8).collect();
        let big_pkt = make_packet(0x200, &big_payload);
        let head_len = MPDU_DATA_LEN;
        let head = big_pkt[..head_len].to_vec();
        let tail = big_pkt[head_len..].to_vec();
        // Region 1: FHP=0 (packet header at start), full data
        // field is the head of the big packet.
        let mut region1 = vec![0_u8; MPDU_HEADER_LEN + MPDU_DATA_LEN];
        region1[0..2].copy_from_slice(&0_u16.to_be_bytes());
        region1[MPDU_HEADER_LEN..].copy_from_slice(&head);
        // Region 2: FHP = tail.len() means "next packet header
        // lives at byte offset tail.len() of this data field."
        let fhp2 = tail.len() as u16;
        let mut region2 = vec![0_u8; MPDU_HEADER_LEN + MPDU_DATA_LEN];
        region2[0..2].copy_from_slice(&(fhp2 & FHP_MASK).to_be_bytes());
        region2[MPDU_HEADER_LEN..MPDU_HEADER_LEN + tail.len()].copy_from_slice(&tail);
        let mut r = MpduReassembler::new();
        let out1 = r.push(&region1).expect("push 1");
        assert!(out1.is_empty(), "no complete packet after region 1");
        let out2 = r.push(&region2).expect("push 2");
        // First emission is the recovered big packet; trailing
        // zeros in region 2 (after the tail) parse as APID-0
        // fill packets which the demux filters out downstream.
        assert!(
            !out2.is_empty(),
            "reassembler must emit at least one packet"
        );
        assert_eq!(out2[0], big_pkt);
    }

    #[test]
    fn rejects_wrong_region_length() {
        let mut r = MpduReassembler::new();
        let err = r.push(&[0_u8; 100]).expect_err("must reject short input");
        assert!(matches!(err, MpduError::BadRegionLength(100)));
    }

    #[test]
    fn lose_sync_clears_buffer() {
        let pkt = make_packet(0x100, b"world");
        let region = build_region(0, std::slice::from_ref(&pkt));
        let mut r = MpduReassembler::new();
        r.push(&region).expect("push");
        assert!(r.is_in_sync());
        r.lose_sync();
        assert!(!r.is_in_sync());
        // Next push with FHP_NO_HEADER emits nothing (we discard
        // the no-header continuation).
        let mut bad_region = vec![0_u8; MPDU_HEADER_LEN + MPDU_DATA_LEN];
        bad_region[0..2].copy_from_slice(&FHP_NO_HEADER.to_be_bytes());
        let out = r.push(&bad_region).expect("push noheader");
        assert!(out.is_empty());
    }

    #[test]
    fn integrity_gate_rejects_implausible_packet() {
        // After RS decode an Ok(...) result CAN still contain
        // miscorrected bytes (see RsError docs). The M_PDU layer
        // is the first place we can sanity-check what falls out:
        // CCSDS packet version must be 0, length must be in a
        // realistic range. This test plants a "valid-looking"
        // packet header with a 60 KB length field — well past
        // PKT_MAX_LEN — and asserts the reassembler drops the
        // packet AND loses sync rather than emitting it.
        let mut region = vec![0_u8; MPDU_HEADER_LEN + MPDU_DATA_LEN];
        region[0..2].copy_from_slice(&0_u16.to_be_bytes()); // FHP=0
        // Bytes 8-9 of the buffer are bytes 6-7 of the packet
        // (length field). Set length field = 0xEFFF → total_len
        // = 6 + 0xEFFF + 1 = 0xF006 (~60 KB), past PKT_MAX_LEN.
        region[MPDU_HEADER_LEN + 4] = 0xEF;
        region[MPDU_HEADER_LEN + 5] = 0xFF;
        let mut r = MpduReassembler::new();
        let out = r.push(&region).expect("push");
        assert!(out.is_empty(), "implausible-length packet must not emit");
        assert!(!r.is_in_sync(), "integrity-gate failure should lose sync");
    }

    #[test]
    fn integrity_gate_rejects_nonzero_version() {
        // CCSDS 133.0-B-1: packet version is always 000. A non-
        // zero top-3-bits value of byte 0 means RS miscorrected
        // (or the buffer is misaligned). Drop + lose sync.
        let mut region = vec![0_u8; MPDU_HEADER_LEN + MPDU_DATA_LEN];
        region[0..2].copy_from_slice(&0_u16.to_be_bytes()); // FHP=0
        // Plant a packet with version=001 (top 3 bits of byte 0).
        region[MPDU_HEADER_LEN] = 0b0010_0001;
        // Sane length so the only failure mode is the version check.
        region[MPDU_HEADER_LEN + 4] = 0;
        region[MPDU_HEADER_LEN + 5] = 5; // total_len = 6+5+1 = 12
        let mut r = MpduReassembler::new();
        let out = r.push(&region).expect("push");
        assert!(out.is_empty(), "non-zero-version packet must not emit");
        assert!(!r.is_in_sync(), "integrity-gate failure should lose sync");
    }

    #[test]
    fn malformed_fhp_doesnt_panic() {
        // FHP that's larger than the data field (defensive check
        // for malformed CADUs that get through despite RS).
        // Should silently skip rather than panic.
        let mut r = MpduReassembler::new();
        let mut region = vec![0_u8; MPDU_HEADER_LEN + MPDU_DATA_LEN];
        // FHP = 1500 — way past the 882-byte data field.
        // 0x05DC = 1500 decimal. Larger than the 882-byte data
        // field on purpose; the masking is there to keep the FHP
        // in the valid 11-bit range.
        let bad_fhp: u16 = 0x05DC & FHP_MASK;
        region[0..2].copy_from_slice(&bad_fhp.to_be_bytes());
        let out = r.push(&region).expect("push");
        assert!(out.is_empty());
        assert!(!r.is_in_sync());
    }
}
