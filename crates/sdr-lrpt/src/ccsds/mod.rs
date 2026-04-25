//! CCSDS framing layer for Meteor-M LRPT.
//!
//! Parses Virtual Channel Data Units (VCDUs) from the post-FEC
//! byte stream, demultiplexes by virtual-channel ID, and
//! reassembles Multiplexed Protocol Data Units (`M_PDU`s) — CCSDS
//! packets that span multiple VCDUs.
//!
//! Constants are lifted verbatim from `MeteorDemod`'s
//! `decoder/protocol/lrpt/decoder.h` and `decoder/protocol/vcdu.h`,
//! the canonical modern reference for Meteor's CVCDU layout.
//!
//! References (read-only):
//! - `original/MeteorDemod/decoder/protocol/vcdu.h` (header layout)
//! - `original/MeteorDemod/decoder/protocol/lrpt/decoder.cpp`
//!   (`M_PDU` reassembly + VCID routing)
//! - `original/medet/met_packet.pas` (alternate older reference)

pub mod demux;
pub mod mpdu;
pub mod vcdu;

pub use demux::{Demux, ImagePacket};
pub use mpdu::{MpduError, MpduReassembler};
pub use vcdu::{Vcdu, VcduError};

// --- Canonical Meteor LRPT framing constants ---

/// Length of the VCDU primary header (per `MeteorDemod`'s
/// `VCDU::size()`).
pub const VCDU_HEADER_LEN: usize = 8;

/// Length of the `M_PDU` header — 11-bit first-header-pointer in
/// the high bits of a 2-byte word, top 5 bits reserved.
pub const MPDU_HEADER_LEN: usize = 2;

/// Length of the `M_PDU` data field carried in each VCDU.
/// `MeteorDemod`'s `cPDUDataSize`.
pub const MPDU_DATA_LEN: usize = 882;

/// Total VCDU bytes seen by the framing layer (primary header +
/// `M_PDU` header + `M_PDU` data field).
pub const VCDU_TOTAL_LEN: usize = VCDU_HEADER_LEN + MPDU_HEADER_LEN + MPDU_DATA_LEN;

/// FHP sentinel: "no CCSDS packet header in this VCDU's data
/// field" (frame is entirely a continuation). `MeteorDemod`'s
/// `cNoHeaderMark`.
pub const FHP_NO_HEADER: u16 = 0x7FF;

/// Virtual-channel ID for the AVHRR imaging stream. `MeteorDemod`'s
/// `cVCIDAVHRR`.
pub const VCID_AVHRR: u8 = 5;
