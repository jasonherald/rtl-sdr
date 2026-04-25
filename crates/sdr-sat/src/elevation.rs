//! Lat/lon → ground elevation lookup.
//!
//! Sister helper to [`crate::postal_lookup`]: once we know a ground
//! station's latitude/longitude (typed in by hand or resolved from a
//! ZIP code), this module fills in the altitude field. Pass prediction
//! is barely sensitive to station altitude — a NOAA at ~850 km doesn't
//! notice a 100 m height difference — but populating the field anyway
//! makes the panel feel "complete" after a ZIP lookup. Surprising the
//! user with a bare 0 m sea-level altitude when they're at 600 m in
//! Boulder is the kind of thing that erodes trust.
//!
//! We hit the public, no-auth Open-Elevation endpoint
//! (`api.open-elevation.com/api/v1/lookup?locations=lat,lon`). World
//! coverage, 30 m SRTM-derived dataset, JSON. Like the rest of
//! sdr-sat's HTTP plumbing, the call is **blocking** and meant to be
//! invoked from a worker thread.

use std::sync::OnceLock;
use std::time::Duration;

/// Default timeout for the Open-Elevation round trip. Generous —
/// the upstream is community-run and occasionally slow under load.
/// We'd rather wait 15 s once than spuriously fail a UX nicety.
pub const DEFAULT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Cached HTTP client — built on first call so consecutive lookups
/// reuse the TLS session. Same `OnceLock` pattern as the postal
/// lookup module and the TLE cache; same rationale.
static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

/// Errors from an elevation lookup.
#[derive(Debug, thiserror::Error)]
pub enum ElevationLookupError {
    /// Caller passed a lat/lon that's outside the valid WGS84 range.
    /// Caught client-side rather than burning a request on something
    /// the upstream would reject anyway.
    #[error("invalid coordinates: lat={lat_deg}, lon={lon_deg}")]
    InvalidCoords {
        /// Latitude that failed the range check.
        lat_deg: f64,
        /// Longitude that failed the range check.
        lon_deg: f64,
    },
    /// Upstream returned a non-2xx status, the request timed out, or
    /// reqwest itself failed to send.
    #[error("elevation lookup HTTP error: {0}")]
    Http(String),
    /// Body wasn't JSON or didn't have the fields we expected.
    #[error("elevation lookup parse error: {0}")]
    Parse(String),
    /// JSON parsed but no result records — Open-Elevation returns
    /// `{"results":[]}` when its dataset has no coverage at the
    /// requested point (rare but observed near polar regions).
    #[error("no elevation data available for the requested point")]
    NoData,
}

/// Look up ground elevation in metres at `(lat_deg, lon_deg)`.
///
/// # Errors
///
/// See [`ElevationLookupError`]. Notable cases:
///
/// * [`ElevationLookupError::InvalidCoords`] — coordinates out of
///   range. No HTTP request is made.
/// * [`ElevationLookupError::NoData`] — point isn't covered by the
///   upstream dataset (rare; mostly polar/oceanic gaps).
pub fn lookup_elevation_m(lat_deg: f64, lon_deg: f64) -> Result<f64, ElevationLookupError> {
    if !is_valid_coords(lat_deg, lon_deg) {
        return Err(ElevationLookupError::InvalidCoords { lat_deg, lon_deg });
    }

    // `{:.6}` is plenty: 0.000001° is ~11 cm at the equator — far
    // below the dataset's 30 m grid resolution. Locks the URL down
    // to a deterministic shape for caching/debugging.
    let url =
        format!("https://api.open-elevation.com/api/v1/lookup?locations={lat_deg:.6},{lon_deg:.6}");
    let client = client()?;
    let body = client
        .get(&url)
        .send()
        .map_err(|e| ElevationLookupError::Http(format!("GET {url}: {e}")))?
        .error_for_status()
        .map_err(|e| ElevationLookupError::Http(format!("HTTP status: {e}")))?
        .text()
        .map_err(|e| ElevationLookupError::Http(format!("response body: {e}")))?;

    parse_response(&body)
}

/// WGS84 valid-range check — applied before we burn a request on a
/// nonsense coordinate (a misclick that put the `SpinRow` at lat=999
/// shouldn't reach the network).
fn is_valid_coords(lat_deg: f64, lon_deg: f64) -> bool {
    lat_deg.is_finite()
        && lon_deg.is_finite()
        && (-90.0..=90.0).contains(&lat_deg)
        && (-180.0..=180.0).contains(&lon_deg)
}

fn client() -> Result<reqwest::blocking::Client, ElevationLookupError> {
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let new_client = reqwest::blocking::Client::builder()
        .timeout(DEFAULT_LOOKUP_TIMEOUT)
        .user_agent(concat!("sdr-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ElevationLookupError::Http(format!("client build: {e}")))?;
    let _ = CLIENT.set(new_client.clone());
    Ok(CLIENT.get().cloned().unwrap_or(new_client))
}

/// Pull the first result's elevation out of an Open-Elevation
/// response. Free function so the hermetic JSON tests below pin the
/// parsing rules without touching the network.
fn parse_response(body: &str) -> Result<f64, ElevationLookupError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| ElevationLookupError::Parse(format!("not JSON: {e}")))?;
    let results = value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ElevationLookupError::Parse("missing `results` array".to_string()))?;
    let first = results.first().ok_or(ElevationLookupError::NoData)?;
    first
        .get("elevation")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| ElevationLookupError::Parse("missing/non-numeric elevation".to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_coords_accepts_in_range() {
        assert!(is_valid_coords(0.0, 0.0));
        assert!(is_valid_coords(90.0, 180.0));
        assert!(is_valid_coords(-90.0, -180.0));
        assert!(is_valid_coords(37.1548, -80.4184));
    }

    #[test]
    fn is_valid_coords_rejects_out_of_range() {
        assert!(!is_valid_coords(91.0, 0.0));
        assert!(!is_valid_coords(0.0, 181.0));
        assert!(!is_valid_coords(-91.0, 0.0));
    }

    #[test]
    fn is_valid_coords_rejects_non_finite() {
        assert!(!is_valid_coords(f64::NAN, 0.0));
        assert!(!is_valid_coords(0.0, f64::INFINITY));
    }

    #[test]
    fn lookup_rejects_invalid_coords_without_calling_network() {
        // No mock client — validator short-circuits before HTTP.
        // Test would hang or fail noisily if it actually tried to send.
        let err = lookup_elevation_m(91.0, 0.0).unwrap_err();
        assert!(matches!(err, ElevationLookupError::InvalidCoords { .. }));
    }

    #[test]
    fn parse_response_extracts_first_elevation() {
        // Real Open-Elevation body for (37.1548, -80.4184) — pasted
        // verbatim so a schema regression breaks the test.
        let body = r#"{"results":[{"latitude":37.1548,"longitude":-80.4184,"elevation":647.0}]}"#;
        let elev = parse_response(body).unwrap();
        assert_eq!(elev, 647.0);
    }

    #[test]
    fn parse_response_handles_integer_elevation() {
        // Some upstream responses serialise elevation as a JSON
        // integer (`647` not `647.0`). `as_f64` accepts both.
        let body = r#"{"results":[{"latitude":37.0,"longitude":-80.0,"elevation":647}]}"#;
        let elev = parse_response(body).unwrap();
        assert_eq!(elev, 647.0);
    }

    #[test]
    fn parse_response_returns_no_data_for_empty_results() {
        let err = parse_response(r#"{"results":[]}"#).unwrap_err();
        assert!(matches!(err, ElevationLookupError::NoData));
    }

    #[test]
    fn parse_response_surfaces_parse_error_for_garbage() {
        let err = parse_response("<html>oops</html>").unwrap_err();
        assert!(matches!(err, ElevationLookupError::Parse(_)));
    }

    #[test]
    fn parse_response_surfaces_parse_error_for_missing_elevation() {
        // Right top-level shape, but the result record is missing
        // elevation. Surface as Parse rather than silently
        // returning 0.0 — better an error than a wrong altitude.
        let body = r#"{"results":[{"latitude":37.0,"longitude":-80.0}]}"#;
        let err = parse_response(body).unwrap_err();
        assert!(matches!(err, ElevationLookupError::Parse(_)));
    }

    #[test]
    fn parse_response_surfaces_parse_error_for_missing_results_array() {
        let err = parse_response(r#"{"foo":"bar"}"#).unwrap_err();
        assert!(matches!(err, ElevationLookupError::Parse(_)));
    }
}
