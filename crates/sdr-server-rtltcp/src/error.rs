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
}
