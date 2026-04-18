//! Error types.

use thiserror::Error;

/// Errors from advertiser / browser setup and runtime.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// Passed through from the underlying `mdns-sd` crate. Covers
    /// daemon startup failures, bind errors, and malformed service
    /// registrations.
    #[error("mDNS daemon error: {0}")]
    Mdns(#[from] mdns_sd::Error),

    /// A TXT record field contained content that mDNS can't carry
    /// (e.g. a NUL byte, a value too long for the 255-byte per-entry
    /// cap, or a key with an `=` in it).
    #[error("invalid TXT record field: {0}")]
    InvalidTxt(String),

    /// Generic IO error — thread spawn failures, socket operations
    /// that surface an unrelated error, etc. Hostname lookup no
    /// longer errors (it uses `libc::gethostname` + a
    /// `"localhost"` fallback), but kept broad so future discovery
    /// paths can reuse.
    #[error("IO error: {0}")]
    Io(std::io::Error),
}
