use super::*;
use crate::messages::SstvImageStart;

/// Synthesize a `SstvEvent::VisDetected` for tests. The
/// inner field values don't matter for counter-update tests
/// — we only care that the event variant is `VisDetected`.
fn fake_vis_event() -> slowrx::SstvEvent {
    slowrx::SstvEvent::VisDetected {
        mode: slowrx::SstvMode::Robot36,
        sample_offset: 0,
        hedr_shift_hz: 0.0,
    }
}

/// Synthesize a `SstvEvent::LineDecoded` for tests. Empty
/// pixel buffer is fine — `record_event` only inspects the
/// variant tag, not contents.
fn fake_line_event() -> slowrx::SstvEvent {
    slowrx::SstvEvent::LineDecoded {
        mode: slowrx::SstvMode::Robot36,
        line_index: 0,
        pixels: Vec::new(),
    }
}

/// Synthesize a `SstvEvent::ImageComplete` for tests. Uses
/// `SstvImage::new` (the public constructor — `SstvImage` is
/// `#[non_exhaustive]` so direct struct-literal init is
/// rejected by the compiler) with a minimal 1×1 size. The
/// counter logic doesn't read the image dimensions; the fake
/// just has to be a valid variant. `partial: false` matches
/// the V1 slowrx contract (only the final clean image surface
/// emits this event).
fn fake_image_complete_event() -> slowrx::SstvEvent {
    image_complete_with_quality(0.9)
}

/// `ImageComplete` with a given line-sync quality score.
fn image_complete_with_quality(quality: f32) -> slowrx::SstvEvent {
    slowrx::SstvEvent::ImageComplete {
        image: slowrx::SstvImage::new(slowrx::SstvMode::Robot36, 1, 1),
        sync_quality: slowrx::SyncQuality::new(quality),
        partial: false,
    }
}

#[test]
fn sstv_pass_stats_default_is_empty() {
    let stats = SstvPassStats::default();
    assert_eq!(stats.vis_count, 0);
    assert_eq!(stats.image_complete_count, 0);
    assert_eq!(stats.lines_decoded, 0);
    assert!(
        !stats.saw_any_event(),
        "default stats must report no events — drives the \
         skip-summary-log decision in reset_imaging_decoders"
    );
}

#[test]
fn sstv_pass_stats_increments_per_event_kind() {
    // Counter dispatch table: each event variant maps to
    // exactly one counter. Per #648.
    let mut stats = SstvPassStats::default();
    stats.record_event(&fake_vis_event());
    assert_eq!(stats.vis_count, 1);
    assert_eq!(stats.image_complete_count, 0);
    assert_eq!(stats.lines_decoded, 0);

    stats.record_event(&fake_line_event());
    stats.record_event(&fake_line_event());
    assert_eq!(stats.lines_decoded, 2);
    assert_eq!(stats.vis_count, 1, "line events must not bump vis_count");

    stats.record_event(&fake_image_complete_event());
    assert_eq!(stats.image_complete_count, 1);
    assert_eq!(stats.vis_count, 1, "image-complete must not bump vis_count");
    assert_eq!(
        stats.lines_decoded, 2,
        "image-complete must not bump lines_decoded"
    );

    assert!(
        stats.saw_any_event(),
        "stats with non-zero counters must report saw_any_event"
    );
}

#[test]
fn sstv_pass_stats_counts_a_realistic_ariss_pass() {
    // Realistic Series 32 pass: 3 VIS bursts (ARISS duty
    // cycle = 36 sec ON / 2 min OFF, typical 7-min pass
    // catches ~3 windows), 2 complete images, 1 partial
    // (~80 lines into the 240-line PD120 image when LOS
    // truncated it). This is the shape we expect post-#648
    // log analysis to reveal. Per #648.
    //
    // Burst-shape constants extracted per CR round 1 — keeps a
    // future test reader from wondering whether 240 / 80 / 3 / 2
    // are arbitrary or load-bearing. Each constant carries the
    // PD120 / Series 32 reference so a rebase against a future
    // mode change updates one place.
    /// VIS bursts captured in the pass — Series 32 duty cycle
    /// fits ~3 windows in a typical 7-minute overpass.
    const ARISS_EXPECTED_VIS_BURSTS: u32 = 3;
    /// Complete images decoded — bursts 1 + 2 finished, burst
    /// 3 was truncated by LOS.
    const ARISS_EXPECTED_COMPLETE_IMAGES: u32 = 2;
    /// PD120 image height in scan lines. Used here to fully
    /// populate bursts 1 + 2 in the synthetic event stream.
    const ARISS_FULL_IMAGE_LINES: usize = 240;
    /// Partial-image scan-line count for burst 3 — mid-frame
    /// when LOS / duty-cycle OFF cut the decode short. ~1/3
    /// of the way into the PD120 image.
    const ARISS_PARTIAL_IMAGE_LINES: usize = 80;

    let mut stats = SstvPassStats::default();

    // Burst 1: VIS → full image lines → ImageComplete
    stats.record_event(&fake_vis_event());
    for _ in 0..ARISS_FULL_IMAGE_LINES {
        stats.record_event(&fake_line_event());
    }
    stats.record_event(&fake_image_complete_event());

    // Burst 2: VIS → full image lines → ImageComplete
    stats.record_event(&fake_vis_event());
    for _ in 0..ARISS_FULL_IMAGE_LINES {
        stats.record_event(&fake_line_event());
    }
    stats.record_event(&fake_image_complete_event());

    // Burst 3: VIS → partial lines → LOS (no ImageComplete)
    stats.record_event(&fake_vis_event());
    for _ in 0..ARISS_PARTIAL_IMAGE_LINES {
        stats.record_event(&fake_line_event());
    }

    assert_eq!(stats.vis_count, ARISS_EXPECTED_VIS_BURSTS);
    assert_eq!(stats.image_complete_count, ARISS_EXPECTED_COMPLETE_IMAGES);
    assert_eq!(
        stats.lines_decoded,
        (ARISS_FULL_IMAGE_LINES * 2 + ARISS_PARTIAL_IMAGE_LINES) as u64,
    );
    // The "partial image" diagnostic: vis_count > image_complete_count
    // AND lines_decoded > 0 means we got imagery but lost it
    // before the final scan-line.
    assert!(stats.vis_count > stats.image_complete_count);
    assert!(stats.lines_decoded > 0);
}

/// #736 — a new VIS means a new image: the in-flight buffer must be
/// reset so a different-geometry mode after an incomplete image is
/// not silently dropped row by row (and the old rows saved as it).
#[test]
fn sstv_vis_detected_resets_the_in_flight_image() {
    const STALE_W: u32 = 320;
    const STALE_H: u32 = 256;
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::sstv_image::SstvImage::new();
    let handle = image.handle();
    handle.write_line(0, STALE_W, STALE_H, &[[1, 2, 3]; STALE_W as usize]);
    assert!(
        handle.snapshot().is_some(),
        "test premise: a stale row exists"
    );
    state.sstv_image = Some(handle.clone());
    let _ = drain(&dsp_rx);

    handle_sstv_event(&mut state, &dsp_tx, fake_vis_event());

    assert!(
        handle.snapshot().is_none(),
        "VIS must reset the in-flight image buffer"
    );
    let events = drain(&dsp_rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DspToUi::SstvVisDetected { .. })),
        "the VIS notification must still reach the UI, got {events:?}"
    );
}

/// A `VisFallback` event (slowrx's lost-VIS fallback).
fn fake_vis_fallback_event() -> slowrx::SstvEvent {
    slowrx::SstvEvent::VisFallback {
        mode: slowrx::SstvMode::Robot36,
        sample_offset: 0,
        hedr_shift_hz: 0.0,
    }
}

#[test]
fn sstv_pass_stats_counts_fallback_starts_separately() {
    let mut stats = SstvPassStats::default();
    stats.record_event(&fake_vis_fallback_event());
    assert_eq!(stats.vis_fallback_count, 1);
    assert_eq!(stats.vis_count, 0, "an assumed header is not a decoded VIS");
    assert!(
        stats.saw_any_event(),
        "a fallback start alone is a pass event"
    );
}

/// A decoded VIS remembers its mode as the lost-VIS fallback for the
/// live decoder and for decoders built in later passes.
#[test]
fn sstv_vis_detected_sets_the_lost_vis_fallback() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    state.sstv_decoder = Some(slowrx::SstvDecoder::new(48_000).unwrap());
    assert_eq!(state.sstv_last_vis_mode, None, "no guess before a VIS");

    handle_sstv_event(&mut state, &dsp_tx, fake_vis_event());

    assert_eq!(state.sstv_last_vis_mode, Some(slowrx::SstvMode::Robot36));
    assert_eq!(
        state.sstv_decoder.as_ref().unwrap().fallback_mode(),
        Some(slowrx::SstvMode::Robot36),
        "the live decoder picks it up immediately"
    );
    // Survives the per-pass reset; the next pass's decoder is built with it.
    reset_imaging_decoders(&mut state);
    assert!(state.sstv_decoder.is_none());
    assert_eq!(state.sstv_last_vis_mode, Some(slowrx::SstvMode::Robot36));
}

/// An assumed start resets the in-flight image like a decoded VIS and
/// tells the UI the mode was assumed.
#[test]
fn sstv_vis_fallback_starts_an_image_flagged_as_assumed() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::sstv_image::SstvImage::new();
    let handle = image.handle();
    handle.write_line(0, 320, 240, &[[1, 2, 3]; 320]);
    state.sstv_image = Some(handle.clone());
    let _ = drain(&dsp_rx);

    handle_sstv_event(&mut state, &dsp_tx, fake_vis_fallback_event());

    assert!(handle.snapshot().is_none(), "in-flight image reset");
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DspToUi::SstvVisDetected {
                mode_label: "Robot 36",
                start: SstvImageStart::LostVis
            }
        )),
        "UI told the mode was assumed, got {events:?}"
    );
}

/// A `SyncStart` event (slowrx started an image from its sync rhythm).
fn fake_sync_start_event() -> slowrx::SstvEvent {
    slowrx::SstvEvent::SyncStart {
        mode: slowrx::SstvMode::Robot36,
        sample_offset: 0,
    }
}

/// An image started from its sync rhythm (no header heard) resets the
/// in-flight image, is counted, and tells the UI how it started.
#[test]
fn sstv_sync_start_starts_an_image_flagged_as_sync_rhythm() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let image = sdr_radio::sstv_image::SstvImage::new();
    let handle = image.handle();
    handle.write_line(0, 320, 240, &[[1, 2, 3]; 320]);
    state.sstv_image = Some(handle.clone());
    let _ = drain(&dsp_rx);

    handle_sstv_event(&mut state, &dsp_tx, fake_sync_start_event());

    assert!(handle.snapshot().is_none(), "in-flight image reset");
    assert_eq!(state.sstv_pass_stats.sync_start_count, 1);
    assert_eq!(state.sstv_pass_stats.vis_count, 0);
    assert!(state.sstv_pass_stats.saw_any_event());
    let events = drain(&dsp_rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DspToUi::SstvVisDetected {
                mode_label: "Robot 36",
                start: SstvImageStart::SyncRhythm
            }
        )),
        "UI told the image started from its sync rhythm, got {events:?}"
    );
}

/// An "image" whose sync pulses never lined up is noise (a VIS whose
/// transmission faded or ended): it is not handed to the UI for the LOS
/// save, and the pass summary counts it.
#[test]
fn sstv_image_that_looks_like_noise_is_not_saved() {
    let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let _ = drain(&dsp_rx);

    handle_sstv_event(&mut state, &dsp_tx, image_complete_with_quality(0.05));
    assert!(
        !drain(&dsp_rx)
            .iter()
            .any(|e| matches!(e, DspToUi::SstvImageComplete { .. })),
        "a noise image must not be queued for saving"
    );
    assert_eq!(state.sstv_pass_stats.noise_images_dropped, 1);
    assert_eq!(state.sstv_pass_stats.image_complete_count, 1);

    handle_sstv_event(&mut state, &dsp_tx, image_complete_with_quality(0.45));
    assert!(
        drain(&dsp_rx)
            .iter()
            .any(|e| matches!(e, DspToUi::SstvImageComplete { .. })),
        "a weak but real image is still saved"
    );
    assert_eq!(state.sstv_pass_stats.noise_images_dropped, 1);
}

/// The modes slowrx 0.8.0 added get real names in the viewer header
/// instead of the generic "SSTV".
#[test]
fn sstv_mode_label_names_the_bw_and_wraase_modes() {
    for (mode, label) in [
        (slowrx::SstvMode::Robot24Bw, "Robot 24 B/W"),
        (slowrx::SstvMode::Robot12Bw, "Robot 12 B/W"),
        (slowrx::SstvMode::Robot8Bw, "Robot 8 B/W"),
        (slowrx::SstvMode::Wraase120, "Wraase SC-2 120"),
        (slowrx::SstvMode::Wraase180, "Wraase SC-2 180"),
    ] {
        assert_eq!(crate::controller::sstv::sstv_mode_label(mode), label);
    }
}

/// The tap's decoder demodulates the analytic signal (slowrx #71), which
/// removes the diagonal cross-hatch slowrx's real-signal demod draws in
/// dark areas.
#[test]
fn sstv_decoder_uses_the_analytic_demod() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    sstv_decode_tap(&mut state, &dsp_tx, 0);
    let decoder = state.sstv_decoder.as_ref().expect("decoder initialised");
    assert!(decoder.analytic_demod());
}

/// A row of `width` pixels all set to `value`.
fn grey_row(width: u32, value: u8) -> Vec<[u8; 3]> {
    vec![[value; 3]; width as usize]
}

/// Robot 12 B/W sends 120 lines that slowrx shows two rows tall
/// (`ModeSpec::line_height`): each decoded line fills both rows of a
/// 320×240 image, as slowrx's saved images do.
#[test]
fn a_double_height_line_fills_two_rows_of_the_image() {
    let (dsp_tx, _dsp_rx) = mpsc::channel::<DspToUi>();
    let mut state = DspState::new(dsp_tx.clone()).unwrap();
    let handle = sdr_radio::sstv_image::SstvImage::new().handle();
    state.sstv_image = Some(handle.clone());
    for line_index in 0..2 {
        let event = slowrx::SstvEvent::LineDecoded {
            mode: slowrx::SstvMode::Robot12Bw,
            line_index,
            pixels: grey_row(320, 10 + u8::try_from(line_index).unwrap()),
        };
        handle_sstv_event(&mut state, &dsp_tx, event);
    }
    let snap = handle.snapshot().expect("rows written");
    assert_eq!((snap.width, snap.height), (320, 240));
    assert_eq!(snap.lines_written, 4);
    let row = |r: usize| snap.pixels[r * 320];
    assert_eq!([row(0), row(1), row(2), row(3)], [[10; 3], [10; 3], [11; 3], [11; 3]]);
}

/// The image handed to the UI for saving is the double-height one, both
/// from the shared handle and from slowrx's own buffer when no viewer
/// handle is wired.
#[test]
fn a_double_height_image_is_saved_at_display_height() {
    for wired in [true, false] {
        let (dsp_tx, dsp_rx) = mpsc::channel::<DspToUi>();
        let mut state = DspState::new(dsp_tx.clone()).unwrap();
        if wired {
            state.sstv_image = Some(sdr_radio::sstv_image::SstvImage::new().handle());
        }
        let mut image = slowrx::SstvImage::new(slowrx::SstvMode::Robot12Bw, 320, 120);
        for (row, chunk) in image.pixels.chunks_mut(320).enumerate() {
            chunk.fill([u8::try_from(row).unwrap(); 3]);
            let event = slowrx::SstvEvent::LineDecoded {
                mode: slowrx::SstvMode::Robot12Bw,
                line_index: u32::try_from(row).unwrap(),
                pixels: chunk.to_vec(),
            };
            handle_sstv_event(&mut state, &dsp_tx, event);
        }
        let complete = slowrx::SstvEvent::ImageComplete {
            image,
            sync_quality: slowrx::SyncQuality::new(0.9),
            partial: false,
        };
        handle_sstv_event(&mut state, &dsp_tx, complete);
        let saved = drain(&dsp_rx).into_iter().find_map(|e| match e {
            DspToUi::SstvImageComplete {
                width,
                height,
                pixels,
            } => Some((width, height, pixels)),
            _ => None,
        });
        let (width, height, pixels) = saved.expect("image queued for saving");
        assert_eq!((width, height, pixels.len()), (320, 240, 320 * 240), "wired={wired}");
        assert_eq!(pixels[239 * 320], [119; 3], "wired={wired}");
    }
}
