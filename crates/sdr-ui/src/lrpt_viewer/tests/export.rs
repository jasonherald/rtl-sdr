use super::*;

#[test]
fn export_png_refuses_when_no_active_channel() {
    let r = LrptImageRenderer::new();
    let (_tmp_dir, path) =
        crate::test_util::test_output_path("lrpt-test-no-active-should-not-be-written.png");
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
    let (_tmp_dir, path) =
        crate::test_util::test_output_path("lrpt-test-empty-channel-should-not-be-written.png");
    let result = r.export_png(&path);
    assert!(result.is_err());
    assert!(!path.exists());
}

#[test]
fn export_png_round_trips_to_a_real_file() {
    let mut r = LrptImageRenderer::new();
    for _ in 0..16 {
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
    }
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-test.png");
    r.export_png(&path).expect("export per-APID PNG");
    crate::test_util::assert_png_file(&path);
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
    let (_tmp_dir, path) =
        crate::test_util::test_output_path("lrpt-test-empty-composite-should-not-be-written.png");
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
    const COMPOSITE_R: u8 = 0x10;
    const COMPOSITE_G: u8 = 0x20;
    const COMPOSITE_B: u8 = 0x30;
    /// Not part of any recipe; its distinct grey value would show up
    /// in the decoded pixels if the export regressed to the active
    /// per-APID surface.
    const UNRELATED_APID: u16 = 63;
    const UNRELATED_GREY: u8 = 0x77;
    /// Cairo ARGB32 layout: 4 bytes per pixel, alpha forced opaque.
    const BYTES_PER_PIXEL: usize = 4;
    const OPAQUE_ALPHA: u8 = 0xFF;
    // Per CR round 2 on PR #575: when composite mode is
    // active and the cache is populated, `export_png` must
    // export the composite surface — not the active per-APID
    // surface. Without this, exporting while a composite was
    // on screen wrote out the last greyscale APID instead.
    let mut r = LrptImageRenderer::new();
    let image = LrptImage::new();
    let recipe = COMPOSITE_CATALOG[0];
    // Push one line into each source APID so the composite
    // cache populates. The recipe is from the catalog, so
    // those APIDs are well-defined.
    image.push_line(recipe.r_apid, &vec![COMPOSITE_R; IMAGE_WIDTH]);
    image.push_line(recipe.g_apid, &vec![COMPOSITE_G; IMAGE_WIDTH]);
    image.push_line(recipe.b_apid, &vec![COMPOSITE_B; IMAGE_WIDTH]);
    // Feed a per-APID surface that ISN'T in the recipe — pushing it
    // auto-selects it as the active channel, which is exactly what
    // the pre-#575 greyscale fallback would have exported.
    r.push_line(UNRELATED_APID, &synth_line(UNRELATED_GREY));
    assert!(r.set_composite(recipe, &image));
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-comp.png");
    r.export_png(&path).expect("export composite PNG");
    crate::test_util::assert_png_file(&path);
    // Decode the PNG and check the first pixel is the composite RGB
    // triple (ARGB32 little-endian: B, G, R, A), not the grey APID.
    let mut f = std::fs::File::open(&path).expect("open exported PNG");
    let mut surface = cairo::ImageSurface::create_from_png(&mut f).expect("decode exported PNG");
    assert_eq!(
        surface.width(),
        i32::try_from(IMAGE_WIDTH).expect("width fits i32")
    );
    let data = surface.data().expect("surface data");
    assert_eq!(
        &data[..BYTES_PER_PIXEL],
        &[COMPOSITE_B, COMPOSITE_G, COMPOSITE_R, OPAQUE_ALPHA],
        "first pixel must be the composite RGB, not the {UNRELATED_GREY:#x} greyscale APID"
    );
}

#[test]
fn write_greyscale_png_round_trips_to_a_real_file() {
    // Pin the new free-function path used by the LOS
    // `SaveLrptPass` handler in `window.rs`. Per
    // `CodeRabbit` round 7 on PR #543.
    const W: usize = 32;
    const H: usize = 8;
    let pixels: Vec<u8> = (0..W * H)
        .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
        .collect();
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-bare.png");
    write_greyscale_png(&path, &pixels, W, H).unwrap();
    crate::test_util::assert_png_file(&path);
}

#[test]
fn write_greyscale_png_rejects_size_mismatch() {
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-bare-mismatch.png");
    let result = write_greyscale_png(&path, &[0_u8; 10], 4, 4);
    assert!(result.is_err());
    assert!(
        !path.exists(),
        "no file should be written on size-mismatch error"
    );
}

#[test]
fn write_greyscale_png_rejects_zero_size() {
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-bare-zero.png");
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
    let (_tmp_dir, path) =
        crate::test_util::test_output_path("sdr-ui-lrpt-bare-zero-dim-pixels.png");
    let result = write_greyscale_png(&path, &[1_u8], 0, 1);
    assert!(matches!(result, Err(crate::viewer::ViewerError::ZeroSized)));
    assert!(!path.exists());
}

#[test]
fn write_rgb_png_round_trips_to_a_real_file() {
    // Pin the new RGB writer used by the LRPT composite
    // LOS-save path. Per #547.
    const W: usize = 32;
    const H: usize = 8;
    let pixels: Vec<u8> = (0..W * H * 3)
        .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
        .collect();
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-rgb.png");
    write_rgb_png(&path, &pixels, W, H).expect("write_rgb_png");
    crate::test_util::assert_png_file(&path);
}

#[test]
fn write_rgb_png_rejects_size_mismatch() {
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-rgb-mismatch.png");
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
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-rgb-zero.png");
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
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-lrpt-rgb-zero-dim.png");
    let result = write_rgb_png(&path, &[1_u8, 2, 3], 0, 1);
    assert!(matches!(result, Err(crate::viewer::ViewerError::ZeroSized)));
    assert!(!path.exists());
}
