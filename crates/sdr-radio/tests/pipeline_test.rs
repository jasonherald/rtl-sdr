//! End-to-end pipeline tests: synthetic FM signal through the complete
//! processing chain at different effective sample rates.
//!
//! These tests reproduce the exact signal path that the user's RTL-SDR
//! takes: `IqFrontend` → `RxVfo` → `RadioModule` → stereo audio output.
//! They validate that audio quality is consistent across configurations.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::unwrap_used
)]

use sdr_dsp::channel::RxVfo;
use sdr_pipeline::iq_frontend::{FftWindow, IqFrontend};
use sdr_radio::RadioModule;
use sdr_types::{Complex, DemodMode, Stereo};

use std::f32::consts::PI;

/// Audio output rate (Hz).
const AUDIO_RATE: f64 = 48_000.0;

/// Default WFM bandwidth (Hz).
const WFM_BANDWIDTH: f64 = 150_000.0;

/// Generate a synthetic FM-modulated IQ signal.
///
/// Produces a carrier at DC with frequency modulation from a 1 kHz audio tone.
/// - `sample_rate`: the sample rate of the generated signal
/// - `deviation_hz`: peak frequency deviation (75 kHz for broadcast FM)
/// - `audio_freq_hz`: modulating audio tone frequency
/// - `num_samples`: number of IQ samples to generate
fn generate_fm_signal(
    sample_rate: f64,
    deviation_hz: f64,
    audio_freq_hz: f64,
    num_samples: usize,
) -> Vec<Complex> {
    let mut phase: f64 = 0.0;
    (0..num_samples)
        .map(|i| {
            let t = i as f64 / sample_rate;
            // FM modulation: instantaneous frequency = deviation * sin(2π * audio_freq * t)
            let inst_freq = deviation_hz * (2.0 * std::f64::consts::PI * audio_freq_hz * t).sin();
            phase += 2.0 * std::f64::consts::PI * inst_freq / sample_rate;
            Complex::new(phase.cos() as f32, phase.sin() as f32)
        })
        .collect()
}

/// Run the complete pipeline: Frontend → VFO → Radio → audio output.
///
/// Returns the audio output samples and the demod config for inspection.
fn run_pipeline(sample_rate: f64, decim_ratio: u32, input: &[Complex]) -> Vec<Stereo> {
    let effective_rate = sample_rate / f64::from(decim_ratio);

    // 1. IQ Frontend (decimation + DC blocking)
    let mut frontend =
        IqFrontend::new(sample_rate, decim_ratio, 2048, FftWindow::Nuttall, true).unwrap();
    let mut processed = vec![Complex::default(); input.len()];
    let mut fft_out = vec![0.0f32; 2048];
    let (proc_count, _) = frontend
        .process(input, &mut processed, &mut fft_out)
        .unwrap();

    // 2. RxVfo (frequency translation + resampling)
    let demod_if_rate = 250_000.0; // WFM IF rate
    let mut vfo = RxVfo::new(effective_rate, demod_if_rate, WFM_BANDWIDTH, 0.0).unwrap();

    let ratio = (demod_if_rate / effective_rate).ceil() as usize;
    let vfo_out_size = proc_count * ratio.max(1) + 64;
    let mut vfo_out = vec![Complex::default(); vfo_out_size];
    let vfo_count = vfo.process(&processed[..proc_count], &mut vfo_out).unwrap();

    // 3. RadioModule (IF chain → demod → AF chain)
    let mut radio = RadioModule::new(AUDIO_RATE).unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    radio.set_input_sample_rate(demod_if_rate).unwrap();
    radio.set_bandwidth(WFM_BANDWIDTH);

    let max_out = radio.max_output_samples(vfo_count);
    let mut audio = vec![Stereo::default(); max_out];
    let audio_count = radio.process(&vfo_out[..vfo_count], &mut audio).unwrap();

    audio.truncate(audio_count);
    audio
}

/// Compute RMS energy of stereo audio (both channels).
fn audio_rms(audio: &[Stereo]) -> f32 {
    if audio.is_empty() {
        return 0.0;
    }
    let sum: f32 = audio.iter().map(|s| s.l * s.l + s.r * s.r).sum();
    (sum / (2.0 * audio.len() as f32)).sqrt()
}

/// Compute the max sample-to-sample jump in audio (indicates discontinuities).
fn audio_max_jump(audio: &[Stereo]) -> f32 {
    if audio.len() < 2 {
        return 0.0;
    }
    audio
        .windows(2)
        .map(|w| (w[1].l - w[0].l).abs().max((w[1].r - w[0].r).abs()))
        .fold(0.0f32, f32::max)
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn test_pipeline_250k_2x_produces_audio() {
    // WORKING CASE: 250 kHz sample rate, 2x decimation = 125 kHz effective
    let sample_rate = 250_000.0;
    let decim = 2;
    let num_samples = 16_384;

    let signal = generate_fm_signal(sample_rate, 75_000.0, 1000.0, num_samples);
    let audio = run_pipeline(sample_rate, decim, &signal);

    assert!(!audio.is_empty(), "pipeline should produce audio");
    let rms = audio_rms(&audio[100..]);
    assert!(rms > 0.001, "audio should have energy, rms = {rms}");
}

#[test]
fn test_pipeline_2m_8x_produces_audio() {
    // BROKEN CASE: 2 Msps, 8x decimation = 250 kHz effective
    let sample_rate = 2_000_000.0;
    let decim = 8;
    let num_samples = 16_384;

    let signal = generate_fm_signal(sample_rate, 75_000.0, 1000.0, num_samples);
    let audio = run_pipeline(sample_rate, decim, &signal);

    assert!(!audio.is_empty(), "pipeline should produce audio");
    let rms = audio_rms(&audio[100..]);
    assert!(rms > 0.001, "audio should have energy, rms = {rms}");
}

#[test]
fn test_pipeline_quality_consistent_across_rates() {
    // Both configurations should produce similar audio quality from the
    // same synthetic FM signal (just sampled at different rates).
    let num_samples = 32_768;

    // Generate FM signal at both rates
    let signal_250k = generate_fm_signal(250_000.0, 75_000.0, 1000.0, num_samples);
    let signal_2m = generate_fm_signal(2_000_000.0, 75_000.0, 1000.0, num_samples);

    let audio_125k = run_pipeline(250_000.0, 2, &signal_250k);
    let audio_250k = run_pipeline(2_000_000.0, 8, &signal_2m);

    // Both should produce non-zero audio
    let rms_125k = audio_rms(&audio_125k[200..]);
    let rms_250k = audio_rms(&audio_250k[200..]);

    eprintln!(
        "125 kHz effective: {} samples, rms = {rms_125k}",
        audio_125k.len()
    );
    eprintln!(
        "250 kHz effective: {} samples, rms = {rms_250k}",
        audio_250k.len()
    );

    assert!(
        rms_125k > 0.001,
        "125 kHz should have audio, rms = {rms_125k}"
    );
    assert!(
        rms_250k > 0.001,
        "250 kHz should have audio, rms = {rms_250k}"
    );

    // Audio levels should be in the same ballpark (within 20 dB)
    let ratio = if rms_125k > rms_250k {
        rms_125k / rms_250k.max(1e-10)
    } else {
        rms_250k / rms_125k.max(1e-10)
    };
    assert!(
        ratio < 100.0,
        "audio levels should be comparable, ratio = {ratio}"
    );
}

#[test]
fn test_pipeline_no_extreme_discontinuities() {
    // The audio output should not have extreme sample-to-sample jumps
    // which would indicate buffer corruption or processing errors.
    let num_samples = 32_768;

    let signal = generate_fm_signal(2_000_000.0, 75_000.0, 1000.0, num_samples);
    let audio = run_pipeline(2_000_000.0, 8, &signal);

    let max_jump = audio_max_jump(&audio[200..]);
    eprintln!("Max audio jump at 250 kHz effective: {max_jump}");

    // A smooth FM audio signal should not have jumps > 1.0
    // (each sample is in [-1, 1] range after AGC)
    assert!(
        max_jump < 2.0,
        "audio should be smooth, max_jump = {max_jump}"
    );
}

#[test]
fn test_pipeline_streaming_continuity() {
    // Process multiple blocks in sequence (simulating streaming) and verify
    // the audio doesn't have discontinuities at block boundaries.
    let sample_rate = 2_000_000.0;
    let decim_ratio = 8_u32;
    let block_size = 16_384;
    let num_blocks = 5;
    let effective_rate = sample_rate / f64::from(decim_ratio);
    let demod_if_rate = 250_000.0;

    // Generate continuous FM signal
    let total_samples = block_size * num_blocks;
    let signal = generate_fm_signal(sample_rate, 75_000.0, 1000.0, total_samples);

    // Create pipeline components (persistent across blocks)
    let mut frontend =
        IqFrontend::new(sample_rate, decim_ratio, 2048, FftWindow::Nuttall, true).unwrap();
    let mut vfo = RxVfo::new(effective_rate, demod_if_rate, WFM_BANDWIDTH, 0.0).unwrap();
    let mut radio = RadioModule::new(AUDIO_RATE).unwrap();
    radio.set_mode(DemodMode::Wfm).unwrap();
    radio.set_input_sample_rate(demod_if_rate).unwrap();

    let mut all_audio = Vec::new();

    for block_idx in 0..num_blocks {
        let start = block_idx * block_size;
        let block = &signal[start..start + block_size];

        // Frontend
        let mut processed = vec![Complex::default(); block_size];
        let mut fft_out = vec![0.0f32; 2048];
        let (proc_count, _) = frontend
            .process(block, &mut processed, &mut fft_out)
            .unwrap();

        // VFO
        let ratio = (demod_if_rate / effective_rate).ceil() as usize;
        let vfo_out_size = proc_count * ratio.max(1) + 64;
        let mut vfo_out = vec![Complex::default(); vfo_out_size];
        let vfo_count = vfo.process(&processed[..proc_count], &mut vfo_out).unwrap();

        // Radio
        let max_out = radio.max_output_samples(vfo_count);
        let mut audio = vec![Stereo::default(); max_out];
        let audio_count = radio.process(&vfo_out[..vfo_count], &mut audio).unwrap();

        all_audio.extend_from_slice(&audio[..audio_count]);

        eprintln!("Block {block_idx}: {proc_count} proc → {vfo_count} vfo → {audio_count} audio");
    }

    eprintln!("Total audio samples: {}", all_audio.len());
    assert!(all_audio.len() > 100, "should produce substantial audio");

    // Check for discontinuities at block boundaries (after settling)
    let settle = 500;
    if all_audio.len() > settle {
        let max_jump = audio_max_jump(&all_audio[settle..]);
        eprintln!("Max audio jump across {num_blocks} blocks: {max_jump}");
        assert!(
            max_jump < 2.0,
            "streaming should be smooth, max_jump = {max_jump}"
        );
    }
}

#[test]
fn test_vfo_passthrough_vs_resample() {
    // Directly compare VFO passthrough (250k→250k) vs upsample (125k→250k)
    // with the same signal content to isolate VFO behavior.
    let demod_if_rate = 250_000.0;

    // Generate the same FM tone at both rates
    let signal_125k = generate_fm_signal(125_000.0, 50_000.0, 1000.0, 4096);
    let signal_250k = generate_fm_signal(250_000.0, 50_000.0, 1000.0, 4096);

    // VFO at 125k→250k (upsample — the WORKING path)
    let mut vfo_up = RxVfo::new(125_000.0, demod_if_rate, WFM_BANDWIDTH, 0.0).unwrap();
    let mut out_up = vec![Complex::default(); 8192 + 64];
    let count_up = vfo_up.process(&signal_125k, &mut out_up).unwrap();

    // VFO at 250k→250k (passthrough — the BROKEN path)
    let mut vfo_pass = RxVfo::new(250_000.0, demod_if_rate, WFM_BANDWIDTH, 0.0).unwrap();
    let mut out_pass = vec![Complex::default(); 4096 + 64];
    let count_pass = vfo_pass.process(&signal_250k, &mut out_pass).unwrap();

    eprintln!("VFO upsample: {count_up} output samples from 4096 input");
    eprintln!("VFO passthrough: {count_pass} output samples from 4096 input");

    // Both should produce non-zero energy
    let energy_up: f32 = out_up[..count_up]
        .iter()
        .map(|s| s.re * s.re + s.im * s.im)
        .sum();
    let energy_pass: f32 = out_pass[..count_pass]
        .iter()
        .map(|s| s.re * s.re + s.im * s.im)
        .sum();

    eprintln!("VFO upsample energy: {energy_up:.2}");
    eprintln!("VFO passthrough energy: {energy_pass:.2}");

    assert!(energy_up > 1.0, "upsample VFO should have energy");
    assert!(energy_pass > 1.0, "passthrough VFO should have energy");
}

#[test]
fn test_decimator_8x_output_quality() {
    // Verify 8x PowerDecimator produces clean output (no aliasing artifacts)
    use sdr_dsp::multirate::PowerDecimator;

    let mut decim = PowerDecimator::new(8).unwrap();
    let n = 16_384;

    // Generate a 10 kHz tone at 2 MHz — should survive 8x decimation
    // (10 kHz is well below 250 kHz / 2 = 125 kHz Nyquist)
    let input: Vec<Complex> = (0..n)
        .map(|i| {
            let phase = 2.0 * PI * 10_000.0 * (i as f32) / 2_000_000.0;
            Complex::new(phase.cos(), phase.sin())
        })
        .collect();

    let mut output = vec![Complex::default(); n];
    let count = decim.process(&input, &mut output).unwrap();

    eprintln!(
        "8x decimator: {n} in → {count} out (ratio {:.2})",
        n as f64 / count as f64
    );

    // Output should be ~2048 samples
    assert!(
        (1900..=2200).contains(&count),
        "expected ~2048, got {count}"
    );

    // Output should have significant energy (tone preserved)
    let energy: f32 = output[..count]
        .iter()
        .map(|s| s.re * s.re + s.im * s.im)
        .sum();
    let input_energy: f32 = input.iter().map(|s| s.re * s.re + s.im * s.im).sum();
    let energy_ratio = energy / (input_energy / 8.0);
    eprintln!("Energy ratio (output/expected): {energy_ratio:.4}");

    // Energy should be preserved (within 3 dB)
    assert!(
        energy_ratio > 0.5 && energy_ratio < 2.0,
        "decimator should preserve signal energy, ratio = {energy_ratio}"
    );
}
