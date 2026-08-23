use super::*;

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
