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
pub use pw_impl::{AudioDevice, AudioSink, list_audio_sinks};

#[cfg(not(feature = "pipewire"))]
mod stub_impl;

#[cfg(not(feature = "pipewire"))]
pub use stub_impl::AudioSink;

/// Audio device info (stub for non-PipeWire builds).
#[cfg(not(feature = "pipewire"))]
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// Human-readable name.
    pub display_name: String,
    /// PipeWire node name.
    pub node_name: String,
}

/// Stub for non-PipeWire builds — returns only "Default".
#[cfg(not(feature = "pipewire"))]
pub fn list_audio_sinks() -> Vec<AudioDevice> {
    vec![AudioDevice {
        display_name: "Default".to_string(),
        node_name: String::new(),
    }]
}
