//! Concrete `TranscriptionBackend` implementations.
//!
//! Each backend is a self-contained module. The engine in `lib.rs`
//! constructs one based on the [`crate::backend::ModelChoice`] variant
//! and delegates lifecycle to it.

pub mod sherpa;
pub mod whisper;

#[cfg(test)]
pub mod mock;
