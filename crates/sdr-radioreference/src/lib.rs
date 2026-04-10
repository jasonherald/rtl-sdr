//! `RadioReference.com` SOAP API client.
//!
//! Provides `RrClient` for querying the `RadioReference` frequency database.
//! All methods are blocking — call from a background thread when used with GTK.

pub mod mode_map;
pub mod soap;
pub mod types;

pub use soap::SoapError;
pub use types::{CountyInfo, RrCategory, RrFrequency, RrSubcategory, RrTag, ZipInfo};

/// Application API key — identifies the SDR-RS app to `RadioReference`.
/// This is not a secret; it identifies the application, not the user.
///
/// Override via `RADIOREFERENCE_APP_KEY` env var at build time if needed.
const APP_KEY: &str = match option_env!("RADIOREFERENCE_APP_KEY") {
    Some(key) => key,
    None => "2e4b8c24-341a-11f1-bb32-0ef97433b5f9",
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

    /// Get detailed county info including categories and subcategories.
    pub fn get_county_info(&self, county_id: u32) -> Result<CountyInfo, SoapError> {
        tracing::debug!(county_id, "querying RadioReference county info");
        soap::get_county_info(&self.http, &self.auth, county_id)
    }

    /// Get all frequencies for a county by fetching each subcategory.
    ///
    /// Returns `(county_name, frequencies)`. Calls `getCountyInfo` to discover
    /// subcategories, then `getSubcatFreqs` for each one. Attaches the
    /// category/subcategory name as a tag on each frequency for UI filtering.
    pub fn get_county_frequencies(
        &self,
        county_id: u32,
    ) -> Result<(String, Vec<RrFrequency>), SoapError> {
        let info = self.get_county_info(county_id)?;
        let county_name = info.county_name.clone();

        tracing::info!(
            county = %info.county_name,
            categories = info.categories.len(),
            subcategories = info.categories.iter().map(|c| c.subcategories.len()).sum::<usize>(),
            "county structure loaded"
        );

        let mut all_freqs: Vec<RrFrequency> = Vec::new();
        for cat in &info.categories {
            tracing::debug!(
                category = %cat.name,
                subcategories = cat.subcategories.len(),
                "processing category"
            );
            for subcat in &cat.subcategories {
                match soap::get_subcat_freqs(&self.http, &self.auth, subcat.scid) {
                    Ok(mut freqs) => {
                        tracing::debug!(
                            scid = subcat.scid,
                            name = %subcat.name,
                            category = %cat.name,
                            count = freqs.len(),
                            "fetched subcategory frequencies"
                        );
                        // Attach category/subcategory as a tag if none present
                        for freq in &mut freqs {
                            if freq.tags.is_empty() {
                                freq.tags.push(RrTag {
                                    id: subcat.scid,
                                    description: format!("{} - {}", cat.name, subcat.name),
                                });
                            }
                        }
                        all_freqs.extend(freqs);
                    }
                    Err(e) => {
                        tracing::warn!(
                            scid = subcat.scid,
                            name = %subcat.name,
                            "failed to fetch subcategory: {e}"
                        );
                        // Continue with other subcategories
                    }
                }
            }
        }

        tracing::info!(
            county_id,
            county = %info.county_name,
            total = all_freqs.len(),
            "fetched county frequencies"
        );

        Ok((county_name, all_freqs))
    }
}
