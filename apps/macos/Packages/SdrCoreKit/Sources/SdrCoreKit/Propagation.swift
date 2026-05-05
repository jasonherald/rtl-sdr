//
// Propagation.swift — Mac-side mirror of
// `crates/sdr-dsp/src/propagation.rs`.
//
// Free-space path loss (FSPL) and watts↔dBm helpers backing the
// Radio panel's Distance Estimator section (issue #486 / Linux
// PR #467).
//
// FSPL is the textbook idealised line-of-sight loss between two
// isotropic antennas in free space — no terrain, no buildings,
// no multipath. Real-world receive levels drop off faster, so a
// distance estimate from these helpers is best read as an
// **upper bound** ("the transmitter is *at most* this far") not
// a ranging measurement.
//
// Same engine-vs-Swift trade-off as `Antenna.swift`: the math is
// 15 lines, stable, and well-tested on the Rust side. Anchor
// tests in `PropagationTests.swift` pin parity with the Rust
// fixtures so drift between the two frontends is loud.

import Foundation

public enum Propagation {

    /// Speed of light in m/s. Exact by definition.
    private static let speedOfLightMetersPerSecond: Double = 299_792_458.0

    /// FSPL additive constant `20·log10(c / 4π)` ≈ 147.55 dB.
    /// Computed analytically from the exact-by-definition speed
    /// of light so the formulas match any textbook derivation.
    /// Not a stored constant because `Double.log10` isn't
    /// available at compile time in Swift; the math is one
    /// division + one log10 and is called at most a handful of
    /// times per second, so recomputing inline is fine.
    private static func fsplConstantDb() -> Double {
        20.0 * (log10(speedOfLightMetersPerSecond / (4.0 * .pi)))
    }

    /// Convert transmitter output power from watts to dBm.
    /// `P(dBm) = 30 + 10·log10(P_watts)`. Returns
    /// `-Double.infinity` for non-positive watts (physically
    /// "infinite attenuation"), which makes downstream
    /// `fsplDistanceMeters` return zero distance — the sensible
    /// thing for "no transmitter".
    public static func wattsToDbm(_ watts: Double) -> Double {
        guard watts > 0 else { return -.infinity }
        return 30.0 + 10.0 * log10(watts)
    }

    /// Convert power from dBm back to watts. Inverse of
    /// `wattsToDbm`.
    public static func dbmToWatts(_ dbm: Double) -> Double {
        pow(10.0, (dbm - 30.0) / 10.0)
    }

    /// Compute FSPL in dB for a given distance and frequency.
    /// Building block for tests + sanity-checking round-trips
    /// against `fsplDistanceMeters`. Returns `Double.nan` for
    /// non-positive distance or frequency.
    public static func fsplDb(distanceMeters: Double, frequencyHz: Double) -> Double {
        guard distanceMeters > 0, frequencyHz > 0 else { return .nan }
        return 20.0 * log10(distanceMeters)
            + 20.0 * log10(frequencyHz)
            - fsplConstantDb()
    }

    /// Estimate distance in metres from transmitter ERP, received
    /// signal level, and carrier frequency.
    ///
    /// Implied path loss is `erpDbm - receivedDbm`; the inverse
    /// FSPL formula maps that loss + frequency to a distance.
    ///
    /// - Returns `0` when the received signal is at or above the
    ///   transmitter's own output (physically impossible under
    ///   FSPL — implies miscalibration or near-field coupling).
    /// - Returns `Double.nan` for non-finite or non-positive
    ///   frequency / non-finite power inputs.
    public static func fsplDistanceMeters(
        erpDbm: Double,
        receivedDbm: Double,
        frequencyHz: Double
    ) -> Double {
        guard frequencyHz.isFinite, frequencyHz > 0 else { return .nan }
        guard erpDbm.isFinite, receivedDbm.isFinite else { return .nan }

        let pathLossDb = erpDbm - receivedDbm
        if pathLossDb <= 0 { return 0 }

        // d = 10 ^ ((FSPL - 20·log10(f) + 147.55) / 20)
        let exponent = (pathLossDb - 20.0 * log10(frequencyHz) + fsplConstantDb()) / 20.0
        return pow(10.0, exponent)
    }

    /// Auto-scale a distance in metres into a human-readable
    /// string. Intended for the Radio panel's "Distance" row;
    /// pairs with `fsplDistanceMeters` output.
    ///
    /// - `>= 10 km`  → "X km" (whole)
    /// - `>= 1 km`   → "X.X km"
    /// - `>= 100 m`  → "X m" (whole)
    /// - `>= 1 m`    → "X.X m"
    /// - `< 1 m`     → "X.X cm"
    /// - non-finite or non-positive → "—"
    public static func formatDistance(_ meters: Double) -> String {
        guard meters.isFinite, meters > 0 else { return "—" }
        if meters >= 10_000 {
            return String(format: "%.0f km", meters / 1_000.0)
        } else if meters >= 1_000 {
            return String(format: "%.1f km", meters / 1_000.0)
        } else if meters >= 100 {
            return String(format: "%.0f m", meters)
        } else if meters >= 1 {
            return String(format: "%.1f m", meters)
        } else {
            return String(format: "%.1f cm", meters * 100.0)
        }
    }
}
