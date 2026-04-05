#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::needless_range_loop,
    clippy::redundant_closure_for_method_calls,
    clippy::unnecessary_literal_bound,
    clippy::doc_markdown,
    clippy::manual_midpoint,
    clippy::redundant_closure
)]
//! Audio output sink — PipeWire (Linux) / CoreAudio (macOS).
//!
//! Ports SDR++ `AudioSinkModule`. Platform-specific audio output
//! using PipeWire on Linux and CoreAudio on macOS.
//!
//! Note: PipeWire/CoreAudio integration requires platform-specific
//! crates. This module provides the framework and will be connected
//! to the actual audio backend when those dependencies are added.

use sdr_pipeline::sink_manager::Sink;
use sdr_types::SinkError;

/// Default audio sample rate (Hz).
const DEFAULT_AUDIO_SAMPLE_RATE: f64 = 48_000.0;

/// Audio output sink.
///
/// Outputs demodulated audio to the system's audio device.
pub struct AudioSink {
    device_name: String,
    sample_rate: f64,
    running: bool,
}

impl AudioSink {
    /// Create a new audio sink with the default device.
    pub fn new() -> Self {
        Self {
            device_name: "default".to_string(),
            sample_rate: DEFAULT_AUDIO_SAMPLE_RATE,
            running: false,
        }
    }

    /// Set the audio device by name.
    pub fn set_device(&mut self, name: &str) {
        self.device_name = name.to_string();
    }

    /// Get the current device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}

impl Default for AudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for AudioSink {
    fn name(&self) -> &str {
        "Audio"
    }

    fn start(&mut self) -> Result<(), SinkError> {
        // Fail fast — no PipeWire/CoreAudio backend yet
        tracing::warn!(
            "Audio sink backend not yet implemented (device: {})",
            self.device_name
        );
        Err(SinkError::OpenFailed(
            "audio backend not yet implemented".to_string(),
        ))
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        self.running = false;
        tracing::info!("Audio sink stopped");
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(SinkError::OpenFailed(format!(
                "sample rate must be positive and finite, got {rate}"
            )));
        }
        self.sample_rate = rate;
        Ok(())
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let sink = AudioSink::new();
        assert_eq!(sink.name(), "Audio");
        assert!((sink.sample_rate() - 48_000.0).abs() < f64::EPSILON);
        assert_eq!(sink.device_name(), "default");
    }

    #[test]
    fn test_start_fails_no_backend() {
        let mut sink = AudioSink::new();
        assert!(sink.start().is_err(), "start should fail without backend");
    }
}
