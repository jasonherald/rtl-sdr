use super::*;

/// #699 — the spectrum spans the raw rate but the VFO runs at the
/// post-decimation rate; an offset past ±effective/2 wraps to a
/// different station while the readout claims the clicked one.
#[test]
fn set_vfo_offset_is_clamped_to_half_effective_rate() {
    const OVERSHOOT_FACTOR: f64 = 4.0;
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    rebuild_vfo(&mut state).unwrap();
    let half = state.frontend.effective_sample_rate() / 2.0;
    let _ = drain(&dsp_rx);

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetVfoOffset(half * OVERSHOOT_FACTOR),
    );
    assert!(
        (state.vfo_offset - half).abs() < f64::EPSILON,
        "offset must clamp to +effective/2, got {}",
        state.vfo_offset
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::VfoOffsetChanged(o) if (o - half).abs() < f64::EPSILON)),
        "the echo must carry the clamped value, got {events:?}"
    );

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetVfoOffset(-half * OVERSHOOT_FACTOR),
    );
    assert!((state.vfo_offset + half).abs() < f64::EPSILON);
}

/// #699 (CR round 1) — an offset that was reachable can become
/// unreachable when decimation shrinks the effective rate; the
/// rebuild must re-clamp it and echo the applied value.
#[test]
fn decimation_change_reclamps_vfo_offset_and_echoes() {
    const DECIM_START: u32 = 1;
    const DECIM_NARROW: u32 = 8;
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetDecimation(DECIM_START));
    let wide_half = state.frontend.effective_sample_rate() / 2.0;
    handle_command(&mut state, &dsp_tx, UiToDsp::SetVfoOffset(wide_half));
    assert!((state.vfo_offset - wide_half).abs() < f64::EPSILON);
    let _ = drain(&dsp_rx);

    handle_command(&mut state, &dsp_tx, UiToDsp::SetDecimation(DECIM_NARROW));
    let narrow_half = state.frontend.effective_sample_rate() / 2.0;
    assert!(
        narrow_half < wide_half,
        "test premise: decimation narrowed the span"
    );
    assert!(
        (state.vfo_offset - narrow_half).abs() < f64::EPSILON,
        "offset must re-clamp to the new ±effective/2, got {}",
        state.vfo_offset
    );
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(
            |e| matches!(e, DspToUi::VfoOffsetChanged(o) if (o - narrow_half).abs() < f64::EPSILON)
        ),
        "expected VfoOffsetChanged({narrow_half}), got {events:?}"
    );
}

/// #699 (CR round 3) — `rebuild_vfo` must be transactional: if the
/// new VFO cannot be built, neither the offset nor the old VFO change.
#[test]
fn rebuild_vfo_failure_leaves_state_untouched() {
    const UNREACHABLE_OFFSET_HZ: f64 = 1.0e9;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    rebuild_vfo(&mut state).unwrap();
    state.vfo_offset = UNREACHABLE_OFFSET_HZ;
    state.bandwidth = 0.0; // RxVfo::new rejects a zero-width channel
    assert!(
        rebuild_vfo(&mut state).is_err(),
        "test premise: rebuild fails"
    );
    assert!(
        (state.vfo_offset - UNREACHABLE_OFFSET_HZ).abs() < f64::EPSILON,
        "a failed rebuild must not clamp the stored offset"
    );
    assert!(state.vfo.is_some(), "the previous VFO must survive");
}

/// #692 — the IQ-correction switch must not share state with DC blocking.
#[test]
fn set_iq_correction_does_not_alias_dc_blocking() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetDcBlocking(true));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(false));
    assert!(
        state.dc_blocking,
        "IQ correction off must leave DC blocking on"
    );
    assert!(!state.iq_correction);
    assert!(!state.frontend.iq_correction());

    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    handle_command(&mut state, &dsp_tx, UiToDsp::SetDcBlocking(false));
    assert!(
        state.iq_correction,
        "DC blocking off must leave IQ correction on"
    );
    assert!(state.frontend.iq_correction());
    assert!(!state.dc_blocking);
}

/// #692 — a frontend rebuild (sample-rate change) must carry the IQ-correction setting.
#[test]
fn rebuild_frontend_preserves_iq_correction() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    rebuild_frontend(&mut state).unwrap();
    assert!(state.frontend.iq_correction());
}

/// #692 (CR round 1) — every path that replaces the frontend must
/// carry the IQ-correction setting, not just `rebuild_frontend`.
#[test]
fn set_fft_size_preserves_iq_correction() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetFftSize(DEFAULT_FFT_SIZE * 2),
    );
    assert_eq!(state.frontend.fft_size(), DEFAULT_FFT_SIZE * 2);
    assert!(state.frontend.iq_correction());
}

#[test]
fn set_window_function_preserves_iq_correction() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetIqCorrection(true));
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman),
    );
    assert!(state.frontend.iq_correction());
}

/// #697 — a mode switch resets the bandwidth to the mode default and
/// must tell the UI so row / overlay / status bar agree with the engine.
#[test]
fn set_demod_mode_emits_bandwidth_changed_with_mode_default() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    rebuild_vfo(&mut state).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetBandwidth(12_000.0));
    let _ = drain(&dsp_rx);

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetDemodMode(sdr_types::DemodMode::Am),
    );
    let expected = state.radio.demod_config().default_bandwidth;
    assert!((state.bandwidth - expected).abs() < f64::EPSILON);
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(
            |e| matches!(e, DspToUi::BandwidthChanged(bw) if (bw - expected).abs() < f64::EPSILON)
        ),
        "expected BandwidthChanged({expected}), got {events:?}"
    );
}

/// #764 — tuning to a new centre frequency is a fresh start: the VFO
/// offset must reset to 0 in the engine (the UI overlay already does)
/// and the reset must be echoed so every readout agrees.
#[test]
fn tune_resets_vfo_offset_and_echoes_it() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    rebuild_vfo(&mut state).unwrap();
    handle_command(&mut state, &dsp_tx, UiToDsp::SetVfoOffset(50_000.0));
    assert!((state.vfo_offset - 50_000.0).abs() < f64::EPSILON);
    let _ = drain(&dsp_rx);

    handle_command(&mut state, &dsp_tx, UiToDsp::Tune(101_000_000.0));
    assert!(
        state.vfo_offset.abs() < f64::EPSILON,
        "Tune must reset the engine VFO offset"
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::VfoOffsetChanged(o) if o.abs() < f64::EPSILON)),
        "expected VfoOffsetChanged(0.0), got {events:?}"
    );
}

#[test]
fn rebuild_vfo_creates_vfo_and_sets_radio_rate() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    // Simulate what open_source does: frontend is already built at default rate.
    rebuild_vfo(&mut state).unwrap();
    assert!(state.vfo.is_some());
}

#[test]
fn rebuild_vfo_after_mode_switch_changes_rates() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    // Start with NFM (default) — IF rate 50 kHz
    rebuild_vfo(&mut state).unwrap();

    // Switch to WFM — IF rate 250 kHz
    state.radio.set_mode(sdr_types::DemodMode::Wfm).unwrap();
    rebuild_vfo(&mut state).unwrap();
    assert!(state.vfo.is_some());

    // Switch to NFM — IF rate 50 kHz (different from WFM)
    state.radio.set_mode(sdr_types::DemodMode::Nfm).unwrap();
    rebuild_vfo(&mut state).unwrap();
    assert!(state.vfo.is_some());
}

#[test]
fn vfo_preserves_signal_at_zero_offset() {
    // Create an RxVfo at same in/out rate, full bandwidth, offset 0.
    // The signal at DC should pass through essentially unchanged.
    let rate = 250_000.0;
    let mut vfo = RxVfo::new(rate, rate, rate, 0.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); 1000];
    let mut output = vec![Complex::default(); 1100];
    let count = vfo.process(&input, &mut output).unwrap();
    assert_eq!(count, 1000);
    // DC signal at zero offset should pass through with ~unity amplitude.
    for (i, s) in output[..count].iter().enumerate() {
        assert!(
            s.amplitude() > 0.9,
            "sample {i}: amplitude {} too low",
            s.amplitude()
        );
    }
}

#[test]
fn vfo_translates_offset_signal_to_baseband() {
    // Generate a tone at +10 kHz offset within a 250 kHz stream.
    // Set VFO offset to +10 kHz so the tone lands at DC after translation.
    let in_rate = 250_000.0;
    let offset_hz = 10_000.0;
    let n = 2500;

    // Generate a pure tone at +offset_hz.
    let input: Vec<Complex> = (0..n)
        .map(|i| {
            let phase = 2.0 * std::f64::consts::PI * offset_hz * (i as f64) / in_rate;
            #[allow(clippy::cast_possible_truncation)]
            Complex::new(phase.cos() as f32, phase.sin() as f32)
        })
        .collect();

    let mut vfo = RxVfo::new(in_rate, in_rate, in_rate, offset_hz).unwrap();
    let mut output = vec![Complex::default(); n + 100];
    let count = vfo.process(&input, &mut output).unwrap();
    assert!(count > 0);

    // After translation by -offset_hz, the signal should be near DC.
    // Skip the first few samples (filter settling) and check that the
    // imaginary part is small (signal is near real-only at DC).
    let settle = count / 4;
    let avg_imag: f32 = output[settle..count]
        .iter()
        .map(|s| s.im.abs())
        .sum::<f32>()
        / (count - settle) as f32;
    assert!(
        avg_imag < 0.15,
        "after translation, signal should be near DC — avg |imag| = {avg_imag}"
    );
}

#[test]
fn vfo_resamples_250k_to_50k() {
    // Simulates WFM frontend (250 kHz) feeding NFM demod (50 kHz).
    let in_rate = 250_000.0;
    let out_rate = 50_000.0;
    let bandwidth = 12_500.0;
    let n = 2500; // 10 ms at 250 kHz

    let mut vfo = RxVfo::new(in_rate, out_rate, bandwidth, 0.0).unwrap();
    let input = vec![Complex::new(1.0, 0.0); n];
    let mut output = vec![Complex::default(); n]; // more than enough
    let count = vfo.process(&input, &mut output).unwrap();

    // Expected ~500 samples (2500 * 50k/250k)
    assert!(
        (400..=600).contains(&count),
        "expected ~500 samples at 50 kHz, got {count}"
    );
}
