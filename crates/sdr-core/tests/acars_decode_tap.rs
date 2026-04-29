//! Tests for the per-block `acars_decode_tap` function.
//! These cover the init-on-first-call lifecycle and the
//! one-shot init-failed guard. End-to-end frame decoding is
//! covered by the sub-project 1 e2e test (`sdr-acars`); this
//! suite is purely about the controller-side wiring.

use std::sync::mpsc;

use sdr_core::acars_airband_lock::{ACARS_CENTER_HZ, ACARS_SOURCE_RATE_HZ, US_SIX_CHANNELS_HZ};
use sdr_core::messages::DspToUi;
use sdr_core::testing::acars_decode_tap;
use sdr_types::Complex;

#[test]
fn tap_is_a_no_op_when_bank_slot_is_none_and_stays_silent() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = false;
    let (tx, rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];

    // No bank yet, init_failed not set — tap must lazily
    // initialize. Successful init at airband geometry.
    acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &US_SIX_CHANNELS_HZ,
        &iq,
        &tx,
    );
    assert!(bank.is_some(), "first call should initialize the bank");
    assert!(!init_failed);
    // Silent IQ produces no messages.
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

#[test]
fn tap_skips_processing_after_init_failure() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = true; // Simulate prior failure.
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];

    acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &US_SIX_CHANNELS_HZ,
        &iq,
        &tx,
    );
    assert!(bank.is_none(), "init_failed=true must short-circuit");
    assert!(init_failed);
}

#[test]
fn tap_records_init_failure_on_invalid_channel_list() {
    let mut bank: Option<sdr_acars::ChannelBank> = None;
    let mut init_failed = false;
    let (tx, _rx) = mpsc::channel::<DspToUi>();
    let iq = vec![Complex::default(); 1024];
    let bad_channels: [f64; 6] = [0.0; 6]; // outside source bandwidth

    acars_decode_tap(
        &mut bank,
        &mut init_failed,
        ACARS_SOURCE_RATE_HZ,
        ACARS_CENTER_HZ,
        &bad_channels,
        &iq,
        &tx,
    );
    assert!(bank.is_none());
    assert!(init_failed, "bad channels should set init_failed");
}
