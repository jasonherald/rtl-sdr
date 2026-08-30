//! Orbcomm packet handling — framing, checksums, decoding.

/// Modulus of the Fletcher-16 running sums. The Orbcomm downlink uses the
/// mod-**256** variant (each sum is one byte wide), not the more common
/// mod-255 one — named here so [`fletcher16`] and
/// [`fletcher16_check_bytes`] can never drift onto different moduli.
const FLETCHER_MODULUS: u32 = 256;

/// Fletcher-16 with mod-256 running sums, as used by the Orbcomm
/// downlink (reference: `helpers.py::fletcher_checksum`). A valid
/// packet (payload + 2 trailing check bytes) sums to zero.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn fletcher16(bytes: &[u8]) -> u16 {
    let mut sum1: u32 = 0;
    let mut sum2: u32 = 0;
    for b in bytes {
        sum1 = (sum1 + u32::from(*b)) % FLETCHER_MODULUS;
        sum2 = (sum2 + sum1) % FLETCHER_MODULUS;
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
    let c0 = (FLETCHER_MODULUS - (s1 + s2) % FLETCHER_MODULUS) % FLETCHER_MODULUS;
    let c1 = (FLETCHER_MODULUS - (s1 + c0) % FLETCHER_MODULUS) % FLETCHER_MODULUS;
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

/// GPS epoch (1980-01-06 00:00:00 UTC) as a Unix timestamp. The
/// reference decoder applies no leap-second correction, so neither do we.
const GPS_EPOCH_UNIX: i64 = 315_964_800;
/// Seconds in a GPS week.
const SECONDS_PER_WEEK: i64 = 604_800;
/// Full-scale ECEF position magnitude for a 20-bit field, metres
/// (Orbcomm Serial Interface Specification E80050015 Rev F).
const MAX_R_SAT_M: f64 = 8_378_155.0;
/// Full-scale ECEF velocity magnitude for a 20-bit field, m/s.
const MAX_V_SAT_MS: f64 = 7_700.0;
/// 2^20 — the 20-bit field's full-scale count.
const VAL_20_BITS: f64 = 1_048_576.0;
/// WGS84 semi-major axis, metres.
const WGS84_A: f64 = 6_378_137.0;
/// WGS84 inverse flattening.
const WGS84_INV_F: f64 = 298.257_223_563;
/// Lower bound of the plausible Orbcomm altitude band, metres.
const MIN_PLAUSIBLE_ALT_M: f64 = 400_000.0;
/// Upper bound of the plausible Orbcomm altitude band, metres.
const MAX_PLAUSIBLE_ALT_M: f64 = 1_000_000.0;

// ---------------------------------------------------------------------------
// Ephemeris payload layout
// ---------------------------------------------------------------------------
// Shared by `decode_ephemeris` and the test-only encoder below. Ported from
// `original/ORBCOMM-receiver/file_decoder.py` (the `Ephemeris` branch), which
// works on the packet's hex-character string.
//
// A 24-byte ephemeris packet is:
//   b0        header byte 0x1F
//   b1        sat_id
//   b2..=b21  20-byte ephemeris payload
//   b22, b23  Fletcher-16 check bytes
//
// The reference forms `payload` by reversing the payload's byte order:
//   payload_byte[j] = bytes[21 - j]        for j in 0..20
// (Python: `''.join(packet[xx:xx+2] for xx in range(42, 2, -2))`, i.e. hex
// offsets 42, 40, … 4 → bytes 21, 20, … 2.) It then slices the resulting 40
// hex characters — nibbles — where
//   nib[2j] = payload_byte[j] >> 4,  nib[2j + 1] = payload_byte[j] & 0x0F
//
// Field map over `nib` (half-open index ranges):
//   0..4    GPS week number     big-endian 16-bit
//   4..10   GPS time of week    big-endian 24-bit, seconds
//   10..15  z velocity          20-bit scaled
//   15..20  y velocity          20-bit scaled
//   20..25  x velocity          20-bit scaled
//   25..30  z position          20-bit scaled
//   30..35  y position          20-bit scaled
//   35..40  x position          20-bit scaled
//
// Each 20-bit field is read by the reference as
//   int(r[0:2][::-1], 16) + 256 * int(r[2:4][::-1], 16) + 65536 * int(r[4:], 16)
// over `r`, the *character-reversed* 5-nibble slice. Writing the in-order
// nibbles as n0..n4, we have r = [n4, n3, n2, n1, n0], so
//   r[0:2][::-1] = n3 n4 → n3*16 + n4
//   r[2:4][::-1] = n1 n2 → n1*16 + n2
//   r[4:]        = n0
//   total = 65536*n0 + 4096*n1 + 256*n2 + 16*n3 + n4
// The two reversals cancel: the value is the plain big-endian nibble read
//   raw = n0 << 16 | n1 << 12 | n2 << 8 | n3 << 4 | n4
//
// Scaling back to physical units (same spec):
//   val = 2 * raw * MAX / 2^20 - MAX
// with MAX = MAX_R_SAT_M for position and MAX_V_SAT_MS for velocity.
// ---------------------------------------------------------------------------

/// Number of nibbles in the ephemeris payload.
const EPH_NIBBLES: usize = 40;
/// Nibble offsets of the six 20-bit scaled fields, in `nib` order.
const NIB_WEEK: usize = 0;
const NIB_TOW: usize = 4;
const NIB_VZ: usize = 10;
const NIB_VY: usize = 15;
const NIB_VX: usize = 20;
const NIB_PZ: usize = 25;
const NIB_PY: usize = 30;
const NIB_PX: usize = 35;
/// Width in nibbles of a 20-bit scaled field.
const NIB_SCALED_LEN: usize = 5;

/// Split the 24-byte packet's payload into the reference's 40-nibble array.
/// See the layout block above. Caller guarantees `bytes.len() == 24`.
fn payload_nibbles(bytes: &[u8]) -> [u8; EPH_NIBBLES] {
    let mut nib = [0u8; EPH_NIBBLES];
    for (j, pair) in nib.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        // payload_byte[j] = bytes[21 - j] — the reversed byte order.
        let b = bytes[21 - j];
        pair[0] = b >> 4;
        pair[1] = b & 0x0F;
    }
    nib
}

/// Big-endian read of `len` nibbles starting at `start`.
fn nibbles_be(nib: &[u8; EPH_NIBBLES], start: usize, len: usize) -> u32 {
    nib[start..start + len]
        .iter()
        .fold(0u32, |acc, n| (acc << 4) | u32::from(*n))
}

/// Convert WGS84 ECEF metres to (latitude °, longitude °, altitude m).
/// Port of the reference's `helpers.py::ecef_to_lla` (EPSG guidance note 7-2).
// Single-letter bindings are the geodetic formula's own symbols (f, b, p, q,
// v, h); renaming them would make the port harder to check against the source.
#[allow(clippy::many_single_char_names)]
pub(crate) fn ecef_to_lla(x_ecef: f64, y_ecef: f64, z_ecef: f64) -> (f64, f64, f64) {
    let f = 1.0 / WGS84_INV_F;
    let b = WGS84_A * (1.0 - f);
    let e_sqrd = 1.0 - (b * b) / (WGS84_A * WGS84_A);
    let eps = e_sqrd / (1.0 - e_sqrd);
    let p = x_ecef.hypot(y_ecef);
    let q = (z_ecef * WGS84_A).atan2(p * b);
    let phi = (z_ecef + eps * b * q.sin().powi(3)).atan2(p - e_sqrd * WGS84_A * q.cos().powi(3));
    let lambda = y_ecef.atan2(x_ecef);
    let v = WGS84_A / (1.0 - e_sqrd * phi.sin().powi(2)).sqrt();
    let h = (p / phi.cos()) - v;
    (phi.to_degrees(), lambda.to_degrees(), h)
}

/// Satellite ephemeris payload: GPS timestamp plus the satellite's own
/// reported WGS84 position and speed, decoded from the 20-bit scaled
/// ECEF fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Ephemeris {
    /// Sending satellite's id byte.
    pub sat_id: u8,
    /// Satellite-reported time as a Unix timestamp (GPS week + time of
    /// week against the 1980-01-06 epoch, no leap-second correction).
    pub sat_time_unix: i64,
    /// Sub-satellite latitude, degrees north.
    pub lat_deg: f64,
    /// Sub-satellite longitude, degrees east.
    pub lon_deg: f64,
    /// Altitude above the WGS84 ellipsoid, metres.
    pub alt_m: f64,
    /// Magnitude of the reported ECEF velocity, m/s.
    pub vel_ms: f64,
}

/// Decode the ephemeris payload of a 24-byte, checksum-valid packet.
/// Returns `None` when the packet is the wrong length, the GPS time of
/// week is out of range, or the decoded altitude falls outside the
/// plausible Orbcomm band — cheap guards against payloads that survived
/// the checksum but are not ephemeris.
pub(crate) fn decode_ephemeris(bytes: &[u8]) -> Option<Ephemeris> {
    if bytes.len() != PacketType::Ephemeris.packet_len() {
        return None;
    }
    let nib = payload_nibbles(bytes);

    let week = i64::from(nibbles_be(&nib, NIB_WEEK, 4));
    let time_of_week = i64::from(nibbles_be(&nib, NIB_TOW, 6));
    // The field is 24 bits wide but a GPS time of week only spans
    // `0..SECONDS_PER_WEEK`, so anything at or above a full week means we
    // decoded something that is not an ephemeris timestamp. Reject it the
    // same way the altitude gate below does — `parse_packet` keeps the raw
    // bytes as `Other` rather than publishing a bogus time.
    if time_of_week >= SECONDS_PER_WEEK {
        return None;
    }
    let sat_time_unix = GPS_EPOCH_UNIX + week * SECONDS_PER_WEEK + time_of_week;

    // val = 2 * raw * MAX / 2^20 - MAX
    let scaled = |start: usize, max: f64| -> f64 {
        2.0 * f64::from(nibbles_be(&nib, start, NIB_SCALED_LEN)) * max / VAL_20_BITS - max
    };
    let ecef_vel = [
        scaled(NIB_VX, MAX_V_SAT_MS),
        scaled(NIB_VY, MAX_V_SAT_MS),
        scaled(NIB_VZ, MAX_V_SAT_MS),
    ];
    let ecef_pos = [
        scaled(NIB_PX, MAX_R_SAT_M),
        scaled(NIB_PY, MAX_R_SAT_M),
        scaled(NIB_PZ, MAX_R_SAT_M),
    ];

    let vel_ms = ecef_vel.iter().map(|v| v * v).sum::<f64>().sqrt();
    let (lat_deg, lon_deg, alt_m) = ecef_to_lla(ecef_pos[0], ecef_pos[1], ecef_pos[2]);

    // Plausibility gate: Orbcomm flies a ~715 km circular orbit, so anything
    // outside 400–1000 km means we decoded noise that happened to checksum.
    // A NaN altitude (degenerate polar geometry) also fails this test.
    if !(MIN_PLAUSIBLE_ALT_M..=MAX_PLAUSIBLE_ALT_M).contains(&alt_m) {
        return None;
    }

    Some(Ephemeris {
        sat_id: bytes[1],
        sat_time_unix,
        lat_deg,
        lon_deg,
        alt_m,
        vel_ms,
    })
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
        PacketType::Ephemeris => match decode_ephemeris(bytes) {
            Some(eph) => OrbcommPacket::Ephemeris(eph),
            // Checksum-valid but implausible: keep the raw bytes rather
            // than publish a bogus position.
            None => OrbcommPacket::Other {
                packet_type: ty,
                bytes: bytes.to_vec(),
            },
        },
        _ => OrbcommPacket::Other {
            packet_type: ty,
            bytes: bytes.to_vec(),
        },
    })
}

/// Build a valid 24-byte ephemeris packet from physical values — the exact
/// inverse of [`decode_ephemeris`], sharing the layout block above.
///
/// Test-only, but `pub(crate)` so sibling modules' tests can mint valid
/// ephemeris packets for deframer round-trips.
#[cfg(test)]
pub(crate) fn encode_ephemeris_for_test(
    sat_id: u8,
    week: u16,
    tow_s: u32,
    ecef_pos: [f64; 3],
    ecef_vel: [f64; 3],
) -> Vec<u8> {
    /// Write `value` big-endian across `len` nibbles starting at `start`.
    fn put(nib: &mut [u8; EPH_NIBBLES], start: usize, len: usize, value: u32) {
        for (i, slot) in nib[start..start + len].iter_mut().enumerate() {
            let shift = 4 * (len - 1 - i);
            #[allow(clippy::cast_possible_truncation)]
            {
                *slot = ((value >> shift) & 0x0F) as u8;
            }
        }
    }
    /// Inverse of `val = 2*raw*max/2^20 - max`, saturating at full scale.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn quantize(val: f64, max: f64) -> u32 {
        let raw = ((val + max) * VAL_20_BITS / (2.0 * max)).round();
        raw.clamp(0.0, VAL_20_BITS - 1.0) as u32
    }

    let mut nib = [0u8; EPH_NIBBLES];
    put(&mut nib, NIB_WEEK, 4, u32::from(week));
    put(&mut nib, NIB_TOW, 6, tow_s & 0x00FF_FFFF);
    for (start, val, max) in [
        (NIB_VZ, ecef_vel[2], MAX_V_SAT_MS),
        (NIB_VY, ecef_vel[1], MAX_V_SAT_MS),
        (NIB_VX, ecef_vel[0], MAX_V_SAT_MS),
        (NIB_PZ, ecef_pos[2], MAX_R_SAT_M),
        (NIB_PY, ecef_pos[1], MAX_R_SAT_M),
        (NIB_PX, ecef_pos[0], MAX_R_SAT_M),
    ] {
        put(&mut nib, start, NIB_SCALED_LEN, quantize(val, max));
    }

    // Pack nibbles into the reversed-order payload bytes, then undo the
    // reversal so they land at bytes[2..22].
    let mut payload = [0u8; EPH_NIBBLES / 2];
    for (dst, pair) in payload.iter_mut().zip(nib.as_chunks::<2>().0) {
        *dst = (pair[0] << 4) | pair[1];
    }
    let mut bytes = vec![PacketType::Ephemeris.header_byte(), sat_id];
    bytes.extend(payload.iter().rev());
    let (c0, c1) = fletcher16_check_bytes(&bytes);
    bytes.push(c0);
    bytes.push(c1);
    bytes
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

    #[test]
    fn header_byte_round_trips_for_every_variant() {
        for ty in [
            PacketType::Sync,
            PacketType::Message,
            PacketType::UplinkInfo,
            PacketType::DownlinkInfo,
            PacketType::Network,
            PacketType::Fill,
            PacketType::Ephemeris,
            PacketType::Orbital,
        ] {
            assert_eq!(PacketType::from_header(ty.header_byte()), Some(ty));
        }
    }

    #[test]
    #[allow(clippy::panic)]
    fn undecoded_type_parses_to_other_with_raw_bytes() {
        // A valid 12-byte Fill packet has no typed form yet — it must come
        // back as `Other` carrying the full packet, check bytes included.
        let mut p = vec![0x1Eu8, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99];
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        match parse_packet(&p) {
            Some(OrbcommPacket::Other { packet_type, bytes }) => {
                assert_eq!(packet_type, PacketType::Fill);
                assert_eq!(bytes, p);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// Altitude the decoder should report for an ECEF position, via the
    /// same WGS84 conversion it uses.
    fn expected_alt_for(pos: [f64; 3]) -> f64 {
        ecef_to_lla(pos[0], pos[1], pos[2]).2
    }

    #[test]
    #[allow(clippy::panic)]
    fn ephemeris_round_trips_position_and_time() {
        // ~630 km orbit point; |v| ≈ 6.2 km/s. Both well inside the
        // 20-bit fields' full scale.
        let pos = [3_000_000.0, 4_000_000.0, 4_900_000.0];
        let vel = [-5_000.0, 3_000.0, 2_000.0];
        let pkt = encode_ephemeris_for_test(0x2C, 2434, 400_000, pos, vel);
        assert_eq!(pkt.len(), 24);
        assert_eq!(fletcher16(&pkt), 0);

        let parsed = parse_packet(&pkt);
        let Some(OrbcommPacket::Ephemeris(e)) = parsed else {
            panic!("expected ephemeris, got {parsed:?}");
        };
        assert_eq!(e.sat_id, 0x2C);
        // GPS epoch + weeks + time of week, no leap correction (as reference).
        assert_eq!(e.sat_time_unix, 315_964_800 + 2434 * 604_800 + 400_000);

        // 20-bit quantization: position step ≈ 16 m, velocity step ≈ 0.015 m/s.
        let expected_vel = (vel[0].powi(2) + vel[1].powi(2) + vel[2].powi(2)).sqrt();
        assert!((e.vel_ms - expected_vel).abs() < 1.0, "vel {}", e.vel_ms);
        assert!(
            (e.alt_m - expected_alt_for(pos)).abs() < 100.0,
            "alt {}",
            e.alt_m
        );
        let (want_lat, want_lon, _) = ecef_to_lla(pos[0], pos[1], pos[2]);
        assert!((e.lat_deg - want_lat).abs() < 0.01, "lat {}", e.lat_deg);
        assert!((e.lon_deg - want_lon).abs() < 0.01, "lon {}", e.lon_deg);
    }

    #[test]
    #[allow(clippy::panic)]
    fn implausible_ephemeris_degrades_to_raw() {
        // All-zero payload decodes to ECEF (-MAX, -MAX, -MAX) — an altitude
        // far outside the 400–1000 km band, so parse must fall back to Other.
        let mut p = vec![0x1Fu8, 0x2C];
        p.extend_from_slice(&[0u8; 20]);
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        match parse_packet(&p) {
            Some(OrbcommPacket::Other {
                packet_type: PacketType::Ephemeris,
                ..
            }) => {}
            other => panic!("expected raw fallback, got {other:?}"),
        }
    }

    /// `CodeRabbit` round 1 on PR #871: the 24-bit time-of-week field can
    /// express values far beyond a GPS week, and a noise payload that
    /// happens to checksum could carry one. Such a packet must degrade to
    /// raw `Other` rather than publish a satellite time days out.
    #[test]
    #[allow(clippy::panic)]
    fn out_of_range_time_of_week_degrades_to_raw() {
        // A position/velocity pair that passes the altitude gate, so the
        // time-of-week check is the only thing that can reject the packet.
        let pos = [3_000_000.0, 4_000_000.0, 4_900_000.0];
        let vel = [-5_000.0, 3_000.0, 2_000.0];

        // One second past the end of the week: the first invalid value.
        // The encoder masks to 24 bits, and 604_800 < 0xFF_FFFF, so it
        // reaches `decode_ephemeris` intact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let over = encode_ephemeris_for_test(0x2C, 2434, SECONDS_PER_WEEK as u32, pos, vel);
        match parse_packet(&over) {
            Some(OrbcommPacket::Other {
                packet_type: PacketType::Ephemeris,
                ..
            }) => {}
            other => panic!("expected raw fallback for an over-limit TOW, got {other:?}"),
        }

        // The last valid instant of the week still decodes.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let last = encode_ephemeris_for_test(0x2C, 2434, SECONDS_PER_WEEK as u32 - 1, pos, vel);
        let parsed = parse_packet(&last);
        let Some(OrbcommPacket::Ephemeris(e)) = parsed else {
            panic!("expected the last second of the week to decode, got {parsed:?}");
        };
        assert_eq!(
            e.sat_time_unix,
            GPS_EPOCH_UNIX + 2434 * SECONDS_PER_WEEK + SECONDS_PER_WEEK - 1
        );
    }

    #[test]
    fn ecef_to_lla_matches_known_reference_points() {
        // On the equator at the prime meridian, at the ellipsoid surface.
        let (lat, lon, alt) = ecef_to_lla(WGS84_A, 0.0, 0.0);
        assert!(lat.abs() < 1e-9);
        assert!(lon.abs() < 1e-9);
        assert!(alt.abs() < 1e-6, "alt {alt}");
        // 100 km up, 90° east.
        let (lat, lon, alt) = ecef_to_lla(0.0, WGS84_A + 100_000.0, 0.0);
        assert!(lat.abs() < 1e-9);
        assert!((lon - 90.0).abs() < 1e-9);
        assert!((alt - 100_000.0).abs() < 1e-6, "alt {alt}");
    }

    #[test]
    fn nibble_layout_matches_reference_slicing() {
        // Sanity-check the byte reversal + big-endian nibble read against a
        // hand-built packet: put 0x12345 in the x-position field.
        let mut bytes = vec![0x1Fu8, 0x00];
        bytes.extend_from_slice(&[0u8; 20]);
        // nib[35..40] = 1,2,3,4,5 ⇒ payload bytes 17 (low nibble) .. 19.
        // payload_byte[j] = bytes[21 - j]: j=17→b4, j=18→b3, j=19→b2.
        bytes[4] |= 0x01; // payload_byte[17] low nibble = nib[35]
        bytes[3] = 0x23; // payload_byte[18] = nib[36..38]
        bytes[2] = 0x45; // payload_byte[19] = nib[38..40]
        let nib = payload_nibbles(&bytes);
        assert_eq!(nibbles_be(&nib, NIB_PX, NIB_SCALED_LEN), 0x0001_2345);
    }
}
