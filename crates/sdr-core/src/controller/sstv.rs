//! ISS SSTV decode tap — slowrx decoder bridge, per-pass statistics, and
//! SSTV event handling.

use super::{DspState, DspToUi, SstvDecoder, mpsc};

/// Per-SSTV-pass diagnostic counters. Reset between passes by
/// [`reset_imaging_decoders`] (which logs a summary first). Lets
/// a post-pass log analysis answer:
/// - "Did the satellite transmit at all?" (VIS count > 0)
/// - "How many complete images did we get?" (`image_complete_count`)
/// - "Did any decode get cut short?" (`lines_decoded` > 0 with
///   `image_complete_count` == 0 means partial — the duty-cycle
///   OFF window or the satellite going below horizon truncated a
///   mid-decode image).
///
/// All fields are pure observational counters — incremented from
/// the SSTV event loop in `sstv_decode_tap`. No processing impact.
/// Per #648.
#[derive(Debug, Default)]
pub(super) struct SstvPassStats {
    /// VIS headers detected since the last reset. Each detection
    /// indicates the start of a new image (Robot 36 / PD120 /
    /// Scottie / etc.).
    pub(super) vis_count: u32,
    /// Complete images decoded. A "complete" image is one where
    /// the decoder emitted `SstvEvent::ImageComplete` — partial
    /// images that ran out of audio mid-frame don't count here
    /// (use `lines_decoded` to see if any imagery was captured).
    pub(super) image_complete_count: u32,
    /// Total scan lines decoded across all images this pass
    /// (complete + partial). A pass with `vis_count > 0`,
    /// `image_complete_count == 0`, and `lines_decoded > 0` got
    /// imagery but lost it before the final scan-line — a hint
    /// that the pass elevation dropped or the duty-cycle OFF
    /// window started before image completion.
    pub(super) lines_decoded: u64,
}

impl SstvPassStats {
    /// Whether any SSTV event fired this pass. Drives the
    /// "log summary or skip" decision in [`reset_imaging_decoders`]
    /// — a no-op reset (e.g. between two non-SSTV passes) shouldn't
    /// emit a "0 / 0 / 0" log line and clutter the trace.
    pub(super) fn saw_any_event(&self) -> bool {
        self.vis_count > 0 || self.image_complete_count > 0 || self.lines_decoded > 0
    }

    /// Mutate counters for one [`slowrx::SstvEvent`]. Extracted from
    /// the inline match arms in `sstv_decode_tap` so the counter
    /// logic is testable without spinning up a real `SstvDecoder`
    /// (which would need synthetic SSTV-mode tone generation, a
    /// significant scaffolding investment that's deferred per #648).
    /// Per #648.
    pub(super) fn record_event(&mut self, event: &slowrx::SstvEvent) {
        match event {
            slowrx::SstvEvent::VisDetected { .. } => {
                self.vis_count += 1;
            }
            slowrx::SstvEvent::LineDecoded { .. } => {
                self.lines_decoded += 1;
            }
            slowrx::SstvEvent::ImageComplete { .. } => {
                self.image_complete_count += 1;
            }
            // `SstvEvent` is `#[non_exhaustive]` — silently ignore
            // future variants so a slowrx update doesn't require a
            // source change here. Same forward-compat stance as the
            // event-dispatch loop in `sstv_decode_tap`.
            _ => {}
        }
    }
}

/// ISS SSTV decode tap. Mirrors [`apt_decode_tap`]'s shape: lazy-init
/// the [`SstvDecoder`] at the `RadioModule`'s current audio sample
/// rate, downmix the post-`radio.process` stereo audio block to mono,
/// feed the decoder, and dispatch events through the DSP→UI channel.
///
/// Only runs in NFM mode — the SSTV 1200–2300 Hz subcarrier rides on
/// wide-FM-style audio which the NFM demod captures cleanly.
///
/// Events dispatched:
/// - `SstvEvent::LineDecoded` → calls `image_handle.write_line` if
///   the handle is wired; also sends `DspToUi::SstvLineDecoded` for
///   the viewer's redraw trigger.
/// - `SstvEvent::ImageComplete` → calls `image_handle.take_completed`
///   and sends `DspToUi::SstvImageComplete` so the wiring layer can
///   queue the completed image for LOS save.
/// - `SstvEvent::VisDetected` → logged via `tracing::info!` only.
///
/// Per epic #472.
pub(super) fn sstv_decode_tap(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    audio_count: usize,
) {
    // Lazy-init. Audio rate comes from `RadioModule::audio_sample_rate`
    // (typically 48 kHz, well within slowrx's accepted range).
    if state.sstv_decoder.is_none() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate_hz = state.radio.audio_sample_rate() as u32;
        // Guard against retry-spamming the warn log on a rate the
        // decoder will reject. Mirrors `apt_init_failed_at_rate`.
        if state.sstv_init_failed_at_rate == Some(rate_hz) {
            return;
        }
        match SstvDecoder::new(rate_hz) {
            Ok(decoder) => {
                tracing::info!("SSTV decoder initialised at {rate_hz} Hz");
                state.sstv_decoder = Some(decoder);
                state.sstv_init_failed_at_rate = None;
            }
            Err(e) => {
                tracing::warn!("SSTV decoder init failed at {rate_hz} Hz: {e}");
                state.sstv_init_failed_at_rate = Some(rate_hz);
                return;
            }
        }
    }
    let Some(decoder) = state.sstv_decoder.as_mut() else {
        return;
    };

    // Mono downmix. SSTV is mono — averaging L+R is equivalent to
    // either channel for FM-demodulated audio. Mirrors `apt_mono_buf`.
    // Pre-gate audio for the same reason as the APT tap (#734).
    super::audio::downmix_pre_gate_mono(&state.radio, audio_count, &mut state.sstv_mono_buf);

    // `SstvDecoder::process` returns a `Vec<SstvEvent>` — iterate and
    // dispatch. `SstvEvent` is `#[non_exhaustive]`, so a wildcard arm
    // handles future mode additions without a compile break.
    for event in decoder.process(&state.sstv_mono_buf) {
        handle_sstv_event(state, dsp_tx, event);
    }
}

/// Dispatch one slowrx event: pass-stats bookkeeping, the shared
/// image buffer, and the UI notifications. Split out of
/// [`sstv_decode_tap`] so it can be exercised without a decoder.
pub(super) fn handle_sstv_event(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    event: slowrx::SstvEvent,
) {
    // Update per-pass diagnostic counters before dispatch — the
    // counters tell us at LOS how many VIS were detected, how
    // many images completed, and how many lines decoded across
    // all (complete + partial) images. Per #648.
    state.sstv_pass_stats.record_event(&event);
    {
        match event {
            slowrx::SstvEvent::VisDetected {
                mode,
                sample_offset,
                hedr_shift_hz,
            } => {
                tracing::info!(
                    ?mode,
                    sample_offset,
                    hedr_shift_hz,
                    "SSTV VIS detected — new image starting"
                );
                // A new VIS is a new image. The shared buffer latches
                // its geometry on the first row and only resets on
                // `take_completed` / `clear`, so an incomplete image
                // (fade-out mid-pass) followed by a different mode
                // would have every row of the new image silently
                // dropped and the old rows saved as it (#736).
                if let Some(handle) = state.sstv_image.as_ref() {
                    handle.clear();
                }
                // Surface the mode in the viewer's header so the
                // user can see which PD-family variant is being
                // decoded. `&'static str` because the slowrx mode
                // name list is bounded and stable. Per epic #472
                // mode-display follow-up.
                let _ = dsp_tx.send(DspToUi::SstvVisDetected {
                    mode_label: sstv_mode_label(mode),
                });
            }
            slowrx::SstvEvent::LineDecoded {
                mode,
                line_index,
                ref pixels,
            } => {
                // Write into the shared image handle (if the UI wired one).
                if let Some(ref handle) = state.sstv_image {
                    // Derive width/height from the mode spec. The PD
                    // family (PD120 / PD180 / PD240) is all 640 px
                    // wide; other modes vary. `SstvMode` is
                    // `#[non_exhaustive]` so we use the spec helper
                    // (`slowrx::for_mode` — the free function in
                    // `slowrx::modespec`, re-exported at the crate root).
                    let spec = slowrx::for_mode(mode);
                    handle.write_line(line_index, spec.line_pixels, spec.image_lines, pixels);
                }
                let _ = dsp_tx.send(DspToUi::SstvLineDecoded(line_index));
            }
            slowrx::SstvEvent::ImageComplete { image, .. } => {
                handle_sstv_image_complete(state, dsp_tx, image);
            }
            _ => {
                // `SstvEvent` is `#[non_exhaustive]` — silently
                // ignore future variants so a slowrx update doesn't
                // require a source change here.
            }
        }
    }
}

/// Finish one decoded SSTV frame: drain the completed image out of
/// the shared handle (or fall back to the slowrx-owned buffer when no
/// handle is wired) and hand it to the UI for the LOS save. Split out
/// of [`handle_sstv_event`] per CR on PR #841.
fn handle_sstv_image_complete(
    state: &mut DspState,
    dsp_tx: &mpsc::Sender<DspToUi>,
    image: slowrx::SstvImage,
) {
    // `take_completed` atomically swaps out the in-flight
    // pixel buffer and resets for the next VIS detection.
    // Move the completed image (via the shared handle's
    // take path) into the DspToUi message for the wiring
    // layer to save at LOS. If no handle is wired, fall
    // back to the slowrx-owned `image` directly.
    let completed = state
        .sstv_image
        .as_ref()
        .and_then(sdr_radio::sstv_image::SstvImageHandle::take_completed);
    let width = image.width;
    let height = image.height;
    // `map_or` consumes `image.pixels` directly in the
    // `None` arm, avoiding the `.clone()` the
    // `map_or_else` call had. The `Some` arm returns
    // the handle's buffer — both arms are owned moves.
    let pixels = completed.map_or(image.pixels, |c| c.pixels);
    let _ = dsp_tx.send(DspToUi::SstvImageComplete {
        width,
        height,
        pixels,
    });
}

/// Map a [`slowrx::SstvMode`] to a `&'static str` mode name for
/// the UI's viewer-header display. The string list intentionally
/// uses `&'static str` (not `String`) because the slowrx mode
/// names are bounded and stable — VIS detect fires once per
/// image and we don't want to allocate per event.
///
/// Wildcard fallback `"SSTV"` covers future slowrx modes that
/// land before we add an explicit arm here. `SstvMode` is
/// `#[non_exhaustive]` so this match must always have one.
/// Per epic #472 mode-display follow-up.
pub(super) fn sstv_mode_label(mode: slowrx::SstvMode) -> &'static str {
    match mode {
        slowrx::SstvMode::Pd120 => "PD120",
        slowrx::SstvMode::Pd180 => "PD180",
        slowrx::SstvMode::Pd240 => "PD240",
        slowrx::SstvMode::Robot24 => "Robot 24",
        slowrx::SstvMode::Robot36 => "Robot 36",
        slowrx::SstvMode::Robot72 => "Robot 72",
        slowrx::SstvMode::Scottie1 => "Scottie 1",
        slowrx::SstvMode::Scottie2 => "Scottie 2",
        slowrx::SstvMode::ScottieDx => "Scottie DX",
        slowrx::SstvMode::Martin1 => "Martin 1",
        slowrx::SstvMode::Martin2 => "Martin 2",
        _ => "SSTV",
    }
}
