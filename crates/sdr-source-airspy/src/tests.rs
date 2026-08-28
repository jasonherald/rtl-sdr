#![allow(
    clippy::float_cmp,
    reason = "asserted rates are copied verbatim (no arithmetic), so exact \
              comparison is the correct check"
)]
//! Hardware-free unit tests: sample conversion, gain-ladder mapping,
//! rate clamping, and the bridge / drain bookkeeping.

use std::sync::atomic::{AtomicU64, Ordering};

use libairspy_rs::conversion::Samples;

use super::*;

/// Fixture rates mirroring the R2 firmware table (IQ, Hz).
const R2_RATES: &[f64] = &[2_500_000.0, 10_000_000.0];

#[test]
fn convert_samples_pairs_interleaved_iq_at_pipeline_fullscale() {
    // Driver fullscale ±0.5 maps to pipeline fullscale ±1.0.
    let raw = [0.5_f32, -0.5, 0.25, -0.25];
    let mut out = [Complex::new(0.0, 0.0); 4];
    let n = convert_samples(&raw, &mut out);
    assert_eq!(n, 2);
    assert_eq!(out[0], Complex::new(1.0, -1.0));
    assert_eq!(out[1], Complex::new(0.5, -0.5));
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
    assert_eq!(source.last_linearity_step, Some(7));
    assert_eq!(source.last_gain_manual, Some(false));
    assert!(source.bias_tee);
    assert!((source.frequency - 137_900_000.0).abs() < f64::EPSILON);
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

// ── bridge_transfer: block delivery + drop accounting ──────────────

#[test]
fn bridge_delivers_float32_blocks_in_order() {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
    let dropped = AtomicU64::new(0);
    let block = [0.1_f32, 0.2, 0.3, 0.4];
    assert!(bridge_transfer(&tx, &Samples::Float32(&block), 0, &dropped));
    assert_eq!(rx.recv().expect("block delivered"), block.to_vec());
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

#[test]
fn bridge_drops_block_and_counts_when_channel_full() {
    // Bound of 1, prefilled: the DSP is "behind". The bridge must
    // drop (keep streaming = true) and count, never block the
    // driver's consumer thread.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(1);
    let dropped = AtomicU64::new(0);
    assert!(bridge_transfer(
        &tx,
        &Samples::Float32(&[1.0, 2.0]),
        0,
        &dropped
    ));
    assert!(bridge_transfer(
        &tx,
        &Samples::Float32(&[3.0, 4.0]),
        0,
        &dropped
    ));
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
    // The first block survived; the second was dropped.
    assert_eq!(rx.recv().expect("first block"), vec![1.0, 2.0]);
    assert!(rx.try_recv().is_err());
}

#[test]
fn bridge_stops_stream_when_receiver_gone() {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
    drop(rx);
    let dropped = AtomicU64::new(0);
    assert!(!bridge_transfer(
        &tx,
        &Samples::Float32(&[1.0, 2.0]),
        0,
        &dropped
    ));
}

#[test]
fn bridge_rejects_non_float32_blocks() {
    // The sample type is latched to Float32Iq before start_rx; any
    // other variant is a driver contract violation and must stop the
    // stream rather than feed garbage samples to the DSP.
    let (tx, _rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
    let dropped = AtomicU64::new(0);
    let int_block = [1_i16, 2, 3, 4];
    assert!(!bridge_transfer(
        &tx,
        &Samples::Int16(&int_block),
        0,
        &dropped
    ));
}

#[test]
fn bridge_driver_drop_report_does_not_stop_stream() {
    // Driver-side ring drops are informational (logged); the stream
    // must continue and the block still delivers.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
    let dropped = AtomicU64::new(0);
    assert!(bridge_transfer(
        &tx,
        &Samples::Float32(&[5.0, 6.0]),
        123,
        &dropped
    ));
    assert_eq!(rx.recv().expect("delivered"), vec![5.0, 6.0]);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

// ── Upconverter offset (#848 phase 4) ─────────────────────────────

#[test]
fn converter_offset_shifts_hardware_tune_only() {
    let mut source = AirspySource::new();
    source
        .set_converter_offset(120_000_000.0)
        .expect("offset stores when closed");
    source.tune(10_000_000.0).expect("display tune");
    // Display state stays in display terms…
    assert!((source.frequency - 10_000_000.0).abs() < f64::EPSILON);
    // …while the hardware target carries the offset.
    assert_eq!(
        source.hardware_freq_hz(10_000_000.0).expect("in range"),
        130_000_000
    );
}

#[test]
fn converter_offset_zero_is_identity() {
    let source = AirspySource::new();
    assert_eq!(
        source.hardware_freq_hz(100_000_000.0).expect("in range"),
        100_000_000
    );
}

#[test]
fn converter_offset_out_of_range_is_rejected() {
    let mut source = AirspySource::new();
    // A -90 MHz offset is valid at the 100 MHz default display
    // frequency (hardware 10 MHz)…
    source
        .set_converter_offset(-90_000_000.0)
        .expect("offset valid at current display frequency");
    // …but tuning the display to 10 MHz would put the hardware at
    // -80 MHz — rejected, not wrapped.
    assert!(matches!(
        source.hardware_freq_hz(10_000_000.0),
        Err(SourceError::TuneFailed(_))
    ));
}

#[test]
fn rejected_converter_offset_is_not_retained() {
    // CR round 2 on PR #851 (same contract as the RTL-SDR source): an
    // offset that fails validation for the current display frequency
    // must not be committed.
    let mut source = AirspySource::new();
    source.tune(10_000_000.0).expect("display tune");
    assert!(matches!(
        source.set_converter_offset(-200_000_000.0),
        Err(SourceError::TuneFailed(_))
    ));
    assert_eq!(
        source.hardware_freq_hz(10_000_000.0).expect("offset rolled back"),
        10_000_000
    );
    source
        .set_converter_offset(120_000_000.0)
        .expect("valid offset accepted");
    assert_eq!(
        source.hardware_freq_hz(10_000_000.0).expect("in range"),
        130_000_000
    );
}
