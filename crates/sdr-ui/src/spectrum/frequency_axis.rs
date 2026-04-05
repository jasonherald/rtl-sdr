//! Frequency axis formatting and grid-line computation.
//!
//! Provides smart frequency formatting ("100.0 MHz", "433.5 MHz", "1.2 GHz")
//! and grid-line placement logic for FFT plot and waterfall axes.

/// Threshold above which to display in GHz.
const GHZ_THRESHOLD: f64 = 1_000_000_000.0;
/// Threshold above which to display in MHz.
const MHZ_THRESHOLD: f64 = 1_000_000.0;
/// Threshold above which to display in kHz.
const KHZ_THRESHOLD: f64 = 1_000.0;

/// Hz per GHz.
const HZ_PER_GHZ: f64 = 1_000_000_000.0;
/// Hz per MHz.
const HZ_PER_MHZ: f64 = 1_000_000.0;
/// Hz per kHz.
const HZ_PER_KHZ: f64 = 1_000.0;

/// Format a frequency in Hz to a human-readable string with appropriate unit.
///
/// # Examples
///
/// ```
/// # use sdr_ui::spectrum::frequency_axis::format_frequency;
/// assert_eq!(format_frequency(100_000_000.0), "100.000 MHz");
/// assert_eq!(format_frequency(1_200_000_000.0), "1.200 GHz");
/// assert_eq!(format_frequency(7_055_000.0), "7.055 MHz");
/// assert_eq!(format_frequency(455.0), "455.0 Hz");
/// ```
pub fn format_frequency(hz: f64) -> String {
    let abs = hz.abs();
    let sign = if hz < 0.0 { "-" } else { "" };

    if abs >= GHZ_THRESHOLD {
        format!("{sign}{:.3} GHz", abs / HZ_PER_GHZ)
    } else if abs >= MHZ_THRESHOLD {
        format!("{sign}{:.3} MHz", abs / HZ_PER_MHZ)
    } else if abs >= KHZ_THRESHOLD {
        format!("{sign}{:.1} kHz", abs / HZ_PER_KHZ)
    } else {
        format!("{sign}{abs:.1} Hz")
    }
}

/// Candidate step sizes in Hz, from small to large.
/// Each step divides common bandwidth ranges into sensible intervals.
const STEP_CANDIDATES: &[f64] = &[
    1.0,
    2.0,
    5.0,
    10.0,
    20.0,
    50.0,
    100.0,
    200.0,
    500.0,
    1_000.0, // 1 kHz
    2_000.0,
    5_000.0,
    10_000.0, // 10 kHz
    20_000.0,
    50_000.0,
    100_000.0, // 100 kHz
    200_000.0,
    500_000.0,
    1_000_000.0, // 1 MHz
    2_000_000.0,
    5_000_000.0,
    10_000_000.0, // 10 MHz
    20_000_000.0,
    50_000_000.0,
    100_000_000.0, // 100 MHz
    200_000_000.0,
    500_000_000.0,
    1_000_000_000.0, // 1 GHz
];

/// Compute grid line positions and labels for a frequency axis.
///
/// Returns a list of `(frequency_hz, label)` pairs spaced at round intervals
/// that produce at most `max_lines` grid lines within the given range.
///
/// # Arguments
///
/// * `start_hz` — Left edge of the display in Hz.
/// * `end_hz` — Right edge of the display in Hz.
/// * `max_lines` — Maximum number of grid lines desired.
#[allow(clippy::cast_precision_loss)]
pub fn compute_grid_lines(start_hz: f64, end_hz: f64, max_lines: usize) -> Vec<(f64, String)> {
    if max_lines == 0 || end_hz <= start_hz {
        return Vec::new();
    }

    let span = end_hz - start_hz;

    // Find the smallest step that gives at most `max_lines` lines.
    // Use strict `<` because the line count is `floor(span/step) + 1` in the
    // worst case (when both range endpoints align to a step boundary).
    let step = STEP_CANDIDATES
        .iter()
        .copied()
        .find(|&s| (span / s) < max_lines as f64)
        .unwrap_or(span * 2.0); // fallback: step > span guarantees at most 1 line

    // First grid line at or after start_hz, snapped to `step`.
    let first = (start_hz / step).ceil() * step;

    let mut lines = Vec::new();
    let mut freq = first;
    while freq <= end_hz {
        lines.push((freq, format_frequency(freq)));
        freq += step;
    }

    lines
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn format_hz() {
        assert_eq!(format_frequency(455.0), "455.0 Hz");
        assert_eq!(format_frequency(0.0), "0.0 Hz");
    }

    #[test]
    fn format_khz() {
        assert_eq!(format_frequency(7_055.0), "7.1 kHz");
        assert_eq!(format_frequency(1_000.0), "1.0 kHz");
    }

    #[test]
    fn format_mhz() {
        assert_eq!(format_frequency(100_000_000.0), "100.000 MHz");
        assert_eq!(format_frequency(433_500_000.0), "433.500 MHz");
        assert_eq!(format_frequency(7_055_000.0), "7.055 MHz");
    }

    #[test]
    fn format_ghz() {
        assert_eq!(format_frequency(1_200_000_000.0), "1.200 GHz");
        assert_eq!(format_frequency(2_400_000_000.0), "2.400 GHz");
    }

    #[test]
    fn format_negative() {
        assert_eq!(format_frequency(-100_000_000.0), "-100.000 MHz");
    }

    #[test]
    fn grid_lines_empty_on_zero_max() {
        let lines = compute_grid_lines(0.0, 1_000_000.0, 0);
        assert!(lines.is_empty());
    }

    #[test]
    fn grid_lines_empty_on_inverted_range() {
        let lines = compute_grid_lines(1_000_000.0, 0.0, 10);
        assert!(lines.is_empty());
    }

    #[test]
    fn grid_lines_reasonable_count() {
        // 2 MHz span, up to 10 lines
        let lines = compute_grid_lines(99_000_000.0, 101_000_000.0, 10);
        assert!(!lines.is_empty());
        assert!(
            lines.len() <= 10,
            "expected at most 10 lines, got {}",
            lines.len()
        );
    }

    #[test]
    fn grid_lines_within_range() {
        let start = 100_000_000.0;
        let end = 102_000_000.0;
        let lines = compute_grid_lines(start, end, 10);
        for (freq, _label) in &lines {
            assert!(
                *freq >= start && *freq <= end,
                "grid line {freq} outside range [{start}, {end}]"
            );
        }
    }

    #[test]
    fn grid_lines_are_sorted() {
        let lines = compute_grid_lines(88_000_000.0, 108_000_000.0, 20);
        for pair in lines.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "grid lines should be ascending: {} >= {}",
                pair[0].0,
                pair[1].0
            );
        }
    }
}
