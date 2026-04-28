//! Error type for the `sdr-acars` crate.
//!
//! Per project library-crate rules: all fallible paths return
//! `Result<_, AcarsError>` — no `unwrap()`, no `panic!()`, no
//! stringly-typed errors.

use thiserror::Error;

/// All ways `sdr-acars` can fail.
#[derive(Debug, Error)]
pub enum AcarsError {
    /// `ChannelBank::new` got an invalid configuration: empty
    /// channel list, source rate / center freq combination that
    /// can't fit all channels, or per-channel rate mismatch.
    #[error("invalid channel configuration: {0}")]
    InvalidChannelConfig(String),

    /// Decimation factor isn't an integer for the requested
    /// source rate / IF rate combo. Source rate must be an
    /// integer multiple of `12_500` Hz.
    #[error(
        "source rate {source_rate_hz} Hz is not an integer multiple of IF rate {if_rate_hz} Hz"
    )]
    NonIntegerDecimation {
        source_rate_hz: f64,
        if_rate_hz: f64,
    },

    /// CLI / file I/O — failed to read input file.
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// CLI — input file format isn't recognized (WAV header
    /// missing, IQ file size not a multiple of 4 bytes for
    /// interleaved i16 I/Q, etc.).
    #[error("invalid input format: {0}")]
    InvalidInput(String),
}
