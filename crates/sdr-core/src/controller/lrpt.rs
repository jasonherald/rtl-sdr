//! Meteor-M LRPT decode tap — bridges the post-VFO IQ buffer into the
//! QPSK demod / FEC pipeline and the shared `LrptImage` assembler.

use super::{Complex, DspState, LrptDecoder, LrptDownlink};

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

/// Handler for `UiToDsp::SetLrptImage`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_lrpt_image(state: &mut DspState, image: sdr_radio::lrpt_image::LrptImage) {
    tracing::info!("LRPT image handle attached — decoder tap will push lines");
    state.lrpt_image = Some(image);
    // Decoder state intentionally NOT dropped here.
    // `AppState::lrpt_image` is a long-lived singleton
    // — every `SetLrptImage` carries the same handle —
    // so reattach is logically a no-op for the decoder.
    // Earlier draft dropped it defensively, but that
    // turned the round-11 (`CodeRabbit` PR #543)
    // defensive re-send in `open_lrpt_viewer_if_needed`
    // into a mid-pass decoder reset that lost Viterbi /
    // sync state on every viewer reuse. Decoder
    // lifecycle stays owned by source-stop cleanup —
    // same contract `ClearLrptImage` codifies (round 1).
}

/// Handler for `UiToDsp::SetLrptDownlink`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_set_lrpt_downlink(
    state: &mut DspState,
    downlink: sdr_radio::lrpt_decoder::LrptDownlink,
) {
    tracing::info!("LRPT downlink profile set to {downlink:?}");
    // Drop the existing decoder iff the profile actually
    // changed — re-init lazily on the next IQ chunk with
    // the new chains. A no-op repeat (auto-record
    // re-sending the same profile across overlapping
    // passes) won't cost a Viterbi reset.
    if state.lrpt_downlink != downlink {
        state.lrpt_downlink = downlink;
        // The harvest holds back the in-progress row group
        // (#725); hand it to the viewer before the decoder
        // goes away.
        if let Some(decoder) = state.lrpt_decoder.as_mut() {
            decoder.flush_pending_lines();
        }
        state.lrpt_decoder = None;
        state.lrpt_init_failed = false;
    }
}

/// Handler for `UiToDsp::ClearLrptImage`, extracted from `handle_command`
/// (#816 PR B).
pub(super) fn handle_clear_lrpt_image(state: &mut DspState) {
    tracing::info!("LRPT image handle cleared — decoder tap is silent");
    state.lrpt_image = None;
    // Decoder state stays alive — the tap is already
    // disabled because `lrpt_image` is None, and
    // teardown / reset belong to the source-stop
    // cleanup path. Mirrors the APT decoder, which
    // also keeps its state across stop-listening /
    // resume-listening cycles so resumed listening
    // doesn't pay re-init cost. The `messages.rs`
    // doc-comment for `ClearLrptImage` codifies this
    // contract; an earlier draft contradicted it by
    // dropping the decoder here. Per CodeRabbit
    // round 1 on PR #543.
}
