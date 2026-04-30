//! ACARS (Aircraft Communications Addressing and Reporting
//! System) decoder. Faithful Rust port of
//! [acarsdec](https://github.com/TLeconte/acarsdec) — pure
//! DSP + parsing, no GTK, no SDR-driver dependency.
//!
//! # Example: multi-channel decode from a 2.5 `MSps` complex IQ stream
//!
//! ```no_run
//! use num_complex::Complex32;
//! use sdr_acars::ChannelBank;
//!
//! const US_ACARS: &[f64] = &[
//!     129_125_000.0, 130_025_000.0, 130_425_000.0,
//!     130_450_000.0, 131_525_000.0, 131_550_000.0,
//! ];
//!
//! # fn read_iq_block() -> Vec<Complex32> { Vec::new() }
//! // Center on the midpoint of the channel extremes (130.3375 MHz)
//! // so the 2.425 MHz cluster fits inside the 2.5 MHz Nyquist window.
//! let mut bank =
//!     ChannelBank::new(2_500_000.0, 130_337_500.0, US_ACARS)?;
//! loop {
//!     let iq: Vec<Complex32> = read_iq_block();
//!     if iq.is_empty() { break; }
//!     bank.process(&iq, |msg| {
//!         let label = String::from_utf8_lossy(&msg.label);
//!         println!("{} {label} {}", msg.aircraft, msg.text);
//!     });
//! }
//! # Ok::<(), sdr_acars::AcarsError>(())
//! ```
//!
//! For pre-decimated 12.5 kHz IF input (e.g. WAV files written
//! by acarsdec's `--save` mode, one channel per WAV channel),
//! drive [`msk::MskDemod`] + [`frame::FrameParser`] directly
//! instead — see `bin/sdr-acars-cli.rs` for the WAV path.

pub mod channel;
pub mod crc;
pub mod error;
pub mod frame;
pub mod json;
pub mod label;
pub mod label_parsers;
pub mod msk;
pub mod reassembly;
pub mod syndrom;

pub use channel::{ChannelBank, ChannelLockState, ChannelStats};
pub use error::AcarsError;
pub use frame::{AcarsMessage, FrameParser};
pub use json::serialize_message as serialize_acars_json;
pub use label::lookup as lookup_label;
pub use label_parsers::{Oooi, decode_label};
pub use msk::{IF_RATE_HZ, MskDemod};
pub use reassembly::{MessageAssembler, REASSEMBLY_TIMEOUT};
