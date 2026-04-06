//! Stub audio sink — used when PipeWire feature is not enabled.

use sdr_pipeline::sink_manager::Sink;
use sdr_types::{SinkError, Stereo};

/// Default audio sample rate (Hz).
const DEFAULT_AUDIO_SAMPLE_RATE: f64 = 48_000.0;

/// Log once every N calls to `write_samples` to confirm the stub is active.
const STUB_LOG_INTERVAL: u64 = 1_000;

/// Stub audio output sink (no backend).
pub struct AudioSink {
    sample_rate: f64,
    write_count: u64,
}

impl AudioSink {
    /// Create a new stub audio sink.
    pub fn new() -> Self {
        Self {
            sample_rate: DEFAULT_AUDIO_SAMPLE_RATE,
            write_count: 0,
        }
    }

    /// Stub — drops samples with periodic debug logging.
    ///
    /// Logs once every 1000 calls so operators can confirm audio data
    /// is flowing even when no real backend is compiled in.
    ///
    /// # Errors
    ///
    /// Always returns `Ok` (samples are discarded).
    pub fn write_samples(&mut self, samples: &[Stereo]) -> Result<(), SinkError> {
        self.write_count += 1;
        if self.write_count % STUB_LOG_INTERVAL == 0 {
            tracing::debug!(
                calls = self.write_count,
                samples = samples.len(),
                "stub audio sink: discarding samples (no backend)"
            );
        }
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
