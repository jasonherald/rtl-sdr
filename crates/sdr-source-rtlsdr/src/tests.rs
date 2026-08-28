use super::*;

#[test]
fn test_convert_samples() {
    // 127 should give ~-0.003 (near zero), 255 should give ~0.997
    let raw = [127, 127, 255, 0, 0, 255];
    let mut output = [Complex::default(); 3];
    let count = RtlSdrSource::convert_samples(&raw, &mut output);
    assert_eq!(count, 3);

    // Sample 0: (127 - 127.4) / 128 ≈ -0.003125
    assert!((output[0].re - (-0.003_125)).abs() < 0.001);
    assert!((output[0].im - (-0.003_125)).abs() < 0.001);

    // Sample 1: re = (255 - 127.4) / 128 ≈ 0.997
    assert!((output[1].re - 0.997).abs() < 0.01);
    // im = (0 - 127.4) / 128 ≈ -0.995
    assert!((output[1].im - (-0.995)).abs() < 0.01);
}

#[test]
fn test_sample_rates() {
    assert_eq!(SAMPLE_RATES.len(), 11);
    assert!((SAMPLE_RATES[0] - 250_000.0).abs() < 1.0);
    assert!((SAMPLE_RATES[10] - 3_200_000.0).abs() < 1.0);
}

#[test]
fn test_new() {
    let source = RtlSdrSource::new(0);
    assert_eq!(source.name(), "RTL-SDR");
    assert!((source.sample_rate() - 2_400_000.0).abs() < 1.0);
    assert_eq!(source.direct_sampling_mode, DIRECT_SAMPLING_OFF);
}

#[test]
fn set_direct_sampling_is_remembered_without_open_device() {
    let mut source = RtlSdrSource::new(0);
    source
        .set_direct_sampling(DIRECT_SAMPLING_Q)
        .expect("valid mode");
    assert_eq!(source.direct_sampling_mode, DIRECT_SAMPLING_Q);
    assert!(source.set_direct_sampling(DIRECT_SAMPLING_Q + 1).is_err());
    assert_eq!(
        source.direct_sampling_mode, DIRECT_SAMPLING_Q,
        "invalid mode must not overwrite"
    );
}

/// #739 — RTL AGC and gain-by-index are real operations on the USB
/// dongle, not trait-default no-ops. Without a device they are
/// remembered / validated; with one they reach the hardware.
/// #742 — a slot whose `len` is odd could never be released: the
/// consumer only advances by whole IQ pairs, so `consumed` parked at
/// `len - 1`, the writer spun on the full slot and `read_samples`
/// returned `Ok(0)` forever. The reader now trims to pairs (#785),
/// but the consumer must still release such a slot defensively.
#[test]
fn read_samples_releases_an_odd_length_slot() {
    const ODD_LEN: usize = 5;
    let mut source = RtlSdrSource::new(0);
    let ring = Arc::new(UsbRingBuffer::new(RING_SLOTS, RAW_BUF_SIZE));
    {
        let slot = &ring.slots[0];
        slot.data.lock().expect("slot mutex")[..ODD_LEN].copy_from_slice(&[10, 20, 30, 40, 50]);
        slot.len.store(ODD_LEN, Ordering::Relaxed);
        slot.state.store(RING_SLOT_FULL, Ordering::Release);
    }
    ring.write_idx.store(1, Ordering::Relaxed);
    source.ring = Some(Arc::clone(&ring));

    let mut out = vec![Complex::default(); 8];
    let n = source.read_samples(&mut out).expect("read");
    assert_eq!(n, ODD_LEN / IQ_PAIR_BYTES, "whole pairs are converted");
    assert_eq!(
        ring.slots[0].state.load(Ordering::Acquire),
        RING_SLOT_EMPTY,
        "a slot with a trailing odd byte must be released"
    );
    assert_eq!(ring.read_idx.load(Ordering::Relaxed), 1);
    assert_eq!(
        source.read_samples(&mut out).expect("read"),
        0,
        "nothing left"
    );
}

#[test]
fn set_rtl_agc_is_remembered_without_a_device() {
    let mut source = RtlSdrSource::new(0);
    assert!(!source.rtl_agc_enabled);
    source.set_rtl_agc(true).expect("stored");
    assert!(source.rtl_agc_enabled);
}

#[test]
fn set_gain_by_index_rejects_out_of_range() {
    let mut source = RtlSdrSource::new(0);
    // No device → no gain table → any index is out of range.
    assert!(matches!(
        source.set_gain_by_index(0),
        Err(SourceError::InvalidParameter(_))
    ));
}

#[test]
fn set_gain_mode_is_remembered_without_a_device() {
    let mut source = RtlSdrSource::new(0);
    assert_eq!(source.last_gain_manual, None);
    source.set_gain_mode(false).expect("stored");
    assert_eq!(source.last_gain_manual, Some(false));
}

/// #740 — the reader must tolerate a bounded run of timeouts /
/// zero-length transfers (host suspend, bus contention, USB
/// autosuspend) and only give up on real device loss.
#[test]
fn classify_read_tolerates_bounded_timeouts_and_empty_reads() {
    let mut retries = ReadRetryBudget::default();
    let timeout = || Err::<usize, _>(librtlsdr_rs::RtlSdrError::Usb(rusb::Error::Timeout));
    for _ in 0..MAX_CONSECUTIVE_SOFT_READ_FAILURES {
        assert!(matches!(
            classify_read(timeout(), &mut retries),
            ReadOutcome::Retry
        ));
    }
    assert!(matches!(
        classify_read(timeout(), &mut retries),
        ReadOutcome::Fatal(_)
    ));

    let mut retries = ReadRetryBudget::default();
    for _ in 0..MAX_CONSECUTIVE_SOFT_READ_FAILURES {
        assert!(matches!(
            classify_read(Ok(0), &mut retries),
            ReadOutcome::Retry
        ));
    }
    assert!(matches!(
        classify_read(Ok(0), &mut retries),
        ReadOutcome::Fatal(_)
    ));
}

#[test]
fn classify_read_data_resets_the_retry_budget_and_masks_odd_lengths() {
    let mut retries = ReadRetryBudget::default();
    let timeout = || Err::<usize, _>(librtlsdr_rs::RtlSdrError::Usb(rusb::Error::Timeout));
    for _ in 0..MAX_CONSECUTIVE_SOFT_READ_FAILURES {
        classify_read(timeout(), &mut retries);
    }
    // A successful transfer clears the budget…
    assert!(matches!(
        classify_read(Ok(4096), &mut retries),
        ReadOutcome::Data(4096)
    ));
    assert!(matches!(
        classify_read(timeout(), &mut retries),
        ReadOutcome::Retry
    ));
    // …and an odd byte count is trimmed to whole IQ pairs so the ring
    // slot can always be fully drained.
    assert!(matches!(
        classify_read(Ok(4097), &mut retries),
        ReadOutcome::Data(4096)
    ));
    // A lone odd byte is not data.
    assert!(matches!(
        classify_read(Ok(1), &mut retries),
        ReadOutcome::Retry
    ));
}

#[test]
fn classify_read_device_loss_is_fatal_immediately() {
    let mut retries = ReadRetryBudget::default();
    assert!(matches!(
        classify_read(
            Err(librtlsdr_rs::RtlSdrError::Usb(rusb::Error::NoDevice)),
            &mut retries
        ),
        ReadOutcome::Fatal(_)
    ));
}

#[test]
fn hf_tune_failure_on_r820t_hints_at_direct_sampling() {
    let msg = RtlSdrSource::tune_failure_message(
        TunerType::R820T,
        DIRECT_SAMPLING_OFF,
        4_800_000.0,
        "R82xx: PLL programming failed for 6425000 Hz (no valid VCO divider)",
    );
    assert!(msg.contains("4.800 MHz"), "{msg}");
    assert!(msg.contains("24 MHz floor"), "{msg}");
    assert!(msg.contains("Direct Sampling"), "{msg}");
    assert!(
        msg.contains("no valid VCO divider"),
        "driver detail kept: {msg}"
    );
}

#[test]
fn tune_failure_passthrough_when_hint_does_not_apply() {
    let raw = "some driver error";
    // Already in direct sampling — tuner floor is irrelevant.
    assert_eq!(
        RtlSdrSource::tune_failure_message(TunerType::R820T, 2, 4_800_000.0, raw),
        raw
    );
    // Above the floor — a different failure, don't mislead.
    assert_eq!(
        RtlSdrSource::tune_failure_message(TunerType::R820T, 0, 100_000_000.0, raw),
        raw
    );
    // Non-R82xx tuner — floor constant doesn't apply.
    assert_eq!(
        RtlSdrSource::tune_failure_message(TunerType::E4000, 0, 4_800_000.0, raw),
        raw
    );
}

// ── Upconverter offset (#848 phase 4, CR round 1 on PR #851) ──────

#[test]
fn converter_offset_shifts_hardware_tune_only() {
    let mut source = RtlSdrSource::new(0);
    source
        .set_converter_offset(125_000_000.0)
        .expect("offset stores when closed");
    source.tune(10_000_000.0).expect("display tune");
    // Display state stays in display terms…
    assert!((source.frequency - 10_000_000.0).abs() < f64::EPSILON);
    // …while the hardware target carries the offset.
    assert_eq!(
        source.hardware_freq_hz(10_000_000.0).expect("in range"),
        135_000_000
    );
}

#[test]
fn converter_offset_zero_is_identity() {
    let source = RtlSdrSource::new(0);
    assert_eq!(
        source.hardware_freq_hz(100_000_000.0).expect("in range"),
        100_000_000
    );
}

#[test]
fn converter_offset_out_of_range_is_rejected() {
    let mut source = RtlSdrSource::new(0);
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
fn converter_offset_lifts_hf_display_above_tuner_floor() {
    // Codacy round 1 on PR #851: with an upconverter in the chain, an
    // HF display frequency is legitimate — the floor logic works in
    // hardware terms, so the sum clears the R82xx floor and the
    // direct-sampling hint must NOT fire.
    let mut source = RtlSdrSource::new(0);
    source
        .set_converter_offset(125_000_000.0)
        .expect("offset stores");
    let hardware_hz = source.hardware_freq_hz(10_000_000.0).expect("in range");
    assert_eq!(hardware_hz, 135_000_000);
    assert!(f64::from(hardware_hz) >= R82XX_MIN_TUNER_FREQ_HZ);
    // The failure message for a hardware-terms frequency above the
    // floor passes the driver text through instead of misleading the
    // user toward direct sampling.
    let raw = "some driver error";
    assert_eq!(
        RtlSdrSource::tune_failure_message(
            TunerType::R820T,
            DIRECT_SAMPLING_OFF,
            f64::from(hardware_hz),
            raw
        ),
        raw
    );
}

#[test]
fn rejected_converter_offset_is_not_retained() {
    // CR round 2 on PR #851: an offset that fails validation for the
    // current display frequency must not be committed — otherwise
    // every later tune and restart fails until another offset is set.
    let mut source = RtlSdrSource::new(0);
    source.tune(10_000_000.0).expect("display tune");
    assert!(matches!(
        source.set_converter_offset(-200_000_000.0),
        Err(SourceError::TuneFailed(_))
    ));
    // The previous (zero) offset survives: tuning still works.
    assert_eq!(
        source.hardware_freq_hz(10_000_000.0).expect("offset rolled back"),
        10_000_000
    );
    // And a valid offset still commits afterwards.
    source
        .set_converter_offset(125_000_000.0)
        .expect("valid offset accepted");
    assert_eq!(
        source.hardware_freq_hz(10_000_000.0).expect("in range"),
        135_000_000
    );
}
