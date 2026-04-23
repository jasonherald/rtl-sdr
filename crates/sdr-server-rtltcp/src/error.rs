//! Error types for the rtl_tcp server.

use std::io;
use thiserror::Error;

/// Errors produced when starting or running the server.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The requested bind address is already in use. Surfaced distinctly
    /// so callers (CLI, UI) can offer a retry-on-different-port path
    /// without parsing generic IO errors.
    #[error("TCP bind port already in use: {0}")]
    PortInUse(String),

    /// Generic IO error from the socket or filesystem layer.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// Error from the RTL-SDR device layer (open, USB, register I/O).
    #[error("RTL-SDR device error: {0}")]
    Device(#[from] sdr_rtlsdr::RtlSdrError),

    /// No RTL-SDR dongles plugged in.
    #[error("no RTL-SDR device found")]
    NoDevice,

    /// Requested device index is out of range for the currently connected
    /// dongles.
    #[error("device index {requested} out of range (found {available})")]
    BadDeviceIndex { requested: u32, available: u32 },

    /// `ServerConfig.auth_key` carries an out-of-range length —
    /// either zero (which would silently accept any client, per
    /// `validate_auth_key`'s empty-reject contract) or more than
    /// [`crate::extension::MAX_AUTH_KEY_LEN`] (which would fail
    /// at `AuthKeyMessage::to_bytes` every handshake, leaving
    /// the server started but unusable). `Server::start`
    /// rejects these up-front so the operator sees one clear
    /// config error instead of every client failing at
    /// handshake time. Per `CodeRabbit` round 2 on PR #405.
    /// #394.
    #[error("auth_key length {len} is out of range (must be 1..={max})")]
    InvalidAuthKeyLength { len: usize, max: usize },
}
