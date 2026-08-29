//! Orbcomm packet handling — framing, checksums, decoding.

/// Fletcher-16 with mod-256 running sums, as used by the Orbcomm
/// downlink (reference: `helpers.py::fletcher_checksum`). A valid
/// packet (payload + 2 trailing check bytes) sums to zero.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn fletcher16(bytes: &[u8]) -> u16 {
    let mut sum1: u32 = 0;
    let mut sum2: u32 = 0;
    for b in bytes {
        sum1 = (sum1 + u32::from(*b)) % 256;
        sum2 = (sum2 + sum1) % 256;
    }
    ((sum2 as u16) << 8) | (sum1 as u16)
}

/// Check bytes that make `payload ++ [b0, b1]` fold to zero.
/// Used only by tests and synthetic-packet builders.
#[must_use]
#[allow(clippy::cast_lossless, clippy::cast_possible_truncation)]
pub fn fletcher16_check_bytes(payload: &[u8]) -> (u8, u8) {
    let ck = fletcher16(payload);
    let (s2, s1) = (u32::from(ck >> 8), u32::from(ck & 0xFF));
    // Solve for c0, c1 such that appending zeroes both sums (mod 256).
    // After appending c0: sum1' = (s1 + c0) % 256; sum2' = (s2 + s1 + c0) % 256
    // After appending c1: sum1'' = (s1 + c0 + c1) % 256; sum2'' = (s2 + 2*s1 + 2*c0 + c1) % 256
    // For both to be zero: c0 ≡ -(s1 + s2) (mod 256); c1 ≡ -(s1 + c0) (mod 256)
    let c0 = (256 - (s1 + s2) % 256) % 256;
    let c1 = (256 - (s1 + c0) % 256) % 256;
    (c0 as u8, c1 as u8)
}

/// Orbcomm downlink packet types, keyed by their header byte
/// (reference: `orbcomm_packet.py`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    /// Sync beacon: carries the network sync code and the sending
    /// satellite's `sat_id`.
    Sync,
    /// Subscriber message packet.
    Message,
    /// Uplink channel info.
    UplinkInfo,
    /// Downlink channel info.
    DownlinkInfo,
    /// Network configuration packet.
    Network,
    /// Fill/idle packet with no payload of interest.
    Fill,
    /// Satellite ephemeris (position/velocity/time).
    Ephemeris,
    /// Orbital element set.
    Orbital,
}

impl PacketType {
    /// Look up the packet type for a header byte. Returns `None` for
    /// header bytes not in the spec table.
    #[must_use]
    pub fn from_header(byte: u8) -> Option<Self> {
        Some(match byte {
            0x65 => Self::Sync,
            0x1A => Self::Message,
            0x1B => Self::UplinkInfo,
            0x1C => Self::DownlinkInfo,
            0x1D => Self::Network,
            0x1E => Self::Fill,
            0x1F => Self::Ephemeris,
            0x22 => Self::Orbital,
            _ => return None,
        })
    }

    /// The header byte for this packet type (inverse of [`Self::from_header`]).
    #[must_use]
    pub fn header_byte(self) -> u8 {
        match self {
            Self::Sync => 0x65,
            Self::Message => 0x1A,
            Self::UplinkInfo => 0x1B,
            Self::DownlinkInfo => 0x1C,
            Self::Network => 0x1D,
            Self::Fill => 0x1E,
            Self::Ephemeris => 0x1F,
            Self::Orbital => 0x22,
        }
    }

    /// Total packet length in bytes, including the header byte and
    /// the two trailing Fletcher-16 check bytes. Ephemeris packets
    /// are double-length (24 bytes); every other type is 12 bytes.
    #[must_use]
    pub fn packet_len(self) -> usize {
        match self {
            Self::Ephemeris => 24,
            _ => 12,
        }
    }
}

/// Satellite ephemeris payload. Minimal for now — extended in a
/// follow-up task with the decoded position/velocity/time fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ephemeris {
    /// Sending satellite's id byte.
    pub sat_id: u8,
}

/// A parsed Orbcomm downlink packet.
#[derive(Debug, Clone, PartialEq)]
pub enum OrbcommPacket {
    /// Network sync beacon.
    Sync {
        /// 3-byte sync code (header byte + 2 following bytes).
        code: u32,
        /// Sending satellite's id byte.
        sat_id: u8,
    },
    /// Satellite ephemeris.
    Ephemeris(Ephemeris),
    /// Any other recognized packet type, not yet decoded further.
    Other {
        /// The packet's type.
        packet_type: PacketType,
        /// Raw packet bytes, including header and check bytes.
        bytes: Vec<u8>,
    },
}

/// Parse a checksum-valid, aligned packet into its typed form.
/// Returns `None` for unknown headers or length mismatches.
#[must_use]
pub fn parse_packet(bytes: &[u8]) -> Option<OrbcommPacket> {
    let ty = PacketType::from_header(*bytes.first()?)?;
    if bytes.len() != ty.packet_len() {
        return None;
    }
    Some(match ty {
        PacketType::Sync => OrbcommPacket::Sync {
            code: u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]),
            sat_id: bytes[3],
        },
        PacketType::Ephemeris => OrbcommPacket::Ephemeris(Ephemeris { sat_id: bytes[1] }),
        _ => OrbcommPacket::Other {
            packet_type: ty,
            bytes: bytes.to_vec(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn fletcher16_matches_reference_algorithm() {
        // Same algorithm as the reference decoder's fletcher_checksum
        // (mod-256 running sums over bytes). "abcde" is the classic
        // Wikipedia vector for the mod-255 variant; recompute for
        // mod-256 by hand: sums over [0x61,0x62,0x63,0x64,0x65]:
        // sum1 = (0x61+0x62+0x63+0x64+0x65) % 256 = 0xEF
        // sum2 = 0x61 + 0xC3 + 0x26(+256) ... assert via loop below.
        let mut sum1: u32 = 0;
        let mut sum2: u32 = 0;
        for b in b"abcde" {
            sum1 = (sum1 + u32::from(*b)) % 256;
            sum2 = (sum2 + sum1) % 256;
        }
        let expected = ((sum2 as u16) << 8) | sum1 as u16;
        assert_eq!(fletcher16(b"abcde"), expected);
    }

    #[test]
    fn packet_with_appended_check_bytes_sums_to_zero() {
        // Property the deframer relies on: append the two check bytes
        // (as the protocol does) and the whole packet folds to zero.
        let payload = [0x65u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let (b0, b1) = fletcher16_check_bytes(&payload);
        let mut full = payload.to_vec();
        full.push(b0);
        full.push(b1);
        assert_eq!(fletcher16(&full), 0);
    }

    #[test]
    fn header_bytes_match_spec_table() {
        assert_eq!(PacketType::from_header(0x65), Some(PacketType::Sync));
        assert_eq!(PacketType::from_header(0x1A), Some(PacketType::Message));
        assert_eq!(PacketType::from_header(0x1B), Some(PacketType::UplinkInfo));
        assert_eq!(
            PacketType::from_header(0x1C),
            Some(PacketType::DownlinkInfo)
        );
        assert_eq!(PacketType::from_header(0x1D), Some(PacketType::Network));
        assert_eq!(PacketType::from_header(0x1E), Some(PacketType::Fill));
        assert_eq!(PacketType::from_header(0x1F), Some(PacketType::Ephemeris));
        assert_eq!(PacketType::from_header(0x22), Some(PacketType::Orbital));
        assert_eq!(PacketType::from_header(0x00), None);
        assert_eq!(PacketType::Ephemeris.packet_len(), 24);
        assert_eq!(PacketType::Sync.packet_len(), 12);
    }

    #[test]
    #[allow(clippy::panic)]
    fn sync_packet_parses_code_and_sat_id() {
        // Spec field table (hex-char offsets incl. header): code (0,6) =
        // 3 bytes incl. header byte, sat_id (6,8) = byte 3.
        let mut p = vec![0x65u8, 0xAA, 0xBB, 0x2C, 0, 0, 0, 0, 0, 0];
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        let parsed = parse_packet(&p).expect("valid sync packet");
        match parsed {
            OrbcommPacket::Sync { code, sat_id } => {
                assert_eq!(code, 0x0065_AABB);
                assert_eq!(sat_id, 0x2C);
            }
            other => panic!("expected Sync, got {other:?}"),
        }
    }

    #[test]
    fn unknown_or_short_packets_return_none() {
        assert!(parse_packet(&[0x00; 12]).is_none());
        assert!(parse_packet(&[0x65]).is_none());
    }
}
