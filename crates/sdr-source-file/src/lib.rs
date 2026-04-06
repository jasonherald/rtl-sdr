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

/// Map `hound::Error` to `SourceError`, extracting IO errors when possible.
fn map_hound_error(e: hound::Error) -> SourceError {
    match e {
        hound::Error::IoError(io) => SourceError::Io(io),
        other => SourceError::OpenFailed(other.to_string()),
    }
}

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
            loop {
                let count_before = count;
                let mut samples = state.reader.samples::<f32>();
                while count < output.len() {
                    let re = match samples.next() {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => return Err(map_hound_error(e)),
                        None => break,
                    };
                    let im = match samples.next() {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => return Err(map_hound_error(e)),
                        None => {
                            tracing::warn!("truncated IQ frame: Q sample missing after I");
                            break;
                        }
                    };
                    output[count] = Complex::new(re, im);
                    count += 1;
                }
                if count >= output.len() || !self.looping {
                    break;
                }
                // Guard: if a full pass produced nothing, the file is empty/corrupt.
                if count == count_before {
                    tracing::warn!("looping enabled but no IQ frames available; breaking");
                    break;
                }
                state.reader.seek(0).map_err(SourceError::Io)?;
            }
        } else {
            loop {
                let count_before = count;
                let mut samples = state.reader.samples::<i16>();
                while count < output.len() {
                    let re = match samples.next() {
                        Some(Ok(v)) => f32::from(v) / INT16_SCALE,
                        Some(Err(e)) => return Err(map_hound_error(e)),
                        None => break,
                    };
                    let im_raw = match samples.next() {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => return Err(map_hound_error(e)),
                        None => {
                            tracing::warn!("truncated IQ frame: Q sample missing after I");
                            break;
                        }
                    };
                    let im = f32::from(im_raw) / INT16_SCALE;
                    output[count] = Complex::new(re, im);
                    count += 1;
                }
                if count >= output.len() || !self.looping {
                    break;
                }
                if count == count_before {
                    tracing::warn!("looping enabled but no IQ frames available; breaking");
                    break;
                }
                state.reader.seek(0).map_err(SourceError::Io)?;
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
        let reader = WavReader::open(&self.path).map_err(map_hound_error)?;

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

        // Reject 0 Hz sample rate
        if spec.sample_rate == 0 {
            return Err(SourceError::OpenFailed(
                "WAV file has 0 Hz sample rate".to_string(),
            ));
        }

        // Only update sample_rate after all validation passes
        self.sample_rate = f64::from(spec.sample_rate);

        self.reader = Some(WavReaderState { reader, is_float });

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

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        // Sample rate is fixed by the WAV file — reject mismatches
        if self.sample_rate > 0.0 && (rate - self.sample_rate).abs() > 1.0 {
            return Err(SourceError::OpenFailed(format!(
                "file source sample rate is fixed at {} Hz by WAV header",
                self.sample_rate
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
        let err = source.start().unwrap_err();
        // Opening a nonexistent file yields an IO error
        assert!(
            matches!(err, SourceError::Io(_)),
            "expected Io variant, got {err:?}"
        );
    }

    /// Helper: write a 2-channel float32 WAV to a temp file and return its path.
    fn write_test_wav(name: &str, sample_rate: u32, samples: &[(f32, f32)]) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for &(i, q) in samples {
            writer.write_sample(i).unwrap();
            writer.write_sample(q).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn test_read_samples_no_loop() {
        let iq: Vec<(f32, f32)> = (0..10)
            .map(|i| (i as f32 * 0.1, -(i as f32) * 0.1))
            .collect();
        let path = write_test_wav("sdr_test_no_loop.wav", 48_000, &iq);

        let mut source = FileSource::new(&path);
        source.start().unwrap();
        assert!((source.sample_rate() - 48_000.0).abs() < f64::EPSILON);

        let mut output = vec![Complex::default(); 20];
        let count = source.read_samples(&mut output).unwrap();
        // Should read exactly 10 IQ frames (the file length), not more
        assert_eq!(count, 10);

        // Verify sample values
        for i in 0..10 {
            let expected_re = i as f32 * 0.1;
            let expected_im = -(i as f32) * 0.1;
            assert!(
                (output[i].re - expected_re).abs() < 1e-6,
                "re mismatch at {i}"
            );
            assert!(
                (output[i].im - expected_im).abs() < 1e-6,
                "im mismatch at {i}"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_samples_with_loop() {
        let iq: Vec<(f32, f32)> = (0..5).map(|i| (i as f32 * 0.2, 0.0)).collect();
        let path = write_test_wav("sdr_test_with_loop.wav", 48_000, &iq);

        let mut source = FileSource::new(&path);
        source.set_looping(true);
        source.start().unwrap();

        // Request 12 samples from a 5-sample file with looping
        let mut output = vec![Complex::default(); 12];
        let count = source.read_samples(&mut output).unwrap();
        // Should fill all 12 by wrapping around
        assert_eq!(count, 12);

        // First 5 should match the file
        for i in 0..5 {
            assert!(
                (output[i].re - i as f32 * 0.2).abs() < 1e-6,
                "first pass re mismatch at {i}"
            );
        }
        // Samples 5..10 should be the file again (second pass)
        for i in 0..5 {
            assert!(
                (output[5 + i].re - i as f32 * 0.2).abs() < 1e-6,
                "second pass re mismatch at {i}"
            );
        }
        // Samples 10..12 should be the start of a third pass
        for i in 0..2 {
            assert!(
                (output[10 + i].re - i as f32 * 0.2).abs() < 1e-6,
                "third pass re mismatch at {i}"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_samples_empty_file_no_hang() {
        let path = write_test_wav("sdr_test_empty.wav", 48_000, &[]);

        let mut source = FileSource::new(&path);
        source.set_looping(true);
        source.start().unwrap();

        // Should return 0 immediately, not hang in an infinite loop
        let mut output = vec![Complex::default(); 10];
        let count = source.read_samples(&mut output).unwrap();
        assert_eq!(count, 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_hound_error_mapping_io() {
        // An IoError should map to SourceError::Io
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "test");
        let hound_err = hound::Error::IoError(io_err);
        let source_err = map_hound_error(hound_err);
        assert!(
            matches!(source_err, SourceError::Io(_)),
            "expected Io variant, got {source_err:?}"
        );
    }

    #[test]
    fn test_hound_error_mapping_format() {
        // A non-IO hound error should map to SourceError::OpenFailed
        let hound_err = hound::Error::FormatError("bad format");
        let source_err = map_hound_error(hound_err);
        assert!(
            matches!(source_err, SourceError::OpenFailed(_)),
            "expected OpenFailed variant, got {source_err:?}"
        );
    }
}
