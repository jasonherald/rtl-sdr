use super::*;

/// IF sample rate the WEFAX demod is fixed to (`WEFAX_IF_SAMPLE_RATE`
/// in `sdr_radio::demod::wefax`, private to that module). Matching it
/// here means `radio.process` skips the input resampler, keeping this
/// plumbing test's setup minimal.
const WEFAX_TEST_INPUT_RATE_HZ: f64 = 24_000.0;

/// One block's worth of silent IQ samples — enough for the WEFAX AF
/// chain to produce a non-empty audio block without needing a real
/// phasing/subcarrier tone (line assembly + sync-lock behavior is
/// covered by `sdr_dsp::wefax`'s own fixture-based tests).
const SILENT_IQ_BLOCK_LEN: usize = 2_400;

/// Build a `DspState` switched into WEFAX mode with a matching input
/// rate, ready to drive `state.radio.process`.
fn wefax_test_state() -> (DspState, mpsc::Sender<DspToUi>, mpsc::Receiver<DspToUi>) {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.radio.set_mode(sdr_types::DemodMode::Wefax).unwrap();
    state
        .radio
        .set_input_sample_rate(WEFAX_TEST_INPUT_RATE_HZ)
        .unwrap();
    (state, dsp_tx, dsp_rx)
}

/// Push one block of silent IQ through `radio.process`, mirroring
/// `process_iq_block`'s setup, and return the produced audio-sample
/// count for `wefax_decode_tap`.
fn drive_silent_audio_block(state: &mut DspState) -> usize {
    let iq_input = vec![Complex::default(); SILENT_IQ_BLOCK_LEN];
    let max_out = state.radio.max_output_samples(iq_input.len());
    state.audio_buf.resize(max_out, Stereo::default());
    state
        .radio
        .process(&iq_input, &mut state.audio_buf)
        .unwrap()
}

/// The decode tap must lazily construct the decoder and run cleanly
/// even with no `WefaxImageHandle` wired — the handle-dependent line
/// writes / chart-complete sends are silently skipped, mirroring the
/// SSTV / LRPT contract. Per issue #877.
#[test]
fn wefax_decode_tap_lazy_inits_decoder_without_an_image_handle() {
    let (mut state, dsp_tx, dsp_rx) = wefax_test_state();
    let audio_count = drive_silent_audio_block(&mut state);
    assert!(
        audio_count > 0,
        "test premise: WEFAX demod must produce audio"
    );
    assert!(state.wefax_image.is_none());

    wefax_decode_tap(&mut state, &dsp_tx, audio_count);

    assert!(
        state.wefax_decoder.is_some(),
        "tap must lazy-init the decoder even with no image handle wired"
    );
    assert!(
        state.wefax_init_failed_at_rate.is_none(),
        "a valid audio rate must not be cached as a failure"
    );
    // No panic with a `None` handle is the assertion here; draining
    // just confirms the channel wasn't poisoned by a bad send.
    let _ = drain(&dsp_rx);
}

/// `UiToDsp::SetWefaxImage` / `ClearWefaxImage` must wire and drop
/// `state.wefax_image` — the Task 11 replacement for the Task 10
/// no-op arm. Mirrors the SSTV round trip.
#[test]
fn set_and_clear_wefax_image_round_trips_through_handle_command() {
    let (mut state, dsp_tx, _dsp_rx) = wefax_test_state();
    let image = sdr_radio::wefax_image::WefaxImage::new();

    handle_command(&mut state, &dsp_tx, UiToDsp::SetWefaxImage(image.handle()));
    assert!(
        state.wefax_image.is_some(),
        "SetWefaxImage must wire the handle"
    );

    handle_command(&mut state, &dsp_tx, UiToDsp::ClearWefaxImage);
    assert!(
        state.wefax_image.is_none(),
        "ClearWefaxImage must drop the handle"
    );
}

/// Between-pass / session reset must drop the decoder, clear the
/// rate-failure memo and last-seen state, and wipe stale pixels out
/// of the shared image handle without dropping the handle itself.
/// Mirrors `reset_imaging_decoders_clears_lrpt_image_without_a_decoder`.
#[test]
fn reset_imaging_decoders_clears_wefax_state_and_image() {
    const STALE_WIDTH: u32 = 4;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    let image = sdr_radio::wefax_image::WefaxImage::new();
    let handle = image.handle();
    handle.write_line(0, STALE_WIDTH, &[1, 2, 3, 4]);
    assert!(
        handle.snapshot().is_some(),
        "test premise: a stale row exists"
    );
    state.wefax_image = Some(handle.clone());
    state.wefax_init_failed_at_rate = Some(8_000);
    state.wefax_last_state = Some(sdr_dsp::wefax::WefaxState::Imaging);

    reset_imaging_decoders(&mut state);

    assert!(state.wefax_decoder.is_none());
    assert!(state.wefax_init_failed_at_rate.is_none());
    assert!(state.wefax_last_state.is_none());
    assert!(
        handle.snapshot().is_none(),
        "stale pixels survived the between-pass reset"
    );
    assert!(
        state.wefax_image.is_some(),
        "the handle itself must survive the reset — only its pixels are wiped"
    );
}
