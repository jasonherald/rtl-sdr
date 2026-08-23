use super::*;

/// #700 — the shared LRPT canvas must be cleared between passes even
/// when no decoder is alive to do it as a side effect (e.g. after a
/// modulation change dropped it), or pass 2 composites onto pass 1.
/// #725 (Codacy on PR #802) — the harvest holds back the
/// in-progress row group; a modulation change drops the decoder,
/// so the pending group must be flushed to the shared image first.
#[test]
fn lrpt_modulation_change_flushes_the_pending_row_group() {
    use sdr_dsp::lrpt::LrptMode;
    use sdr_radio::lrpt_decoder::LrptDecoder;
    const APID: u16 = 64;
    /// One JPEG MCU is 8 × 8 px; a row group is `MCU_SIDE` lines.
    const MCU_SIDE: usize = 8;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    let mut decoder =
        LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Oqpsk, false)).unwrap();
    decoder
        .assembler_mut()
        .place_mcu(APID, 0, 0, &[[200_u8; MCU_SIDE]; MCU_SIDE]);
    state.lrpt_downlink = LrptDownlink::new(LrptMode::Oqpsk, false);
    state.lrpt_image = Some(image.clone());
    state.lrpt_decoder = Some(decoder);
    assert!(
        image.snapshot_channel(APID).is_none(),
        "held back until flushed"
    );

    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetLrptDownlink(LrptDownlink::new(LrptMode::Qpsk, false)),
    );

    assert!(state.lrpt_decoder.is_none(), "decoder dropped for re-init");
    let snap = image.snapshot_channel(APID).expect("pending group flushed");
    assert_eq!(snap.lines, MCU_SIDE);
}

/// The lazy-init path builds the decoder with the profile the
/// controller was told about — modulation and precoding — and a
/// later chunk reuses that decoder (#730).
#[test]
fn lrpt_decode_tap_lazily_builds_the_decoder_with_the_profile() {
    use sdr_dsp::lrpt::LrptMode;
    let image = sdr_radio::lrpt_image::LrptImage::new();
    let mut slot: Option<LrptDecoder> = None;
    let mut init_failed = false;
    let profile = LrptDownlink::new(LrptMode::Oqpsk, true);
    let zeros = vec![Complex::default(); 256];
    lrpt_decode_tap(&mut slot, Some(&image), &zeros, &mut init_failed, profile);
    assert!(!init_failed);
    assert_eq!(slot.as_ref().map(LrptDecoder::downlink), Some(profile));
    // A populated slot is reused, not rebuilt.
    lrpt_decode_tap(&mut slot, Some(&image), &zeros, &mut init_failed, profile);
    assert!(!init_failed);
    assert_eq!(slot.as_ref().map(LrptDecoder::downlink), Some(profile));
    // No image handle → no decoder is built.
    let mut no_image: Option<LrptDecoder> = None;
    lrpt_decode_tap(&mut no_image, None, &zeros, &mut init_failed, profile);
    assert!(no_image.is_none());
}

/// AOS ordering: `SetLrptDownlink` flushes the old decoder's
/// held-back row group into the shared image, then
/// `ClearLrptImageContents` wipes the canvas — in that order on
/// the DSP queue, so the previous pass's tail cannot survive onto
/// the new pass (CR on PR #806).
#[test]
fn lrpt_profile_change_then_clear_leaves_an_empty_canvas() {
    use sdr_dsp::lrpt::LrptMode;
    /// One JPEG MCU is 8 × 8 px (sdr-core has no `sdr_lrpt` dep).
    const MCU_SIDE: usize = 8;
    const APID: u16 = 64;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    let mut decoder =
        LrptDecoder::new(image.clone(), LrptDownlink::new(LrptMode::Oqpsk, false)).unwrap();
    decoder
        .assembler_mut()
        .place_mcu(APID, 0, 0, &[[200_u8; MCU_SIDE]; MCU_SIDE]);
    state.lrpt_downlink = LrptDownlink::new(LrptMode::Oqpsk, false);
    state.lrpt_image = Some(image.clone());
    state.lrpt_decoder = Some(decoder);
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetLrptDownlink(LrptDownlink::new(LrptMode::Qpsk, false)),
    );
    assert!(
        image.snapshot_channel(APID).is_some(),
        "flushed tail lands first"
    );
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::ClearLrptImageContents(image.clone()),
    );
    assert!(
        image.snapshot_channel(APID).is_none(),
        "then the canvas is wiped"
    );
}

/// A precoding change alone (same modulation) also drops the
/// decoder so the next init builds the right FEC chain (#730).
#[test]
fn lrpt_precoding_change_drops_the_decoder() {
    use sdr_dsp::lrpt::LrptMode;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    state.lrpt_downlink = LrptDownlink::new(LrptMode::Oqpsk, false);
    state.lrpt_decoder = Some(LrptDecoder::new(image.clone(), state.lrpt_downlink).unwrap());
    state.lrpt_image = Some(image);
    handle_command(
        &mut state,
        &dsp_tx,
        UiToDsp::SetLrptDownlink(LrptDownlink::new(LrptMode::Oqpsk, true)),
    );
    assert!(state.lrpt_decoder.is_none(), "decoder dropped for re-init");
    assert_eq!(
        state.lrpt_downlink,
        LrptDownlink::new(LrptMode::Oqpsk, true)
    );
}

#[test]
fn reset_imaging_decoders_clears_lrpt_image_without_a_decoder() {
    const APID: u16 = 64;
    const LINE_WIDTH: usize = 8;
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx).unwrap();
    let image = sdr_radio::lrpt_image::LrptImage::new();
    image.push_line(APID, &[0x80; LINE_WIDTH]);
    assert!(
        !image.channel_apids().is_empty(),
        "test premise: a line landed"
    );
    state.lrpt_image = Some(image);
    state.lrpt_decoder = None;

    reset_imaging_decoders(&mut state);

    let image = state.lrpt_image.as_ref().unwrap();
    assert!(
        image.channel_apids().is_empty(),
        "stale pixels survived the between-pass reset"
    );
}
