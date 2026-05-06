//! Pure-Rust port of librtlsdr — RTL2832U USB control + tuner drivers.
//!
//! Talks to RTL-SDR dongles directly over USB via [`rusb`], without the
//! C `librtlsdr` library or its headers. Covers all five tuner families
//! shipped in real-world dongles (R820T / R820T2 / R828D, E4000,
//! FC0012, FC0013, FC2580).
//!
//! # Quick start
//!
//! ```no_run
//! use sdr_rtlsdr::{RtlSdrDevice, RtlSdrError};
//!
//! # fn main() -> Result<(), RtlSdrError> {
//! // Open the first dongle plugged in.
//! let mut dev = RtlSdrDevice::open(0)?;
//!
//! // Tune to 100 MHz, 2.048 Msps.
//! dev.set_center_freq(100_000_000)?;
//! dev.set_sample_rate(2_048_000)?;
//!
//! // Manual gain at 14.4 dB (= 144 in tenths-of-dB).
//! dev.set_tuner_gain_mode(true)?;
//! dev.set_tuner_gain(144)?;
//!
//! // Read 64 KB of interleaved I/Q samples.
//! dev.reset_buffer()?;
//! let mut buf = vec![0u8; 65_536];
//! let n = dev.read_sync(&mut buf)?;
//! assert!(n > 0);
//! # Ok(())
//! # }
//! ```
//!
//! # Public surface
//!
//! The committed surface is intentionally narrow:
//!
//! - [`RtlSdrDevice`] — the device handle. All control + streaming
//!   methods live here. Open via [`RtlSdrDevice::open`].
//! - Free enumeration helpers — [`get_device_count`],
//!   [`get_device_name`], [`get_device_usb_strings`],
//!   [`get_index_by_serial`].
//! - [`RtlSdrError`] — the unified error type returned by every
//!   fallible operation.
//! - [`TunerType`] — the IC family identifier returned by
//!   [`RtlSdrDevice::tuner_type`].
//!
//! Sample values are interleaved unsigned 8-bit I/Q pairs, the native
//! RTL-SDR format. Convert to centred `i8` (or `f32` in `[-1, 1]`) at
//! the consumer if needed; we don't impose a sample type on the read
//! path.
//!
//! # USB context + threading
//!
//! [`RtlSdrDevice`] holds an `Arc<rusb::DeviceHandle>` internally so
//! the device handle can be shared across threads safely. The control
//! methods take `&mut self` and serialise on the caller; bulk reads
//! via [`RtlSdrDevice::read_sync`] are concurrency-safe with separate
//! control calls because rusb's bulk transfers are independent of the
//! control endpoint.
//!
//! For raw bulk reads on a worker thread (e.g. an `rtl_tcp`-style
//! server), call [`RtlSdrDevice::usb_handle`] to clone the underlying
//! handle and use [`RtlSdrDevice::BULK_ENDPOINT`] for the endpoint
//! address.
//!
//! # Faithful-port note
//!
//! The crate is a port of the C `librtlsdr` source — register
//! addresses, magic constants, and per-tuner I2C tables are
//! transcribed directly from the upstream code. Some internal items
//! aren't currently called from Rust but are kept for completeness so
//! future hardware-feature work is a register-table read away rather
//! than a re-port. Hardware register manipulation requires extensive
//! integer casts inherent in a faithful port of C driver code.
//!
//! [`rusb`]: https://docs.rs/rusb

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
    clippy::doc_markdown,
    // Faithful port from librtlsdr's C source: register addresses,
    // hardware constants, and tuner-IC enum variants are kept for
    // completeness even when not currently called from Rust. The
    // public surface of the crate (now scoped down — see
    // `RtlSdrDevice` and the top-level re-exports) is what we're
    // committing to; keeping the internals around makes future
    // hardware-feature work a register-table read away rather than
    // a re-port. Per #626 visibility audit.
    dead_code
)]

pub(crate) mod constants;
pub mod device;
pub mod error;
pub(crate) mod reg;
pub mod tuner;
pub(crate) mod usb;

pub use device::{
    RtlSdrDevice, get_device_count, get_device_name, get_device_usb_strings, get_index_by_serial,
};
pub use error::RtlSdrError;
/// Tuner family identifier for the IC inside the dongle.
///
/// Returned by [`RtlSdrDevice::tuner_type`]. Useful for displaying
/// "tuner: R820T2" in a UI or for branching gain-table queries
/// since each tuner has its own discrete gain steps.
pub use reg::TunerType;
