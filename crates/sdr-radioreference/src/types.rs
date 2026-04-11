//! Response types returned by the `RadioReference` SOAP API.

/// Information about a US ZIP code, including its county and state.
///
/// The API returns county/state as numeric IDs only — names are not available
/// from `getZipcodeInfo`.
#[derive(Debug, Clone)]
pub struct ZipInfo {
    /// County ID on `RadioReference`.
    pub county_id: u32,
    /// State ID on `RadioReference`.
    pub state_id: u32,
    /// City name.
    pub city: String,
    /// Latitude.
    pub lat: String,
    /// Longitude.
    pub lon: String,
}

/// Detailed county information from `getCountyInfo`.
#[derive(Debug, Clone)]
pub struct CountyInfo {
    /// County ID.
    pub county_id: u32,
    /// County name.
    pub county_name: String,
    /// State ID.
    pub state_id: u32,
    /// Categories containing subcategories with frequency data.
    pub categories: Vec<RrCategory>,
}

/// A frequency category (e.g. "Public Safety", "Business").
#[derive(Debug, Clone)]
pub struct RrCategory {
    /// Category ID.
    pub id: u32,
    /// Category name.
    pub name: String,
    /// Subcategories within this category.
    pub subcategories: Vec<RrSubcategory>,
}

/// A subcategory within a category (e.g. "Police", "Fire").
#[derive(Debug, Clone)]
pub struct RrSubcategory {
    /// Subcategory ID — used with `getSubcatFreqs`.
    pub scid: u32,
    /// Subcategory name.
    pub name: String,
}

/// A tag (category) applied to a `RadioReference` frequency entry.
#[derive(Debug, Clone)]
pub struct RrTag {
    /// Tag identifier.
    pub id: u32,
    /// Human-readable tag description.
    pub description: String,
}

/// A frequency entry from `RadioReference`.
#[derive(Debug, Clone)]
pub struct RrFrequency {
    /// Frequency ID from `RadioReference` (fid).
    pub id: String,
    /// Output frequency in Hz (converted from the MHz value returned by the API).
    pub freq_hz: u64,
    /// Raw `RadioReference` mode string (e.g. "FM", "FMN", "AM").
    pub mode: String,
    /// CTCSS/PL tone in Hz, or `None` if absent or zero.
    pub tone: Option<f32>,
    /// Frequency description.
    pub description: String,
    /// Short alpha tag label.
    pub alpha_tag: String,
    /// Category tags applied to this frequency.
    pub tags: Vec<RrTag>,
}
