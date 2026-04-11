/// Decimation factor: 48000 / 16000 = 3.
const DECIMATION_FACTOR: usize = 3;

/// Convert interleaved stereo f32 at 48 kHz to mono f32 at 16 kHz.
///
/// Each stereo pair (left, right) is averaged to mono, and every third
/// pair is kept to decimate from 48 kHz to 16 kHz. Output samples are
/// appended to `output`; existing contents are preserved.
pub fn downsample_stereo_to_mono_16k(interleaved_48k: &[f32], output: &mut Vec<f32>) {
    let pair_count = interleaved_48k.len() / 2;
    let mut pair_idx = 0;
    while pair_idx < pair_count {
        let l = interleaved_48k[pair_idx * 2];
        let r = interleaved_48k[pair_idx * 2 + 1];
        output.push(f32::midpoint(l, r));
        pair_idx += DECIMATION_FACTOR;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_produces_empty_output() {
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn single_stereo_pair_produces_one_sample() {
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&[0.6, 0.4], &mut out);
        assert_eq!(out.len(), 1);
        assert!((out[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn three_pairs_produce_one_sample() {
        // 3 stereo pairs at 48 kHz → decimation 3:1 → 1 output sample
        let input = [0.2, 0.8, 0.1, 0.3, 0.5, 0.5];
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&input, &mut out);
        assert_eq!(out.len(), 1);
        // Only pair 0 is kept: (0.2 + 0.8) / 2 = 0.5
        assert!((out[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn six_pairs_produce_two_samples() {
        // 6 stereo pairs → indices 0 and 3 are kept
        let input = [
            0.2, 0.4, // pair 0 → kept
            0.0, 0.0, // pair 1 → skipped
            0.0, 0.0, // pair 2 → skipped
            0.6, 0.8, // pair 3 → kept
            0.0, 0.0, // pair 4 → skipped
            0.0, 0.0, // pair 5 → skipped
        ];
        let mut out = Vec::new();
        downsample_stereo_to_mono_16k(&input, &mut out);
        assert_eq!(out.len(), 2);
        // Pair 0: (0.2 + 0.4) / 2 = 0.3
        assert!((out[0] - 0.3).abs() < f32::EPSILON);
        // Pair 3: (0.6 + 0.8) / 2 = 0.7
        assert!((out[1] - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn output_is_appended_not_replaced() {
        let mut out = vec![1.0, 2.0];
        downsample_stereo_to_mono_16k(&[0.6, 0.4], &mut out);
        assert_eq!(out.len(), 3);
        assert!((out[0] - 1.0).abs() < f32::EPSILON);
        assert!((out[1] - 2.0).abs() < f32::EPSILON);
        assert!((out[2] - 0.5).abs() < f32::EPSILON);
    }
}
