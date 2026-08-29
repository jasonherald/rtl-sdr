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
        let cksum = fletcher16(&payload);
        // check bytes chosen so the total folds to zero:
        let c0 = 255u16.wrapping_sub((cksum >> 8) + (cksum & 0xFF));
        let _ = c0; // derivation done in impl-side helper below
        let (b0, b1) = fletcher16_check_bytes(&payload);
        let mut full = payload.to_vec();
        full.push(b0);
        full.push(b1);
        assert_eq!(fletcher16(&full), 0);
    }
}
