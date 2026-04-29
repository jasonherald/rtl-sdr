//! CRC-CCITT-16 (KERMIT variant) for ACARS frames.
//!
//! Polynomial `0x1021`, **reflected** (`0x8408`), initial value
//! `0x0000`. ACARS feeds bytes LSB-first into the CRC register,
//! matching the on-the-wire bit order. Receiver verification:
//! feeding the entire frame including the trailing 2-byte BCS
//! through the same CRC yields `0` if the frame is intact.
//!
//! The init value matches the canonical `acarsdec` implementation
//! (`crc = 0` before the message-byte loop in `decodeAcars()` —
//! see `original/acarsdec/acars.c` and the `update_crc` macro in
//! `original/acarsdec/syndrom.h`). This is the KERMIT variant of
//! CRC-CCITT, not the more common X-25 variant which inits to
//! `0xFFFF`.

/// Initial value of the CRC register at frame start. KERMIT
/// variant — `acars.c:159` (`crc = 0;`) before the message-
/// byte loop. The X-25 variant uses `0xFFFF`; ACARS does not.
pub const ACARS_CRC_INIT: u16 = 0x0000;

/// Reflected polynomial (`0x1021` reflected). Used by the
/// LSB-first byte-feed update step.
pub const ACARS_CRC_POLY_REFLECTED: u16 = 0x8408;

/// Bits per byte. Exists so the CRC update loop reads as
/// "for each bit in the byte" rather than `for _ in 0..8`.
const BITS_PER_BYTE: usize = 8;

/// Mask for the CRC register's least-significant bit, the
/// "tap" the LSB-first algorithm checks each iteration.
const CRC_LSB_MASK: u16 = 0x0001;

/// Update a running CRC-CCITT-16 register with one byte.
/// Bytes are consumed LSB-first (ACARS wire convention).
#[must_use]
pub fn update(crc: u16, byte: u8) -> u16 {
    let mut crc = crc ^ u16::from(byte);
    for _ in 0..BITS_PER_BYTE {
        if crc & CRC_LSB_MASK != 0 {
            crc = (crc >> 1) ^ ACARS_CRC_POLY_REFLECTED;
        } else {
            crc >>= 1;
        }
    }
    crc
}

/// Compute CRC over a slice from the standard ACARS init value
/// (`ACARS_CRC_INIT`).
#[must_use]
pub fn compute(bytes: &[u8]) -> u16 {
    bytes.iter().fold(ACARS_CRC_INIT, |crc, &b| update(crc, b))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_known_test_vector() {
        // CRC-CCITT (KERMIT) "123456789" check value.
        //
        // The published KERMIT check value is `0x8921`, but that
        // reflects the protocol's transmit convention of sending
        // the low byte first: as a 16-bit register, the raw value
        // is `0x2189` and the bytes go on the wire `0x21, 0x89`
        // which a reader reassembling MSB-first reads as `0x8921`.
        //
        // ACARS sends the BCS low byte first too (see
        // `original/acarsdec/acars.c` CRC1 → CRC2 in that order),
        // so our `compute` returns the raw register `0x2189` and
        // a frame's `(crc & 0xff)` byte goes out before
        // `(crc >> 8)`. The receiver-property test below confirms
        // this layout reproduces the on-the-wire convention.
        let crc = compute(b"123456789");
        assert_eq!(crc, 0x2189, "raw KERMIT register for '123456789'");
    }

    #[test]
    fn crc_is_zero_after_appending_its_own_value() {
        // Receiver-side property: feeding the frame plus its
        // computed BCS yields zero.
        let payload = b"HELLO ACARS";
        let crc = compute(payload);
        let mut frame = payload.to_vec();
        frame.push((crc & 0xff) as u8); // BCS low
        frame.push((crc >> 8) as u8); // BCS high
        // Fold the entire frame through the CRC; result MUST be zero
        // for a correctly-formed transmission.
        assert_eq!(compute(&frame), 0);
    }
}
