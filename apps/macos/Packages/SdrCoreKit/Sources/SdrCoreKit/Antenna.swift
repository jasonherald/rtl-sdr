//
// Antenna.swift — Mac-side mirror of `crates/sdr-ui/src/antenna.rs`.
//
// Pure physics — wavelength + half-wave dipole + quarter-wave
// element + V-dipole angle suggestion derived from the currently
// tuned frequency. No FFI hop: the Linux side keeps the same math
// in `sdr-ui` (GTK-internal), and reimplementing 30 lines of
// constant-time arithmetic in Swift is cheaper than plumbing a
// new FFI command through `sdr-core`. Anchor tests pin parity
// with the Rust fixtures in `antenna.rs::tests`. Issue #487.

import Foundation

public enum Antenna {

    // ----------------------------------------------------------
    //  Constants — values match `crates/sdr-ui/src/antenna.rs`
    //  exactly. Changing one without changing the other will
    //  fail the parity tests in `AntennaTests.swift`.
    // ----------------------------------------------------------

    /// Speed of light in free space, metres per second. Standard-
    /// defined exact constant (the metre is defined FROM this
    /// number).
    public static let speedOfLightMetersPerSecond: Double = 299_792_458.0

    /// Below this frequency the wavelength balloons into the
    /// kilometre range, "cut to length X" stops being a useful
    /// status-bar display, and we hide the antenna line entirely.
    /// Mirrors the Rust constant — bottom of the VLF band.
    public static let minRenderableFrequencyHz: Double = 3_000.0

    private static let metresThresholdMeters: Double = 1.0
    private static let centimetresThresholdMeters: Double = 0.01
    private static let centimetresPerMetre: Double = 100.0
    private static let millimetresPerMetre: Double = 1_000.0

    /// Suggested V-dipole arm angle for a sky-dominated signal —
    /// peaks the radiation pattern around 30° elevation, which
    /// approximately tracks where a polar-orbit satellite spends
    /// the bulk of its pass.
    private static let vAngleSatDegrees: Int = 120
    /// Suggested V-dipole arm angle for a horizon-dominated
    /// signal — straight dipole, peak gain at 0° elevation.
    /// Optimal for FM broadcast, airband, terrestrial repeaters,
    /// HF skip arrival.
    private static let vAngleHorizonDegrees: Int = 180

    private static let vHintSat: String = "sat"
    private static let vHintHorizon: String = "horizon"

    // Band ranges feeding `suggestedVAngle` — exact mirrors of the
    // `BAND_*_MIN_HZ` / `BAND_*_MAX_HZ` constants on the Rust side.
    private static let noaaAptMinHz: Double = 137_000_000.0
    private static let noaaAptMaxHz: Double = 137_950_000.0
    private static let band2mMinHz: Double = 144_000_000.0
    private static let band2mMaxHz: Double = 148_000_000.0
    private static let band70cmSatMinHz: Double = 435_000_000.0
    private static let band70cmSatMaxHz: Double = 438_000_000.0
    private static let band13cmMinHz: Double = 2_320_000_000.0
    private static let band13cmMaxHz: Double = 2_450_000_000.0

    // ----------------------------------------------------------
    //  Public API
    // ----------------------------------------------------------

    /// Wavelength in metres for a given frequency in Hz. Returns
    /// `nil` for non-finite, non-positive, or below-floor inputs.
    public static func wavelengthMeters(freqHz: Double) -> Double? {
        guard freqHz.isFinite, freqHz >= minRenderableFrequencyHz else {
            return nil
        }
        return speedOfLightMetersPerSecond / freqHz
    }

    /// Half-wave dipole total length in metres. Same nil contract
    /// as `wavelengthMeters`.
    public static func halfWaveMeters(freqHz: Double) -> Double? {
        wavelengthMeters(freqHz: freqHz).map { $0 / 2.0 }
    }

    /// Quarter-wave element length in metres — the per-arm length
    /// for a V-dipole, J-pole, or ground-plane antenna. Same nil
    /// contract as `wavelengthMeters`.
    public static func quarterWaveMeters(freqHz: Double) -> Double? {
        wavelengthMeters(freqHz: freqHz).map { $0 / 4.0 }
    }

    /// Format a length in metres with an auto-scaled unit suffix.
    /// Empty string for non-finite or non-positive input so the
    /// caller can concatenate without special-casing.
    ///
    /// - `>= 1 m`  → "X.XX m"
    /// - `>= 1 cm` → "X.X cm"
    /// - `< 1 cm`  → "X.X mm"
    public static func formatLengthMeters(_ lengthMeters: Double) -> String {
        guard lengthMeters.isFinite, lengthMeters > 0 else { return "" }
        if lengthMeters >= metresThresholdMeters {
            return String(format: "%.2f m", lengthMeters)
        } else if lengthMeters >= centimetresThresholdMeters {
            return String(format: "%.1f cm", lengthMeters * centimetresPerMetre)
        } else {
            return String(format: "%.1f mm", lengthMeters * millimetresPerMetre)
        }
    }

    /// Suggested V-dipole arm angle paired with a short hint word
    /// describing where the angle peaks the radiation pattern.
    /// Mirrors `suggested_v_angle` in the Rust crate — the band
    /// table is opinionated and intentionally identical to keep
    /// the two frontends visually aligned for users who switch.
    public static func suggestedVAngle(freqHz: Double) -> (degrees: Int, hint: String) {
        let inSatBand =
            (noaaAptMinHz...noaaAptMaxHz).contains(freqHz)
            || (band2mMinHz...band2mMaxHz).contains(freqHz)
            || (band70cmSatMinHz...band70cmSatMaxHz).contains(freqHz)
            || (band13cmMinHz...band13cmMaxHz).contains(freqHz)
        return inSatBand
            ? (vAngleSatDegrees, vHintSat)
            : (vAngleHorizonDegrees, vHintHorizon)
    }

    /// Build the status-bar antenna line for the current tuned
    /// frequency. Returns `nil` below the renderable floor so the
    /// caller can hide the label entirely instead of showing a
    /// degenerate "λ/2 —". String shape matches the Rust side
    /// byte-for-byte so a user comparing the two frontends sees
    /// the same readout at the same frequency.
    public static func formatAntennaLine(freqHz: Double) -> String? {
        guard let wavelength = wavelengthMeters(freqHz: freqHz) else { return nil }
        let half = wavelength / 2.0
        let quarter = wavelength / 4.0
        let (angle, hint) = suggestedVAngle(freqHz: freqHz)
        return "λ/2 \(formatLengthMeters(half)) · λ/4 \(formatLengthMeters(quarter)) · V \(angle)° \(hint)"
    }
}
