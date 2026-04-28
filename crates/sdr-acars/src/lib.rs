//! ACARS (Aircraft Communications Addressing and Reporting
//! System) decoder. Faithful Rust port of
//! [acarsdec](https://github.com/TLeconte/acarsdec) — pure DSP +
//! parsing, no GTK, no SDR-driver dependency.
//!
//! The crate exposes one entry point: [`ChannelBank::new`] +
//! [`ChannelBank::process`] for multi-channel parallel decode
//! from a single source-rate IQ stream. Decoded
//! [`AcarsMessage`]s are emitted via a callback.
//!
//! Sub-modules ([`msk`], [`frame`], [`channel`]) are public so
//! the CLI binary can drive them directly for WAV input (which
//! arrives pre-decimated to 12.5 kHz IF rate, bypassing
//! `ChannelBank`'s oscillator + decimator stage).

pub mod channel;
pub mod crc;
pub mod error;
pub mod frame;
pub mod label;
pub mod msk;
pub mod syndrom;

pub use error::AcarsError;
