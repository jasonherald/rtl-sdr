//! Hardware integration tests — require a real RTL-SDR device.
//!
//! Run with: `cargo test --test hardware_integration -- --ignored --test-threads=1`
//!
//! IMPORTANT: Use `--test-threads=1` — all tests share one USB device.
//!
//! These tests are `#[ignore]` by default so CI passes without hardware.
//! They exercise the full signal chain from USB device to audio samples.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::float_cmp,
    clippy::map_unwrap_or,
    clippy::expect_fun_call,
    clippy::doc_markdown,
    clippy::panic
)]

use sdr_dsp::fft::{FftEngine, RustFftEngine};
use sdr_pipeline::iq_frontend::{FftWindow, IqFrontend};
use sdr_pipeline::source_manager::Source;
use sdr_radio::RadioModule;
use sdr_rtlsdr::RtlSdrDevice;
use sdr_source_rtlsdr::RtlSdrSource;
use sdr_types::{Complex, DemodMode, Stereo};

/// Default test frequency: 100 MHz (FM broadcast band).
const TEST_FREQ_HZ: u32 = 100_000_000;

/// Default test sample rate: 2.4 MHz.
const TEST_SAMPLE_RATE: u32 = 2_400_000;

/// Number of IQ sample pairs to read per bulk transfer.
const READ_BUF_SIZE: usize = 16384;

/// FFT size for pipeline tests.
const TEST_FFT_SIZE: usize = 2048;

// =========================================================================
// Level 1: Raw device access
// =========================================================================

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn device_enumerate() {
    let count = sdr_rtlsdr::get_device_count();
    assert!(count > 0, "No RTL-SDR devices found — is one plugged in?");
    tracing::info!("Found {count} RTL-SDR device(s)");

    let name = sdr_rtlsdr::get_device_name(0);
    tracing::info!("Device 0: {name}");
    assert!(!name.is_empty());
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn device_open_close() {
    let device = RtlSdrDevice::open(0).expect("Failed to open RTL-SDR device");
    let tuner = device.tuner_type();
    tracing::info!("Tuner type: {tuner:?}");
    drop(device);
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn device_set_sample_rate() {
    let mut device = RtlSdrDevice::open(0).expect("Failed to open device");
    device
        .set_sample_rate(TEST_SAMPLE_RATE)
        .expect("Failed to set sample rate");
    let actual = device.sample_rate();
    tracing::info!("Requested {TEST_SAMPLE_RATE} Hz, got {actual} Hz");
    // RTL-SDR may round slightly
    assert!(
        (actual as i64 - TEST_SAMPLE_RATE as i64).unsigned_abs() < 1000,
        "Sample rate mismatch: expected ~{TEST_SAMPLE_RATE}, got {actual}"
    );
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn device_tune_and_read_iq() {
    let mut device = RtlSdrDevice::open(0).expect("Failed to open device");
    device
        .set_sample_rate(TEST_SAMPLE_RATE)
        .expect("set sample rate");
    device
        .set_center_freq(TEST_FREQ_HZ)
        .expect("set center freq");
    device.reset_buffer().expect("reset buffer");

    // Read a chunk of raw IQ data
    let mut buf = vec![0u8; READ_BUF_SIZE * 2]; // 2 bytes per IQ pair
    let bytes_read = device.read_sync(&mut buf).expect("read_sync failed");
    tracing::info!("Read {bytes_read} bytes ({} IQ pairs)", bytes_read / 2);

    assert!(bytes_read > 0, "No data received from device");
    assert_eq!(bytes_read % 2, 0, "Odd byte count — IQ misalignment");

    // Verify data isn't all zeros (device is producing something)
    let nonzero = buf[..bytes_read].iter().filter(|&&b| b != 0).count();
    assert!(
        nonzero > bytes_read / 4,
        "Too many zero bytes — device may not be streaming"
    );
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn device_convert_uint8_to_complex() {
    let mut device = RtlSdrDevice::open(0).expect("Failed to open device");
    device
        .set_sample_rate(TEST_SAMPLE_RATE)
        .expect("set sample rate");
    device
        .set_center_freq(TEST_FREQ_HZ)
        .expect("set center freq");
    device.reset_buffer().expect("reset buffer");

    let mut raw = vec![0u8; READ_BUF_SIZE * 2];
    let bytes_read = device.read_sync(&mut raw).expect("read_sync");
    let iq_count = bytes_read / 2;

    // Convert to Complex f32
    let mut iq = vec![Complex::default(); iq_count];
    let converted = RtlSdrSource::convert_samples(&raw[..bytes_read], &mut iq);
    assert_eq!(converted, iq_count);

    // All values should be in [-1.0, 1.0] range
    for (i, s) in iq.iter().enumerate().take(converted) {
        assert!(
            s.re >= -1.1 && s.re <= 1.1,
            "Sample {i}: re={} out of range",
            s.re
        );
        assert!(
            s.im >= -1.1 && s.im <= 1.1,
            "Sample {i}: im={} out of range",
            s.im
        );
    }

    // Compute mean magnitude — should be nonzero (device is receiving noise at least)
    let mean_mag: f32 = iq[..converted]
        .iter()
        .map(|s| (s.re * s.re + s.im * s.im).sqrt())
        .sum::<f32>()
        / converted as f32;
    tracing::info!("Mean IQ magnitude: {mean_mag:.4}");
    assert!(mean_mag > 0.001, "Mean magnitude too low: {mean_mag}");
}

// =========================================================================
// Level 2: Pipeline — Source + IqFrontend + FFT
// =========================================================================

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn pipeline_source_start_stop() {
    let mut source = RtlSdrSource::new(0);
    assert_eq!(source.name(), "RTL-SDR");

    source.start().expect("Failed to start source");
    source.stop().expect("Failed to stop source");
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn pipeline_iq_frontend_produces_fft() {
    // Open device and read raw IQ
    let mut device = RtlSdrDevice::open(0).expect("open device");
    device
        .set_sample_rate(TEST_SAMPLE_RATE)
        .expect("set sample rate");
    device
        .set_center_freq(TEST_FREQ_HZ)
        .expect("set center freq");
    device.reset_buffer().expect("reset buffer");

    let mut raw = vec![0u8; READ_BUF_SIZE * 2];
    let bytes_read = device.read_sync(&mut raw).expect("read_sync");
    let iq_count = bytes_read / 2;

    // Convert to Complex
    let mut iq = vec![Complex::default(); iq_count];
    RtlSdrSource::convert_samples(&raw[..bytes_read], &mut iq);

    // Process through IqFrontend
    let mut frontend = IqFrontend::new(
        TEST_SAMPLE_RATE as f64,
        1, // no decimation
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        true, // DC blocking
    )
    .expect("create IqFrontend");

    let mut output = vec![Complex::default(); iq_count];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];

    let (processed, fft_ready) = frontend
        .process(&iq[..iq_count], &mut output, &mut fft_out)
        .expect("IqFrontend::process");

    tracing::info!("IqFrontend: processed {processed} samples, FFT ready: {fft_ready}");
    assert!(processed > 0, "No samples processed");

    assert!(
        iq_count >= TEST_FFT_SIZE,
        "Need at least {TEST_FFT_SIZE} IQ samples for FFT validation, got {iq_count}"
    );
    assert!(fft_ready, "FFT should be ready with {iq_count} samples");

    // FFT output should be in dB range with finite values
    let fft_min = fft_out.iter().copied().reduce(f32::min).unwrap_or(0.0);
    let fft_max = fft_out.iter().copied().reduce(f32::max).unwrap_or(0.0);
    tracing::info!("FFT range: {fft_min:.1} to {fft_max:.1} dB");

    assert!(
        fft_min.is_finite() && fft_max.is_finite(),
        "Non-finite FFT dB values"
    );
    assert!(
        fft_max > fft_min,
        "Flat FFT output indicates invalid spectrum computation"
    );
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn pipeline_fft_standalone() {
    // Read raw IQ, convert, run FFT directly (bypassing IqFrontend)
    let mut device = RtlSdrDevice::open(0).expect("open device");
    device
        .set_sample_rate(TEST_SAMPLE_RATE)
        .expect("set sample rate");
    device
        .set_center_freq(TEST_FREQ_HZ)
        .expect("set center freq");
    device.reset_buffer().expect("reset buffer");

    let mut raw = vec![0u8; TEST_FFT_SIZE * 2 * 2]; // enough for fft_size IQ pairs
    let bytes_read = device.read_sync(&mut raw).expect("read_sync");

    let mut iq = vec![Complex::default(); bytes_read / 2];
    let count = RtlSdrSource::convert_samples(&raw[..bytes_read], &mut iq);

    assert!(
        count >= TEST_FFT_SIZE,
        "Need at least {TEST_FFT_SIZE} IQ samples, got {count}"
    );

    let mut engine = RustFftEngine::new(TEST_FFT_SIZE).expect("create FFT engine");
    let mut fft_buf = iq[..TEST_FFT_SIZE].to_vec();
    engine.forward(&mut fft_buf).expect("FFT forward");

    let mut power = vec![0.0_f32; TEST_FFT_SIZE];
    sdr_dsp::fft::power_spectrum_db(&fft_buf, &mut power).expect("power_spectrum_db");
    assert!(
        power.iter().all(|v| v.is_finite()),
        "Non-finite power spectrum values"
    );

    let peak_bin = power
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let peak_db = power[peak_bin];
    let min_db = power.iter().copied().reduce(f32::min).unwrap_or(peak_db);
    tracing::info!(
        "FFT peak at bin {peak_bin} ({:.1} dB), noise floor ~{:.1} dB",
        peak_db,
        power.iter().sum::<f32>() / power.len() as f32
    );
    assert!(peak_db > min_db, "No spectral dynamic range detected");
}

// =========================================================================
// Level 3: End-to-end — Device → IqFrontend → RadioModule → Audio
// =========================================================================

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn end_to_end_nfm_demod() {
    end_to_end_demod(DemodMode::Nfm, "NFM");
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn end_to_end_wfm_demod() {
    end_to_end_demod(DemodMode::Wfm, "WFM");
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn end_to_end_am_demod() {
    end_to_end_demod(DemodMode::Am, "AM");
}

/// Shared end-to-end test: Device → IqFrontend → RadioModule → Stereo audio
fn end_to_end_demod(mode: DemodMode, mode_name: &str) {
    // 1. Open device and read IQ
    let mut device = RtlSdrDevice::open(0).expect("open device");
    device
        .set_sample_rate(TEST_SAMPLE_RATE)
        .expect("set sample rate");
    device
        .set_center_freq(TEST_FREQ_HZ)
        .expect("set center freq");
    device.reset_buffer().expect("reset buffer");

    let mut raw = vec![0u8; READ_BUF_SIZE * 4]; // read a good chunk
    let bytes_read = device.read_sync(&mut raw).expect("read_sync");
    let iq_count = bytes_read / 2;

    let mut iq = vec![Complex::default(); iq_count];
    RtlSdrSource::convert_samples(&raw[..bytes_read], &mut iq);

    // 2. Process through IqFrontend (no decimation for simplicity)
    let mut frontend = IqFrontend::new(
        TEST_SAMPLE_RATE as f64,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        true,
    )
    .expect("create IqFrontend");

    let mut processed_iq = vec![Complex::default(); iq_count];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    let (processed_count, _) = frontend
        .process(&iq[..iq_count], &mut processed_iq, &mut fft_out)
        .expect("IqFrontend::process");

    tracing::info!("{mode_name}: IqFrontend produced {processed_count} samples");

    // 3. Process through RadioModule
    let mut radio = RadioModule::new(48_000.0).expect("create RadioModule");
    radio.set_mode(mode).expect("set mode");

    let max_out = radio.max_output_samples(processed_count);
    let mut audio_out = vec![Stereo::default(); max_out];

    let audio_count = radio
        .process(&processed_iq[..processed_count], &mut audio_out)
        .expect("RadioModule::process");

    tracing::info!(
        "{mode_name}: RadioModule produced {audio_count} stereo audio samples from {processed_count} IQ"
    );

    assert!(
        audio_count > 0,
        "{mode_name}: expected audio output, got 0 samples"
    );

    // 4. Verify audio is in reasonable range
    let mut max_abs = 0.0_f32;
    for s in &audio_out[..audio_count] {
        max_abs = max_abs.max(s.l.abs()).max(s.r.abs());
    }
    tracing::info!("{mode_name}: max audio amplitude: {max_abs:.4}");

    // Audio should be finite and not absurdly large
    assert!(max_abs.is_finite(), "{mode_name}: audio contains NaN/Inf");
    assert!(
        max_abs < 100.0,
        "{mode_name}: audio amplitude too high: {max_abs}"
    );
}

#[test]
#[ignore = "requires RTL-SDR hardware"]
fn end_to_end_mode_switching() {
    // Read IQ once, then process through all modes
    let mut device = RtlSdrDevice::open(0).expect("open device");
    device
        .set_sample_rate(TEST_SAMPLE_RATE)
        .expect("set sample rate");
    device
        .set_center_freq(TEST_FREQ_HZ)
        .expect("set center freq");
    device.reset_buffer().expect("reset buffer");

    let mut raw = vec![0u8; READ_BUF_SIZE * 4];
    let bytes_read = device.read_sync(&mut raw).expect("read_sync");
    let iq_count = bytes_read / 2;

    let mut iq = vec![Complex::default(); iq_count];
    RtlSdrSource::convert_samples(&raw[..bytes_read], &mut iq);

    let mut frontend = IqFrontend::new(
        TEST_SAMPLE_RATE as f64,
        1,
        TEST_FFT_SIZE,
        FftWindow::Nuttall,
        true,
    )
    .expect("create IqFrontend");

    let mut processed_iq = vec![Complex::default(); iq_count];
    let mut fft_out = vec![0.0_f32; TEST_FFT_SIZE];
    let (processed_count, _) = frontend
        .process(&iq[..iq_count], &mut processed_iq, &mut fft_out)
        .expect("IqFrontend::process");

    let mut radio = RadioModule::new(48_000.0).expect("create RadioModule");

    let modes = [
        DemodMode::Nfm,
        DemodMode::Wfm,
        DemodMode::Am,
        DemodMode::Usb,
        DemodMode::Lsb,
        DemodMode::Dsb,
        DemodMode::Cw,
        DemodMode::Raw,
    ];

    for mode in &modes {
        radio
            .set_mode(*mode)
            .unwrap_or_else(|e| panic!("set mode {mode:?}: {e}"));

        let max_out = radio.max_output_samples(processed_count);
        let mut audio_out = vec![Stereo::default(); max_out];

        let audio_count = radio
            .process(&processed_iq[..processed_count], &mut audio_out)
            .unwrap_or_else(|e| panic!("{mode:?}::process: {e}"));

        tracing::info!("{mode:?}: {audio_count} audio samples from {processed_count} IQ");
        assert!(audio_count > 0, "{mode:?}: no audio output");

        // Verify no NaN/Inf
        for (i, s) in audio_out[..audio_count].iter().enumerate() {
            assert!(
                s.l.is_finite() && s.r.is_finite(),
                "{mode:?}: NaN/Inf at sample {i}"
            );
        }
    }

    tracing::info!("All 8 demod modes produced valid audio from real hardware IQ");
}
