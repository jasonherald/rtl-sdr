//! Threading, streaming, and signal path management.
//!
//! Provides the infrastructure that connects DSP blocks into a real-time
//! signal processing pipeline.

pub mod block;
pub mod chain;
pub mod iq_frontend;
pub mod sink_manager;
pub mod source_manager;
pub mod splitter;
pub mod stream;
pub mod vfo_manager;
