#![allow(
    clippy::float_cmp,
    reason = "asserted rates are copied verbatim (no arithmetic), so exact \
              comparison is the correct check"
)]
//! Hardware-free unit tests: sample conversion, gain-ladder mapping,
//! rate clamping, and the bridge / drain bookkeeping.

use super::*;

/// Fixture rates mirroring the R2 firmware table (IQ, Hz).
const R2_RATES: &[f64] = &[2_500_000.0, 10_000_000.0];

#[test]
fn convert_samples_pairs_interleaved_iq() {
    let raw = [0.5_f32, -0.5, 0.25, -0.25];
    let mut out = [Complex::new(0.0, 0.0); 4];
    let n = convert_samples(&raw, &mut out);
    assert_eq!(n, 2);
    assert_eq!(out[0], Complex::new(0.5, -0.5));
    assert_eq!(out[1], Complex::new(0.25, -0.25));
}

#[test]
fn convert_samples_bounded_by_output() {
    let raw = [1.0_f32; 8];
    let mut out = [Complex::new(0.0, 0.0); 3];
    assert_eq!(convert_samples(&raw, &mut out), 3);
}

#[test]
fn convert_samples_ignores_trailing_odd_value() {
    let raw = [1.0_f32, 2.0, 3.0];
    let mut out = [Complex::new(0.0, 0.0); 4];
    assert_eq!(convert_samples(&raw, &mut out), 1);
}

#[test]
fn linearity_step_round_trips_through_tenths() {
    for (i, &tenths) in LINEARITY_GAIN_TENTHS.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation, reason = "ladder has 22 entries")]
        let expected = i as u8;
        assert_eq!(linearity_step_from_tenths(tenths), expected);
    }
}

#[test]
fn linearity_step_clamps_out_of_range_dispatches() {
    assert_eq!(linearity_step_from_tenths(-50), 0);
    assert_eq!(linearity_step_from_tenths(9_999), LINEARITY_GAIN_STEPS - 1);
}

#[test]
fn nearest_rate_exact_match() {
    assert_eq!(
        nearest_supported_rate(R2_RATES, 10_000_000.0),
        Some(10_000_000.0)
    );
}

#[test]
fn nearest_rate_clamps_rtl_era_persisted_value() {
    // A persisted RTL 2.4 Msps must map to the R2's 2.5 Msps, not
    // fail Play.
    assert_eq!(
        nearest_supported_rate(R2_RATES, 2_400_000.0),
        Some(2_500_000.0)
    );
}

#[test]
fn nearest_rate_empty_table_is_none() {
    assert_eq!(nearest_supported_rate(&[], 1.0), None);
}

#[test]
fn gains_table_matches_ladder_length() {
    let source = AirspySource::new();
    assert_eq!(source.gains().len(), usize::from(LINEARITY_GAIN_STEPS));
    // Strictly increasing — the UI's gain-by-index selection depends
    // on a sorted table.
    assert!(source.gains().windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn set_gain_by_index_rejects_out_of_range() {
    let mut source = AirspySource::new();
    let err = source.set_gain_by_index(u32::from(LINEARITY_GAIN_STEPS));
    assert!(matches!(err, Err(SourceError::InvalidParameter(_))));
}

#[test]
fn dispatches_are_remembered_without_a_device() {
    // Gain / mode / bias dispatched before Play must be replayed by
    // the next start() rather than dropped (#626 regression class).
    let mut source = AirspySource::new();
    source.set_gain(70).expect("set_gain stores when closed");
    source
        .set_gain_mode(false)
        .expect("set_gain_mode stores when closed");
    source
        .set_bias_tee(true)
        .expect("set_bias_tee stores when closed");
    source.tune(137_900_000.0).expect("tune stores when closed");
    assert_eq!(source.sample_rate(), DEFAULT_SAMPLE_RATES[0]);
}

#[test]
fn read_samples_without_start_reports_not_running() {
    let mut source = AirspySource::new();
    let mut out = [Complex::new(0.0, 0.0); 4];
    assert!(matches!(
        source.read_samples(&mut out),
        Err(SourceError::NotRunning)
    ));
}

#[test]
fn set_sample_rate_accepts_any_value_when_closed() {
    // Validation happens at start() against the firmware snapshot.
    let mut source = AirspySource::new();
    source
        .set_sample_rate(2_400_000.0)
        .expect("closed-source rate set is deferred validation");
    assert_eq!(source.sample_rate(), 2_400_000.0);
}
