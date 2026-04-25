//! US-ZIP-code → ground-station coordinates lookup.
//!
//! The satellites scheduler needs a latitude/longitude for the user's
//! ground station. Asking them to type one in by hand is fiddly; asking
//! the OS for a "current location" is a rabbit hole (`GeoClue2` needs a
//! flatpak/snap allowlist to work for most users, Windows location
//! services are coarse, and we'd be cross-platform on day one). A ZIP
//! code is something everyone has memorised, gives lat/lon accurate to
//! ~1 km — way better than IP geolocation — and dodges per-platform
//! permission UIs entirely.
//!
//! We hit the public, no-auth Zippopotam.us endpoint. It serves a flat
//! JSON document keyed by country + postal code; the US dataset is
//! complete and well-maintained. International ZIP support is
//! intentionally out of scope here — non-US users can type lat/lon by
//! hand. (A `country_code` parameter could be threaded through later
//! without changing the [`PostalLocation`] return type.)
//!
//! Like [`crate::tle_cache::TleCache::tle_text`], the HTTP fetch is
//! **blocking**. The scheduler UI calls this from a worker thread via
//! `gio::spawn_blocking`.

use std::sync::OnceLock;
use std::time::Duration;

/// Default timeout for the Zippopotam.us round trip. Generous enough
/// that a sluggish coffee-shop link still resolves, short enough that
/// a network outage doesn't block the worker thread for a minute.
pub const DEFAULT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Cached HTTP client — built on first call so consecutive lookups
/// reuse the same TLS session + connection pool. `OnceLock` mirrors
/// the [`crate::tle_cache::TleCache`] client cache; same rationale.
static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

/// Resolved ground-station location from a postal-code lookup.
#[derive(Debug, Clone)]
pub struct PostalLocation {
    /// Latitude in decimal degrees, north-positive.
    pub lat_deg: f64,
    /// Longitude in decimal degrees, east-positive.
    pub lon_deg: f64,
    /// Place name from the upstream record — typically the largest
    /// settlement in the postal code. Shown back to the user in a
    /// status row so they can sanity-check the lookup.
    pub place: String,
    /// State / region abbreviation when available (e.g. `"VA"`).
    /// Empty string if upstream omitted it.
    pub region: String,
}

/// Errors from a postal-code lookup.
#[derive(Debug, thiserror::Error)]
pub enum PostalLookupError {
    /// Empty / non-numeric / wrong-length input. Caught client-side
    /// rather than burning a request on something Zippopotam.us would
    /// 404 anyway.
    #[error("invalid US ZIP code: {0:?} (expected 5 digits)")]
    InvalidZip(String),
    /// Upstream returned a non-2xx status, the request timed out, or
    /// reqwest itself failed to send.
    #[error("postal lookup HTTP error: {0}")]
    Http(String),
    /// Body wasn't JSON or didn't have the fields we expected.
    /// Zippopotam.us returns `{}` for ZIPs it doesn't know — the empty
    /// `places` array surfaces here as `NotFound`.
    #[error("postal lookup parse error: {0}")]
    Parse(String),
    /// JSON parsed but no place records — ZIP simply isn't in the
    /// upstream dataset.
    #[error("ZIP {0} not found in postal database")]
    NotFound(String),
}

/// Look up the centroid of a US ZIP code. Trims surrounding whitespace
/// and validates "5 digits" before making the network call.
///
/// # Errors
///
/// See [`PostalLookupError`]. Notable cases:
///
/// * [`PostalLookupError::InvalidZip`] — caller passed something that
///   isn't a 5-digit ZIP. No HTTP request is made.
/// * [`PostalLookupError::NotFound`] — ZIP isn't in Zippopotam.us's
///   dataset (typo, brand-new ZIP, military APO, etc.).
pub fn lookup_us_zip(zip: &str) -> Result<PostalLocation, PostalLookupError> {
    let zip = zip.trim();
    if !is_us_zip(zip) {
        return Err(PostalLookupError::InvalidZip(zip.to_string()));
    }

    let url = format!("https://api.zippopotam.us/us/{zip}");
    let client = client()?;
    let body = client
        .get(&url)
        .send()
        .map_err(|e| PostalLookupError::Http(format!("GET {url}: {e}")))?
        .error_for_status()
        .map_err(|e| PostalLookupError::Http(format!("HTTP status: {e}")))?
        .text()
        .map_err(|e| PostalLookupError::Http(format!("response body: {e}")))?;

    parse_response(zip, &body)
}

/// Five ASCII digits — Zippopotam.us only matches whole ZIPs and
/// rejects anything else, so we filter aggressively up front.
fn is_us_zip(zip: &str) -> bool {
    zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit())
}

fn client() -> Result<reqwest::blocking::Client, PostalLookupError> {
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let new_client = reqwest::blocking::Client::builder()
        .timeout(DEFAULT_LOOKUP_TIMEOUT)
        .user_agent(concat!("sdr-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| PostalLookupError::Http(format!("client build: {e}")))?;
    let _ = CLIENT.set(new_client.clone());
    Ok(CLIENT.get().cloned().unwrap_or(new_client))
}

/// Pull `(lat, lon, place, state)` out of a Zippopotam.us US response.
/// Pulled out as a free function so the hermetic JSON tests below can
/// pin the parsing rules without touching the network.
fn parse_response(zip: &str, body: &str) -> Result<PostalLocation, PostalLookupError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| PostalLookupError::Parse(format!("not JSON: {e}")))?;
    let places = value
        .get("places")
        .and_then(serde_json::Value::as_array)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| PostalLookupError::NotFound(zip.to_string()))?;
    let first = &places[0];
    // Upstream serialises lat/lon as JSON strings, not numbers — parse
    // through `str::parse` so a future schema flip to numeric values
    // would still need explicit handling rather than silently breaking.
    let lat_deg = first
        .get("latitude")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| PostalLookupError::Parse("missing/non-numeric latitude".to_string()))?;
    let lon_deg = first
        .get("longitude")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| PostalLookupError::Parse("missing/non-numeric longitude".to_string()))?;
    let place = first
        .get("place name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let region = first
        .get("state abbreviation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(PostalLocation {
        lat_deg,
        lon_deg,
        place,
        region,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn is_us_zip_accepts_five_digits() {
        assert!(is_us_zip("24068"));
        assert!(is_us_zip("00501")); // smallest real ZIP
    }

    #[test]
    fn is_us_zip_rejects_wrong_lengths() {
        assert!(!is_us_zip(""));
        assert!(!is_us_zip("1234"));
        assert!(!is_us_zip("123456"));
    }

    #[test]
    fn is_us_zip_rejects_non_digits() {
        assert!(!is_us_zip("ABCDE"));
        assert!(!is_us_zip("1234A"));
        assert!(!is_us_zip("1 234")); // space
        assert!(!is_us_zip("12-34")); // hyphen
    }

    #[test]
    fn lookup_rejects_invalid_zip_without_calling_network() {
        // No mock client needed — the validator short-circuits before
        // any HTTP call. The test would hang or fail noisily if it
        // actually tried to send.
        let err = lookup_us_zip("not a zip").unwrap_err();
        assert!(matches!(err, PostalLookupError::InvalidZip(_)));
    }

    #[test]
    fn parse_response_extracts_lat_lon_place_state() {
        // Real Zippopotam.us body for ZIP 24068 (Christiansburg, VA),
        // pasted verbatim so we catch field-name regressions if upstream
        // changes the schema.
        let body = r#"{
            "country": "United States",
            "country abbreviation": "US",
            "post code": "24068",
            "places": [{
                "place name": "Christiansburg",
                "longitude": "-80.4184",
                "latitude": "37.1548",
                "state": "Virginia",
                "state abbreviation": "VA"
            }]
        }"#;
        let loc = parse_response("24068", body).unwrap();
        assert!((loc.lat_deg - 37.1548).abs() < 0.0001);
        assert!((loc.lon_deg - -80.4184).abs() < 0.0001);
        assert_eq!(loc.place, "Christiansburg");
        assert_eq!(loc.region, "VA");
    }

    #[test]
    fn parse_response_returns_not_found_for_empty_object() {
        // What Zippopotam.us returns when a ZIP isn't in its dataset.
        // Distinguish from a `Parse` error so the UI can show a
        // user-actionable "ZIP not found" instead of a generic
        // "lookup failed" toast.
        let err = parse_response("99999", "{}").unwrap_err();
        assert!(matches!(err, PostalLookupError::NotFound(_)));
    }

    #[test]
    fn parse_response_returns_not_found_for_empty_places_array() {
        // Defensive case: server replies with the right shape but
        // empty `places`. Should still surface as NotFound.
        let body = r#"{"post code":"99999","places":[]}"#;
        let err = parse_response("99999", body).unwrap_err();
        assert!(matches!(err, PostalLookupError::NotFound(_)));
    }

    #[test]
    fn parse_response_surfaces_parse_error_for_garbage() {
        let err = parse_response("24068", "<html>oops</html>").unwrap_err();
        assert!(matches!(err, PostalLookupError::Parse(_)));
    }

    #[test]
    fn parse_response_surfaces_parse_error_for_missing_fields() {
        // Right top-level shape, but the place record is missing
        // latitude/longitude. We require both — better to error than
        // hand back lat=0 (Atlantic Ocean).
        let body = r#"{"post code":"24068","places":[{"place name":"X"}]}"#;
        let err = parse_response("24068", body).unwrap_err();
        assert!(matches!(err, PostalLookupError::Parse(_)));
    }

    #[test]
    fn lookup_trims_surrounding_whitespace() {
        // User pastes a ZIP with a trailing space — the validator
        // sees the trimmed form and rejects only on actual length /
        // digit failures.
        let err = lookup_us_zip("  abcde  ").unwrap_err();
        // Whitespace is trimmed first, then validation runs. The
        // remaining content "abcde" is non-digits, so InvalidZip.
        assert!(matches!(err, PostalLookupError::InvalidZip(_)));
    }
}
