#![allow(clippy::doc_markdown, clippy::unnecessary_literal_bound)]
//! Audio output sink — PipeWire (Linux).
//!
//! When the `pipewire` feature is enabled, spawns a PipeWire main loop thread,
//! creates a playback stream at 48 kHz stereo f32, and feeds audio from the
//! DSP controller through a bounded channel.
//!
//! When the feature is disabled, provides a stub that logs a warning.

#[cfg(feature = "pipewire")]
mod pw_impl;

#[cfg(feature = "pipewire")]
pub use pw_impl::AudioSink;

#[cfg(not(feature = "pipewire"))]
mod stub_impl;

#[cfg(not(feature = "pipewire"))]
pub use stub_impl::AudioSink;
