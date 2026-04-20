//! Signal type conversion functions.
//!
//! Ports SDR++ `dsp::convert` namespace. Most functions are stateless
//! and operate on slices, converting between Complex, Stereo, and f32
//! types.
//!
//! # Caller-stateful helpers
//!
//! [`stereo_48k_to_mono_16k`] is the one exception — it carries a
//! decimation phase across calls via a `&mut usize` parameter. Callers
//! must preserve that variable between calls on the same stream so
//! successive input chunks whose lengths aren't multiples of the
//! decimation factor form a continuous 3:1 output instead of resetting
//! the decimator's sample phase on every invocation. Initialize the
//! phase to `0` on the first call after enabling the tap (or after a
//! reset); the function leaves `*phase` in `0..DECIMATION_FACTOR` on
//! return. See the function's docstring for the full contract.

use sdr_types::{Complex, DspError, Stereo};

/// Extract real part from complex samples.
///
/// Ports SDR++ `dsp::convert::ComplexToReal`.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
pub fn complex_to_real(input: &[Complex], output: &mut [f32]) -> Result<usize, DspError> {
    if output.len() < input.len() {
        return Err(DspError::BufferTooSmall {
            need: input.len(),
            got: output.len(),
        });
    }
    for (i, s) in input.iter().enumerate() {
        output[i] = s.re;
    }
    Ok(input.len())
}

/// Convert real samples to complex (imaginary part set to zero).
///
/// Ports SDR++ `dsp::convert::RealToComplex`.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
pub fn real_to_complex(input: &[f32], output: &mut [Complex]) -> Result<usize, DspError> {
    if output.len() < input.len() {
        return Err(DspError::BufferTooSmall {
            need: input.len(),
            got: output.len(),
        });
    }
    for (i, &s) in input.iter().enumerate() {
        output[i] = Complex::new(s, 0.0);
    }
    Ok(input.len())
}

/// Convert mono (f32) samples to stereo (same value in both channels).
///
/// Ports SDR++ `dsp::convert::MonoToStereo`.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
pub fn mono_to_stereo(input: &[f32], output: &mut [Stereo]) -> Result<usize, DspError> {
    if output.len() < input.len() {
        return Err(DspError::BufferTooSmall {
            need: input.len(),
            got: output.len(),
        });
    }
    for (i, &s) in input.iter().enumerate() {
        output[i] = Stereo::mono(s);
    }
    Ok(input.len())
}

/// Convert stereo samples to mono by averaging L and R channels.
///
/// Ports SDR++ `dsp::convert::StereoToMono`. Formula: `(l + r) / 2`.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
pub fn stereo_to_mono(input: &[Stereo], output: &mut [f32]) -> Result<usize, DspError> {
    if output.len() < input.len() {
        return Err(DspError::BufferTooSmall {
            need: input.len(),
            got: output.len(),
        });
    }
    for (i, s) in input.iter().enumerate() {
        output[i] = f32::midpoint(s.l, s.r);
    }
    Ok(input.len())
}

/// Interleave separate L and R channel buffers into stereo samples.
///
/// Ports SDR++ `dsp::convert::LRToStereo`. Both inputs must have the same length.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output` is too small, or
/// `DspError::InvalidParameter` if L and R have different lengths.
pub fn lr_to_stereo(left: &[f32], right: &[f32], output: &mut [Stereo]) -> Result<usize, DspError> {
    if left.len() != right.len() {
        return Err(DspError::InvalidParameter(format!(
            "L and R must have same length: {} != {}",
            left.len(),
            right.len()
        )));
    }
    if output.len() < left.len() {
        return Err(DspError::BufferTooSmall {
            need: left.len(),
            got: output.len(),
        });
    }
    for (i, (&l, &r)) in left.iter().zip(right.iter()).enumerate() {
        output[i] = Stereo::new(l, r);
    }
    Ok(left.len())
}

/// Convert complex samples to stereo (re → L, im → R).
///
/// Ports SDR++ `dsp::convert::ComplexToStereo`. Maps re to left channel
/// and im to right channel via explicit field assignment.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output.len() < input.len()`.
pub fn complex_to_stereo(input: &[Complex], output: &mut [Stereo]) -> Result<usize, DspError> {
    if output.len() < input.len() {
        return Err(DspError::BufferTooSmall {
            need: input.len(),
            got: output.len(),
        });
    }
    for (i, s) in input.iter().enumerate() {
        output[i] = Stereo::new(s.re, s.im);
    }
    Ok(input.len())
}

/// Input sample rate expected by `stereo_48k_to_mono_16k`.
///
/// The engine's audio junction delivers samples at this rate — see
/// `AUDIO_SAMPLE_RATE` in `sdr-core::controller`. Kept as a named
/// constant so callers and tests share one source of truth.
pub const AUDIO_TAP_INPUT_RATE_HZ: u32 = 48_000;

/// Output sample rate produced by `stereo_48k_to_mono_16k`. Matches
/// the rate speech recognizers (macOS `SpeechAnalyzer`, Whisper,
/// sherpa-onnx) natively consume.
pub const AUDIO_TAP_OUTPUT_RATE_HZ: u32 = 16_000;

/// Integer decimation ratio = input / output. Must divide evenly —
/// `const _: () = assert!(...)` enforces that so any future rate
/// change that breaks the ratio fails the build.
pub const AUDIO_TAP_DECIMATION_FACTOR: usize =
    (AUDIO_TAP_INPUT_RATE_HZ / AUDIO_TAP_OUTPUT_RATE_HZ) as usize;

const _: () = assert!(
    AUDIO_TAP_INPUT_RATE_HZ.is_multiple_of(AUDIO_TAP_OUTPUT_RATE_HZ),
    "AUDIO_TAP_INPUT_RATE_HZ must be an integer multiple of AUDIO_TAP_OUTPUT_RATE_HZ"
);

/// 48 kHz → 16 kHz mono f32 downsampler for stereo input.
///
/// Input is the engine's post-demod stereo buffer (48 kHz). Output
/// is mono at 16 kHz — the sample rate speech recognizers (macOS
/// `SpeechAnalyzer`, whisper, sherpa-onnx) consume natively.
/// Decimation factor is exactly 3:1; every third stereo sample is
/// averaged to mono and kept.
///
/// # Phase
///
/// `phase` carries the decimation offset across calls so successive
/// chunks form a continuous 3:1 stream even when the input block
/// size isn't a multiple of 3. Pass `0` on the first call after
/// enabling the tap (or after a reset); pass the same variable back
/// on each subsequent call. On return, `*phase` is in `0..3` and is
/// the starting index for the next call.
///
/// Without this, two consecutive calls of length 5 would each emit
/// `ceil(5/3) = 2` samples (producing 4 outputs for 10 inputs → 2.5:1),
/// which skews the 16 kHz timebase and causes duplicate/dropped
/// samples at block boundaries.
///
/// # Output sizing
///
/// Number of output samples: `input.len().saturating_sub(*phase).div_ceil(DECIMATION)`.
/// Callers that can't compute that ahead of time may size
/// `output` at `input.len().div_ceil(DECIMATION)` — always an
/// upper bound — and read the returned count.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output` is too small for
/// the computed count.
///
/// This is a naive keep-every-third-sample decimator — no
/// anti-alias filter — because the engine's audio path has already
/// produced a band-limited voice-rate signal by the time audio
/// reaches this stage. Good enough for speech recognition; NOT
/// suitable as a generic rate converter for arbitrary audio.
pub fn stereo_48k_to_mono_16k(
    input: &[Stereo],
    output: &mut [f32],
    phase: &mut usize,
) -> Result<usize, DspError> {
    // Defensive clamp: a caller that passes a stale or garbage
    // phase shouldn't be able to panic the function — just
    // normalize and proceed.
    if *phase >= AUDIO_TAP_DECIMATION_FACTOR {
        *phase = 0;
    }
    let input_len = input.len();
    let remaining = input_len.saturating_sub(*phase);
    let needed = remaining.div_ceil(AUDIO_TAP_DECIMATION_FACTOR);
    if output.len() < needed {
        return Err(DspError::BufferTooSmall {
            need: needed,
            got: output.len(),
        });
    }
    let mut out_idx = 0;
    let mut in_idx = *phase;
    while in_idx < input_len {
        let s = input[in_idx];
        output[out_idx] = f32::midpoint(s.l, s.r);
        out_idx += 1;
        in_idx += AUDIO_TAP_DECIMATION_FACTOR;
    }
    // `in_idx` now points one DECIMATION past the last kept
    // sample. The offset past the input end is how far into the
    // next chunk the next kept sample falls — i.e. the new phase.
    *phase = in_idx - input_len;
    Ok(out_idx)
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_complex_to_real() {
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [0.0_f32; 2];
        let count = complex_to_real(&input, &mut output).unwrap();
        assert_eq!(count, 2);
        assert_eq!(output[0], 1.0);
        assert_eq!(output[1], 3.0);
    }

    #[test]
    fn test_real_to_complex() {
        let input = [1.0_f32, 2.0, 3.0];
        let mut output = [Complex::default(); 3];
        let count = real_to_complex(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        assert_eq!(output[0].re, 1.0);
        assert_eq!(output[0].im, 0.0);
        assert_eq!(output[2].re, 3.0);
    }

    #[test]
    fn test_mono_to_stereo() {
        let input = [0.5_f32, 0.75];
        let mut output = [Stereo::default(); 2];
        let count = mono_to_stereo(&input, &mut output).unwrap();
        assert_eq!(count, 2);
        assert_eq!(output[0].l, 0.5);
        assert_eq!(output[0].r, 0.5);
        assert_eq!(output[1].l, 0.75);
        assert_eq!(output[1].r, 0.75);
    }

    #[test]
    fn test_stereo_to_mono() {
        let input = [Stereo::new(1.0, 3.0), Stereo::new(2.0, 4.0)];
        let mut output = [0.0_f32; 2];
        let count = stereo_to_mono(&input, &mut output).unwrap();
        assert_eq!(count, 2);
        assert_eq!(output[0], 2.0); // (1+3)/2
        assert_eq!(output[1], 3.0); // (2+4)/2
    }

    #[test]
    fn test_lr_to_stereo() {
        let left = [1.0_f32, 2.0];
        let right = [3.0_f32, 4.0];
        let mut output = [Stereo::default(); 2];
        let count = lr_to_stereo(&left, &right, &mut output).unwrap();
        assert_eq!(count, 2);
        assert_eq!(output[0].l, 1.0);
        assert_eq!(output[0].r, 3.0);
        assert_eq!(output[1].l, 2.0);
        assert_eq!(output[1].r, 4.0);
    }

    #[test]
    fn test_lr_to_stereo_mismatched_lengths() {
        let left = [1.0_f32, 2.0];
        let right = [3.0_f32];
        let mut output = [Stereo::default(); 2];
        let err = lr_to_stereo(&left, &right, &mut output).unwrap_err();
        assert!(
            matches!(err, DspError::InvalidParameter(_)),
            "expected InvalidParameter, got {err:?}"
        );
    }

    #[test]
    fn test_complex_to_stereo() {
        let input = [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        let mut output = [Stereo::default(); 2];
        let count = complex_to_stereo(&input, &mut output).unwrap();
        assert_eq!(count, 2);
        assert_eq!(output[0].l, 1.0);
        assert_eq!(output[0].r, 2.0);
        assert_eq!(output[1].l, 3.0);
        assert_eq!(output[1].r, 4.0);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_empty() {
        let mut output = [0.0_f32; 0];
        let mut phase = 0;
        let count = stereo_48k_to_mono_16k(&[], &mut output, &mut phase).unwrap();
        assert_eq!(count, 0);
        assert_eq!(phase, 0);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_single_sample() {
        // One stereo sample → one mono output at 16k.
        let input = [Stereo::new(0.6, 0.4)];
        let mut output = [0.0_f32; 1];
        let mut phase = 0;
        let count = stereo_48k_to_mono_16k(&input, &mut output, &mut phase).unwrap();
        assert_eq!(count, 1);
        assert_eq!(output[0], 0.5);
        // Consumed index 0; next kept index would be 3 — phase=2.
        assert_eq!(phase, 2);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_three_to_one() {
        // Three 48k samples → one kept (index 0), two skipped.
        let input = [
            Stereo::new(0.2, 0.8), // kept → midpoint 0.5
            Stereo::new(0.0, 0.0),
            Stereo::new(0.0, 0.0),
        ];
        let mut output = [0.0_f32; 1];
        let mut phase = 0;
        let count = stereo_48k_to_mono_16k(&input, &mut output, &mut phase).unwrap();
        assert_eq!(count, 1);
        assert_eq!(output[0], 0.5);
        assert_eq!(phase, 0);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_six_to_two() {
        // Six 48k samples → indices 0 and 3 kept.
        let input = [
            Stereo::new(0.2, 0.4),
            Stereo::new(0.0, 0.0),
            Stereo::new(0.0, 0.0),
            Stereo::new(0.6, 0.8),
            Stereo::new(0.0, 0.0),
            Stereo::new(0.0, 0.0),
        ];
        let mut output = [0.0_f32; 2];
        let mut phase = 0;
        let count = stereo_48k_to_mono_16k(&input, &mut output, &mut phase).unwrap();
        assert_eq!(count, 2);
        // f32 midpoint — use epsilon tolerance since (0.2+0.4)/2
        // and (0.6+0.8)/2 don't round-trip exactly through
        // binary32.
        assert!((output[0] - 0.3).abs() < f32::EPSILON);
        assert!((output[1] - 0.7).abs() < f32::EPSILON);
        assert_eq!(phase, 0);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_buffer_too_small() {
        // 3 samples → ceil(3/3) = 1 output; buffer of 0 must fail.
        let input = [Stereo::default(); 3];
        let mut output = [0.0_f32; 0];
        let mut phase = 0;
        let err = stereo_48k_to_mono_16k(&input, &mut output, &mut phase).unwrap_err();
        assert!(
            matches!(err, DspError::BufferTooSmall { need: 1, got: 0 }),
            "expected BufferTooSmall {{ need: 1, got: 0 }}, got {err:?}"
        );
        // Phase must NOT advance on an error return, otherwise
        // a retry would silently desynchronize the decimation
        // across chunks.
        assert_eq!(phase, 0);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_phase_carries_across_chunks() {
        // Simulate the DSP thread delivering 5-sample blocks
        // (not a multiple of 3). Over three blocks the naïve
        // stateless version would emit ceil(5/3)*3 = 6 samples,
        // but the correct 3:1 count over 15 input samples is 5.
        // This test pins the phase-carrying behavior — per
        // CodeRabbit round 1 on PR #349.
        let chunks: [[Stereo; 5]; 3] = [
            [
                Stereo::new(1.0, 1.0),
                Stereo::new(2.0, 2.0),
                Stereo::new(3.0, 3.0),
                Stereo::new(4.0, 4.0),
                Stereo::new(5.0, 5.0),
            ],
            [
                Stereo::new(6.0, 6.0),
                Stereo::new(7.0, 7.0),
                Stereo::new(8.0, 8.0),
                Stereo::new(9.0, 9.0),
                Stereo::new(10.0, 10.0),
            ],
            [
                Stereo::new(11.0, 11.0),
                Stereo::new(12.0, 12.0),
                Stereo::new(13.0, 13.0),
                Stereo::new(14.0, 14.0),
                Stereo::new(15.0, 15.0),
            ],
        ];

        let mut phase = 0;
        let mut all_out: Vec<f32> = Vec::new();
        for chunk in &chunks {
            let mut out = vec![0.0_f32; chunk.len()];
            let n = stereo_48k_to_mono_16k(chunk, &mut out, &mut phase).unwrap();
            all_out.extend_from_slice(&out[..n]);
        }
        // Expected kept indices in the concatenated 15-sample
        // stream: 0, 3, 6, 9, 12 → values 1, 4, 7, 10, 13.
        assert_eq!(all_out, vec![1.0, 4.0, 7.0, 10.0, 13.0]);
        // 15 is a multiple of 3; phase ends at 0.
        assert_eq!(phase, 0);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_clamps_bogus_phase() {
        // A garbage phase (>= DECIMATION) must be normalized,
        // not panic.
        let input = [Stereo::new(0.6, 0.4)];
        let mut output = [0.0_f32; 1];
        let mut phase = 99;
        let count = stereo_48k_to_mono_16k(&input, &mut output, &mut phase).unwrap();
        assert_eq!(count, 1);
        assert_eq!(output[0], 0.5);
    }

    #[test]
    fn test_roundtrip_real_complex() {
        let original = [1.0_f32, 2.0, 3.0, 4.0, 5.0];
        let mut complex = [Complex::default(); 5];
        let mut back = [0.0_f32; 5];
        real_to_complex(&original, &mut complex).unwrap();
        complex_to_real(&complex, &mut back).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn test_roundtrip_mono_stereo() {
        let original = [1.0_f32, 2.0, 3.0];
        let mut stereo = [Stereo::default(); 3];
        let mut back = [0.0_f32; 3];
        mono_to_stereo(&original, &mut stereo).unwrap();
        stereo_to_mono(&stereo, &mut back).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn test_buffer_too_small() {
        let input = [Complex::default(); 5];
        let mut output = [0.0_f32; 3];
        let err = complex_to_real(&input, &mut output).unwrap_err();
        assert!(
            matches!(err, DspError::BufferTooSmall { need: 5, got: 3 }),
            "expected BufferTooSmall {{need: 5, got: 3}}, got {err:?}"
        );
    }

    #[test]
    fn test_empty_input() {
        let input: &[Complex] = &[];
        let mut output = [0.0_f32; 0];
        let count = complex_to_real(input, &mut output).unwrap();
        assert_eq!(count, 0);
    }
}
