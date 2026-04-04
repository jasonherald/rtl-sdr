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

/// Reinterpret complex samples as stereo (re → L, im → R).
///
/// Ports SDR++ `dsp::convert::ComplexToStereo`. This is a direct memory
/// reinterpretation since both types have identical layout (two f32 fields).
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
        assert!(lr_to_stereo(&left, &right, &mut output).is_err());
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
        assert!(complex_to_real(&input, &mut output).is_err());
    }

    #[test]
    fn test_empty_input() {
        let input: &[Complex] = &[];
        let mut output = [0.0_f32; 0];
        let count = complex_to_real(input, &mut output).unwrap();
        assert_eq!(count, 0);
    }
}
