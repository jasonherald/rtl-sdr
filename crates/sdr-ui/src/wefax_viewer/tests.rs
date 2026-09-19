use super::*;

/// Build a `WefaxSnapshot` of `width × height` solid mid-grey pixels
/// (deterministic writes).
fn snap(width: u32, height: u32) -> WefaxSnapshot {
    let n = (width as usize) * (height as usize);
    WefaxSnapshot {
        width,
        height,
        pixels: vec![0x80; n],
    }
}

#[test]
fn gray_to_argb_replicates_channels() {
    let argb = gray_to_argb(&[0u8, 128, 255], 3);
    // little-endian B, G, R, A; grey replicates across B = G = R.
    assert_eq!(&argb[4..8], &[128, 128, 128, 255]);
}

#[test]
fn gray_to_argb_first_and_last_pixels() {
    let argb = gray_to_argb(&[0u8, 128, 255], 3);
    assert_eq!(&argb[0..4], &[0, 0, 0, 255]);
    assert_eq!(&argb[8..12], &[255, 255, 255, 255]);
}

#[test]
fn gray_to_argb_pads_short_row_with_black() {
    // A `gray` slice shorter than `width` must not panic — missing
    // columns pack as black (0x00) rather than reading out of bounds.
    let argb = gray_to_argb(&[255u8], 3);
    assert_eq!(argb.len(), 12);
    assert_eq!(&argb[0..4], &[255, 255, 255, 255]);
    assert_eq!(&argb[4..8], &[0, 0, 0, 255]);
    assert_eq!(&argb[8..12], &[0, 0, 0, 255]);
}

#[test]
fn renderer_initial_state_is_empty() {
    let r = WefaxImageRenderer::new();
    assert!(r.surface.is_none());
    assert_eq!(r.width, 0);
    assert_eq!(r.height, 0);
    assert_eq!(r.lines_written, 0);
    assert!(r.last_snapshot.is_none());
}

#[test]
fn renderer_first_snapshot_allocates_surface() {
    let mut r = WefaxImageRenderer::new();
    let changed = r.update_from_snapshot(snap(1809, 1));
    assert!(
        changed,
        "first snapshot with one line should report changed"
    );
    assert!(
        r.surface.is_some(),
        "surface should allocate on first update"
    );
    assert_eq!(r.width, 1809);
    assert_eq!(r.lines_written, 1);
    assert!(r.last_snapshot.is_some());
}

#[test]
fn renderer_growing_snapshot_advances() {
    // WEFAX charts have no fixed line count — every snapshot with
    // more rows than before should repaint.
    let mut r = WefaxImageRenderer::new();
    let _ = r.update_from_snapshot(snap(1809, 100));
    assert_eq!(r.lines_written, 100);

    let changed = r.update_from_snapshot(snap(1809, 250));
    assert!(changed, "chart growth should report changed");
    assert_eq!(r.lines_written, 250);
}

#[test]
fn renderer_same_height_reports_unchanged() {
    let mut r = WefaxImageRenderer::new();
    let _ = r.update_from_snapshot(snap(1809, 100));

    let changed = r.update_from_snapshot(snap(1809, 100));
    assert!(!changed, "same height should report unchanged");
    assert_eq!(r.lines_written, 100);
}

#[test]
fn renderer_shrunk_snapshot_resets_instead_of_suppressing() {
    // Regression for the live-hit "new lines tile in at the bottom"
    // bug: `WefaxImageHandle::take_completed` resets the shared
    // buffer to height 0 when a chart completes, so the next chart's
    // early (small) snapshots must reset the renderer's watermark
    // and repaint from the top — not be swallowed by the stale
    // `lines_written` left over from the previous, taller chart.
    let mut r = WefaxImageRenderer::new();
    let _ = r.update_from_snapshot(snap(1809, 500));
    assert_eq!(r.lines_written, 500);

    let changed = r.update_from_snapshot(snap(1809, 3));
    assert!(
        changed,
        "a shorter snapshot after a completed chart must reset and repaint, not suppress"
    );
    assert_eq!(
        r.lines_written, 3,
        "watermark should reset to the new chart's height, not stay stuck at the old chart's"
    );
}

#[test]
fn renderer_width_change_rebuilds_surface() {
    // A new chart (different width) rebuilds even if the reported
    // height is smaller than the previous chart's.
    let mut r = WefaxImageRenderer::new();
    let _ = r.update_from_snapshot(snap(1809, 100));
    let changed = r.update_from_snapshot(snap(1200, 5));
    assert!(changed);
    assert_eq!(r.width, 1200);
    assert_eq!(r.lines_written, 5);
}

#[test]
fn renderer_clear_resets_to_empty() {
    let mut r = WefaxImageRenderer::new();
    let _ = r.update_from_snapshot(snap(1809, 100));
    r.clear();
    assert_eq!(r.lines_written, 0);
    assert_eq!(r.width, 0);
    assert!(r.surface.is_none());
    assert!(r.last_snapshot.is_none());
}

#[test]
fn paused_update_freezes_canvas_and_resume_resyncs() {
    // Regression for the Pause button breaking after the scroll-follow
    // rework (commit f3eba3bb): that change moved the drawing area's
    // `set_content_width/height` growth *outside* the `paused` gate, and
    // growing a `DrawingArea`'s content size forces GTK to repaint the
    // freshly-updated surface — so the "frozen" canvas kept advancing
    // while paused. This asserts the visible canvas (content height)
    // stays frozen while paused and re-syncs to the accumulated height
    // on resume.
    //
    // GTK widget test: needs an initialized GTK toolkit + a display.
    // Skips gracefully in headless CI (no display); runs on dev boxes.
    if gtk4::init().is_err() {
        return;
    }
    let view = WefaxImageView::new();
    let image = sdr_radio::wefax_image::WefaxImage::new();
    let handle = image.handle();

    // First line, not paused → canvas grows to the chart's height.
    handle.write_line(0, 1809, &vec![0x80; 1809]);
    view.update_from_handle(&handle);
    assert_eq!(
        view.drawing_area().content_height(),
        1,
        "unpaused update should grow the canvas to the chart height"
    );

    // Pause, then accumulate more lines and pump an update. The frozen
    // canvas must NOT track the new lines.
    view.set_paused(true);
    for row in 1..50 {
        handle.write_line(row, 1809, &vec![0x80; 1809]);
    }
    view.update_from_handle(&handle);
    assert_eq!(
        view.drawing_area().content_height(),
        1,
        "paused canvas must stay frozen — content height must not track lines decoded while paused"
    );

    // Resume → canvas re-syncs to the height accumulated while paused.
    view.set_paused(false);
    assert_eq!(
        view.drawing_area().content_height(),
        50,
        "resume must re-sync the canvas to the lines accumulated while paused"
    );
}

#[test]
fn zero_width_snapshot_is_dropped_without_panic() {
    // `WefaxSnapshot` is public, so a caller can supply width == 0.
    // The renderer must drop it at the boundary rather than panic in
    // `chunks_exact(0)`.
    let mut r = WefaxImageRenderer::new();
    let changed = r.update_from_snapshot(snap(0, 5));
    assert!(
        !changed,
        "zero-width snapshot should be dropped, not rendered"
    );
    assert!(
        r.surface.is_none(),
        "no surface should be allocated for width 0"
    );
    assert_eq!(r.lines_written, 0);
}

#[test]
fn wefax_state_label_maps_all_variants() {
    assert_eq!(wefax_state_label(WefaxState::Idle), "Idle");
    assert_eq!(wefax_state_label(WefaxState::Phasing), "Phasing");
    assert_eq!(wefax_state_label(WefaxState::Imaging), "Imaging");
    assert_eq!(wefax_state_label(WefaxState::Stopped), "Stopped");
}
