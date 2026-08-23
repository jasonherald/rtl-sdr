use super::{DistanceDisplay, MAX_MEANINGFUL_DISTANCE_M};

/// Compile-time validation that bandwidth constants are consistent.
const _: () = {
    assert!(super::MIN_BANDWIDTH_HZ <= super::MAX_BANDWIDTH_HZ);
    assert!(super::DEFAULT_BANDWIDTH_HZ >= super::MIN_BANDWIDTH_HZ);
    assert!(super::DEFAULT_BANDWIDTH_HZ <= super::MAX_BANDWIDTH_HZ);
    assert!(super::BANDWIDTH_STEP_HZ > 0.0);
};

/// Compile-time validation that squelch constants are consistent.
const _: () = {
    assert!(super::MIN_SQUELCH_DB <= super::MAX_SQUELCH_DB);
    assert!(super::DEFAULT_SQUELCH_DB >= super::MIN_SQUELCH_DB);
    assert!(super::DEFAULT_SQUELCH_DB <= super::MAX_SQUELCH_DB);
    assert!(super::SQUELCH_STEP_DB > 0.0);
};

/// Compile-time validation that NB level constants are consistent.
const _: () = {
    assert!(super::MIN_NB_LEVEL >= 1.0); // NoiseBlanker requires >= 1.0
    assert!(super::MIN_NB_LEVEL <= super::MAX_NB_LEVEL);
    assert!(super::DEFAULT_NB_LEVEL >= super::MIN_NB_LEVEL);
    assert!(super::DEFAULT_NB_LEVEL <= super::MAX_NB_LEVEL);
    assert!(super::NB_LEVEL_STEP > 0.0);
    assert!(super::NB_LEVEL_PAGE > 0.0);
};

/// Compile-time validation that notch frequency constants are consistent.
const _: () = {
    assert!(super::MIN_NOTCH_FREQ_HZ <= super::MAX_NOTCH_FREQ_HZ);
    assert!(super::DEFAULT_NOTCH_FREQ_HZ >= super::MIN_NOTCH_FREQ_HZ);
    assert!(super::DEFAULT_NOTCH_FREQ_HZ <= super::MAX_NOTCH_FREQ_HZ);
    assert!(super::NOTCH_FREQ_STEP_HZ > 0.0);
    assert!(super::NOTCH_FREQ_PAGE_HZ > 0.0);
};

/// Compile-time validation that distance-estimator ERP
/// constants are consistent.
const _: () = {
    assert!(super::MIN_ERP_WATTS <= super::MAX_ERP_WATTS);
    assert!(super::DEFAULT_ERP_WATTS >= super::MIN_ERP_WATTS);
    assert!(super::DEFAULT_ERP_WATTS <= super::MAX_ERP_WATTS);
    assert!(super::ERP_STEP_WATTS > 0.0);
    assert!(super::ERP_PAGE_WATTS > 0.0);
};

/// Compile-time validation that distance-estimator calibration
/// constants are consistent.
const _: () = {
    assert!(super::MIN_CALIBRATION_DB <= super::MAX_CALIBRATION_DB);
    assert!(super::DEFAULT_CALIBRATION_DB >= super::MIN_CALIBRATION_DB);
    assert!(super::DEFAULT_CALIBRATION_DB <= super::MAX_CALIBRATION_DB);
    assert!(super::CALIBRATION_STEP_DB > 0.0);
    assert!(super::CALIBRATION_PAGE_DB > 0.0);
};

// ─── Distance estimator state machine (ticket #164) ──────

/// ERP for the scenarios below: a 25 W public-safety mobile,
/// which is one of the defaults we suggest in the UI. Doesn't
/// affect any of the state-transition boundaries these tests
/// pin — just needs to be a realistic value.
const TEST_ERP_WATTS: f64 = 25.0;
const TEST_FREQ_HZ: f64 = 155e6;

#[test]
fn no_data_when_signal_cache_empty() {
    let s = DistanceDisplay::compute(None, Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
    assert_eq!(s, DistanceDisplay::NoData);
    assert_eq!(s.format(), "—");
}

#[test]
fn no_data_when_frequency_missing() {
    let s = DistanceDisplay::compute(Some(-80.0), None, TEST_ERP_WATTS, 0.0);
    assert_eq!(s, DistanceDisplay::NoData);
}

#[test]
fn no_active_signal_below_receiver_sensitivity() {
    // -120 dB raw + -20 dB calibration → -140 dBm received,
    // below our -130 dBm "active signal" threshold. This
    // matches the exact user report of "squelch on, no audio,
    // showing millions of km" (ticket #164 user feedback).
    let s = DistanceDisplay::compute(Some(-120.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, -20.0);
    assert_eq!(s, DistanceDisplay::NoActiveSignal);
    assert_eq!(s.format(), "No active signal");
}

#[test]
fn check_calibration_when_received_exceeds_transmitted() {
    // 25 W ERP ≈ 44 dBm. Received at +50 dBm is louder than
    // the transmitter — impossible under FSPL; user needs to
    // check their calibration offset or ERP value.
    let s = DistanceDisplay::compute(Some(50.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
    assert_eq!(s, DistanceDisplay::CheckCalibration);
    assert_eq!(s.format(), "Check calibration");
}

#[test]
fn too_weak_when_signal_present_but_fspl_saturates() {
    // Received level above the sensitivity threshold but
    // implying a distance past Earth's great-circle max.
    // -50 dB raw + -75 dB cal → -125 dBm received, just above
    // the -130 dBm threshold; 25W at 155 MHz says distance is
    // about 35,000 km — past the 20,000 km cap.
    let s = DistanceDisplay::compute(Some(-50.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, -75.0);
    assert_eq!(s, DistanceDisplay::TooWeak);
    assert_eq!(s.format(), "Too weak to measure");
}

#[test]
fn value_for_plausible_signal() {
    // 25W ERP, -80 dBm received at 155 MHz → 44 dBm path loss
    // of 124 dB → ~244 km FSPL. Hand-computed:
    //   d = 10 ^ ((124 - 163.8 + 147.55) / 20)
    //     = 10 ^ (107.75 / 20)  ≈  243 km
    // Bounds generous (100–1000 km) so micro-drift in the
    // 147.55 constant doesn't fail the test.
    let s = DistanceDisplay::compute(Some(-80.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
    let DistanceDisplay::Value(d) = s else {
        unreachable!("expected Value, got {s:?}");
    };
    assert!(
        (100_000.0..1_000_000.0).contains(&d),
        "expected 100-1000 km for -80 dBm received from 25W at 155 MHz, got {d} m"
    );
}

#[test]
fn value_for_strong_signal() {
    // Anchors the user-reported "9 km" scenario. For 25W at
    // 155 MHz to produce a ~9 km estimate the received level
    // has to be roughly -51 dBm — a very strong nearby
    // station. The test just checks the same state-machine
    // path returns `Value` and a modest (single-to-tens-of-km)
    // distance.
    let s = DistanceDisplay::compute(Some(-51.0), Some(TEST_FREQ_HZ), TEST_ERP_WATTS, 0.0);
    let DistanceDisplay::Value(d) = s else {
        unreachable!("expected Value, got {s:?}");
    };
    assert!(
        (1_000.0..50_000.0).contains(&d),
        "expected 1-50 km for -51 dBm received from 25W at 155 MHz, got {d} m"
    );
}

#[test]
fn exactly_at_sensitivity_threshold_still_evaluates() {
    // Boundary: exactly at the threshold should NOT trip
    // `NoActiveSignal` — we use `<` not `<=` so legitimate
    // edge-of-sensitivity receptions still get measured.
    // The threshold is -130 dBm; explicit literal rather than
    // casting `NO_ACTIVE_SIGNAL_DBM` because `as f32` would
    // trip clippy's `cast_possible_truncation` even though
    // -130.0 is exactly representable.
    let signal_at_threshold_db: f32 = -130.0;
    let s = DistanceDisplay::compute(
        Some(signal_at_threshold_db),
        Some(TEST_FREQ_HZ),
        TEST_ERP_WATTS,
        0.0,
    );
    assert_ne!(s, DistanceDisplay::NoActiveSignal);
}

// ─── format() rendering tests ─────────────────────────────

#[test]
fn format_sub_kilometre_shows_metres() {
    assert_eq!(DistanceDisplay::Value(1.4).format(), "1 m");
    assert_eq!(DistanceDisplay::Value(837.5).format(), "838 m");
    assert_eq!(DistanceDisplay::Value(999.0).format(), "999 m");
}

#[test]
fn format_kilometre_range_has_one_decimal() {
    assert_eq!(DistanceDisplay::Value(1_000.0).format(), "1.0 km");
    assert_eq!(DistanceDisplay::Value(12_345.0).format(), "12.3 km");
    assert_eq!(DistanceDisplay::Value(999_000.0).format(), "999.0 km");
}

#[test]
fn format_large_distances_round_to_10_km() {
    assert_eq!(DistanceDisplay::Value(1_234_000.0).format(), "1230 km");
    assert_eq!(DistanceDisplay::Value(1_500_000.0).format(), "1500 km");
}

#[test]
fn format_exactly_at_cap_still_shown_as_number() {
    // Boundary case: the cap value itself is still a real
    // distance (half of Earth's circumference). Only values
    // STRICTLY above it transition to `TooWeak`.
    let formatted = DistanceDisplay::Value(MAX_MEANINGFUL_DISTANCE_M).format();
    assert!(formatted.ends_with(" km"));
}
