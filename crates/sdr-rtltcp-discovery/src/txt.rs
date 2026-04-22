//! TXT record schema for `_rtl_tcp._tcp.local.` advertisements.
//!
//! Keys are kept short (under 10 chars) because mDNS packs TXT entries
//! into a single DNS record limited to 400 bytes in practice. Under
//! that limit, clients see the record in one resolve without follow-up
//! queries.

use std::collections::HashMap;

use crate::error::DiscoveryError;

/// TXT record payload attached to a server advertisement. Each field
/// serializes to `key=value` in the mDNS TXT record; missing fields
/// are omitted entirely so older clients don't see empty-string junk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecord {
    /// Tuner family as reported by the dongle (e.g. "R820T", "E4000").
    /// Lets the client show "this is an R820T" without connecting.
    pub tuner: String,

    /// Advertiser version — our crate version. Clients can use this to
    /// show "running sdr-rs 0.x.y" vs. "unknown rtl_tcp source."
    pub version: String,

    /// Number of discrete gain steps the tuner exposes. The actual
    /// gain table is NOT in TXT — clients assume the R820T table for
    /// dB display or drive via set-gain-by-index and show step N of M.
    pub gains: u32,

    /// Human-readable nickname (user-editable). Defaults to the host's
    /// hostname on the server side; clients render this as the primary
    /// label in the discovered-servers list.
    pub nickname: String,

    /// Optional buffer-depth hint (bytes) for latency awareness.
    pub txbuf: Option<usize>,

    /// Optional codec bitmask (`sdr-server-rtltcp::CodecMask`'s raw
    /// wire byte) advertising which stream codecs the server is
    /// willing to negotiate in the extended `"RTLX"` handshake.
    /// `None` means "unknown" — older servers don't publish this
    /// key; clients should treat its absence as "legacy
    /// uncompressed only" and NOT send an extended hello (the
    /// hello corrupts vanilla command framing). Issue #307.
    ///
    /// Kept as a plain `u8` here so the discovery crate stays
    /// independent of the server crate; the caller converts to /
    /// from `CodecMask` at the boundary.
    pub codecs: Option<u8>,
}

impl TxtRecord {
    /// Maximum combined TXT byte count we'll emit. mDNS DNS records can
    /// be larger in theory, but staying under 400 bytes keeps the whole
    /// registration in a single UDP packet and avoids the "truncated,
    /// follow up with a query" path that some clients handle poorly.
    pub const MAX_TOTAL_BYTES: usize = 400;

    /// Render as an `mdns-sd` properties map. Omits `txbuf` when None
    /// so the field is simply absent rather than stored as `"txbuf="`.
    /// Returns `Err(InvalidTxt)` if any value contains a NUL byte or
    /// is too long for a single TXT entry (255 bytes).
    pub fn to_properties(&self) -> Result<HashMap<String, String>, DiscoveryError> {
        let mut m = HashMap::new();
        insert_checked(&mut m, "tuner", &self.tuner)?;
        insert_checked(&mut m, "version", &self.version)?;
        insert_checked(&mut m, "gains", &self.gains.to_string())?;
        insert_checked(&mut m, "nickname", &self.nickname)?;
        if let Some(n) = self.txbuf {
            insert_checked(&mut m, "txbuf", &n.to_string())?;
        }
        if let Some(c) = self.codecs {
            insert_checked(&mut m, "codecs", &c.to_string())?;
        }
        let total: usize = m.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
        if total > Self::MAX_TOTAL_BYTES {
            return Err(DiscoveryError::InvalidTxt(format!(
                "TXT record total {total} bytes exceeds {} byte cap",
                Self::MAX_TOTAL_BYTES
            )));
        }
        Ok(m)
    }

    /// Parse an mDNS properties slice back into a `TxtRecord`. Missing
    /// fields get sensible defaults ("unknown" / 0) so a partial /
    /// corrupt advertisement still renders instead of dropping the
    /// server entry.
    pub fn from_properties<I, K, V>(properties: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut tuner = String::from("unknown");
        let mut version = String::from("unknown");
        let mut gains: u32 = 0;
        let mut nickname = String::new();
        let mut txbuf: Option<usize> = None;
        let mut codecs: Option<u8> = None;
        for (k, v) in properties {
            match k.as_ref() {
                "tuner" => tuner = v.as_ref().to_string(),
                "version" => version = v.as_ref().to_string(),
                "gains" => gains = v.as_ref().parse().unwrap_or(0),
                "nickname" => nickname = v.as_ref().to_string(),
                "txbuf" => txbuf = v.as_ref().parse().ok(),
                "codecs" => codecs = v.as_ref().parse().ok(),
                _ => {
                    tracing::trace!(
                        key = %k.as_ref(),
                        "unknown rtl_tcp TXT key, ignoring"
                    );
                }
            }
        }
        Self {
            tuner,
            version,
            gains,
            nickname,
            txbuf,
            codecs,
        }
    }
}

/// TXT entry checker — rejects NUL bytes (mDNS doesn't allow them in
/// keys OR values) and anything over 255 bytes for a single entry.
fn insert_checked(
    m: &mut HashMap<String, String>,
    key: &str,
    value: &str,
) -> Result<(), DiscoveryError> {
    if key.contains('\0') || value.contains('\0') {
        return Err(DiscoveryError::InvalidTxt(format!(
            "key or value for `{key}` contains NUL byte"
        )));
    }
    let entry_len = key.len() + value.len() + 1; // +1 for the `=`
    if entry_len > 255 {
        return Err(DiscoveryError::InvalidTxt(format!(
            "`{key}` entry is {entry_len} bytes, exceeds 255 byte cap"
        )));
    }
    m.insert(key.to_string(), value.to_string());
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> TxtRecord {
        TxtRecord {
            tuner: "R820T".into(),
            version: "0.1.0".into(),
            gains: 29,
            nickname: "home-scanner".into(),
            txbuf: Some(128 * 1024),
            // CodecMask::NONE_AND_LZ4 raw byte — stable wire value
            // avoided referencing the server crate here to keep the
            // discovery crate's test independent.
            codecs: Some(0b11),
        }
    }

    #[test]
    fn to_properties_includes_all_fields() {
        let props = sample().to_properties().unwrap();
        assert_eq!(props.get("tuner").map(String::as_str), Some("R820T"));
        assert_eq!(props.get("version").map(String::as_str), Some("0.1.0"));
        assert_eq!(props.get("gains").map(String::as_str), Some("29"));
        assert_eq!(
            props.get("nickname").map(String::as_str),
            Some("home-scanner")
        );
        assert_eq!(props.get("txbuf").map(String::as_str), Some("131072"));
        assert_eq!(props.get("codecs").map(String::as_str), Some("3"));
    }

    #[test]
    fn to_properties_omits_missing_txbuf() {
        let mut r = sample();
        r.txbuf = None;
        let props = r.to_properties().unwrap();
        assert!(!props.contains_key("txbuf"));
    }

    #[test]
    fn to_properties_omits_missing_codecs() {
        // An older server that doesn't advertise a codec mask must
        // not emit an empty `codecs=` entry — clients interpret
        // absence as "legacy only" per #307.
        let mut r = sample();
        r.codecs = None;
        let props = r.to_properties().unwrap();
        assert!(!props.contains_key("codecs"));
    }

    #[test]
    fn from_properties_fills_defaults_for_missing_fields() {
        let r = TxtRecord::from_properties(std::iter::empty::<(&str, &str)>());
        assert_eq!(r.tuner, "unknown");
        assert_eq!(r.version, "unknown");
        assert_eq!(r.gains, 0);
        assert_eq!(r.nickname, "");
        assert_eq!(r.txbuf, None);
        assert_eq!(r.codecs, None);
    }

    #[test]
    fn from_properties_parses_codecs() {
        let r = TxtRecord::from_properties([("codecs", "3")]);
        assert_eq!(r.codecs, Some(3));
    }

    #[test]
    fn from_properties_tolerates_garbage_codecs() {
        // Non-numeric `codecs` value falls back to None (unknown)
        // rather than rejecting the whole record — same safety
        // pattern as `gains`.
        let r = TxtRecord::from_properties([("codecs", "not-a-number")]);
        assert_eq!(r.codecs, None);
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let original = sample();
        let props = original.to_properties().unwrap();
        let roundtripped =
            TxtRecord::from_properties(props.iter().map(|(k, v)| (k.clone(), v.clone())));
        assert_eq!(original, roundtripped);
    }

    #[test]
    fn rejects_nul_bytes_in_values() {
        let mut r = sample();
        r.nickname = "has\0nul".into();
        assert!(matches!(
            r.to_properties(),
            Err(DiscoveryError::InvalidTxt(_))
        ));
    }

    #[test]
    fn rejects_oversized_single_entry() {
        // Single entry above 255 bytes should fail. Nickname is the
        // user-controlled one most likely to be pathological.
        let mut r = sample();
        r.nickname = "x".repeat(300);
        assert!(matches!(
            r.to_properties(),
            Err(DiscoveryError::InvalidTxt(_))
        ));
    }

    #[test]
    fn from_properties_ignores_unknown_keys() {
        // A server running a future schema that added a `compression`
        // key shouldn't break our client — we just ignore it.
        let r = TxtRecord::from_properties([
            ("tuner", "R820T"),
            ("version", "2.0.0"),
            ("compression", "zstd"),
            ("future_field", "surprise"),
        ]);
        assert_eq!(r.tuner, "R820T");
        assert_eq!(r.version, "2.0.0");
    }

    #[test]
    fn gains_parse_failure_becomes_zero_not_error() {
        // Corrupt / non-numeric `gains` shouldn't prevent the server
        // from showing up in the list — just render "0 gain steps."
        let r = TxtRecord::from_properties([("gains", "not-a-number")]);
        assert_eq!(r.gains, 0);
    }
}
