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

// ------------------------------------------------------------
//  Length-formatting unit policy for [`format_length_m`]
//
//  Thresholds for auto-scaling the displayed unit between metres,
//  centimetres, and millimetres. Named constants per the project's
//  "no magic numbers" rule — per `CodeRabbit` round 1 on PR #418.
// ------------------------------------------------------------

/// At or above this length (metres), render in "m" with two
/// decimal places. 1 m is the natural break: below it the leading
/// `0.` digits waste bar space vs. showing 99.9 cm.
const METRES_THRESHOLD_M: f64 = 1.0;
/// At or above this length (metres) but below [`METRES_THRESHOLD_M`],
/// render in "cm" with one decimal place. 1 cm = the natural break
/// below which the displayed number slips under 1.0 and the `.x cm`
/// format loses resolution vs. jumping to millimetres.
const CENTIMETRES_THRESHOLD_M: f64 = 0.01;
/// Conversion factor from metres to centimetres — purely a naming
/// aid so the formatter reads as `length * CM_PER_M` instead of
/// `length * 100.0`.
const CM_PER_M: f64 = 100.0;
/// Conversion factor from metres to millimetres — same naming aid
/// rationale as [`CM_PER_M`].
const MM_PER_M: f64 = 1_000.0;

// ------------------------------------------------------------
//  V-dipole angle suggestion
//
//  The V-angle between the two dipole arms tilts the radiation
//  pattern upward as it narrows from 180° (straight dipole,
//  peaks at horizon) toward 90° (peaks near zenith). The
//  [`suggested_v_angle`] helper picks an angle based on what
//  kind of signal typically lives at the current tuned
//  frequency: sky-dominated signals (weather sats, amateur
//  LEO birds) get a narrower V; horizon-dominated signals (FM
//  broadcast, airband, repeaters, HF skip) get the straight
//  dipole.
//
//  The V-angle tilts the pattern peak upward; a V of 120°
//  puts the peak around 30° elevation — a reasonable middle-
//  of-sky target for a typical polar-orbit satellite pass,
//  which sweeps from horizon to zenith through roughly that
//  elevation over half its track.
//
//  Companion to the 3D-printed V-dipole angle gauge at
//  https://github.com/jasonherald/aentenna-measure which sets
//  the angle in 5° detents.
// ------------------------------------------------------------

/// Suggested V-angle for a sky-dominated signal (sat pass). 120°
/// peaks the V-dipole's elevation pattern around 30° above the
/// horizon — approximately where a typical polar-orbit satellite
/// pass spends most of its track.
const V_ANGLE_SAT_DEGREES: u16 = 120;
/// Suggested V-angle for a horizon-dominated signal. 180° is a
/// straight dipole, peak gain at 0° elevation — optimal for FM
/// broadcast, airband, terrestrial repeaters, and HF skip
/// arrival angles.
const V_ANGLE_HORIZON_DEGREES: u16 = 180;

/// Short hint string displayed alongside a sky-band V-angle.
/// Three-char so the status-bar line stays compact.
const V_HINT_SAT: &str = "sat";
/// Short hint string for horizon-band.
const V_HINT_HORIZON: &str = "horizon";

/// NOAA APT weather-satellite band (polar-orbit birds). Active
/// discrete frequencies are 137.1 MHz (NOAA-18), 137.62 MHz
/// (NOAA-19), and 137.9125 MHz (NOAA-15 / historical). The
/// ±0.95 MHz window wraps all three plus the nearby sat-
/// telemetry bumpers so a user tuned slightly off-centre
/// still gets the satellite suggestion.
const NOAA_APT_MIN_HZ: f64 = 137_000_000.0;
const NOAA_APT_MAX_HZ: f64 = 137_950_000.0;

/// 2 m amateur band (144–148 MHz). ISS voice + APRS + a stack
/// of LEO amateur sats use this window.
const BAND_2M_MIN_HZ: f64 = 144_000_000.0;
const BAND_2M_MAX_HZ: f64 = 148_000_000.0;

/// 70 cm amateur satellite sub-band (435–438 MHz). Most LEO
/// amateur-radio satellites downlink here. The full ham 70cm
/// allocation is wider (420–450 MHz in ITU Region 2) but the
/// sat-specific segment is what we optimise for.
const BAND_70CM_SAT_MIN_HZ: f64 = 435_000_000.0;
const BAND_70CM_SAT_MAX_HZ: f64 = 438_000_000.0;

/// 13 cm amateur band (2300–2450 MHz). Higher-frequency
/// amateur satellites + some ISM co-habitate this range.
/// We pick the sat-friendly subset.
const BAND_13CM_MIN_HZ: f64 = 2_320_000_000.0;
const BAND_13CM_MAX_HZ: f64 = 2_450_000_000.0;

/// Suggested V-dipole angle for the current tuned frequency,
/// paired with a short hint word describing WHERE the angle
/// peaks the radiation pattern. Always returns a value for any
/// frequency at or above [`MIN_RENDERABLE_FREQUENCY_HZ`] —
/// callers gate on the enclosing line builder
/// ([`format_antenna_line`]) which already handles the below-
/// floor case.
///
/// **Band mapping is intentionally opinionated** — the exact
/// boundaries will shift as the companion aentenna-measure
/// gauge sees real-world use. Horizon (180°) is the legacy-safe
/// default for anything outside the recognised sat bands.
#[must_use]
pub fn suggested_v_angle(freq_hz: f64) -> (u16, &'static str) {
    let is_sat_band = (NOAA_APT_MIN_HZ..=NOAA_APT_MAX_HZ).contains(&freq_hz)
        || (BAND_2M_MIN_HZ..=BAND_2M_MAX_HZ).contains(&freq_hz)
        || (BAND_70CM_SAT_MIN_HZ..=BAND_70CM_SAT_MAX_HZ).contains(&freq_hz)
        || (BAND_13CM_MIN_HZ..=BAND_13CM_MAX_HZ).contains(&freq_hz);
    if is_sat_band {
        (V_ANGLE_SAT_DEGREES, V_HINT_SAT)
    } else {
        (V_ANGLE_HORIZON_DEGREES, V_HINT_HORIZON)
    }
}

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
/// - `>= 1 m`  → "X.XX m" (e.g. "1.17 m on HF 40m")
/// - `>= 1 cm` → "X.X cm" (e.g. "58.8 cm on VHF air")
/// - `< 1 cm`  → "X.X mm"
///
/// Returns an empty string for non-finite / non-positive inputs so the
/// caller can concatenate without special-casing the render site.
#[must_use]
pub fn format_length_m(length_m: f64) -> String {
    if !length_m.is_finite() || length_m <= 0.0 {
        return String::new();
    }
    if length_m >= METRES_THRESHOLD_M {
        format!("{length_m:.2} m")
    } else if length_m >= CENTIMETRES_THRESHOLD_M {
        let cm = length_m * CM_PER_M;
        format!("{cm:.1} cm")
    } else {
        let mm = length_m * MM_PER_M;
        format!("{mm:.1} mm")
    }
}

/// Build the status-bar line that pairs the half-wave total-dipole length
/// with the quarter-wave element length and the suggested V-angle hint.
/// Format: `"λ/2 58.8 cm · λ/4 29.4 cm · V 180° horizon"` (at 255 MHz ATIS)
/// or `"λ/2 109.4 cm · λ/4 54.7 cm · V 120° sat"` (at 137.62 MHz NOAA).
///
/// Returns `None` when the frequency is below [`MIN_RENDERABLE_FREQUENCY_HZ`]
/// so the caller can hide the label entirely rather than showing `"λ/2 —"`.
#[must_use]
pub fn format_antenna_line(freq_hz: f64) -> Option<String> {
    let half = half_wave_m(freq_hz)?;
    let quarter = quarter_wave_m(freq_hz)?;
    let (angle, hint) = suggested_v_angle(freq_hz);
    Some(format!(
        "λ/2 {} · λ/4 {} · V {angle}° {hint}",
        format_length_m(half),
        format_length_m(quarter)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------
    //  Typed, rationale-documented test fixtures per the repo's
    //  "no magic numbers in tests" convention. Per `CodeRabbit`
    //  round 1 on PR #418.
    // ----------------------------------------------------------

    /// f64 tolerance for exact-math wavelength comparisons. The
    /// underlying arithmetic is `c / f` where both operands are
    /// representable — 1e-6 catches rounding drift without
    /// false-failing on legit precision jitter.
    const FLOAT_EPS_M: f64 = 1e-6;
    /// Wider tolerance for the `half_wave_atis_255_mhz_*` test —
    /// the integer frequency `255_000_000` doesn't divide evenly
    /// into c, so the expected value `0.587_828` is approximate.
    const ATIS_MATCH_TOLERANCE_M: f64 = 1e-4;

    /// 100 MHz — FM broadcast band center, convenient for a
    /// whole-metre wavelength sanity check (`λ = c / 100e6 =
    /// 2.9979 m`).
    const FREQ_100_MHZ: f64 = 100_000_000.0;
    /// Expected wavelength at [`FREQ_100_MHZ`], spelled out to
    /// match `c / f` to f64 precision.
    const WAVELENGTH_AT_100_MHZ_M: f64 = 2.997_924_58;
    /// ATIS air-band frequency — reference example quoted in
    /// issue #157's acceptance ("255 MHz → half-wave ≈ 58.8 cm").
    const FREQ_ATIS_255_MHZ: f64 = 255_000_000.0;
    /// Expected half-wave dipole length at [`FREQ_ATIS_255_MHZ`],
    /// matched within [`ATIS_MATCH_TOLERANCE_M`].
    const HALF_WAVE_ATIS_M: f64 = 0.587_828;
    /// 2 m ham band center — standard reference for the
    /// "quarter wave is half of half wave" identity test.
    const FREQ_2M_CENTER_HZ: f64 = 146_000_000.0;
    /// Top-of-UHF stress-test frequency — 30 GHz drives λ/4 into
    /// the millimetre range so the unit-scaling branch of
    /// `format_length_m` gets exercised.
    const FREQ_30_GHZ: f64 = 30_000_000_000.0;
    /// Frequency just below the renderable floor (3 kHz).
    /// [`wavelength_m`] must reject this even though it's
    /// finite and positive.
    const FREQ_JUST_BELOW_FLOOR_HZ: f64 = 2_999.0;
    /// Frequency below the renderable floor (1 kHz < 3 kHz),
    /// used to verify `format_antenna_line` rejects sub-floor
    /// input in the same way it rejects zero / negative /
    /// non-finite. Per `CodeRabbit` round 2 on PR #418.
    const FREQ_1_KHZ: f64 = 1_000.0;

    /// NOAA APT centre band exemplar (NOAA-19 downlink frequency).
    /// Used to pin the sat-band V-angle recommendation.
    const FREQ_NOAA_19_MHZ: f64 = 137_620_000.0;
    /// 2 m ham band — ISS voice downlink frequency.
    const FREQ_ISS_VOICE_MHZ: f64 = 145_800_000.0;
    /// 70 cm ham satellite centre — typical LEO-sat downlink.
    const FREQ_70CM_SAT_MHZ: f64 = 436_500_000.0;
    /// 13 cm ham band example.
    const FREQ_13CM_SAT_MHZ: f64 = 2_400_000_000.0;
    /// FM broadcast band exemplar — `WXPN` Philadelphia,
    /// classic horizon-dominated reception target.
    const FREQ_FM_BROADCAST_MHZ: f64 = 88_500_000.0;
    /// Airband exemplar — standard ATIS frequency.
    const FREQ_AIRBAND_MHZ: f64 = 124_050_000.0;

    // Length fixtures for [`format_length_m`] branch coverage.
    /// Metre-range exemplar — the `.xx m` output branch.
    const LEN_1_M_EXAMPLE: f64 = 1.176_25;
    /// Expected rendering for [`LEN_1_M_EXAMPLE`].
    const LEN_1_M_EXPECTED: &str = "1.18 m";
    /// Centimetre-range exemplar — matches the ATIS half-wave.
    const LEN_CM_EXAMPLE: f64 = 0.587_8;
    /// Expected rendering for [`LEN_CM_EXAMPLE`].
    const LEN_CM_EXPECTED: &str = "58.8 cm";
    /// Millimetre-range exemplar — 7 mm.
    const LEN_MM_EXAMPLE: f64 = 0.007;
    /// Expected rendering for [`LEN_MM_EXAMPLE`].
    const LEN_MM_EXPECTED: &str = "7.0 mm";

    #[test]
    fn wavelength_100_mhz_is_2_998_m() {
        let w = wavelength_m(FREQ_100_MHZ).expect("valid freq");
        assert!((w - WAVELENGTH_AT_100_MHZ_M).abs() < FLOAT_EPS_M);
    }

    #[test]
    fn half_wave_atis_255_mhz_matches_design_ticket_example() {
        // Ticket #157 quotes ATIS at 255 MHz → half-wave ≈ 58.8 cm.
        // Exact: 299_792_458 / (255_000_000 * 2) = 0.587_828... m.
        let half = half_wave_m(FREQ_ATIS_255_MHZ).expect("valid");
        assert!((half - HALF_WAVE_ATIS_M).abs() < ATIS_MATCH_TOLERANCE_M);
    }

    #[test]
    fn quarter_wave_is_half_of_half_wave() {
        let half = half_wave_m(FREQ_2M_CENTER_HZ).expect("valid");
        let quarter = quarter_wave_m(FREQ_2M_CENTER_HZ).expect("valid");
        assert!((half - 2.0 * quarter).abs() < FLOAT_EPS_M);
    }

    #[test]
    fn sub_3khz_frequencies_return_none() {
        // Renderable-floor guard: a mis-tuned value near DC doesn't
        // blow up the status bar with "λ/2: 149_896 km".
        assert!(wavelength_m(0.0).is_none());
        assert!(wavelength_m(-100.0).is_none());
        assert!(wavelength_m(FREQ_JUST_BELOW_FLOOR_HZ).is_none());
        assert!(wavelength_m(f64::NAN).is_none());
        assert!(wavelength_m(f64::INFINITY).is_none());
    }

    #[test]
    fn format_length_auto_scales_units() {
        // >= 1 m
        assert_eq!(format_length_m(LEN_1_M_EXAMPLE), LEN_1_M_EXPECTED);
        // >= 1 cm
        assert_eq!(format_length_m(LEN_CM_EXAMPLE), LEN_CM_EXPECTED);
        // >= 1 mm
        assert_eq!(format_length_m(LEN_MM_EXAMPLE), LEN_MM_EXPECTED);
        // Guard
        assert_eq!(format_length_m(0.0), "");
        assert_eq!(format_length_m(-1.0), "");
        assert_eq!(format_length_m(f64::NAN), "");
    }

    #[test]
    fn format_antenna_line_combines_both_values() {
        // ATIS 255 MHz → λ/2 58.8 cm, λ/4 29.4 cm, V 180° horizon
        // (255 MHz is outside every known sat band, so the default
        // horizon-dipole suggestion lands).
        let line = format_antenna_line(FREQ_ATIS_255_MHZ).expect("valid");
        assert_eq!(line, "λ/2 58.8 cm · λ/4 29.4 cm · V 180° horizon");
    }

    #[test]
    fn suggested_v_angle_noaa_apt_is_sat_120() {
        let (angle, hint) = suggested_v_angle(FREQ_NOAA_19_MHZ);
        assert_eq!(angle, 120);
        assert_eq!(hint, "sat");
    }

    #[test]
    fn suggested_v_angle_2m_ham_is_sat_120() {
        let (angle, hint) = suggested_v_angle(FREQ_ISS_VOICE_MHZ);
        assert_eq!(angle, 120);
        assert_eq!(hint, "sat");
    }

    #[test]
    fn suggested_v_angle_70cm_sat_is_sat_120() {
        let (angle, hint) = suggested_v_angle(FREQ_70CM_SAT_MHZ);
        assert_eq!(angle, 120);
        assert_eq!(hint, "sat");
    }

    #[test]
    fn suggested_v_angle_13cm_sat_is_sat_120() {
        let (angle, hint) = suggested_v_angle(FREQ_13CM_SAT_MHZ);
        assert_eq!(angle, 120);
        assert_eq!(hint, "sat");
    }

    #[test]
    fn suggested_v_angle_fm_broadcast_is_horizon_180() {
        let (angle, hint) = suggested_v_angle(FREQ_FM_BROADCAST_MHZ);
        assert_eq!(angle, 180);
        assert_eq!(hint, "horizon");
    }

    #[test]
    fn suggested_v_angle_airband_is_horizon_180() {
        let (angle, hint) = suggested_v_angle(FREQ_AIRBAND_MHZ);
        assert_eq!(angle, 180);
        assert_eq!(hint, "horizon");
    }

    #[test]
    fn suggested_v_angle_just_outside_2m_band_is_horizon() {
        // Boundary check — 2 m band is 144..=148 MHz per the
        // `BAND_2M_*` consts. 149 MHz should fall to the horizon
        // default. Pins the "inclusive upper edge" contract so
        // `..=` vs `..` can't drift silently.
        let (angle, _) = suggested_v_angle(149_000_000.0);
        assert_eq!(angle, 180);
    }

    #[test]
    fn format_antenna_line_returns_none_below_renderable_floor() {
        assert!(format_antenna_line(0.0).is_none());
        assert!(format_antenna_line(FREQ_1_KHZ).is_none());
    }

    #[test]
    fn high_frequency_uhf_formats_in_mm_range() {
        let line = format_antenna_line(FREQ_30_GHZ).expect("valid");
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
