//! V1 spacecraft labeling.
//!
//! No public sat_id-byte → spacecraft-name table exists: the reference
//! project's `sat_db.py` is keyed by name → NORAD id / frequency, and its
//! decoders only ever display the raw `sat_id` byte. Matching the
//! self-broadcast ephemeris position against an Orbcomm TLE set to recover
//! real spacecraft names is the planned V2 path (see
//! `docs/superpowers/specs/2026-08-29-orbcomm-decoder-design.md`,
//! "`sat_id` labeling" bullet). Until then, V1 labels spacecraft by their
//! raw id byte.

/// Format a satellite id byte as a stable, human-readable label
/// (e.g. `0x2C` → `"Sat 0x2C"`). Not a lookup — see the module docs for why.
#[must_use]
pub fn sat_label(sat_id: u8) -> String {
    format!("Sat {sat_id:#04X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_id_byte_as_hex() {
        assert_eq!(sat_label(0x2C), "Sat 0x2C");
    }

    #[test]
    fn zero_pads_low_bytes() {
        assert_eq!(sat_label(0x05), "Sat 0x05");
    }
}
