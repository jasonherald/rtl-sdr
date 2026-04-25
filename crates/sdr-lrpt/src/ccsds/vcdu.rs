//! VCDU primary header parsing for Meteor LRPT.
//!
//! Layout (post-RS-decode, pre-`M_PDU`-strip — bytes from the start
//! of the [`super::VCDU_TOTAL_LEN`]-byte CCSDS frame):
//!
//! ```text
//! Bytes  Field
//! 0      2-bit version + 6-bit spacecraft ID (high)
//! 1      2-bit spacecraft ID (low) + 6-bit VCID
//! 2-4    24-bit virtual-channel frame counter (big-endian)
//! 5      1-bit replay flag + 7-bit spare
//! 6-7    16-bit insert zone (Meteor-specific extension)
//! 8-9    16-bit `M_PDU` header — top 5 bits reserved, low 11 bits
//!        are the first-header-pointer (FHP)
//! 10..   882 bytes of `M_PDU` data
//! ```
//!
//! Field offsets match `MeteorDemod::VCDU` constructor exactly
//! (offset shifted by -4 because `MeteorDemod` indexes from a CADU
//! pointer that includes the 4-byte ASM; we work post-ASM-strip).

use super::{VCDU_HEADER_LEN, VCDU_TOTAL_LEN};

/// Parsed VCDU primary header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vcdu {
    pub version: u8,
    pub spacecraft_id: u16,
    pub virtual_channel_id: u8,
    /// Monotonic 24-bit counter per virtual channel.
    pub counter: u32,
    pub replay_flag: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum VcduError {
    #[error("input too short: expected {VCDU_TOTAL_LEN} bytes, got {actual}")]
    TooShort { actual: usize },
}

impl Vcdu {
    /// Parse the primary header from a [`VCDU_TOTAL_LEN`]-byte
    /// VCDU buffer. Does NOT validate the data field or the
    /// `M_PDU` header — those are the `M_PDU` reassembler's job.
    ///
    /// # Errors
    ///
    /// Returns `VcduError::TooShort` if the input is smaller than
    /// the full VCDU.
    pub fn parse(input: &[u8]) -> Result<Self, VcduError> {
        if input.len() < VCDU_TOTAL_LEN {
            return Err(VcduError::TooShort {
                actual: input.len(),
            });
        }
        let version = input[0] >> 6;
        // SCID spans bits 0-7 of the second byte plus the low 6
        // bits of the first byte's bottom 6 bits — reconstruct
        // by concatenating, matching `MeteorDemod`'s bit packing.
        let scid_high = u16::from(input[0] & 0b0011_1111);
        let scid_low = u16::from(input[1] >> 6);
        let spacecraft_id = (scid_high << 2) | scid_low;
        let virtual_channel_id = input[1] & 0x3F;
        let counter =
            (u32::from(input[2]) << 16) | (u32::from(input[3]) << 8) | u32::from(input[4]);
        let replay_flag = (input[5] & 0x80) != 0;
        Ok(Self {
            version,
            spacecraft_id,
            virtual_channel_id,
            counter,
            replay_flag,
        })
    }

    /// Borrow the `M_PDU` header + data field slice from a VCDU.
    /// Returns the bytes after the [`VCDU_HEADER_LEN`]-byte
    /// primary header.
    ///
    /// # Errors
    ///
    /// Returns `VcduError::TooShort` if the input is smaller than
    /// the full VCDU.
    pub fn mpdu_region(input: &[u8]) -> Result<&[u8], VcduError> {
        if input.len() < VCDU_TOTAL_LEN {
            return Err(VcduError::TooShort {
                actual: input.len(),
            });
        }
        Ok(&input[VCDU_HEADER_LEN..VCDU_TOTAL_LEN])
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

    /// Build a synthetic VCDU buffer with the given fields. `M_PDU`
    /// region (bytes 8..[`VCDU_TOTAL_LEN`]) gets a recognizable
    /// pattern for slicing tests.
    fn synthetic_vcdu(scid: u16, vcid: u8, counter: u32) -> Vec<u8> {
        let mut buf = vec![0_u8; VCDU_TOTAL_LEN];
        // Byte 0: version=01 (top 2 bits) + scid_high (low 6).
        buf[0] = (1 << 6) | ((scid >> 2) & 0b0011_1111) as u8;
        // Byte 1: scid_low (top 2 bits) + vcid (low 6).
        buf[1] = (((scid & 0b11) as u8) << 6) | (vcid & 0x3F);
        buf[2] = ((counter >> 16) & 0xFF) as u8;
        buf[3] = ((counter >> 8) & 0xFF) as u8;
        buf[4] = (counter & 0xFF) as u8;
        // Byte 5: replay flag clear.
        buf[5] = 0;
        // Bytes 6-7: insert zone — leave zero.
        // Bytes 8..VCDU_TOTAL_LEN: recognizable pattern.
        for (i, slot) in buf.iter_mut().enumerate().skip(VCDU_HEADER_LEN) {
            *slot = (i & 0xFF) as u8;
        }
        buf
    }

    #[test]
    fn parse_roundtrip_typical_meteor_fields() {
        let v = synthetic_vcdu(140, super::super::VCID_AVHRR, 0x00AB_CDEF);
        let parsed = Vcdu::parse(&v).expect("parse");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.spacecraft_id, 140);
        assert_eq!(parsed.virtual_channel_id, super::super::VCID_AVHRR);
        assert_eq!(parsed.counter, 0x00AB_CDEF);
        assert!(!parsed.replay_flag);
    }

    #[test]
    fn parses_replay_flag() {
        let mut v = synthetic_vcdu(42, 5, 1);
        v[5] = 0x80; // set replay flag
        let parsed = Vcdu::parse(&v).expect("parse");
        assert!(parsed.replay_flag);
    }

    #[test]
    fn rejects_short_input() {
        let err = Vcdu::parse(&[0_u8; 100]).expect_err("must reject short input");
        assert!(matches!(err, VcduError::TooShort { actual: 100 }));
    }

    #[test]
    fn mpdu_region_has_documented_size() {
        let v = synthetic_vcdu(140, 5, 1);
        let region = Vcdu::mpdu_region(&v).expect("mpdu_region");
        assert_eq!(
            region.len(),
            super::super::MPDU_HEADER_LEN + super::super::MPDU_DATA_LEN,
        );
        // First byte of the region is the byte at VCDU_HEADER_LEN
        // in the source — confirm we sliced from the right
        // offset.
        assert_eq!(region[0], (VCDU_HEADER_LEN & 0xFF) as u8);
    }
}
