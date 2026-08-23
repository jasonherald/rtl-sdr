//! Meteor-M LRPT decode tap — bridges the post-VFO IQ buffer into the
//! QPSK demod / FEC pipeline and the shared `LrptImage` assembler.

use super::{Complex, LrptDecoder, LrptDownlink};

/// Meteor-M LRPT decode tap — IQ counterpart of [`apt_decode_tap`].
/// Lazy-initialises the decoder against the shared
/// `LrptImage` handle the wiring layer set via
/// `UiToDsp::SetLrptImage`, then streams the post-VFO IQ slice
/// (`radio_input` — already at 144 ksps thanks to the
/// `DemodMode::Lrpt` IF rate) through the full LRPT chain
/// (QPSK demod, FEC, image assembler). Emitted scan lines
/// land in the shared `LrptImage` for the live viewer to read.
///
/// Only runs when (a) `current_mode == DemodMode::Lrpt` and (b)
/// the wiring layer has handed us a `LrptImage` handle. Without
/// the handle, the tap is silent — manual LRPT-mode use without
/// a viewer harmlessly produces no output. Per epic #469 task 7.
///
/// Takes the decoder + image references directly (rather than
/// `&mut DspState`) so the call site can hold a live borrow of
/// `radio_input` — which itself points into a separate state
/// field (`vfo_buf` or `processed_buf`) — without violating
/// borrow-disjointness.
pub(super) fn lrpt_decode_tap(
    decoder_slot: &mut Option<LrptDecoder>,
    image: Option<&sdr_radio::lrpt_image::LrptImage>,
    radio_input: &[Complex],
    init_failed: &mut bool,
    downlink: LrptDownlink,
) {
    let Some(image) = image else {
        return;
    };
    // One-shot guard: a previous init attempt failed. Skip
    // until source-stop cleanup clears the flag, otherwise
    // we'd warn-log on every IQ chunk (~100 Hz). Per
    // `CodeRabbit` round 12 on PR #543 — mirrors the
    // `apt_init_failed_at_rate` guard on the APT side.
    if *init_failed {
        return;
    }
    if decoder_slot.is_none() {
        match LrptDecoder::new(image.clone(), downlink) {
            Ok(decoder) => {
                tracing::info!(
                    "LRPT decoder initialised at {} Hz IF rate, downlink = {downlink:?}",
                    sdr_dsp::lrpt::SAMPLE_RATE_HZ
                );
                *decoder_slot = Some(decoder);
            }
            Err(e) => {
                tracing::warn!("LRPT decoder init failed: {e}");
                *init_failed = true;
                return;
            }
        }
    }
    let Some(decoder) = decoder_slot.as_mut() else {
        return;
    };
    decoder.process(radio_input);
}
