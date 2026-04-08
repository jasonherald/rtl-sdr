//! `RadioReference.com` SOAP API client.
//!
//! Provides `RrClient` for querying the `RadioReference` frequency database.
//! All methods are blocking — call from a background thread when used with GTK.

pub mod mode_map;
pub mod soap;
pub mod types;

pub use soap::SoapError;
pub use types::{RrFrequency, RrTag, ZipInfo};

/// Application API key — identifies the SDR-RS app to `RadioReference`.
/// This is not a secret; it's distributed with the application.
///
/// Set via `RADIOREFERENCE_APP_KEY` env var at build time, or defaults to a
/// placeholder that will fail at runtime with an auth error.
const APP_KEY: &str = match option_env!("RADIOREFERENCE_APP_KEY") {
    Some(key) => key,
    None => "PENDING_API_KEY",
};

/// `RadioReference` SOAP API client.
///
/// Holds user credentials and an HTTP client. All methods are blocking.
pub struct RrClient {
    auth: soap::SoapAuth,
    http: reqwest::blocking::Client,
}

impl RrClient {
    ///
    /// Configures HTTP timeouts to prevent indefinite hangs.
    ///
    /// # Errors
    ///
    /// Returns a [`SoapError`] if the HTTP client cannot be constructed.
    pub fn new(username: &str, password: &str) -> Result<Self, SoapError> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            auth: soap::SoapAuth {
                username: username.to_string(),
                password: password.to_string(),
                app_key: APP_KEY.to_string(),
            },
            http,
        })
    }

    /// Test the connection by querying zip code 90210.
    ///
    /// Returns `Ok(())` if credentials are valid, `Err` otherwise.
    pub fn test_connection(&self) -> Result<(), SoapError> {
        soap::get_zipcode_info(&self.http, &self.auth, "90210")?;
        Ok(())
    }

    /// Look up county/state info for a US zip code.
    pub fn get_zip_info(&self, zipcode: &str) -> Result<ZipInfo, SoapError> {
        tracing::debug!(zipcode, "querying RadioReference zip info");
        soap::get_zipcode_info(&self.http, &self.auth, zipcode)
    }

    /// Get all frequencies for a county.
    ///
    /// Calls `getCountyFreqsByTag` with `tag=0` to request all categories.
    pub fn get_county_frequencies(&self, county_id: u32) -> Result<Vec<RrFrequency>, SoapError> {
        tracing::debug!(county_id, "querying RadioReference county frequencies");
        soap::get_county_freqs_by_tag(&self.http, &self.auth, county_id, 0)
    }
}
