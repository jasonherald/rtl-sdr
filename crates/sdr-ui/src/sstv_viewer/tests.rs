use super::*;

/// Build an `SstvSnapshot` with `lines_written` rows of solid grey
/// (so writes are deterministic). PD-family modes (PD120 / PD180
/// / PD240) all use width 640.
fn snap(width: u32, height: u32, lines_written: u32) -> SstvSnapshot {
    let n = (width as usize) * (height as usize);
    SstvSnapshot {
        width,
        height,
        pixels: vec![[0x80, 0x80, 0x80]; n],
        lines_written,
    }
}

#[test]
fn renderer_initial_state_is_empty() {
    let r = SstvImageRenderer::new();
    assert!(r.surface.is_none());
    assert_eq!(r.width, 0);
    assert_eq!(r.height, 0);
    assert_eq!(r.lines_written, 0);
    assert!(r.last_snapshot.is_none());
}

#[test]
fn renderer_first_snapshot_allocates_surface() {
    let mut r = SstvImageRenderer::new();
    let changed = r.update_from_snapshot(snap(640, 496, 1));
    assert!(
        changed,
        "first snapshot with one line should report changed"
    );
    assert!(
        r.surface.is_some(),
        "surface should allocate on first update"
    );
    assert_eq!(r.width, 640);
    assert_eq!(r.height, 496);
    assert_eq!(r.lines_written, 1);
    assert!(r.last_snapshot.is_some());
}

#[test]
fn renderer_incremental_advances_only_by_delta() {
    // Two updates, each advancing the line counter, should be
    // additive without re-painting already-written rows.
    let mut r = SstvImageRenderer::new();
    let _ = r.update_from_snapshot(snap(640, 496, 100));
    assert_eq!(r.lines_written, 100);

    let changed = r.update_from_snapshot(snap(640, 496, 250));
    assert!(changed, "advancing should report changed");
    assert_eq!(r.lines_written, 250);
}

#[test]
fn renderer_no_advance_reports_unchanged() {
    let mut r = SstvImageRenderer::new();
    let _ = r.update_from_snapshot(snap(640, 496, 100));

    let changed = r.update_from_snapshot(snap(640, 496, 100));
    assert!(!changed, "same lines_written should report unchanged");
    assert_eq!(r.lines_written, 100);
}

#[test]
fn renderer_same_dimension_rollover_clears_for_new_image() {
    // Same-dimensions rollover (PD120 → PD120, or PD120 → PD180,
    // or any other PD-family mode swap) starts a fresh image —
    // slowrx resets the line counter on the new VIS detection.
    // The renderer must detect the rollover and clear stale
    // rows. Per CR round 1 #7 on PR #599; this test pins that fix.
    let mut r = SstvImageRenderer::new();
    let _ = r.update_from_snapshot(snap(640, 496, 496)); // full first image
    assert_eq!(r.lines_written, 496);

    // New image starts: lines_written drops back to 1.
    let changed = r.update_from_snapshot(snap(640, 496, 1));
    assert!(changed, "rollover with one new line should report changed");
    assert_eq!(r.lines_written, 1, "counter reset to new image's progress");
}

#[test]
fn renderer_dimension_change_rebuilds_surface() {
    // PD-family (640×496) → some hypothetical 320×240 mode
    // would resize. Surface gets rebuilt; old contents discarded.
    let mut r = SstvImageRenderer::new();
    let _ = r.update_from_snapshot(snap(640, 496, 100));
    let _ = r.update_from_snapshot(snap(320, 240, 50));
    assert_eq!(r.width, 320);
    assert_eq!(r.height, 240);
    assert_eq!(r.lines_written, 50);
}

#[test]
fn renderer_clear_resets_to_empty() {
    let mut r = SstvImageRenderer::new();
    let _ = r.update_from_snapshot(snap(640, 496, 100));
    r.clear();
    assert_eq!(r.lines_written, 0);
    assert!(r.last_snapshot.is_none());
}
