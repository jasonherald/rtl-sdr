use super::*;

#[test]
fn tune_accepts_reasonable_frequency() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_tune(h, 100_700_000.0) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn tune_rejects_nan_and_inf() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_tune(h, f64::NAN) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_tune(h, f64::INFINITY) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_sample_rate_rejects_non_positive() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_sample_rate(h, 0.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_sample_rate(h, -1.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_decimation_rejects_non_power_of_two() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_decimation(h, 0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_decimation(h, 3) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_decimation(h, 8) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn demod_mode_c_to_rust_covers_all_variants() {
    assert_eq!(demod_mode_from_c(SDR_DEMOD_WFM), Some(DemodMode::Wfm));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_NFM), Some(DemodMode::Nfm));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_AM), Some(DemodMode::Am));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_USB), Some(DemodMode::Usb));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_LSB), Some(DemodMode::Lsb));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_DSB), Some(DemodMode::Dsb));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_CW), Some(DemodMode::Cw));
    assert_eq!(demod_mode_from_c(SDR_DEMOD_RAW), Some(DemodMode::Raw));
    assert_eq!(demod_mode_from_c(99), None);
    assert_eq!(demod_mode_from_c(-1), None);
}

#[test]
fn deemphasis_c_to_rust_covers_all_variants() {
    assert_eq!(
        deemphasis_from_c(SDR_DEEMPH_NONE),
        Some(DeemphasisMode::None)
    );
    assert_eq!(
        deemphasis_from_c(SDR_DEEMPH_US75),
        Some(DeemphasisMode::Us75)
    );
    assert_eq!(
        deemphasis_from_c(SDR_DEEMPH_EU50),
        Some(DeemphasisMode::Eu50)
    );
    assert_eq!(deemphasis_from_c(99), None);
}

#[test]
fn fft_window_c_to_rust_covers_all_variants() {
    assert_eq!(
        fft_window_from_c(SDR_FFT_WIN_RECT),
        Some(FftWindow::Rectangular)
    );
    assert_eq!(
        fft_window_from_c(SDR_FFT_WIN_BLACKMAN),
        Some(FftWindow::Blackman)
    );
    assert_eq!(
        fft_window_from_c(SDR_FFT_WIN_NUTTALL),
        Some(FftWindow::Nuttall)
    );
    assert_eq!(fft_window_from_c(99), None);
}

#[test]
fn set_demod_mode_rejects_unknown_value() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_demod_mode(h, 99) },
        SdrCoreError::InvalidArg.as_int()
    );
    // And accepts valid ones.
    assert_eq!(
        unsafe { sdr_core_set_demod_mode(h, SDR_DEMOD_WFM) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_volume_clamps_out_of_range() {
    // Clamping is internal — the engine receives the clamped
    // value and accepts it. We can't directly observe the
    // clamped value from the FFI side without hooking the
    // event channel, so just prove the call succeeds for
    // out-of-range inputs.
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_volume(h, -1.0) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_volume(h, 2.0) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_volume(h, 0.5) },
        SdrCoreError::Ok.as_int()
    );
    // NaN is rejected (not finite).
    assert_eq!(
        unsafe { sdr_core_set_volume(h, f32::NAN) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_fft_size_rejects_non_power_of_two() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, 0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, 1000) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, 2048) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_fft_size_rejects_values_above_max() {
    // Guards against a host passing usize::MAX (or, on
    // Swift, a sign-cast of a negative Int) and tripping
    // an unbounded allocation in rustfft. The boundary is
    // a power of two so the "not a power of two" check
    // wouldn't catch it.
    let h = make_handle();

    // MAX_FFT_SIZE itself must be accepted.
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, super::MAX_FFT_SIZE) },
        SdrCoreError::Ok.as_int()
    );

    // 2 * MAX_FFT_SIZE is a power of two but over the cap.
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, super::MAX_FFT_SIZE * 2) },
        SdrCoreError::InvalidArg.as_int()
    );

    // usize::MAX isn't a power of two, so it already gets
    // caught by the earlier check — but the upper-bound
    // check is defense in depth. Pick a large power of two
    // that's over the cap to exercise the new arm.
    let large_power_of_two: usize = 1 << 30; // 1 GiB worth of bins
    assert_eq!(
        unsafe { sdr_core_set_fft_size(h, large_power_of_two) },
        SdrCoreError::InvalidArg.as_int()
    );

    destroy(h);
}

#[test]
fn set_nb_level_accepts_at_minimum_and_rejects_below() {
    let h = make_handle();
    // Exactly at the minimum must be accepted — the engine
    // treats `1.0` as "no clipping margin," which is the
    // lower edge of the usable range.
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, NB_LEVEL_MIN) },
        SdrCoreError::Ok.as_int()
    );
    // Any value below minimum must be rejected.
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, NB_LEVEL_MIN - 0.0001) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, 0.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_nb_level(h, -1.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_nb_level_rejects_nan_and_infinity() {
    let h = make_handle();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            unsafe { sdr_core_set_nb_level(h, bad) },
            SdrCoreError::InvalidArg.as_int(),
            "nb_level must reject {bad}"
        );
    }
    destroy(h);
}

#[test]
fn set_notch_frequency_accepts_positive_rejects_nonpositive() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_notch_frequency(h, 1_000.0) },
        SdrCoreError::Ok.as_int()
    );
    // Exactly at the exclusive lower bound must be rejected.
    assert_eq!(
        unsafe { sdr_core_set_notch_frequency(h, NOTCH_FREQUENCY_MIN_HZ_EXCLUSIVE) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_notch_frequency(h, -50.0) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_notch_frequency_rejects_nan_and_infinity() {
    let h = make_handle();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            unsafe { sdr_core_set_notch_frequency(h, bad) },
            SdrCoreError::InvalidArg.as_int(),
            "notch_frequency must reject {bad}"
        );
    }
    destroy(h);
}

// Legacy bool entry point — pins the tristate-forwarding
// semantics so a future refactor can't silently break
// pre-0.13 hosts. Per `CodeRabbit` round 2 on PR #371.
#[test]
fn set_agc_legacy_bool_round_trips() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_agc(h, true) },
        SdrCoreError::Ok.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_agc(h, false) },
        SdrCoreError::Ok.as_int()
    );
    destroy(h);
}

#[test]
fn set_agc_legacy_bool_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_set_agc(std::ptr::null_mut(), false) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn set_agc_type_accepts_valid_variants() {
    let h = make_handle();
    for t in [SDR_AGC_OFF, SDR_AGC_HARDWARE, SDR_AGC_SOFTWARE] {
        assert_eq!(
            unsafe { sdr_core_set_agc_type(h, t) },
            SdrCoreError::Ok.as_int(),
            "AGC type {t} should be accepted"
        );
    }
    destroy(h);
}

#[test]
fn set_agc_type_rejects_out_of_range() {
    let h = make_handle();
    assert_eq!(
        unsafe { sdr_core_set_agc_type(h, SDR_AGC_SOFTWARE + 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    assert_eq!(
        unsafe { sdr_core_set_agc_type(h, SDR_AGC_OFF - 1) },
        SdrCoreError::InvalidArg.as_int()
    );
    destroy(h);
}

#[test]
fn set_agc_type_rejects_null_handle() {
    assert_eq!(
        unsafe { sdr_core_set_agc_type(std::ptr::null_mut(), SDR_AGC_OFF) },
        SdrCoreError::InvalidHandle.as_int()
    );
}

#[test]
fn advanced_demod_bool_setters_accept_both_polarities() {
    // The four bool-typed advanced setters have no validation
    // beyond handle + panic catch — this just pins that they
    // don't silently regress to rejecting a valid input.
    let h = make_handle();
    for &on in &[true, false] {
        assert_eq!(
            unsafe { sdr_core_set_nb_enabled(h, on) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_set_fm_if_nr_enabled(h, on) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_set_wfm_stereo(h, on) },
            SdrCoreError::Ok.as_int()
        );
        assert_eq!(
            unsafe { sdr_core_set_notch_enabled(h, on) },
            SdrCoreError::Ok.as_int()
        );
    }
    destroy(h);
}
