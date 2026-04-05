//! Threading, streaming, and signal path management.
//!
//! Provides the infrastructure that connects DSP blocks into a real-time
//! signal processing pipeline.

pub mod block;
pub mod chain;
pub mod splitter;
pub mod stream;
