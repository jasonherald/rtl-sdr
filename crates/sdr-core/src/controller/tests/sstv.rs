use super::*;

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
    slowrx::SstvEvent::ImageComplete {
        image: slowrx::SstvImage::new(slowrx::SstvMode::Robot36, 1, 1),
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
