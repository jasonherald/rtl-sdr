use super::*;

/// APID used in renderer tests. AVHRR convention: 64 = ch1.
/// Same value the rest of the LRPT test suite uses for
/// single-channel cases.
const APID_TEST: u16 = 64;
/// Secondary APID for multi-channel checks.
const APID_TEST_2: u16 = 65;
/// Pixel marker — distinct from 0/0xFF so a regression that
/// returned a default-allocated surface would fail loudly.
const TEST_PIXEL: u8 = 0x42;
const TEST_PIXEL_2: u8 = 0xC0;

fn synth_line(value: u8) -> Vec<u8> {
    vec![value; IMAGE_WIDTH]
}

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
fn export_png_refuses_when_no_active_channel() {
    let r = LrptImageRenderer::new();
    let path = std::env::temp_dir().join("lrpt-test-no-active-should-not-be-written.png");
    let result = r.export_png(&path);
    assert!(result.is_err());
    assert!(!path.exists(), "no file should be created on empty export");
}

#[test]
fn export_png_refuses_when_active_channel_has_no_data() {
    // Force-set active to an APID we never pushed to (via
    // the test-only path: renderer's HashMap entry exists
    // because we push one line then... wait, no — we need
    // a way to test "active set, but channel empty". Push
    // then clear partway: clear() drops active too, so
    // that's not it. Instead use the renderer's contract:
    // set_active_apid can't succeed for an unknown channel
    // either, so the only reachable "active set, n_lines==0"
    // case is "freshly pushed once, then..." — actually
    // n_lines becomes 1 the moment we push. So the first
    // branch (no active) is the only reachable empty error.
    // We test the second branch by directly mutating the
    // channel's n_lines back to 0 via the test-only access
    // below.
    let mut r = LrptImageRenderer::new();
    r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    r.channels.get_mut(&APID_TEST).unwrap().n_lines = 0;
    let path = std::env::temp_dir().join("lrpt-test-empty-channel-should-not-be-written.png");
    let result = r.export_png(&path);
    assert!(result.is_err());
    assert!(!path.exists());
}

#[test]
fn export_png_round_trips_to_a_real_file() {
    use std::io::Read;
    let mut r = LrptImageRenderer::new();
    for _ in 0..16 {
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-test-{nanos}.png"));
    r.export_png(&path).expect("export per-APID PNG");
    let metadata = std::fs::metadata(&path).expect("metadata");
    assert!(metadata.len() > 0, "PNG file shouldn't be empty");
    let mut header = [0_u8; 8];
    let mut f = std::fs::File::open(&path).expect("open");
    f.read_exact(&mut header).expect("read_exact");
    assert_eq!(
        &header, b"\x89PNG\r\n\x1a\n",
        "exported file isn't a valid PNG (header mismatch)",
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn export_png_refuses_when_composite_active_but_cache_empty() {
    // Per CR round 4 on PR #575: composite mode is
    // authoritative — when a recipe is active but the
    // cache hasn't built yet (source APIDs missing/empty),
    // `export_png` must fail loudly with `EmptyComposite`
    // rather than silently fall through to whatever
    // single-APID was last selected. The dropdown still
    // says "Composite — ..." in that state, so a per-APID
    // export would mislead.
    let mut r = LrptImageRenderer::new();
    // Activate a composite without any source data — this
    // sets active_composite + leaves cache None.
    let recipe = COMPOSITE_CATALOG[0];
    let empty = LrptImage::new();
    assert!(!r.set_composite(recipe, &empty));
    // Also feed one line into a per-APID surface that
    // ISN'T part of the recipe — the bug we're guarding
    // against is "fall through and export this one".
    r.push_line(99, &synth_line(TEST_PIXEL));
    let path = std::env::temp_dir().join("lrpt-test-empty-composite-should-not-be-written.png");
    let result = r.export_png(&path);
    assert!(matches!(
        result,
        Err(crate::viewer::ViewerError::EmptyComposite { recipe_name })
            if recipe_name == recipe.name
    ));
    assert!(
        !path.exists(),
        "no file should be created on EmptyComposite",
    );
}

#[test]
fn render_paints_only_background_when_composite_active_but_cache_empty() {
    // Sibling guarantee to `export_png_refuses_when_composite_active_but_cache_empty`:
    // the on-screen render path must also stay on
    // background-only paint when composite is active but
    // unbuilt — never fall through to per-APID. We can't
    // easily inspect Cairo's surface state in a unit test,
    // but the function returns `Ok(())` without panicking
    // along the no-fall-through path, and the no-image-
    // surface-blit branch is the only one that reaches
    // that state with both `composite_cache: None` and
    // `active_composite: Some(_)`. Per CR round 4 on
    // PR #575.
    let mut r = LrptImageRenderer::new();
    let recipe = COMPOSITE_CATALOG[0];
    let empty = LrptImage::new();
    assert!(!r.set_composite(recipe, &empty));
    // Per-APID surface for an unrelated APID — what the
    // pre-fix render would have painted.
    r.push_line(99, &synth_line(TEST_PIXEL));
    let surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, 32, 32).expect("test surface alloc");
    let cr = cairo::Context::new(&surface).expect("cairo ctx");
    r.render(&cr, 32, 32).expect("render");
}

#[test]
fn export_png_uses_composite_cache_when_active() {
    // Per CR round 2 on PR #575: when composite mode is
    // active and the cache is populated, `export_png` must
    // export the composite surface — not the active per-APID
    // surface. Without this, exporting while a composite was
    // on screen wrote out the last greyscale APID instead.
    use std::io::Read;
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    // Push one line into each source APID so the composite
    // cache populates. The recipe is from the catalog, so
    // those APIDs are well-defined.
    image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
    image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
    image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
    // Also feed one line to a per-APID surface that ISN'T in
    // the recipe — this is what the previous greyscale fallback
    // would have written. If the export silently wrote that
    // surface instead of the composite, the test below would
    // still pass the PNG header check; we only confirm here
    // that export succeeds end-to-end, not which pixels it
    // wrote (the byte-level guarantee is covered by
    // `build_argb32_from_rgb_writes_bgra_byte_order`).
    assert!(r.set_composite(recipe, &image));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-comp-{nanos}.png"));
    r.export_png(&path).expect("export composite PNG");
    let metadata = std::fs::metadata(&path).expect("metadata");
    assert!(metadata.len() > 0, "PNG file shouldn't be empty");
    let mut header = [0_u8; 8];
    let mut f = std::fs::File::open(&path).expect("open");
    f.read_exact(&mut header).expect("read_exact");
    assert_eq!(&header, b"\x89PNG\r\n\x1a\n", "not a PNG");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_greyscale_png_round_trips_to_a_real_file() {
    // Pin the new free-function path used by the LOS
    // `SaveLrptPass` handler in `window.rs`. Per
    // `CodeRabbit` round 7 on PR #543.
    use std::io::Read;
    const W: usize = 32;
    const H: usize = 8;
    let pixels: Vec<u8> = (0..W * H)
        .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-bare-{nanos}.png"));
    write_greyscale_png(&path, &pixels, W, H).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    assert!(metadata.len() > 0);
    let mut header = [0_u8; 8];
    let mut f = std::fs::File::open(&path).unwrap();
    f.read_exact(&mut header).unwrap();
    assert_eq!(&header, b"\x89PNG\r\n\x1a\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_greyscale_png_rejects_size_mismatch() {
    let path = std::env::temp_dir().join("sdr-ui-lrpt-bare-mismatch.png");
    let result = write_greyscale_png(&path, &[0_u8; 10], 4, 4);
    assert!(result.is_err());
    assert!(
        !path.exists(),
        "no file should be written on size-mismatch error"
    );
}

#[test]
fn write_greyscale_png_rejects_zero_size() {
    let path = std::env::temp_dir().join("sdr-ui-lrpt-bare-zero.png");
    let result = write_greyscale_png(&path, &[], 0, 0);
    assert!(result.is_err());
    assert!(!path.exists());
}

#[test]
fn write_greyscale_png_zero_dim_with_pixels_reports_zero_sized() {
    // Pin the CR-requested ordering: a zero-dim call with a
    // non-empty pixel buffer must surface as `ZeroSized`, not
    // mask as the generic `InvalidBuffer` length-mismatch.
    // Per CR on PR #550.
    let path = std::env::temp_dir().join("sdr-ui-lrpt-bare-zero-dim-pixels.png");
    let result = write_greyscale_png(&path, &[1_u8], 0, 1);
    assert!(matches!(result, Err(crate::viewer::ViewerError::ZeroSized)));
    assert!(!path.exists());
}

// ─── Composite catalog (#547) ───────────────────────────

#[test]
fn composite_catalog_is_non_empty() {
    // Defensive — if a future maintainer ever empties the
    // catalog the dropdown silently loses every composite
    // option. Catch that loud-and-early.
    assert!(!COMPOSITE_CATALOG.is_empty());
}

#[test]
fn composite_catalog_has_unique_names() {
    // Names show up in the dropdown with a `Composite — `
    // prefix; duplicates would render two indistinguishable
    // entries. Pin uniqueness so a copy-paste typo can't
    // ship.
    let names: Vec<&str> = COMPOSITE_CATALOG.iter().map(|r| r.name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        names.len(),
        sorted.len(),
        "duplicate composite name in catalog",
    );
}

#[test]
fn composite_catalog_apid_triples_are_distinct_per_entry() {
    // A recipe with R == G (or any pair equal) collapses to a
    // 2-channel composite — almost certainly a typo. The
    // assembler still renders, but the result is misleading
    // (one channel painted into two RGB slots). Pin
    // distinctness as a sanity guard.
    for r in COMPOSITE_CATALOG {
        assert_ne!(r.r_apid, r.g_apid, "{}: r and g APIDs are the same", r.name);
        assert_ne!(r.g_apid, r.b_apid, "{}: g and b APIDs are the same", r.name);
        assert_ne!(r.r_apid, r.b_apid, "{}: r and b APIDs are the same", r.name);
    }
}

#[test]
fn renderer_starts_in_single_channel_mode() {
    let r = LrptImageRenderer::new();
    assert!(!r.is_composite_active());
    assert!(r.active_composite().is_none());
}

#[test]
fn set_composite_returns_false_when_source_apids_missing() {
    // No data pushed yet — every recipe's source APIDs are
    // missing, so `composite_rgb` returns None and
    // `set_composite` reports false. The recipe is still
    // remembered as the active composite (so the drain
    // tick will retry every poll), but the cache stays
    // empty.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    assert!(!r.set_composite(recipe, &image));
    assert!(r.is_composite_active());
    assert_eq!(r.active_composite(), Some(recipe));
}

#[test]
fn set_composite_succeeds_when_all_three_apids_have_data() {
    // Push one line per source APID for the first catalog
    // recipe, then activate it. The cache should populate
    // and `is_composite_active` stays true.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
    image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
    image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
    assert!(r.set_composite(recipe, &image));
    assert!(r.is_composite_active());
    assert_eq!(r.active_composite(), Some(recipe));
}

#[test]
fn clear_composite_drops_recipe_and_cache() {
    // Activate composite, then clear. Both the recipe and
    // the cache must be gone so the next render falls back
    // to single-channel mode.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
    image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
    image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
    r.set_composite(recipe, &image);
    r.clear_composite();
    assert!(!r.is_composite_active());
    assert!(r.active_composite().is_none());
}

#[test]
fn composite_min_height_tracks_min_of_three_channels() {
    // Pin the gate the dropdown tick uses to skip no-op
    // composite rebuilds. Build with channels at lines
    // (3, 5, 4); the renderer should remember 3 — only
    // advancing the limiting channel can change the output.
    // Per CR round 3 on PR #575.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    for _ in 0..3 {
        image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
    }
    for _ in 0..5 {
        image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
    }
    for _ in 0..4 {
        image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
    }
    assert!(r.set_composite(recipe, &image));
    assert_eq!(r.composite_min_height(), Some(3));

    // Clearing the composite must drop the cached min so the
    // tick treats the next activation as a fresh build.
    r.clear_composite();
    assert_eq!(r.composite_min_height(), None);

    // Re-activating, then `clear()` (between-pass cleanup)
    // also drops the cached min.
    r.set_composite(recipe, &image);
    assert_eq!(r.composite_min_height(), Some(3));
    r.clear();
    assert_eq!(r.composite_min_height(), None);
}

#[test]
fn composite_min_height_is_none_when_source_apids_missing() {
    // Empty image — `set_composite` returns false and the
    // cached min stays `None` so the tick keeps retrying
    // (compares as `None != current_min`) until source
    // channels appear. Per CR round 3 on PR #575.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    assert!(!r.set_composite(recipe, &image));
    assert_eq!(r.composite_min_height(), None);
}

#[test]
fn selection_enum_makes_apid_and_composite_mutually_exclusive() {
    // Per CR round 5 on PR #575: collapsing the parallel
    // `active`/`active_composite` fields into one enum
    // means switching modes is atomic — picking an APID
    // implicitly drops the composite cache, picking a
    // composite implicitly hides any per-APID. Pin both
    // directions so a future field-split-and-add doesn't
    // silently regress.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    // Seed three source channels + a non-recipe APID.
    image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
    image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
    image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
    r.push_line(99, &synth_line(TEST_PIXEL));

    // APID first → set composite. Composite mode wins;
    // active_apid() must return None (not the prior APID).
    assert!(r.set_active_apid(99));
    assert_eq!(r.active_apid(), Some(99));
    assert_eq!(r.active_composite(), None);
    assert!(r.set_composite(recipe, &image));
    assert_eq!(r.active_apid(), None);
    assert_eq!(r.active_composite(), Some(recipe));

    // Composite first → set APID. APID wins; cache + min
    // are dropped atomically (no leftover composite state).
    assert!(r.set_active_apid(99));
    assert_eq!(r.active_apid(), Some(99));
    assert_eq!(r.active_composite(), None);
    assert!(!r.is_composite_active());
    assert_eq!(r.composite_min_height(), None);
}

#[test]
fn composite_gen_bumps_on_every_selection_change() {
    // Pin the generation-counter contract: every
    // selection-changing method bumps it so an in-flight
    // worker's captured token mismatches and its result
    // gets dropped on the floor instead of installed.
    // Per CR round 5 on PR #575.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
    image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
    image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
    r.push_line(99, &synth_line(TEST_PIXEL));

    let g0 = r.composite_gen();
    r.set_active_apid(99);
    let g1 = r.composite_gen();
    assert_ne!(g0, g1, "set_active_apid must bump");

    r.mark_composite_pending(recipe);
    let g2 = r.composite_gen();
    assert_ne!(g1, g2, "mark_composite_pending must bump");

    let g3 = r.prepare_composite_build(recipe, 5);
    assert_ne!(g2, g3, "prepare_composite_build must bump");
    assert_eq!(g3, r.composite_gen(), "returned gen matches stored");

    r.clear_composite();
    let g4 = r.composite_gen();
    assert_ne!(g3, g4, "clear_composite must bump");

    r.clear();
    let g5 = r.composite_gen();
    assert_ne!(g4, g5, "clear must bump");
}

#[test]
fn install_composite_cache_drops_stale_workers() {
    // Pin the async-path stale-result guard: a worker that
    // captures gen=N then returns after a selection change
    // (gen=N+1) must not install its surface. Per CR round
    // 5 on PR #575.
    let mut r = LrptImageRenderer::new();
    let recipe = COMPOSITE_CATALOG[0];
    let captured_gen = r.prepare_composite_build(recipe, 5);
    // Simulate the user picking a different recipe before
    // the worker returns — bumps gen.
    let other = COMPOSITE_CATALOG[1];
    r.mark_composite_pending(other);
    // Build a dummy 5-line surface (the worker's output).
    let bytes = vec![0xFFu8; IMAGE_WIDTH * 5 * BYTES_PER_PIXEL];
    let surface = build_argb32_surface_from_bgra(bytes, IMAGE_WIDTH, 5)
        .expect("surface build for stale-worker test");
    // Old gen no longer matches; install must refuse.
    assert!(!r.install_composite_cache(recipe, captured_gen, 5, surface));
}

#[test]
fn build_bgra_composite_bytes_matches_build_argb32_from_rgb() {
    // Cross-check the two paths produce byte-identical
    // pixels: the worker BGRA path (build_bgra_composite_bytes)
    // and the legacy sync path (assemble_rgb_composite +
    // build_argb32_from_rgb). Per CR round 5 on PR #575.
    let height = 4;
    let n = IMAGE_WIDTH * height;
    // Synthetic gradient — three different patterns per
    // channel so any byte mix-up shows.
    let r_pixels: Vec<u8> = (0..n)
        .map(|i| u8::try_from(i & 0xFF).unwrap_or(0))
        .collect();
    let g_pixels: Vec<u8> = (0..n)
        .map(|i| u8::try_from((i.wrapping_mul(3)) & 0xFF).unwrap_or(0))
        .collect();
    let b_pixels: Vec<u8> = (0..n)
        .map(|i| u8::try_from((i.wrapping_mul(7)) & 0xFF).unwrap_or(0))
        .collect();
    let snap = sdr_lrpt::image::CompositeSnapshot {
        r_pixels: r_pixels.clone(),
        g_pixels: g_pixels.clone(),
        b_pixels: b_pixels.clone(),
        height,
    };

    // Worker path: build BGRA bytes directly from the snap.
    let bgra_worker = build_bgra_composite_bytes(&snap);

    // Legacy path: assemble_rgb_composite → build_argb32_from_rgb,
    // then read BGRA out of the surface for compare.
    let rgb = sdr_lrpt::image::assemble_rgb_composite(&r_pixels, &g_pixels, &b_pixels, height);
    let mut surface_legacy =
        build_argb32_from_rgb(&rgb, IMAGE_WIDTH, height).expect("legacy surface build");
    let stride_legacy =
        usize::try_from(surface_legacy.stride()).expect("legacy stride non-negative");
    let data_legacy = surface_legacy.data().expect("legacy surface data");

    // Worker output uses packed stride (width * 4); the
    // legacy surface uses Cairo's stride which is also
    // width * 4 for ARGB32. Compare row-by-row to be
    // robust against future stride drift.
    let row_bytes = IMAGE_WIDTH * BYTES_PER_PIXEL;
    for row in 0..height {
        let worker_row = &bgra_worker[row * row_bytes..row * row_bytes + row_bytes];
        let legacy_row = &data_legacy[row * stride_legacy..row * stride_legacy + row_bytes];
        assert_eq!(
            worker_row, legacy_row,
            "BGRA mismatch on row {row}: worker path vs legacy path",
        );
    }
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

#[test]
fn build_argb32_from_rgb_writes_bgra_byte_order() {
    // Pin Cairo's ARGB32 little-endian byte order — the
    // composite cache and the test assertion below would
    // both flip in lockstep otherwise. R/G/B input bytes
    // land at offsets +2 / +1 / +0 in the surface data;
    // alpha is opaque.
    let rgb = vec![0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33];
    let mut surface = build_argb32_from_rgb(&rgb, 2, 1).expect("argb32 build");
    let data = surface.data().expect("surface data");
    assert_eq!(&data[0..4], &[0xCC, 0xBB, 0xAA, 0xFF]);
    assert_eq!(&data[4..8], &[0x33, 0x22, 0x11, 0xFF]);
}

#[test]
fn build_argb32_from_rgb_rejects_size_mismatch() {
    // Buffer length must equal width*height*3 — anything
    // else is a caller bug. The error string is matched
    // loosely; we just want to confirm the path doesn't
    // build a malformed surface.
    let rgb = vec![0; 10];
    assert!(build_argb32_from_rgb(&rgb, 4, 4).is_err());
}

// ─── write_rgb_png (#547) ───────────────────────────────

#[test]
fn write_rgb_png_round_trips_to_a_real_file() {
    // Pin the new RGB writer used by the LRPT composite
    // LOS-save path. Per #547.
    use std::io::Read;
    const W: usize = 32;
    const H: usize = 8;
    let pixels: Vec<u8> = (0..W * H * 3)
        .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-rgb-{nanos}.png"));
    write_rgb_png(&path, &pixels, W, H).expect("write_rgb_png");
    let metadata = std::fs::metadata(&path).expect("metadata");
    assert!(metadata.len() > 0);
    let mut header = [0_u8; 8];
    let mut f = std::fs::File::open(&path).expect("open");
    f.read_exact(&mut header).expect("read_exact");
    assert_eq!(&header, b"\x89PNG\r\n\x1a\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_rgb_png_rejects_size_mismatch() {
    let path = std::env::temp_dir().join("sdr-ui-lrpt-rgb-mismatch.png");
    // 10 bytes can't equal 4*4*3 = 48 — should fail without
    // creating the file.
    let result = write_rgb_png(&path, &[0_u8; 10], 4, 4);
    assert!(result.is_err());
    assert!(
        !path.exists(),
        "no file should be written on size-mismatch error",
    );
}

#[test]
fn write_rgb_png_rejects_zero_size() {
    let path = std::env::temp_dir().join("sdr-ui-lrpt-rgb-zero.png");
    let result = write_rgb_png(&path, &[], 0, 0);
    assert!(result.is_err());
    assert!(!path.exists());
}

#[test]
fn write_rgb_png_zero_dim_with_pixels_reports_zero_sized() {
    // Same ordering invariant `write_greyscale_png` has: a
    // zero-dim call with a non-empty pixel buffer surfaces as
    // `ZeroSized`, not as the generic `InvalidBuffer`
    // length-mismatch.
    let path = std::env::temp_dir().join("sdr-ui-lrpt-rgb-zero-dim.png");
    let result = write_rgb_png(&path, &[1_u8, 2, 3], 0, 1);
    assert!(matches!(result, Err(crate::viewer::ViewerError::ZeroSized)));
    assert!(!path.exists());
}

// --- #730 ---

/// The AOS wiring's catalog → profile mapping: current Meteors
/// are plain OQPSK; an uncatalogued id falls back to plain QPSK.
#[test]
fn lrpt_downlink_for_maps_the_catalog_profile() {
    use sdr_dsp::lrpt::LrptMode;
    use sdr_radio::lrpt_decoder::LrptDownlink;
    const UNCATALOGUED_NORAD_ID: u32 = 1;
    for norad_id in [sdr_sat::METEOR_M2_3_NORAD_ID, sdr_sat::METEOR_M2_4_NORAD_ID] {
        assert_eq!(
            lrpt_downlink_for(norad_id),
            LrptDownlink::new(LrptMode::Oqpsk, false)
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
        UiToDsp::SetLrptDownlink(profile) if profile == LrptDownlink::new(LrptMode::Oqpsk, false)
    ));
    assert!(matches!(second, UiToDsp::ClearLrptImageContents(_)));
}
