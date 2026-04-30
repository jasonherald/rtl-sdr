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
// byte_at is called from tests and will be exercised by
// non-Q parsers in subsequent tasks (label.c uses index-based
// char checks in several label families). Keep the allow until
// those parsers land.
#[allow(dead_code)]
fn byte_at(text: &str, idx: usize) -> Option<u8> {
    text.as_bytes().get(idx).copied()
}

/// Extract a 4-char `ArrayString` starting at `start`. `None`
/// if the text is too short or the slice doesn't land on a
/// UTF-8 char boundary. ACARS payloads are 7-bit ASCII so the
/// boundary case is unreachable in practice but `text.get(..)`
/// returns `None` safely either way.
fn slice4(text: &str, start: usize) -> Option<ArrayString<4>> {
    text.get(start..start + 4)
        .and_then(|s| ArrayString::from(s).ok())
}

fn label_q1(text: &str) -> Option<Oooi> {
    // C: sa(0), gout(4), woff(8), won(12), gin(16), da(24)
    let o = Oooi {
        sa: slice4(text, 0),
        gout: slice4(text, 4),
        woff: slice4(text, 8),
        won: slice4(text, 12),
        gin: slice4(text, 16),
        da: slice4(text, 24),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_q2(text: &str) -> Option<Oooi> {
    // C: sa(0), eta(4)
    let o = Oooi {
        sa: slice4(text, 0),
        eta: slice4(text, 4),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qa(text: &str) -> Option<Oooi> {
    // C: sa(0), gout(4)
    let o = Oooi {
        sa: slice4(text, 0),
        gout: slice4(text, 4),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qb(text: &str) -> Option<Oooi> {
    // C: sa(0), woff(4)
    let o = Oooi {
        sa: slice4(text, 0),
        woff: slice4(text, 4),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qc(text: &str) -> Option<Oooi> {
    // C: sa(0), won(4)
    let o = Oooi {
        sa: slice4(text, 0),
        won: slice4(text, 4),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qd(text: &str) -> Option<Oooi> {
    // C: sa(0), gin(4)
    let o = Oooi {
        sa: slice4(text, 0),
        gin: slice4(text, 4),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qe(text: &str) -> Option<Oooi> {
    // C: sa(0), gout(4), da(8)
    let o = Oooi {
        sa: slice4(text, 0),
        gout: slice4(text, 4),
        da: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qf(text: &str) -> Option<Oooi> {
    // C: sa(0), woff(4), da(8)
    let o = Oooi {
        sa: slice4(text, 0),
        woff: slice4(text, 4),
        da: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qg(text: &str) -> Option<Oooi> {
    // C: sa(0), gout(4), gin(8)
    let o = Oooi {
        sa: slice4(text, 0),
        gout: slice4(text, 4),
        gin: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qh(text: &str) -> Option<Oooi> {
    // C: sa(0), gout(4)
    let o = Oooi {
        sa: slice4(text, 0),
        gout: slice4(text, 4),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qk(text: &str) -> Option<Oooi> {
    // C: sa(0), won(4), da(8)
    let o = Oooi {
        sa: slice4(text, 0),
        won: slice4(text, 4),
        da: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_ql(text: &str) -> Option<Oooi> {
    // C: da(0), gin(8), sa(13).
    // Note: skips bytes 4..8 (some separator) and byte 12.
    let o = Oooi {
        da: slice4(text, 0),
        gin: slice4(text, 8),
        sa: slice4(text, 13),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qm(text: &str) -> Option<Oooi> {
    // C: da(0), sa(8). Skips bytes 4..8.
    let o = Oooi {
        da: slice4(text, 0),
        sa: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qn(text: &str) -> Option<Oooi> {
    // C: da(4), eta(8). Skips bytes 0..4.
    let o = Oooi {
        da: slice4(text, 4),
        eta: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qp(text: &str) -> Option<Oooi> {
    // C: sa(0), da(4), gout(8)
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 4),
        gout: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qq(text: &str) -> Option<Oooi> {
    // C: sa(0), da(4), woff(8)
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 4),
        woff: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qr(text: &str) -> Option<Oooi> {
    // C: sa(0), da(4), won(8)
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 4),
        won: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qs(text: &str) -> Option<Oooi> {
    // C: sa(0), da(4), gin(8)
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 4),
        gin: slice4(text, 8),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_qt(text: &str) -> Option<Oooi> {
    // C: sa(0), da(4), gout(8), gin(12)
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 4),
        gout: slice4(text, 8),
        gin: slice4(text, 12),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
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
    match label[0] {
        b'Q' => match label[1] {
            b'1' => label_q1(text),
            b'2' => label_q2(text),
            b'A' => label_qa(text),
            b'B' => label_qb(text),
            b'C' => label_qc(text),
            b'D' => label_qd(text),
            b'E' => label_qe(text),
            b'F' => label_qf(text),
            b'G' => label_qg(text),
            b'H' => label_qh(text),
            b'K' => label_qk(text),
            b'L' => label_ql(text),
            b'M' => label_qm(text),
            b'N' => label_qn(text),
            b'P' => label_qp(text),
            b'Q' => label_qq(text),
            b'R' => label_qr(text),
            b'S' => label_qs(text),
            b'T' => label_qt(text),
            _ => None,
        },
        _ => None,
    }
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

    #[test]
    fn label_q1_extracts_six_fields() {
        // Offsets: sa(0..4) gout(4..8) woff(8..12) won(12..16)
        //          gin(16..20) skip(20..24) da(24..28)
        let txt = "KORD08300945102012450000KSFO";
        let o = decode_label([b'Q', b'1'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.gout.as_deref(), Some("0830"));
        assert_eq!(o.woff.as_deref(), Some("0945"));
        assert_eq!(o.won.as_deref(), Some("1020"));
        assert_eq!(o.gin.as_deref(), Some("1245"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert!(o.eta.is_none());
    }

    #[test]
    fn label_q2_extracts_sa_and_eta() {
        let txt = "KORD0830";
        let o = decode_label([b'Q', b'2'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_qa_extracts_sa_and_gout() {
        let txt = "KORD0830";
        let o = decode_label([b'Q', b'A'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.gout.as_deref(), Some("0830"));
    }

    #[test]
    fn label_qb_extracts_sa_and_woff() {
        let txt = "KORD0945";
        let o = decode_label([b'Q', b'B'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.woff.as_deref(), Some("0945"));
    }

    #[test]
    fn label_qc_extracts_sa_and_won() {
        let txt = "KORD1020";
        let o = decode_label([b'Q', b'C'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.won.as_deref(), Some("1020"));
    }

    #[test]
    fn label_qd_extracts_sa_and_gin() {
        let txt = "KORD1245";
        let o = decode_label([b'Q', b'D'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.gin.as_deref(), Some("1245"));
    }

    #[test]
    fn label_qe_extracts_sa_gout_da() {
        let txt = "KORD0830KSFO";
        let o = decode_label([b'Q', b'E'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.gout.as_deref(), Some("0830"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_qf_extracts_sa_woff_da() {
        let txt = "KORD0945KSFO";
        let o = decode_label([b'Q', b'F'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.woff.as_deref(), Some("0945"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_qg_extracts_sa_gout_gin() {
        let txt = "KORD08301245";
        let o = decode_label([b'Q', b'G'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.gout.as_deref(), Some("0830"));
        assert_eq!(o.gin.as_deref(), Some("1245"));
    }

    #[test]
    fn label_qh_extracts_sa_and_gout() {
        let txt = "KORD0830";
        let o = decode_label([b'Q', b'H'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.gout.as_deref(), Some("0830"));
    }

    #[test]
    fn q_family_short_text_returns_none() {
        // All Q-family parsers should bail when text is too short
        // for even the first slice4.
        for second in [b'1', b'2', b'A', b'B', b'C', b'D', b'E', b'F', b'G', b'H'] {
            assert!(
                decode_label([b'Q', second], "AB").is_none(),
                "Q{} should be None for short text",
                second as char
            );
        }
    }

    #[test]
    fn label_qk_extracts_sa_won_da() {
        let txt = "KORD1020KSFO";
        let o = decode_label([b'Q', b'K'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.won.as_deref(), Some("1020"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_ql_extracts_da_gin_sa() {
        // Offsets: da(0..4) skip(4..8) gin(8..12) skip(12) sa(13..17)
        let txt = "KSFO____1245_KORD";
        let o = decode_label([b'Q', b'L'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.gin.as_deref(), Some("1245"));
        assert_eq!(o.sa.as_deref(), Some("KORD"));
    }

    #[test]
    fn label_qm_extracts_da_and_sa() {
        // Offsets: da(0..4) skip(4..8) sa(8..12)
        let txt = "KSFO____KORD";
        let o = decode_label([b'Q', b'M'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.sa.as_deref(), Some("KORD"));
    }

    #[test]
    fn label_qn_extracts_da_and_eta() {
        // Offsets: skip(0..4) da(4..8) eta(8..12)
        let txt = "____KSFO0830";
        let o = decode_label([b'Q', b'N'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_qp_extracts_sa_da_gout() {
        let txt = "KORDKSFO0830";
        let o = decode_label([b'Q', b'P'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.gout.as_deref(), Some("0830"));
    }

    #[test]
    fn label_qq_extracts_sa_da_woff() {
        let txt = "KORDKSFO0945";
        let o = decode_label([b'Q', b'Q'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.woff.as_deref(), Some("0945"));
    }

    #[test]
    fn label_qr_extracts_sa_da_won() {
        let txt = "KORDKSFO1020";
        let o = decode_label([b'Q', b'R'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.won.as_deref(), Some("1020"));
    }

    #[test]
    fn label_qs_extracts_sa_da_gin() {
        let txt = "KORDKSFO1245";
        let o = decode_label([b'Q', b'S'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.gin.as_deref(), Some("1245"));
    }

    #[test]
    fn label_qt_extracts_sa_da_gout_gin() {
        let txt = "KORDKSFO08301245";
        let o = decode_label([b'Q', b'T'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.gout.as_deref(), Some("0830"));
        assert_eq!(o.gin.as_deref(), Some("1245"));
    }

    #[test]
    fn q_unknown_second_char_returns_none() {
        // Q4 / QI / QJ / QU etc. don't have parsers.
        assert!(decode_label([b'Q', b'4'], "anything").is_none());
        assert!(decode_label([b'Q', b'I'], "anything").is_none());
        assert!(decode_label([b'Q', b'J'], "anything").is_none());
        assert!(decode_label([b'Q', b'U'], "anything").is_none());
    }
}
