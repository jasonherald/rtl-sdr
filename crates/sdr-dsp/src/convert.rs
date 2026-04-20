//! Signal type conversion functions.
//!
//! Ports SDR++ `dsp::convert` namespace. All functions are stateless and
//! operate on slices, converting between Complex, Stereo, and f32 types.

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

/// 48 kHz → 16 kHz mono f32 downsampler for stereo input.
///
/// Input is the engine's post-demod stereo buffer (48 kHz). Output
/// is mono at 16 kHz — the sample rate speech recognizers (macOS
/// `SpeechAnalyzer`, whisper, sherpa-onnx) consume natively.
/// Decimation factor is exactly 3:1 (48000 / 16000); every third
/// stereo sample is averaged to mono and kept.
///
/// Number of output samples: `input.len().div_ceil(3)`. Callers
/// should size `output` with that expression (the returned count
/// is the exact number of samples written).
///
/// This is a naive keep-every-third-sample decimator — no
/// anti-alias filter — because the engine's audio path has already
/// produced a band-limited voice-rate signal by the time audio
/// reaches this stage. Good enough for speech recognition; NOT
/// suitable as a generic rate converter for arbitrary audio.
///
/// # Errors
///
/// Returns `DspError::BufferTooSmall` if `output` has fewer than
/// `ceil(input.len() / 3)` elements.
pub fn stereo_48k_to_mono_16k(input: &[Stereo], output: &mut [f32]) -> Result<usize, DspError> {
    let needed = input.len().div_ceil(3);
    if output.len() < needed {
        return Err(DspError::BufferTooSmall {
            need: needed,
            got: output.len(),
        });
    }
    let mut out_idx = 0;
    let mut in_idx = 0;
    while in_idx < input.len() {
        let s = input[in_idx];
        output[out_idx] = f32::midpoint(s.l, s.r);
        out_idx += 1;
        in_idx += 3;
    }
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
        let count = stereo_48k_to_mono_16k(&[], &mut output).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_single_sample() {
        // One stereo sample → one mono output at 16k.
        let input = [Stereo::new(0.6, 0.4)];
        let mut output = [0.0_f32; 1];
        let count = stereo_48k_to_mono_16k(&input, &mut output).unwrap();
        assert_eq!(count, 1);
        assert_eq!(output[0], 0.5);
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
        let count = stereo_48k_to_mono_16k(&input, &mut output).unwrap();
        assert_eq!(count, 1);
        assert_eq!(output[0], 0.5);
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
        let count = stereo_48k_to_mono_16k(&input, &mut output).unwrap();
        assert_eq!(count, 2);
        // f32 midpoint — use epsilon tolerance since (0.2+0.4)/2
        // and (0.6+0.8)/2 don't round-trip exactly through
        // binary32.
        assert!((output[0] - 0.3).abs() < f32::EPSILON);
        assert!((output[1] - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn test_stereo_48k_to_mono_16k_buffer_too_small() {
        // 3 samples → ceil(3/3) = 1 output; buffer of 0 must fail.
        let input = [Stereo::default(); 3];
        let mut output = [0.0_f32; 0];
        let err = stereo_48k_to_mono_16k(&input, &mut output).unwrap_err();
        assert!(
            matches!(err, DspError::BufferTooSmall { need: 1, got: 0 }),
            "expected BufferTooSmall {{ need: 1, got: 0 }}, got {err:?}"
        );
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
