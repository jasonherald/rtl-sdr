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
//! WAV file IQ playback source module.
//!
//! Reads IQ samples from WAV files for testing and replay.

use hound::WavReader;

/// Required number of WAV channels for IQ data (I + Q).
const REQUIRED_CHANNELS: u16 = 2;

/// Expected bits per sample for supported float format.
const FLOAT32_BPS: u16 = 32;

/// Expected bits per sample for supported integer format.
const INT16_BPS: u16 = 16;

/// Int16 normalization divisor.
const INT16_SCALE: f32 = 32768.0;
use sdr_pipeline::source_manager::Source;
use sdr_types::{Complex, SourceError};
use std::path::PathBuf;

/// WAV file IQ playback source.
///
/// Reads interleaved I/Q samples from a WAV file.
/// Supports float32 and int16 WAV formats.
pub struct FileSource {
    path: PathBuf,
    sample_rate: f64,
    reader: Option<WavReaderState>,
    looping: bool,
}

struct WavReaderState {
    reader: WavReader<std::io::BufReader<std::fs::File>>,
    #[allow(dead_code)]
    channels: u16,
    #[allow(dead_code)]
    bits_per_sample: u16,
    is_float: bool,
}

impl FileSource {
    /// Create a new file source from a WAV file path.
    pub fn new(path: &std::path::Path) -> Self {
        Self {
            path: path.to_path_buf(),
            sample_rate: 0.0,
            reader: None,
            looping: false,
        }
    }

    /// Enable or disable looping playback.
    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    /// Read IQ samples from the WAV file.
    ///
    /// Returns the number of Complex samples written.
    /// Each complex sample uses 2 WAV channels (I = ch0, Q = ch1).
    pub fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        let state = self.reader.as_mut().ok_or(SourceError::NotRunning)?;
        let mut count = 0;

        if state.is_float {
            let mut samples = state.reader.samples::<f32>();
            while count < output.len() {
                let re = match samples.next() {
                    Some(Ok(v)) => v,
                    Some(Err(e)) => return Err(SourceError::OpenFailed(e.to_string())),
                    None => {
                        if self.looping {
                            // Would need to seek back to start — simplified for now
                            break;
                        }
                        break;
                    }
                };
                let im = match samples.next() {
                    Some(Ok(v)) => v,
                    Some(Err(e)) => return Err(SourceError::OpenFailed(e.to_string())),
                    None => {
                        tracing::warn!("truncated IQ frame: Q sample missing after I");
                        break;
                    }
                };
                output[count] = Complex::new(re, im);
                count += 1;
            }
        } else {
            let mut samples = state.reader.samples::<i16>();
            while count < output.len() {
                let re = match samples.next() {
                    Some(Ok(v)) => f32::from(v) / INT16_SCALE,
                    Some(Err(e)) => return Err(SourceError::OpenFailed(e.to_string())),
                    None => break,
                };
                let im_raw = match samples.next() {
                    Some(Ok(v)) => v,
                    Some(Err(e)) => return Err(SourceError::OpenFailed(e.to_string())),
                    None => {
                        tracing::warn!("truncated IQ frame: Q sample missing after I");
                        break;
                    }
                };
                let im = f32::from(im_raw) / INT16_SCALE;
                output[count] = Complex::new(re, im);
                count += 1;
            }
        }

        Ok(count)
    }
}

impl Source for FileSource {
    fn name(&self) -> &str {
        "File"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        let reader =
            WavReader::open(&self.path).map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        let spec = reader.spec();

        // Validate WAV layout: need 2 channels (I + Q)
        if spec.channels != REQUIRED_CHANNELS {
            return Err(SourceError::OpenFailed(format!(
                "WAV file must have 2 channels (I/Q), got {}",
                spec.channels
            )));
        }

        // Validate supported sample format before mutating state
        let is_float = match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Float, FLOAT32_BPS) => true,
            (hound::SampleFormat::Int, INT16_BPS) => false,
            (fmt, bps) => {
                return Err(SourceError::OpenFailed(format!(
                    "unsupported WAV format: {fmt:?} {bps}-bit; only Float32 and Int16 are supported"
                )));
            }
        };

        // Only update sample_rate after all validation passes
        self.sample_rate = f64::from(spec.sample_rate);

        self.reader = Some(WavReaderState {
            reader,
            channels: spec.channels,
            bits_per_sample: spec.bits_per_sample,
            is_float,
        });

        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.reader = None;
        Ok(())
    }

    fn tune(&mut self, _frequency_hz: f64) -> Result<(), SourceError> {
        // File source doesn't tune
        Ok(())
    }

    fn sample_rates(&self) -> &[f64] {
        // Sample rate is determined by the WAV file
        &[]
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, _rate: f64) -> Result<(), SourceError> {
        // Sample rate is fixed by the WAV file
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let source = FileSource::new(std::path::Path::new("test.wav"));
        assert_eq!(source.name(), "File");
        assert!((source.sample_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_start_nonexistent() {
        let mut source = FileSource::new(std::path::Path::new("/nonexistent.wav"));
        assert!(source.start().is_err());
    }
}
