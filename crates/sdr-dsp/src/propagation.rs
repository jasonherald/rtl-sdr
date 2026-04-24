//! Free-space path loss and signal-to-distance conversions.
//!
//! Implements the textbook FSPL (Free-Space Path Loss) formula:
//!
//! ```text
//! FSPL(dB) = 20·log10(d) + 20·log10(f) - 147.55
//! ```
//!
//! Where `d` is distance in metres, `f` is frequency in Hz, and the
//! constant `147.55` is `20·log10(c / 4π)` with c = 299 792 458 m/s.
//! Solving for distance given a measured path loss:
//!
//! ```text
//! d = 10 ^ ((FSPL - 20·log10(f) + 147.55) / 20)
//! ```
//!
//! # Caveats
//!
//! FSPL is the **idealised line-of-sight** loss between two isotropic
//! antennas in free space — no terrain, no buildings, no multipath,
//! no atmospheric effects, no antenna directivity. Real-world receive
//! levels generally drop off faster than ideal FSPL because of
//! obstructions and diffraction, so a distance estimate computed
//! with these helpers should be read as an **upper bound**
//! ("the transmitter is *at most* this far away for this received
//! level") rather than a ranging measurement.
//!
//! Pure-math module — no threading, no I/O, no allocation.

use std::f64::consts::PI;

/// Speed of light in m/s. Exact by definition.
const C_M_PER_S: f64 = 299_792_458.0;

/// The FSPL additive constant, `20·log10(c / 4π)`.
///
/// The value is approximately 147.55 dB — what you'll see in RF
/// engineering tables. We evaluate it analytically here so the
/// formulas match any textbook derivation precisely.
///
/// Not a `const` because `f64::log10` isn't `const fn`. The
/// arithmetic is cheap (one division + one `log10`) and nothing
/// here is a hot path — FSPL is called at most once per FFT
/// display frame (~60 Hz), so skipping a `OnceCell` dep and just
/// recomputing inline keeps the code simpler at negligible cost.
#[inline]
fn fspl_constant_db() -> f64 {
    // 20 · log10(c / (4π))
    20.0 * (C_M_PER_S / (4.0 * PI)).log10()
}

/// Convert transmitter output power from watts to dBm.
///
/// Definition: `P(dBm) = 10·log10(P_mW)`, so for watts the formula is
/// `10·log10(P_watts · 1000) = 30 + 10·log10(P_watts)`. Handy for
/// ERP comparisons since radio handheld specs are usually in watts
/// but the FSPL formula wants dBm.
///
/// Returns `f64::NEG_INFINITY` for non-positive watts — physically
/// "infinite attenuation", which makes downstream `fspl_distance_m`
/// return zero distance (the sensible thing for "no transmitter").
#[must_use]
pub fn watts_to_dbm(watts: f64) -> f64 {
    if watts <= 0.0 {
        return f64::NEG_INFINITY;
    }
    30.0 + 10.0 * watts.log10()
}

/// Convert power from dBm back to watts.
///
/// Inverse of [`watts_to_dbm`].
#[must_use]
pub fn dbm_to_watts(dbm: f64) -> f64 {
    10_f64.powf((dbm - 30.0) / 10.0)
}

/// Compute FSPL in dB for a given distance and frequency.
///
/// Primarily a building block for tests and for sanity-checking
/// round-trips against [`fspl_distance_m`]. Callers doing distance
/// estimation should use [`fspl_distance_m`] directly.
///
/// Returns `f64::NAN` for non-positive `distance_m` or
/// `frequency_hz` — both are non-physical inputs and we'd rather
/// propagate NaN than silently return a misleading number.
#[must_use]
pub fn fspl_db(distance_m: f64, frequency_hz: f64) -> f64 {
    if distance_m <= 0.0 || frequency_hz <= 0.0 {
        return f64::NAN;
    }
    20.0 * distance_m.log10() + 20.0 * frequency_hz.log10() - fspl_constant_db()
}

/// Estimate distance (metres) from the path loss implied by
/// transmitter ERP, received signal level, and carrier frequency.
///
/// `erp_dbm` is the effective radiated power at the transmitter
/// (see [`watts_to_dbm`] for converting specs from watts). The
/// implied path loss is `erp_dbm - received_dbm`, and the inverse
/// FSPL formula maps that loss + frequency to a distance.
///
/// Returns `0.0` when the received signal is at or above the
/// transmitter's own output (physically impossible under FSPL,
/// implies either miscalibration or near-field coupling), and
/// `f64::NAN` for non-finite or non-positive frequency.
///
/// ```
/// use sdr_dsp::propagation::{fspl_distance_m, watts_to_dbm};
///
/// // A 50 W FM broadcast at 155 MHz received at -90 dBm should be
/// // estimated at roughly 1000 km under idealised line-of-sight
/// // conditions (real-world much less due to terrain / multipath).
/// let erp = watts_to_dbm(50.0);
/// let d = fspl_distance_m(erp, -90.0, 155e6);
/// assert!(d > 100_000.0 && d < 10_000_000.0);
/// ```
#[must_use]
pub fn fspl_distance_m(erp_dbm: f64, received_dbm: f64, frequency_hz: f64) -> f64 {
    if !frequency_hz.is_finite() || frequency_hz <= 0.0 {
        return f64::NAN;
    }
    if !erp_dbm.is_finite() || !received_dbm.is_finite() {
        return f64::NAN;
    }

    let path_loss_db = erp_dbm - received_dbm;
    if path_loss_db <= 0.0 {
        return 0.0;
    }

    // d = 10 ^ ((FSPL - 20·log10(f) + 147.55) / 20)
    let exponent = (path_loss_db - 20.0 * frequency_hz.log10() + fspl_constant_db()) / 20.0;
    10_f64.powf(exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance for direct formula comparisons (the additive
    /// constant is about 147.5517 dB depending on how many digits
    /// you carry, so references from tables to three decimals land
    /// within this).
    const DB_TOL: f64 = 0.01;

    /// Tolerance for distance round-trips — `10^x` with f64
    /// round-trips introduce bit-level drift; one part in 1e-9 is
    /// plenty.
    const DIST_REL_TOL: f64 = 1e-9;

    #[test]
    fn fspl_constant_matches_textbook() {
        // 20·log10(c / 4π) ≈ 147.55 dB. Exact value with the defined
        // speed of light is 147.5517...; any decent reference gives
        // 147.55 to two decimals.
        let k = fspl_constant_db();
        assert!(
            (k - 147.55).abs() < 0.01,
            "FSPL constant {k} deviates from textbook 147.55"
        );
    }

    #[test]
    fn watts_dbm_round_trip() {
        for &watts in &[0.001, 0.1, 1.0, 5.0, 25.0, 50.0, 100.0, 1000.0] {
            let dbm = watts_to_dbm(watts);
            let back = dbm_to_watts(dbm);
            let rel_err = (back - watts).abs() / watts;
            assert!(
                rel_err < 1e-12,
                "watts round-trip failed: {watts} → {dbm} dBm → {back} W (rel err {rel_err:e})"
            );
        }
    }

    #[test]
    fn watts_to_dbm_anchors() {
        // Common reference points every RF engineer has memorised.
        // 1 W = 30 dBm, 1 mW = 0 dBm, 100 W = 50 dBm.
        assert!((watts_to_dbm(1.0) - 30.0).abs() < DB_TOL);
        assert!((watts_to_dbm(0.001) - 0.0).abs() < DB_TOL);
        assert!((watts_to_dbm(100.0) - 50.0).abs() < DB_TOL);
    }

    #[test]
    fn watts_to_dbm_edge_cases() {
        // Zero or negative input → −∞ (no transmitter). Using
        // `is_infinite` + `is_sign_negative` rather than direct
        // float compare to keep clippy's `float_cmp` lint happy
        // while still asserting the exact sentinel.
        assert!(watts_to_dbm(0.0).is_infinite() && watts_to_dbm(0.0).is_sign_negative());
        assert!(watts_to_dbm(-1.0).is_infinite() && watts_to_dbm(-1.0).is_sign_negative());
    }

    #[test]
    fn fspl_db_anchors() {
        // At 100 MHz and 1 m: 20·log10(1) = 0, 20·log10(1e8) = 160,
        // so FSPL = 0 + 160 - 147.55 = 12.45 dB.
        let loss = fspl_db(1.0, 100e6);
        assert!(
            (loss - 12.45).abs() < DB_TOL,
            "expected ~12.45 dB at 100 MHz / 1 m, got {loss}"
        );

        // At 1 GHz and 10 km: 20·log10(1e4) = 80, 20·log10(1e9) = 180,
        // FSPL = 80 + 180 - 147.55 = 112.45 dB.
        let loss = fspl_db(10_000.0, 1e9);
        assert!(
            (loss - 112.45).abs() < DB_TOL,
            "expected ~112.45 dB at 1 GHz / 10 km, got {loss}"
        );
    }

    #[test]
    fn fspl_round_trip_distance_to_loss_to_distance() {
        // For a range of frequencies and distances, compute the
        // loss, then recover the distance from the loss, and
        // confirm we recover the original distance.
        for &freq in &[50e6, 155e6, 446e6, 1.575e9, 2.4e9] {
            for &d in &[1.0_f64, 100.0, 1_000.0, 50_000.0, 1e6] {
                let loss = fspl_db(d, freq);
                // Treat the path loss as (erp - received) with
                // erp = 0 dBm (so received = -loss dBm).
                let d_back = fspl_distance_m(0.0, -loss, freq);
                let rel = (d_back - d).abs() / d;
                assert!(
                    rel < DIST_REL_TOL,
                    "round-trip failed at f={freq}, d={d}: got {d_back} (rel err {rel:e})"
                );
            }
        }
    }

    #[test]
    fn fspl_distance_rejects_non_physical_inputs() {
        // Non-positive frequency → NaN.
        assert!(fspl_distance_m(30.0, -80.0, 0.0).is_nan());
        assert!(fspl_distance_m(30.0, -80.0, -1.0).is_nan());

        // Non-finite powers → NaN.
        assert!(fspl_distance_m(f64::NAN, -80.0, 100e6).is_nan());
        assert!(fspl_distance_m(30.0, f64::NAN, 100e6).is_nan());
        assert!(fspl_distance_m(f64::INFINITY, -80.0, 100e6).is_nan());

        // Received ≥ transmitted → physically impossible FSPL.
        // Return 0 rather than a negative or NaN distance so the
        // UI can treat this as "calibration issue" gracefully.
        // `< f64::EPSILON` instead of `== 0.0` because clippy's
        // `float_cmp` lint doesn't allow direct equality on
        // floats even when we know the value is exactly zero.
        assert!(fspl_distance_m(30.0, 30.0, 100e6) < f64::EPSILON);
        assert!(fspl_distance_m(30.0, 40.0, 100e6) < f64::EPSILON);
    }

    #[test]
    fn public_safety_vhf_scenario() {
        // Textbook scenario from the ticket: 50 W transmitter at
        // VHF (155 MHz), received at -90 dBm. Expected distance
        // under ideal FSPL is ~1100 km. This is a sanity check
        // that the output is in the right ball-park for a
        // reader spot-checking the feature.
        let erp = watts_to_dbm(50.0);
        let d = fspl_distance_m(erp, -90.0, 155e6);
        assert!(
            (500_000.0..2_000_000.0).contains(&d),
            "expected 500 km - 2 Mm for 50W @ 155 MHz @ -90 dBm FSPL, got {d} m"
        );
    }
}
