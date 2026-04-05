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
//! RTL-SDR source module — wraps sdr-rtlsdr for the pipeline.
//!
//! Converts raw uint8 IQ samples from the USB device to f32 Complex
//! samples for the signal processing pipeline.

use sdr_pipeline::source_manager::Source;
use sdr_rtlsdr::RtlSdrDevice;
use sdr_types::{Complex, SourceError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// IQ sample conversion factor: `(sample - 127.4) / 128.0`
///
/// Matches SDR++ `RTLSDRSourceModule::asyncHandler`.
const IQ_OFFSET: f32 = 127.4;
const IQ_SCALE: f32 = 128.0;

/// RTL-SDR USB sample rates (Hz).
pub const SAMPLE_RATES: &[f64] = &[
    250_000.0,
    1_024_000.0,
    1_536_000.0,
    1_792_000.0,
    1_920_000.0,
    2_048_000.0,
    2_160_000.0,
    2_400_000.0,
    2_560_000.0,
    2_880_000.0,
    3_200_000.0,
];

/// RTL-SDR IQ source for the pipeline.
///
/// Ports SDR++ `RTLSDRSourceModule`. Opens the RTL-SDR device,
/// configures it, and converts uint8 IQ pairs to f32 Complex samples.
pub struct RtlSdrSource {
    device: Option<RtlSdrDevice>,
    device_index: u32,
    sample_rate: f64,
    frequency: f64,
    running: Arc<AtomicBool>,
}

impl RtlSdrSource {
    /// Create a new RTL-SDR source for the device at the given index.
    pub fn new(device_index: u32) -> Self {
        Self {
            device: None,
            device_index,
            sample_rate: SAMPLE_RATES[7], // 2.4 MHz default
            frequency: 100_000_000.0,     // 100 MHz default
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Convert a buffer of raw uint8 IQ pairs to Complex f32 samples.
    ///
    /// Ports the conversion from SDR++ `asyncHandler`:
    /// `re = (buf[i*2] - 127.4) / 128.0; im = (buf[i*2+1] - 127.4) / 128.0`
    pub fn convert_samples(raw: &[u8], output: &mut [Complex]) -> usize {
        let sample_count = raw.len() / 2;
        let count = sample_count.min(output.len());
        for i in 0..count {
            let re = (f32::from(raw[i * 2]) - IQ_OFFSET) / IQ_SCALE;
            let im = (f32::from(raw[i * 2 + 1]) - IQ_OFFSET) / IQ_SCALE;
            output[i] = Complex::new(re, im);
        }
        count
    }
}

impl Source for RtlSdrSource {
    fn name(&self) -> &str {
        "RTL-SDR"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        let mut device = RtlSdrDevice::open(self.device_index)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        device
            .set_sample_rate(self.sample_rate as u32)
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        device
            .set_center_freq(self.frequency as u32)
            .map_err(|e| SourceError::TuneFailed(e.to_string()))?;

        device
            .reset_buffer()
            .map_err(|e| SourceError::OpenFailed(e.to_string()))?;

        self.device = Some(device);
        self.running.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.running.store(false, Ordering::Relaxed);
        self.device = None; // Drop closes the device
        Ok(())
    }

    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        self.frequency = frequency_hz;
        if let Some(device) = &mut self.device {
            device
                .set_center_freq(frequency_hz as u32)
                .map_err(|e| SourceError::TuneFailed(e.to_string()))?;
        }
        Ok(())
    }

    fn sample_rates(&self) -> &[f64] {
        SAMPLE_RATES
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        self.sample_rate = rate;
        if let Some(device) = &mut self.device {
            device
                .set_sample_rate(rate as u32)
                .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_samples() {
        // 127 should give ~-0.003 (near zero), 255 should give ~0.997
        let raw = [127, 127, 255, 0, 0, 255];
        let mut output = [Complex::default(); 3];
        let count = RtlSdrSource::convert_samples(&raw, &mut output);
        assert_eq!(count, 3);

        // Sample 0: (127 - 127.4) / 128 ≈ -0.003125
        assert!((output[0].re - (-0.003_125)).abs() < 0.001);
        assert!((output[0].im - (-0.003_125)).abs() < 0.001);

        // Sample 1: re = (255 - 127.4) / 128 ≈ 0.997
        assert!((output[1].re - 0.997).abs() < 0.01);
        // im = (0 - 127.4) / 128 ≈ -0.995
        assert!((output[1].im - (-0.995)).abs() < 0.01);
    }

    #[test]
    fn test_sample_rates() {
        assert_eq!(SAMPLE_RATES.len(), 11);
        assert!((SAMPLE_RATES[0] - 250_000.0).abs() < 1.0);
        assert!((SAMPLE_RATES[10] - 3_200_000.0).abs() < 1.0);
    }

    #[test]
    fn test_new() {
        let source = RtlSdrSource::new(0);
        assert_eq!(source.name(), "RTL-SDR");
        assert!((source.sample_rate() - 2_400_000.0).abs() < 1.0);
    }
}
