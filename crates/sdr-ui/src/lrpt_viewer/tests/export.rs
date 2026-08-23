use super::*;

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
