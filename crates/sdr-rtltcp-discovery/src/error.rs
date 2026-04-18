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

    /// Local hostname lookup failed. We need it to build the mDNS
    /// registration's default instance name; callers can pass an
    /// explicit nickname to bypass.
    #[error("failed to determine local hostname: {0}")]
    Hostname(std::io::Error),

    /// Generic IO error — thread spawn failures, socket operations
    /// that surface an unrelated error, etc. Distinct from `Hostname`
    /// so a downstream error-matcher doesn't conflate "hostname
    /// lookup failed" with "couldn't spawn a worker thread."
    #[error("IO error: {0}")]
    Io(std::io::Error),
}
