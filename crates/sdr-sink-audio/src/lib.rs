#![allow(clippy::doc_markdown, clippy::unnecessary_literal_bound)]
//! Audio output sink — PipeWire (Linux) / CoreAudio (macOS).
//!
//! When the `pipewire` feature is enabled, spawns a PipeWire main loop thread,
//! creates a playback stream at 48 kHz stereo f32, and feeds audio from the
//! DSP controller through a bounded channel.
//!
//! When the `coreaudio` feature is enabled, opens an AUHAL default-output
//! AudioUnit and feeds audio through the same shared ring buffer pattern as
//! the PipeWire backend.
//!
//! When neither backend feature is enabled, provides a stub that logs a
//! warning.

// Shared SPSC ring buffer used by both real backends. The stub backend
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
//   • macOS + `coreaudio` feature → coreaudio_impl
//   • everything else             → stub_impl (logs and discards)
//
// `sdr-core/Cargo.toml` enables the right feature per `target_os` so
// downstream crates don't have to think about it. The stub fallback
// keeps `cargo build --workspace` working on bare Linux/macOS without
// either feature flag (e.g., for fast feature-less syntax checks).

#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pw_impl;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub use pw_impl::{AudioDevice, AudioSink, list_audio_sinks};

#[cfg(all(target_os = "macos", feature = "coreaudio"))]
mod coreaudio_impl;
#[cfg(all(target_os = "macos", feature = "coreaudio"))]
pub use coreaudio_impl::{AudioDevice, AudioSink, list_audio_sinks};

// Fallback stub: any target without an explicit backend feature lands
// here. The workspace baseline `cargo build` on macOS used this until
// the `coreaudio` feature shipped (now wired via sdr-core); it remains
// the fallback for unusual build configurations and for the workspace
// no-default-features check.
#[cfg(not(any(
    all(target_os = "linux", feature = "pipewire"),
    all(target_os = "macos", feature = "coreaudio"),
)))]
mod stub_impl;
#[cfg(not(any(
    all(target_os = "linux", feature = "pipewire"),
    all(target_os = "macos", feature = "coreaudio"),
)))]
pub use stub_impl::AudioSink;

/// Audio device info (stub backend).
#[cfg(not(any(
    all(target_os = "linux", feature = "pipewire"),
    all(target_os = "macos", feature = "coreaudio"),
)))]
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// Human-readable name.
    pub display_name: String,
    /// Caller-opaque device identifier — empty means "system default".
    pub node_name: String,
}

/// Stub `list_audio_sinks` — returns only "Default".
#[cfg(not(any(
    all(target_os = "linux", feature = "pipewire"),
    all(target_os = "macos", feature = "coreaudio"),
)))]
#[must_use]
pub fn list_audio_sinks() -> Vec<AudioDevice> {
    vec![AudioDevice {
        display_name: "Default".to_string(),
        node_name: String::new(),
    }]
}
