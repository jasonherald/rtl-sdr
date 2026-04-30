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

fn label_10(text: &str) -> Option<Oooi> {
    // C: prefix "ARR01"; then da(12), eta(16).
    if !text.starts_with("ARR01") {
        return None;
    }
    let o = Oooi {
        da: slice4(text, 12),
        eta: slice4(text, 16),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_11(text: &str) -> Option<Oooi> {
    // C: txt[13..17] == "/DS "; then da(17). txt[21..26] ==
    // "/ETA "; then eta(26).
    if text.get(13..17) != Some("/DS ") {
        return None;
    }
    let da = slice4(text, 17);
    if text.get(21..26) != Some("/ETA ") {
        return None;
    }
    let eta = slice4(text, 26);
    let o = Oooi {
        da,
        eta,
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_12(text: &str) -> Option<Oooi> {
    // C: txt[4]==','; then sa(0), da(5).
    if byte_at(text, 4) != Some(b',') {
        return None;
    }
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 5),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_15(text: &str) -> Option<Oooi> {
    // C: prefix "FST01"; then sa(5), da(9).
    if !text.starts_with("FST01") {
        return None;
    }
    let o = Oooi {
        sa: slice4(text, 5),
        da: slice4(text, 9),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_17(text: &str) -> Option<Oooi> {
    // C: prefix "ETA "; then eta(4). txt[8]==',' → sa(9).
    // txt[13]==',' → da(14).
    if !text.starts_with("ETA ") {
        return None;
    }
    let eta = slice4(text, 4);
    if byte_at(text, 8) != Some(b',') {
        return None;
    }
    let sa = slice4(text, 9);
    if byte_at(text, 13) != Some(b',') {
        return None;
    }
    let da = slice4(text, 14);
    let o = Oooi {
        sa,
        da,
        eta,
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_1g(text: &str) -> Option<Oooi> {
    // C: txt[4]==','; then sa(0), da(5).
    if byte_at(text, 4) != Some(b',') {
        return None;
    }
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 5),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_20(text: &str) -> Option<Oooi> {
    // C: prefix "RST"; then sa(22), da(26).
    if !text.starts_with("RST") {
        return None;
    }
    let o = Oooi {
        sa: slice4(text, 22),
        da: slice4(text, 26),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_21(text: &str) -> Option<Oooi> {
    // C: txt[6]==',' → sa(7); txt[11]==',' → da(12).
    if byte_at(text, 6) != Some(b',') {
        return None;
    }
    let sa = slice4(text, 7);
    if byte_at(text, 11) != Some(b',') {
        return None;
    }
    let da = slice4(text, 12);
    let o = Oooi {
        sa,
        da,
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_26(text: &str) -> Option<Oooi> {
    // C: prefix "VER/077"; find first '\n'; check "SCH/"; find
    // next '/'; sa(p+1), da(p+6); find next '\n'; check "ETA/";
    // eta(p+4). Each "find" failing past the SCH point still
    // returns 1 with sa/da populated.
    if !text.starts_with("VER/077") {
        return None;
    }
    let nl1 = text.find('\n')?;
    let after_nl1 = &text[nl1 + 1..];
    if !after_nl1.starts_with("SCH/") {
        return None;
    }
    // Walk past "SCH/" (4 chars) and find the next '/'.
    let after_sch = &after_nl1[4..];
    let slash_off = after_sch.find('/')?;
    let after_slash = &after_sch[slash_off + 1..];
    let sa = slice4(after_slash, 0);
    let da = slice4(after_slash, 5);
    // Look for an optional "\nETA/...". Absence means we still
    // succeed with sa/da populated.
    let o = if let Some(nl2) = after_slash.find('\n') {
        let after_nl2 = &after_slash[nl2 + 1..];
        if after_nl2.starts_with("ETA/") {
            let eta = slice4(after_nl2, 4);
            Oooi {
                sa,
                da,
                eta,
                ..Oooi::default()
            }
        } else {
            // C: returns 0 if "\n" present but next line isn't
            // "ETA/". Mirror that.
            return None;
        }
    } else {
        Oooi {
            sa,
            da,
            ..Oooi::default()
        }
    };
    o.has_any().then_some(o)
}

fn label_2n(text: &str) -> Option<Oooi> {
    // C: prefix "TKO01"; then txt[11]=='/' → sa(20), da(24).
    if !text.starts_with("TKO01") {
        return None;
    }
    if byte_at(text, 11) != Some(b'/') {
        return None;
    }
    let o = Oooi {
        sa: slice4(text, 20),
        da: slice4(text, 24),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_2z(text: &str) -> Option<Oooi> {
    // C: da(0)
    let o = Oooi {
        da: slice4(text, 0),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_33(text: &str) -> Option<Oooi> {
    // C: txt[0]==',' && txt[20]==',' → sa(21); txt[25]==',' → da(26).
    if byte_at(text, 0) != Some(b',') {
        return None;
    }
    if byte_at(text, 20) != Some(b',') {
        return None;
    }
    let sa = slice4(text, 21);
    if byte_at(text, 25) != Some(b',') {
        return None;
    }
    let da = slice4(text, 26);
    let o = Oooi {
        sa,
        da,
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_39(text: &str) -> Option<Oooi> {
    // C: prefix "GTA01"; then txt[15]=='/' → sa(24), da(28).
    if !text.starts_with("GTA01") {
        return None;
    }
    if byte_at(text, 15) != Some(b'/') {
        return None;
    }
    let o = Oooi {
        sa: slice4(text, 24),
        da: slice4(text, 28),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_44(text: &str) -> Option<Oooi> {
    // C: optional "00" prefix shifts the slice base by 2;
    // then prefix in {"POS0", "ETA0"}; txt[4] in {'2','3'};
    // txt[23..29..33..38..43..]: separator commas; eta is
    // overwritten — first at offset 29, then again at 44 (the
    // C source assigns oooi->eta twice; last write wins).
    let base = text.strip_prefix("00").unwrap_or(text);
    if !base.starts_with("POS0") && !base.starts_with("ETA0") {
        return None;
    }
    let kind_byte = byte_at(base, 4);
    if kind_byte != Some(b'2') && kind_byte != Some(b'3') {
        return None;
    }
    if byte_at(base, 23) != Some(b',') {
        return None;
    }
    let da = slice4(base, 24);
    if byte_at(base, 28) != Some(b',') {
        return None;
    }
    // First eta extraction. Will be overwritten below if the
    // remaining separators match (mirrors C's double-assign).
    #[allow(unused_assignments)]
    let mut eta = slice4(base, 29);
    if byte_at(base, 33) != Some(b',') {
        return None;
    }
    if byte_at(base, 38) != Some(b',') {
        return None;
    }
    if byte_at(base, 43) != Some(b',') {
        return None;
    }
    eta = slice4(base, 44);
    let o = Oooi {
        da,
        eta,
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_45(text: &str) -> Option<Oooi> {
    // C: txt[0]=='A' → da(1).
    if byte_at(text, 0) != Some(b'A') {
        return None;
    }
    let o = Oooi {
        da: slice4(text, 1),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_80(text: &str) -> Option<Oooi> {
    // C: memcmp(&txt[6], "/DEST/", 5) → only first 5 chars
    // compared, so check `text[6..11] == "/DEST"`. Then da(12).
    if text.get(6..11) != Some("/DEST") {
        return None;
    }
    let o = Oooi {
        da: slice4(text, 12),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_83(text: &str) -> Option<Oooi> {
    // C: txt[4]==',' → sa(0), da(5).
    if byte_at(text, 4) != Some(b',') {
        return None;
    }
    let o = Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 5),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_8d(text: &str) -> Option<Oooi> {
    // C: txt[4]==',' && txt[35]==',' → sa(36); txt[40]==',' →
    // da(41).
    if byte_at(text, 4) != Some(b',') {
        return None;
    }
    if byte_at(text, 35) != Some(b',') {
        return None;
    }
    let sa = slice4(text, 36);
    if byte_at(text, 40) != Some(b',') {
        return None;
    }
    let da = slice4(text, 41);
    let o = Oooi {
        sa,
        da,
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_8e(text: &str) -> Option<Oooi> {
    // C: txt[4]==',' → da(0), eta(5).
    if byte_at(text, 4) != Some(b',') {
        return None;
    }
    let o = Oooi {
        da: slice4(text, 0),
        eta: slice4(text, 5),
        ..Oooi::default()
    };
    o.has_any().then_some(o)
}

fn label_8s(text: &str) -> Option<Oooi> {
    // C: txt[4]==',' → da(0), eta(5). Same body as label_8e.
    if byte_at(text, 4) != Some(b',') {
        return None;
    }
    let o = Oooi {
        da: slice4(text, 0),
        eta: slice4(text, 5),
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
        b'1' => match label[1] {
            b'0' => label_10(text),
            b'1' => label_11(text),
            b'2' => label_12(text),
            b'5' => label_15(text),
            b'7' => label_17(text),
            b'G' => label_1g(text),
            _ => None,
        },
        b'2' => match label[1] {
            b'0' => label_20(text),
            b'1' => label_21(text),
            b'6' => label_26(text),
            b'N' => label_2n(text),
            b'Z' => label_2z(text),
            _ => None,
        },
        b'3' => match label[1] {
            b'3' => label_33(text),
            b'9' => label_39(text),
            _ => None,
        },
        b'4' => match label[1] {
            b'4' => label_44(text),
            b'5' => label_45(text),
            _ => None,
        },
        b'8' => match label[1] {
            b'0' => label_80(text),
            b'3' => label_83(text),
            b'D' => label_8d(text),
            b'E' => label_8e(text),
            b'S' => label_8s(text),
            _ => None,
        },
        b'R' => match label[1] {
            b'B' => label_26(text),
            _ => None,
        },
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

    #[test]
    fn label_10_extracts_da_and_eta() {
        // Offsets: prefix "ARR01" (0..5), skip 5..12, da(12..16),
        // eta(16..20).
        let txt = "ARR01_______KSFO0830";
        let o = decode_label([b'1', b'0'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_10_no_arr01_prefix_returns_none() {
        assert!(decode_label([b'1', b'0'], "XXX01_______KSFO0830").is_none());
    }

    #[test]
    fn label_11_extracts_da_and_eta() {
        // Offsets: skip 0..13, "/DS " at 13..17, da(17..21),
        // "/ETA " at 21..26, eta(26..30).
        let txt = "_____________/DS KSFO/ETA 0830";
        let o = decode_label([b'1', b'1'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_11_missing_separator_returns_none() {
        assert!(
            decode_label([b'1', b'1'], "_____________/DS KSFO/XXX 0830").is_none(),
            "missing /ETA separator"
        );
    }

    #[test]
    fn label_12_extracts_sa_and_da_with_comma_at_offset_4() {
        let txt = "KORD,KSFO";
        let o = decode_label([b'1', b'2'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_12_no_comma_returns_none() {
        assert!(decode_label([b'1', b'2'], "KORD-KSFO").is_none());
    }

    #[test]
    fn label_15_extracts_sa_and_da_after_fst01_prefix() {
        // Offsets: "FST01" 0..5, sa(5..9), da(9..13).
        let txt = "FST01KORDKSFO";
        let o = decode_label([b'1', b'5'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_17_extracts_eta_sa_da_with_eta_prefix_and_commas() {
        // Offsets: "ETA " 0..4, eta(4..8), comma(8), sa(9..13),
        // comma(13), da(14..18).
        let txt = "ETA 0830,KORD,KSFO";
        let o = decode_label([b'1', b'7'], txt).unwrap();
        assert_eq!(o.eta.as_deref(), Some("0830"));
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_1g_extracts_sa_and_da_with_comma_at_offset_4() {
        let txt = "KORD,KSFO";
        let o = decode_label([b'1', b'G'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_20_extracts_sa_da_after_rst_prefix() {
        // Offsets: "RST" 0..3, skip 3..22, sa(22..26), da(26..30).
        let txt = "RST___________________KORDKSFO";
        let o = decode_label([b'2', b'0'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_20_no_rst_prefix_returns_none() {
        assert!(decode_label([b'2', b'0'], "XXX___________________KORDKSFO").is_none());
    }

    #[test]
    fn label_21_extracts_sa_da_with_commas() {
        // Offsets: skip 0..6, comma(6), sa(7..11), comma(11), da(12..16).
        let txt = "______,KORD,KSFO";
        let o = decode_label([b'2', b'1'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_26_extracts_sa_da_eta_through_multiline_walk() {
        // Layout:
        //   line 1: VER/077 ...
        //   line 2: SCH/<anything>/<sa><skip><da><...>
        //   line 3: ETA/<eta>
        let txt = "VER/077\nSCH/X/KORD KSFO\nETA/0830";
        let o = decode_label([b'2', b'6'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_26_without_eta_line_still_succeeds() {
        // No third line — sa/da only.
        let txt = "VER/077\nSCH/X/KORD KSFO";
        let o = decode_label([b'2', b'6'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert!(o.eta.is_none());
    }

    #[test]
    fn label_26_no_ver_077_prefix_returns_none() {
        assert!(decode_label([b'2', b'6'], "VER/078\nSCH/X/KORD KSFO").is_none());
    }

    #[test]
    fn label_26_second_line_not_eta_returns_none() {
        // Per C source (label.c:194-200): if a second \n is
        // present but the line doesn't start with "ETA/", the
        // parser returns 0. Pin that behavior so future edits
        // don't silently relax it to "succeed with sa/da only".
        let txt = "VER/077\nSCH/X/KORD KSFO\nXXX/0830";
        assert!(decode_label([b'2', b'6'], txt).is_none());
    }

    #[test]
    fn label_rb_aliases_label_26() {
        // Same fixture as label_26's "all three lines" test.
        let txt = "VER/077\nSCH/X/KORD KSFO\nETA/0830";
        let o = decode_label([b'R', b'B'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_2n_extracts_sa_da_after_tko01() {
        // Offsets: "TKO01" 0..5, skip 5..11, '/' at 11, skip
        // 12..20, sa(20..24), da(24..28).
        let txt = "TKO01______/________KORDKSFO";
        let o = decode_label([b'2', b'N'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_2z_extracts_da_only() {
        let txt = "KSFO";
        let o = decode_label([b'2', b'Z'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert!(o.sa.is_none());
    }

    #[test]
    fn label_33_extracts_sa_da_with_three_commas() {
        // Offsets: comma(0), skip 1..20, comma(20), sa(21..25),
        // comma(25), da(26..30).
        let txt = ",___________________,KORD,KSFO";
        let o = decode_label([b'3', b'3'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_39_extracts_sa_da_after_gta01_with_slash() {
        // Offsets: "GTA01" 0..5, skip 5..15, '/' at 15, skip
        // 16..24, sa(24..28), da(28..32).
        let txt = "GTA01__________/________KORDKSFO";
        let o = decode_label([b'3', b'9'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_44_extracts_da_and_eta_with_pos02_prefix() {
        // Layout (after no "00" shift):
        //   POS0 (0..4)
        //   '2'  (4)
        //   skip 5..23
        //   ',' (23)
        //   da   (24..28)
        //   ','  (28)
        //   eta1 (29..33) — overwritten
        //   ','  (33)
        //   skip 34..38
        //   ','  (38)
        //   skip 39..43
        //   ','  (43)
        //   eta2 (44..48) — final
        let txt = "POS02__________________,KSFO,XXXX,____,____,0830";
        let o = decode_label([b'4', b'4'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_44_with_00_prefix_shifts_base() {
        let txt = "00POS02__________________,KSFO,XXXX,____,____,0830";
        let o = decode_label([b'4', b'4'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_44_unsupported_kind_byte_returns_none() {
        // txt[4] must be '2' or '3'; '5' rejected.
        assert!(
            decode_label(
                [b'4', b'4'],
                "POS05__________________,KSFO,XXXX,____,____,0830"
            )
            .is_none()
        );
    }

    #[test]
    fn label_45_extracts_da_after_a_prefix() {
        let txt = "AKSFO";
        let o = decode_label([b'4', b'5'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_45_no_a_prefix_returns_none() {
        assert!(decode_label([b'4', b'5'], "BKSFO").is_none());
    }

    #[test]
    fn label_80_extracts_da_after_dest_prefix() {
        // Offsets: skip 0..6, "/DEST" 6..11, skip 11, da(12..16).
        // Note: C compares 5 bytes against "/DEST/" so only
        // "/DEST" matters; the trailing slash is irrelevant.
        let txt = "______/DEST_KSFO";
        let o = decode_label([b'8', b'0'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_80_no_dest_prefix_returns_none() {
        assert!(decode_label([b'8', b'0'], "______/SRCE_KSFO").is_none());
    }

    #[test]
    fn label_83_extracts_sa_and_da_with_comma() {
        let txt = "KORD,KSFO";
        let o = decode_label([b'8', b'3'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_8d_extracts_sa_da_with_three_commas() {
        // Offsets: skip 0..4, ',' at 4, skip 5..35, ',' at 35,
        // sa(36..40), ',' at 40, da(41..45).
        let txt = "____,______________________________,KORD,KSFO";
        let o = decode_label([b'8', b'D'], txt).unwrap();
        assert_eq!(o.sa.as_deref(), Some("KORD"));
        assert_eq!(o.da.as_deref(), Some("KSFO"));
    }

    #[test]
    fn label_8e_extracts_da_and_eta_with_comma() {
        // Offsets: da(0..4), ',' at 4, eta(5..9).
        let txt = "KSFO,0830";
        let o = decode_label([b'8', b'E'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn label_8s_extracts_da_and_eta_with_comma() {
        let txt = "KSFO,0830";
        let o = decode_label([b'8', b'S'], txt).unwrap();
        assert_eq!(o.da.as_deref(), Some("KSFO"));
        assert_eq!(o.eta.as_deref(), Some("0830"));
    }

    #[test]
    fn eight_family_unknown_second_char_returns_none() {
        assert!(decode_label([b'8', b'1'], "anything").is_none());
        assert!(decode_label([b'8', b'X'], "anything").is_none());
    }
}
