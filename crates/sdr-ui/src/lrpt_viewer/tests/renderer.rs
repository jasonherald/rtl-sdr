use super::*;

#[test]
fn renderer_starts_empty_with_no_active_channel() {
    let r = LrptImageRenderer::new();
    assert!(r.is_empty());
    assert!(r.active_apid().is_none());
    assert!(r.known_apids().is_empty());
}

#[test]
fn push_line_lazily_allocates_surface_per_apid() {
    let mut r = LrptImageRenderer::new();
    r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    assert_eq!(r.n_lines(APID_TEST), 1);
    // The other APID has never been pushed — n_lines returns 0
    // and the channel doesn't exist yet.
    assert_eq!(r.n_lines(APID_TEST_2), 0);
    assert_eq!(r.known_apids(), vec![APID_TEST]);
}

#[test]
fn first_push_auto_selects_that_apid() {
    let mut r = LrptImageRenderer::new();
    r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    // The user shouldn't have to manually pick a channel
    // before any data is visible — pushing the first line
    // for any APID auto-selects it.
    assert_eq!(r.active_apid(), Some(APID_TEST));
}

#[test]
fn subsequent_push_for_different_apid_doesnt_steal_active() {
    // First-push auto-select shouldn't keep firing — the
    // user's pick (or the initial pick) stays sticky as
    // additional channels appear.
    let mut r = LrptImageRenderer::new();
    r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    r.push_line(APID_TEST_2, &synth_line(TEST_PIXEL_2));
    assert_eq!(r.active_apid(), Some(APID_TEST));
}

#[test]
fn push_line_caps_at_max_lines_per_channel() {
    let mut r = LrptImageRenderer::new();
    for _ in 0..MAX_LINES {
        assert_eq!(
            r.push_line(APID_TEST, &synth_line(TEST_PIXEL)),
            PushOutcome::Pushed,
        );
    }
    assert_eq!(r.n_lines(APID_TEST), MAX_LINES);
    // One more push past the cap reports `Capped` — caller's
    // watermark should still advance (further pushes won't
    // succeed no matter how many retries).
    assert_eq!(
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL)),
        PushOutcome::Capped,
    );
    assert_eq!(r.n_lines(APID_TEST), MAX_LINES);
}

#[test]
fn push_line_with_wrong_width_is_dropped() {
    let mut r = LrptImageRenderer::new();
    // IMAGE_WIDTH is 1568; deliberately pass a short slice.
    assert_eq!(
        r.push_line(APID_TEST, &[TEST_PIXEL; 16]),
        PushOutcome::InvalidLine,
    );
    // No surface allocated, no line counted.
    assert_eq!(r.n_lines(APID_TEST), 0);
    assert!(r.known_apids().is_empty());
}

#[test]
fn push_outcome_consumed_pins_watermark_semantics() {
    // Pin the contract `LrptImageView::drain_new_lines`
    // depends on: only `TransientFailure` leaves the row
    // in the source for retry. Per `CodeRabbit` round 3
    // on PR #543.
    assert!(PushOutcome::Pushed.consumed());
    assert!(PushOutcome::Capped.consumed());
    assert!(PushOutcome::InvalidLine.consumed());
    assert!(!PushOutcome::TransientFailure.consumed());
}

#[test]
fn set_active_apid_only_succeeds_for_known_channels() {
    let mut r = LrptImageRenderer::new();
    r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    // Existing APID — switch succeeds.
    assert!(r.set_active_apid(APID_TEST));
    assert_eq!(r.active_apid(), Some(APID_TEST));
    // Unknown APID — switch refused, active stays put.
    assert!(!r.set_active_apid(APID_TEST_2));
    assert_eq!(r.active_apid(), Some(APID_TEST));
}

#[test]
fn clear_drops_all_channels_and_active_selection() {
    let mut r = LrptImageRenderer::new();
    r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    r.push_line(APID_TEST_2, &synth_line(TEST_PIXEL_2));
    r.clear();
    assert!(r.is_empty());
    assert!(r.active_apid().is_none());
    assert!(r.known_apids().is_empty());
}

#[test]
fn pixel_layout_is_argb32_with_grayscale_in_bgr_channels() {
    // Same invariant as the APT renderer test: Cairo's
    // ARGB32 little-endian layout = B, G, R, A. Every
    // channel of the input greyscale value goes into all
    // three colour bytes; alpha is opaque.
    let mut r = LrptImageRenderer::new();
    let mut line = vec![0_u8; IMAGE_WIDTH];
    line[0] = 0x80;
    line[1] = 0xC0;
    r.push_line(APID_TEST, &line);
    let surface = &mut r.channels.get_mut(&APID_TEST).unwrap().surface;
    let data = surface.data().unwrap();
    assert_eq!(&data[0..4], &[0x80, 0x80, 0x80, 0xFF]);
    assert_eq!(&data[4..8], &[0xC0, 0xC0, 0xC0, 0xFF]);
}

#[test]
fn renderer_starts_in_single_channel_mode() {
    let r = LrptImageRenderer::new();
    assert!(!r.is_composite_active());
    assert!(r.active_composite().is_none());
}

#[test]
fn renderer_clear_drops_composite_state() {
    // `clear()` is between-pass cleanup; it must drop the
    // composite alongside the per-APID surfaces so a fresh
    // pass doesn't paint stale RGB pixels until the
    // dropdown handler rebuilds.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
    image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
    image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
    r.set_composite(recipe, &image);
    assert!(r.is_composite_active());
    r.clear();
    assert!(!r.is_composite_active());
    assert!(r.active_composite().is_none());
}
