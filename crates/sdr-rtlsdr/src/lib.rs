//! Pure Rust port of librtlsdr — RTL2832U USB control and tuner drivers.
//!
//! This crate provides direct USB communication with RTL-SDR dongles
//! using the `rusb` crate, without requiring the C librtlsdr library.
//!
//! Hardware register manipulation requires extensive integer casts that
//! are inherent in a faithful port of C driver code.

// Allow cast-heavy code throughout this crate (hardware register port)
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::collapsible_if,
    clippy::struct_excessive_bools,
    clippy::wildcard_imports,
    clippy::neg_multiply,
    clippy::range_plus_one,
    clippy::manual_range_contains,
    clippy::needless_range_loop,
    clippy::implicit_saturating_sub,
    clippy::doc_markdown
)]

pub mod constants;
pub mod device;
pub mod error;
pub mod reg;
pub mod tuner;
pub mod usb;

pub use device::RtlSdrDevice;
pub use error::RtlSdrError;
