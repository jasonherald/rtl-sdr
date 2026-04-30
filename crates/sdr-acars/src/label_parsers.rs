//! ACARS label parsers — faithful port of
//! `original/acarsdec/label.c`. For each ACARS message that
//! carries one of ~40 known label codes, extract the Out-Off-
//! On-In (OOOI) metadata embedded in the text body at fixed
//! byte offsets — origin/destination airport codes plus the
//! timestamps for the four OOOI events (gate-out, wheels-off,
//! wheels-on, gate-in) and ETA.
//!
//! Issue #577. Spec at
//! `docs/superpowers/specs/2026-04-30-acars-label-parsers-design.md`.

use arrayvec::ArrayString;

/// Out-Off-On-In metadata extracted from an ACARS message
/// body. Faithful mirror of `oooi_t` in `acarsdec.h`. Each
/// field is `Option<...>` because most labels populate only a
/// subset (e.g. `QA` populates `sa` + `gout` only).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Oooi {
    /// Station of origin (4-char airport code).
    pub sa: Option<ArrayString<4>>,
    /// Destination airport (4-char airport code).
    pub da: Option<ArrayString<4>>,
    /// Gate-out time (4-char HHMM UTC).
    pub gout: Option<ArrayString<4>>,
    /// Wheels-off time.
    pub woff: Option<ArrayString<4>>,
    /// Wheels-on time.
    pub won: Option<ArrayString<4>>,
    /// Gate-in time.
    pub gin: Option<ArrayString<4>>,
    /// Estimated time of arrival.
    pub eta: Option<ArrayString<4>>,
}

impl Oooi {
    /// Returns `true` if at least one field is `Some`. Mirrors
    /// C's "return 1 only if ≥ 1 memcpy ran" semantic — every
    /// parser must surface at least one populated field for the
    /// result to be meaningful, otherwise the dispatch returns
    /// `None`.
    #[must_use]
    pub fn has_any(&self) -> bool {
        self.sa.is_some()
            || self.da.is_some()
            || self.gout.is_some()
            || self.woff.is_some()
            || self.won.is_some()
            || self.gin.is_some()
            || self.eta.is_some()
    }
}

/// Read a single byte at `idx`. `None` if the text is too
/// short. Mirrors C's `txt[idx]` access without the UB.
//
// Dead-code allowed: this helper is called by parser match arms
// that land in subsequent tasks. Remove when the first parser
// lands (it will call this from non-test code).
#[allow(dead_code)]
fn byte_at(text: &str, idx: usize) -> Option<u8> {
    text.as_bytes().get(idx).copied()
}

/// Extract a 4-char `ArrayString` starting at `start`. `None`
/// if the text is too short or the slice doesn't land on a
/// UTF-8 char boundary. ACARS payloads are 7-bit ASCII so the
/// boundary case is unreachable in practice but `text.get(..)`
/// returns `None` safely either way.
//
// Dead-code allowed: same rationale as `byte_at` above.
#[allow(dead_code)]
fn slice4(text: &str, start: usize) -> Option<ArrayString<4>> {
    text.get(start..start + 4)
        .and_then(|s| ArrayString::from(s).ok())
}

/// Decode the OOOI metadata for an ACARS message. Returns
/// `Some(Oooi)` when:
///
/// 1. The label has a parser (one of the 40 known cases), AND
/// 2. Validation passes (e.g. expected separator chars, prefix
///    strings), AND
/// 3. At least one field extracts (text long enough, slice
///    valid UTF-8 at byte boundary).
///
/// Returns `None` otherwise. Mirrors `DecodeLabel` in
/// `original/acarsdec/label.c` returning `0` (failed) or `1`
/// (succeeded).
#[must_use]
pub fn decode_label(label: [u8; 2], text: &str) -> Option<Oooi> {
    // Stub — populated as parsers land in subsequent tasks.
    let _ = (label, text);
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn oooi_default_is_all_none() {
        let o = Oooi::default();
        assert!(!o.has_any());
        assert!(o.sa.is_none());
        assert!(o.da.is_none());
        assert!(o.gout.is_none());
        assert!(o.woff.is_none());
        assert!(o.won.is_none());
        assert!(o.gin.is_none());
        assert!(o.eta.is_none());
    }

    #[test]
    fn oooi_has_any_true_when_any_field_set() {
        let cases = [
            Oooi {
                sa: Some(ArrayString::from("KORD").unwrap()),
                ..Default::default()
            },
            Oooi {
                da: Some(ArrayString::from("KSFO").unwrap()),
                ..Default::default()
            },
            Oooi {
                gout: Some(ArrayString::from("0830").unwrap()),
                ..Default::default()
            },
            Oooi {
                woff: Some(ArrayString::from("0945").unwrap()),
                ..Default::default()
            },
            Oooi {
                won: Some(ArrayString::from("1020").unwrap()),
                ..Default::default()
            },
            Oooi {
                gin: Some(ArrayString::from("1245").unwrap()),
                ..Default::default()
            },
            Oooi {
                eta: Some(ArrayString::from("0830").unwrap()),
                ..Default::default()
            },
        ];
        for o in &cases {
            assert!(o.has_any(), "has_any should be true for {o:?}");
        }
    }

    #[test]
    fn slice4_none_when_slice_starts_inside_multibyte_codepoint() {
        // U+00E9 (é) is 2 bytes in UTF-8: [0xC3, 0xA9]. A slice
        // starting at byte 1 lands inside the codepoint — text.get
        // returns None, which slice4 propagates. ACARS payloads are
        // 7-bit ASCII so this is unreachable in practice, but the
        // doc promises bounds-safety either way.
        let s = "\u{00E9}XYZW"; // bytes: 0xC3 0xA9 'X' 'Y' 'Z' 'W'
        assert!(slice4(s, 1).is_none());
    }

    #[test]
    fn slice4_returns_none_on_short_text() {
        assert!(slice4("KO", 0).is_none());
        assert!(slice4("KORD", 1).is_none()); // 1..5 needs 5 chars
    }

    #[test]
    fn slice4_extracts_four_chars() {
        let s = slice4("KORD0830", 0).unwrap();
        assert_eq!(s.as_str(), "KORD");
        let s = slice4("KORD0830", 4).unwrap();
        assert_eq!(s.as_str(), "0830");
    }

    #[test]
    fn byte_at_returns_none_on_short_text() {
        assert_eq!(byte_at("AB", 5), None);
    }

    #[test]
    fn byte_at_extracts_byte() {
        assert_eq!(byte_at("ABC", 1), Some(b'B'));
    }

    #[test]
    fn decode_label_unknown_returns_none() {
        assert!(decode_label([b'X', b'X'], "anything").is_none());
        assert!(decode_label([b'Z', b'Z'], "").is_none());
    }
}
