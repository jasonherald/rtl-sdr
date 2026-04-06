//! Stub audio sink — used when PipeWire feature is not enabled.

use sdr_pipeline::sink_manager::Sink;
use sdr_types::{SinkError, Stereo};

/// Default audio sample rate (Hz).
const DEFAULT_AUDIO_SAMPLE_RATE: f64 = 48_000.0;

/// Stub audio output sink (no backend).
pub struct AudioSink {
    sample_rate: f64,
}

impl AudioSink {
    /// Create a new stub audio sink.
    pub fn new() -> Self {
        Self {
            sample_rate: DEFAULT_AUDIO_SAMPLE_RATE,
        }
    }

    /// Stub — drops samples silently.
    ///
    /// # Errors
    ///
    /// Always returns `Ok` (samples are discarded).
    pub fn write_samples(&self, _samples: &[Stereo]) -> Result<(), SinkError> {
        Ok(())
    }
}

impl Default for AudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for AudioSink {
    fn name(&self) -> &str {
        "Audio (stub)"
    }

    fn start(&mut self) -> Result<(), SinkError> {
        tracing::warn!(
            "audio sink not available — compile with `pipewire` feature for audio output"
        );
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SinkError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(SinkError::InvalidParameter(format!(
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
