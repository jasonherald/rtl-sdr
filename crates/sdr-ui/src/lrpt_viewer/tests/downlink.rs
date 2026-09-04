use super::*;

/// The AOS wiring's catalog → profile mapping: current Meteors
/// are differentially-precoded OQPSK (#892); an uncatalogued id
/// falls back to plain QPSK.
#[test]
fn lrpt_downlink_for_maps_the_catalog_profile() {
    use sdr_dsp::lrpt::LrptMode;
    use sdr_radio::lrpt_decoder::LrptDownlink;
    const UNCATALOGUED_NORAD_ID: u32 = 1;
    for norad_id in [sdr_sat::METEOR_M2_3_NORAD_ID, sdr_sat::METEOR_M2_4_NORAD_ID] {
        assert_eq!(
            lrpt_downlink_for(norad_id),
            LrptDownlink::new(LrptMode::Oqpsk, true)
        );
    }
    assert_eq!(
        lrpt_downlink_for(UNCATALOGUED_NORAD_ID),
        LrptDownlink::new(LrptMode::Qpsk, false)
    );
}

/// AOS queues the profile before the canvas wipe, so the DSP
/// thread flushes the previous decoder's tail before clearing.
#[test]
fn lrpt_pass_start_sends_profile_then_clear() {
    use sdr_core::messages::UiToDsp;
    use sdr_dsp::lrpt::LrptMode;
    use sdr_radio::lrpt_decoder::LrptDownlink;
    let image = sdr_radio::lrpt_image::LrptImage::new();
    let [first, second] = lrpt_pass_start_commands(sdr_sat::METEOR_M2_4_NORAD_ID, &image);
    assert!(matches!(
        first,
        UiToDsp::SetLrptDownlink(profile) if profile == LrptDownlink::new(LrptMode::Oqpsk, true)
    ));
    assert!(matches!(second, UiToDsp::ClearLrptImageContents(_)));
}
