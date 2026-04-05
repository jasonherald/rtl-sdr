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
            sample_rate: 48_000.0,
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
        // TODO: Initialize PipeWire/CoreAudio stream
        self.running = true;
        tracing::info!("Audio sink started (device: {})", self.device_name);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        self.running = false;
        tracing::info!("Audio sink stopped");
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError> {
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
    fn test_start_stop() {
        let mut sink = AudioSink::new();
        sink.start().unwrap();
        assert!(sink.running);
        sink.stop().unwrap();
        assert!(!sink.running);
    }
}
