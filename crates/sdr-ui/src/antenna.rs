//! Antenna-dimension math derived from the currently tuned frequency.
//!
//! Pure physics — no external data, no device state, no I/O. All functions
//! are free and deterministic so tests can exercise them without spinning
//! up GTK or the engine. Issue #157.
//!
//! The values surface on the status bar next to the center-frequency
//! readout, and feed future V-dipole popover UI that pairs with the
//! [`aentenna-measure` gauge](https://github.com/jasonherald/aentenna-measure)
//! for cutting arm lengths and setting the V-angle in 5° detents.

/// Speed of light in free space, in metres per second. Standard-defined
/// constant; the underlying number is exact by international agreement
/// (the metre is defined FROM this number, not the other way around).
pub const SPEED_OF_LIGHT_M_S: f64 = 299_792_458.0;

/// Minimum frequency (Hz) at which we'll render a meaningful antenna
/// dimension on the status bar. Below this the wavelengths balloon into
/// kilometre territory where "cut your element to X" stops being a
/// helpful display for a hand-held SDR and starts being noise. Matches
/// the bottom of the VLF band (3 kHz) — anything lower is likely a
/// mis-tune or a test signal and the status bar can safely show nothing.
pub const MIN_RENDERABLE_FREQUENCY_HZ: f64 = 3_000.0;

/// Wavelength (metres) for a given frequency (Hz). Returns `None` when
/// the input isn't a finite positive number or is below
/// [`MIN_RENDERABLE_FREQUENCY_HZ`] — callers treat that as "don't render".
#[must_use]
pub fn wavelength_m(freq_hz: f64) -> Option<f64> {
    if !freq_hz.is_finite() || freq_hz < MIN_RENDERABLE_FREQUENCY_HZ {
        return None;
    }
    Some(SPEED_OF_LIGHT_M_S / freq_hz)
}

/// Half-wave dipole total length in metres. `None` on the same conditions
/// as [`wavelength_m`].
#[must_use]
pub fn half_wave_m(freq_hz: f64) -> Option<f64> {
    wavelength_m(freq_hz).map(|w| w / 2.0)
}

/// Quarter-wave element length in metres — the per-arm length for a
/// V-dipole, J-pole, or ground-plane antenna. `None` on the same
/// conditions as [`wavelength_m`].
#[must_use]
pub fn quarter_wave_m(freq_hz: f64) -> Option<f64> {
    wavelength_m(freq_hz).map(|w| w / 4.0)
}

/// Format a length in metres with an auto-scaled unit suffix, keeping the
/// displayed value in the 0.1..=999 range so the status bar reads cleanly
/// across HF-to-UHF:
///
/// - `>= 1 m`  → "X.XX m" (e.g. "58.8 cm on VHF air, 1.17 m on HF 40m")
/// - `>= 1 cm` → "X.X cm"
/// - `< 1 cm`  → "X.X mm"
///
/// Returns an empty string for non-finite / non-positive inputs so the
/// caller can concatenate without special-casing the render site.
#[must_use]
pub fn format_length_m(length_m: f64) -> String {
    if !length_m.is_finite() || length_m <= 0.0 {
        return String::new();
    }
    if length_m >= 1.0 {
        format!("{length_m:.2} m")
    } else if length_m >= 0.01 {
        let cm = length_m * 100.0;
        format!("{cm:.1} cm")
    } else {
        let mm = length_m * 1000.0;
        format!("{mm:.1} mm")
    }
}

/// Build the status-bar line that pairs the half-wave total-dipole length
/// with the quarter-wave element length. Format: `"λ/2 58.8 cm · λ/4 29.4 cm"`.
/// Returns `None` when the frequency is below [`MIN_RENDERABLE_FREQUENCY_HZ`]
/// so the caller can hide the label entirely rather than showing `"λ/2 —"`.
#[must_use]
pub fn format_antenna_line(freq_hz: f64) -> Option<String> {
    let half = half_wave_m(freq_hz)?;
    let quarter = quarter_wave_m(freq_hz)?;
    Some(format!(
        "λ/2 {} · λ/4 {}",
        format_length_m(half),
        format_length_m(quarter)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOAT_EPS_M: f64 = 1e-6;

    #[test]
    fn wavelength_100_mhz_is_2_998_m() {
        // 100 MHz → λ = c / f = 2.998 m exactly (well within f64 precision).
        let w = wavelength_m(100_000_000.0).expect("valid freq");
        assert!((w - 2.997_924_58).abs() < FLOAT_EPS_M);
    }

    #[test]
    fn half_wave_atis_255_mhz_matches_design_ticket_example() {
        // Ticket #157 quotes ATIS at 255 MHz → half-wave ≈ 58.8 cm.
        // Exact: 299_792_458 / (255_000_000 * 2) = 0.587_828... m.
        let half = half_wave_m(255_000_000.0).expect("valid");
        assert!((half - 0.587_828).abs() < 0.000_1);
    }

    #[test]
    fn quarter_wave_is_half_of_half_wave() {
        let f = 146_000_000.0; // 2m ham band center
        let half = half_wave_m(f).expect("valid");
        let quarter = quarter_wave_m(f).expect("valid");
        assert!((half - 2.0 * quarter).abs() < FLOAT_EPS_M);
    }

    #[test]
    fn sub_3khz_frequencies_return_none() {
        // Renderable-floor guard: a mis-tuned value near DC doesn't
        // blow up the status bar with "λ/2: 149_896 km".
        assert!(wavelength_m(0.0).is_none());
        assert!(wavelength_m(-100.0).is_none());
        assert!(wavelength_m(2_999.0).is_none());
        assert!(wavelength_m(f64::NAN).is_none());
        assert!(wavelength_m(f64::INFINITY).is_none());
    }

    #[test]
    fn format_length_auto_scales_units() {
        // >= 1 m
        assert_eq!(format_length_m(1.17625), "1.18 m");
        // >= 1 cm
        assert_eq!(format_length_m(0.5878), "58.8 cm");
        // >= 1 mm
        assert_eq!(format_length_m(0.007), "7.0 mm");
        // Guard
        assert_eq!(format_length_m(0.0), "");
        assert_eq!(format_length_m(-1.0), "");
        assert_eq!(format_length_m(f64::NAN), "");
    }

    #[test]
    fn format_antenna_line_combines_both_values() {
        // ATIS 255 MHz → λ/2 58.8 cm, λ/4 29.4 cm.
        let line = format_antenna_line(255_000_000.0).expect("valid");
        assert_eq!(line, "λ/2 58.8 cm · λ/4 29.4 cm");
    }

    #[test]
    fn format_antenna_line_returns_none_below_renderable_floor() {
        assert!(format_antenna_line(0.0).is_none());
        assert!(format_antenna_line(1_000.0).is_none());
    }

    #[test]
    fn high_frequency_uhf_formats_in_mm_range() {
        // 30 GHz — top of UHF/SHF transition. λ/4 = 2.5 mm.
        let line = format_antenna_line(30_000_000_000.0).expect("valid");
        assert!(line.contains("λ/4 2.5 mm"), "line: {line}");
    }

    #[test]
    fn half_and_quarter_return_none_when_wavelength_would() {
        // The element-length helpers forward the renderable-floor
        // guard, not just wavelength_m itself.
        assert!(half_wave_m(0.0).is_none());
        assert!(quarter_wave_m(0.0).is_none());
    }
}
