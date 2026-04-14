//! Concrete `TranscriptionBackend` implementations.
//!
//! Each backend is a self-contained module gated behind a cargo feature.
//! Whisper and Sherpa are mutually exclusive (see `lib.rs` `compile_error`
//! guards) — exactly one is compiled into any given binary.

#[cfg(feature = "sherpa")]
pub mod sherpa;

#[cfg(feature = "whisper")]
pub mod whisper;

#[cfg(feature = "whisper")]
pub mod earshot_vad;

#[cfg(test)]
pub mod mock;
