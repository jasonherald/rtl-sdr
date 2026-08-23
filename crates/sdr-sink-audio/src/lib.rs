#![allow(clippy::doc_markdown, clippy::unnecessary_literal_bound)]
//! Audio output sink — PipeWire (Linux).
//!
//! When the `pipewire` feature is enabled, spawns a PipeWire main loop thread,
//! creates a playback stream at 48 kHz stereo f32, and feeds audio from the
//! DSP controller through a bounded channel.
//!
//! When the `pipewire` feature is disabled, provides a stub that logs a
//! warning.

// Shared SPSC ring buffer used by the PipeWire backend. The stub backend
// doesn't need it, but it's cheap to compile in unconditionally — about
// 200 lines of plain Rust with no external deps.
mod ring;

// ---------------------------------------------------------------------
//  Backend dispatch
// ---------------------------------------------------------------------
//
// Exactly one backend module compiles in based on cfg + feature flags:
//
//   • Linux + `pipewire` feature  → pw_impl
//   • everything else             → stub_impl (logs and discards)
//
// `sdr-core/Cargo.toml` enables the feature per `target_os` so
// downstream crates don't have to think about it. The stub fallback
// keeps `cargo build --workspace` working without the feature flag
// (e.g., for fast feature-less syntax checks). The CoreAudio backend
// was removed along with the macOS port surface (#838; see the
// `mac-archive` branch).

#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pw_impl;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub use pw_impl::{AudioDevice, AudioSink, list_audio_sinks};

// Fallback stub: any target without the backend feature lands here —
// unusual build configurations and the workspace no-default-features
// check.
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
mod stub_impl;
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
pub use stub_impl::AudioSink;

/// Audio device info (stub backend).
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// Human-readable name.
    pub display_name: String,
    /// Caller-opaque device identifier — empty means "system default".
    pub node_name: String,
}

/// Stub `list_audio_sinks` — returns only "Default".
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
#[must_use]
pub fn list_audio_sinks() -> Vec<AudioDevice> {
    vec![AudioDevice {
        display_name: "Default".to_string(),
        node_name: String::new(),
    }]
}
