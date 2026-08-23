use super::*;

/// Number of lines pushed by the renderer tests. Keeps tests fast
/// while still exercising more than one line of buffer growth.
const TEST_LINE_COUNT: usize = 16;

/// Build a synthetic `AptLine` with deterministic per-line content.
/// Uses high `sync_quality` so the line passes the gap-fill threshold
/// in the parallel `AptImage`. `raw_samples` mirror `pixels` to keep
/// `finalize_grayscale` predictable.
fn synth_line(seed: u8) -> AptLine {
    let mut line = AptLine {
        sync_quality: 0.95,
        ..AptLine::default()
    };
    for (i, p) in line.pixels.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        {
            *p = ((i + usize::from(seed)) & 0xff) as u8;
        }
    }
    for (i, s) in line.raw_samples.iter_mut().enumerate() {
        #[allow(
            clippy::cast_precision_loss,
            reason = "value is in [0, 255]; fits f32 mantissa exactly"
        )]
        let v = ((i + usize::from(seed)) & 0xff) as f32;
        *s = v;
    }
    line
}

#[test]
fn renderer_starts_empty() {
    let r = AptImageRenderer::try_new().unwrap();
    assert!(r.is_empty());
    assert_eq!(r.n_lines(), 0);
}

#[test]
fn push_line_increments_n_lines() {
    let mut r = AptImageRenderer::try_new().unwrap();
    r.push_line(&synth_line(0));
    assert_eq!(r.n_lines(), 1);
    r.push_line(&synth_line(1));
    assert_eq!(r.n_lines(), 2);
}

#[test]
fn surface_dimensions_match_max_lines_at_construction() {
    // The surface is allocated full-size up front so push_line
    // never has to grow it. Lock that invariant down so a future
    // refactor can't accidentally make it lazy and lose the
    // alloc-free hot-path guarantee.
    let r = AptImageRenderer::try_new().unwrap();
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    {
        assert_eq!(r.surface.width(), LINE_PIXELS as i32);
        assert_eq!(r.surface.height(), MAX_LINES as i32);
    }
}

#[test]
fn push_line_caps_at_max_lines() {
    let mut r = AptImageRenderer::try_new().unwrap();
    for i in 0..MAX_LINES {
        #[allow(clippy::cast_possible_truncation)]
        r.push_line(&synth_line(i as u8));
    }
    assert_eq!(r.n_lines(), MAX_LINES);
    // One more push past the cap is a no-op.
    r.push_line(&synth_line(0));
    assert_eq!(r.n_lines(), MAX_LINES);
}

#[test]
fn clear_resets_lines_and_zeroes_surface_pixels() {
    let mut r = AptImageRenderer::try_new().unwrap();
    for _ in 0..TEST_LINE_COUNT {
        r.push_line(&synth_line(0xAA));
    }
    r.clear();
    assert!(r.is_empty());
    assert_eq!(r.n_lines(), 0);
    // The first row of the surface should now be all zeroes
    // (alpha-0 transparent), matching what `cairo::ImageSurface::create`
    // gives a fresh surface.
    let data = r.surface.data().unwrap();
    assert!(
        data[0..LINE_PIXELS * 4].iter().all(|&b| b == 0),
        "clear() should zero the surface bytes",
    );
}

#[test]
fn pixel_layout_is_argb32_with_grayscale_in_bgr_channels() {
    let mut r = AptImageRenderer::try_new().unwrap();
    let mut line = AptLine {
        sync_quality: 0.95,
        ..AptLine::default()
    };
    line.pixels[0] = 0x80;
    line.pixels[1] = 0xC0;
    r.push_line(&line);
    // Cairo ARGB32 little-endian: B, G, R, A
    let data = r.surface.data().unwrap();
    assert_eq!(&data[0..4], &[0x80, 0x80, 0x80, 0xFF]);
    assert_eq!(&data[4..8], &[0xC0, 0xC0, 0xC0, 0xFF]);
}

#[test]
fn export_png_round_trips_to_a_real_file() {
    let mut r = AptImageRenderer::try_new().unwrap();
    for i in 0..TEST_LINE_COUNT {
        #[allow(clippy::cast_possible_truncation)]
        r.push_line(&synth_line(i as u8));
    }
    let (_tmp_dir, path) = crate::test_util::test_output_path("sdr-ui-apt-test.png");
    r.export_png(&path).unwrap();
    crate::test_util::assert_png_file(&path);
}

#[test]
fn export_png_refuses_when_buffer_is_empty() {
    let r = AptImageRenderer::try_new().unwrap();
    let (_tmp_dir, path) =
        crate::test_util::test_output_path("apt-test-empty-should-not-be-written.png");
    let result = r.export_png(&path);
    assert!(result.is_err());
    assert!(!path.exists(), "no file should be created on empty export");
}
