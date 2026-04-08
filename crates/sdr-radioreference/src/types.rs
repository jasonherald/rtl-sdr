//! Response types returned by the `RadioReference` SOAP API.

/// Information about a US ZIP code, including its county and state.
#[derive(Debug, Clone)]
pub struct ZipInfo {
    /// County ID on `RadioReference`.
    pub county_id: u32,
    /// State ID on `RadioReference`.
    pub state_id: u32,
    /// City name.
    pub city: String,
    /// County name.
    pub county_name: String,
    /// State name.
    pub state_name: String,
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
