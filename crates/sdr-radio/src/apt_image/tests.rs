use super::*;
use std::time::Duration;

/// Tight default capacity that's small enough that tests can verify
/// "no reallocation up to N lines" without running for 15 minutes.
const TEST_MAX_LINES: usize = 64;

/// Quality value that comfortably clears [`MIN_VALID_SYNC_QUALITY`].
const TEST_GOOD_QUALITY: f32 = 0.92;
/// Quality value comfortably under the threshold (gap-fill territory).
const TEST_BAD_QUALITY: f32 = 0.30;

/// Modulus for the synthetic pixel pattern. A prime under 256 gives
/// a long, non-repeating ramp inside the 2080-sample line so any
/// "preserved verbatim" assertion fails loudly if pixel order, width,
/// or content gets corrupted.
const TEST_PIXEL_PATTERN_MODULUS: usize = 251;

/// Number of lines pushed by the ordering test. Big enough to span
/// several buffer entries, small enough that the monotonic-timestamp
/// assertion runs in microseconds.
const TEST_ORDERED_INSERT_COUNT: u32 = 5;
/// Spacing (ms) between successive synthetic lines in the ordering
/// test. Half a second mirrors the real APT line cadence.
const TEST_ORDERED_STEP_MS: u64 = 500;

/// Authoritative APT scan width in pixels. Pinned independently of
/// `LINE_PIXELS` so any future protocol-width drift in `sdr-dsp` is
/// caught loudly here instead of silently propagating.
const EXPECTED_APT_WIDTH_PIXELS: usize = 2_080;

/// Build an `AptLine` with the given quality and a deterministic
/// non-zero pixel pattern so we can verify pixel preservation.
/// Both `pixels` (u8) and `raw_samples` (f32) get matching
/// content so pre-/post-finalize semantics are easy to reason
/// about in tests.
fn synth_line(quality: f32) -> AptLine {
    let mut line = AptLine {
        sync_quality: quality,
        ..AptLine::default()
    };
    for (i, p) in line.pixels.iter_mut().enumerate() {
        *p = (i % TEST_PIXEL_PATTERN_MODULUS) as u8;
    }
    for (i, s) in line.raw_samples.iter_mut().enumerate() {
        // Mirror the u8 pattern at unit-amplitude scale so
        // image-wide finalization tests have a deterministic
        // gradient to assert against.
        #[allow(
            clippy::cast_precision_loss,
            reason = "modulus is small (251); result fits in f32 exactly"
        )]
        let v = (i % TEST_PIXEL_PATTERN_MODULUS) as f32;
        *s = v;
    }
    line
}

#[test]
fn empty_image_has_no_lines() {
    let img = AptImage::new(Instant::now());
    assert_eq!(img.len(), 0);
    assert!(img.is_empty());
    assert!(img.lines().is_empty());
    assert!(img.channel_a_id().is_none());
    assert!(img.channel_b_id().is_none());
}

#[test]
fn push_high_quality_line_preserves_pixels() {
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    let line = synth_line(TEST_GOOD_QUALITY);
    let now = Instant::now();
    img.push_line(&line, now);

    assert_eq!(img.len(), 1);
    let stored = &img.lines()[0];
    assert_eq!(stored.pixels, line.pixels);
    assert_eq!(stored.sync_quality, TEST_GOOD_QUALITY);
    assert_eq!(stored.received_at, now);
}

#[test]
fn push_low_quality_line_gap_fills_with_black_keeps_quality() {
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    let line = synth_line(TEST_BAD_QUALITY);
    img.push_line(&line, Instant::now());

    let stored = &img.lines()[0];
    assert!(
        stored.pixels.iter().all(|&p| p == 0),
        "sub-threshold line should be gap-filled to all-zero",
    );
    assert_eq!(
        stored.sync_quality, TEST_BAD_QUALITY,
        "quality score must survive gap-fill so a renderer can still flag the row",
    );
}

#[test]
fn boundary_quality_at_threshold_is_kept_not_gapped() {
    // The constant uses `>=`, so MIN_VALID_SYNC_QUALITY exactly is
    // accepted. Pin that down so future tweaks don't silently flip
    // it to `>` and lose lines that scored exactly the threshold.
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    let line = synth_line(MIN_VALID_SYNC_QUALITY);
    img.push_line(&line, Instant::now());
    assert_eq!(img.lines()[0].pixels, line.pixels);
}

#[test]
fn capacity_does_not_grow_within_reservation() {
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    let initial_capacity = img.lines.capacity();
    assert!(
        initial_capacity >= TEST_MAX_LINES,
        "with_capacity should reserve at least the requested count, got {initial_capacity}",
    );

    let line = synth_line(TEST_GOOD_QUALITY);
    for i in 0..TEST_MAX_LINES {
        img.push_line(&line, Instant::now() + Duration::from_millis(i as u64));
    }
    assert_eq!(img.len(), TEST_MAX_LINES);
    assert_eq!(
        img.lines.capacity(),
        initial_capacity,
        "filling exactly to reservation must not realloc",
    );
}

#[test]
fn lines_are_ordered_by_insertion() {
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    let t0 = Instant::now();
    for i in 0..TEST_ORDERED_INSERT_COUNT {
        let mut line = synth_line(TEST_GOOD_QUALITY);
        // Tag each line by writing its index into pixel 0 so we can
        // verify ordering is preserved by a renderer-style scan.
        line.pixels[0] = i as u8;
        img.push_line(
            &line,
            t0 + Duration::from_millis(u64::from(i) * TEST_ORDERED_STEP_MS),
        );
    }
    for (i, stored) in img.lines().iter().enumerate() {
        assert_eq!(stored.pixels[0], i as u8, "line at index {i} out of order");
    }
    // Timestamps strictly monotonic.
    for pair in img.lines().windows(2) {
        assert!(pair[0].received_at < pair[1].received_at);
    }
}

#[test]
fn channel_ids_are_settable_and_round_trip() {
    let mut img = AptImage::new(Instant::now());
    img.set_channel_a_id(AvhrrChannel::Ch2NearIr);
    img.set_channel_b_id(AvhrrChannel::Ch4ThermalIr);
    assert_eq!(img.channel_a_id(), Some(AvhrrChannel::Ch2NearIr));
    assert_eq!(img.channel_b_id(), Some(AvhrrChannel::Ch4ThermalIr));
}

#[test]
fn pass_start_round_trip() {
    let t = Instant::now();
    let img = AptImage::with_capacity(t, TEST_MAX_LINES);
    assert_eq!(img.pass_start(), t);
}

#[test]
fn width_constant_matches_apt_line_pixels() {
    assert_eq!(AptImage::WIDTH, LINE_PIXELS);
    assert_eq!(AptImage::WIDTH, EXPECTED_APT_WIDTH_PIXELS);
}

#[test]
fn raw_samples_preserved_on_high_quality_push() {
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    let line = synth_line(TEST_GOOD_QUALITY);
    img.push_line(&line, Instant::now());
    let stored = &img.lines()[0];
    // Raw samples must round-trip verbatim — the brightness
    // modes need access to the actual demodulator output.
    assert_eq!(stored.raw_samples, line.raw_samples);
}

#[test]
fn raw_samples_zeroed_on_gap_fill() {
    // Sub-threshold lines have raw_samples zeroed alongside
    // pixels so image-wide normalization doesn't pull from
    // a "gap" line's pre-fill values.
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    let line = synth_line(TEST_BAD_QUALITY);
    img.push_line(&line, Instant::now());
    let stored = &img.lines()[0];
    assert!(
        stored.raw_samples.iter().all(|&v| v == 0.0),
        "sub-threshold raw_samples should be zero-filled"
    );
}

#[test]
fn finalize_grayscale_minmax_maps_to_full_range() {
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    // Build a line with raw_samples that explicitly span [0, 250]
    // (by way of the `i % 251` pattern → values 0..=250).
    let line = synth_line(TEST_GOOD_QUALITY);
    img.push_line(&line, Instant::now());

    let img_pixels = img.finalize_grayscale(BrightnessMode::MinMax);
    assert_eq!(img_pixels.len(), AptImage::WIDTH);
    // MinMax should map 0 → 0 and the max value → 255.
    let min_p = *img_pixels.iter().min().expect("non-empty image");
    let max_p = *img_pixels.iter().max().expect("non-empty image");
    assert_eq!(min_p, 0);
    assert_eq!(max_p, 255);
}

#[test]
fn finalize_grayscale_percentile_clips_outliers() {
    // Build an image with a moderate ramp plus extreme outliers.
    // Percentile mode should clip the outliers to 0/255 and use
    // the bulk distribution as the reference range — MinMax
    // would let the outliers DOMINATE the range, compressing
    // the bulk into a narrow mid-gray band.
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    // 5 lines with a 0..=250 ramp.
    for _ in 0..5 {
        img.push_line(&synth_line(TEST_GOOD_QUALITY), Instant::now());
    }
    // 1 line with extreme outliers that would dominate MinMax.
    let mut outlier = synth_line(TEST_GOOD_QUALITY);
    outlier.raw_samples[0] = -10_000.0;
    outlier.raw_samples[1] = 10_000.0;
    img.push_line(&outlier, Instant::now());

    let pixels_minmax = img.finalize_grayscale(BrightnessMode::MinMax);
    let pixels_pct = img.finalize_grayscale(BrightnessMode::Percentile(0.98));

    // Property 1: Percentile clips the outliers themselves —
    // the negative outlier maps to 0, the positive to 255.
    let neg_outlier_idx = AptImage::WIDTH * 5; // line 5, col 0
    let pos_outlier_idx = AptImage::WIDTH * 5 + 1; // line 5, col 1
    assert_eq!(
        pixels_pct[neg_outlier_idx], 0,
        "negative outlier should clip to 0"
    );
    assert_eq!(
        pixels_pct[pos_outlier_idx], 255,
        "positive outlier should clip to 255"
    );

    // Property 2: MinMax compresses the bulk into a narrow band.
    // The bulk samples (0..=250) should span much less of the
    // 0..255 range under MinMax than under Percentile.
    // Sample a column inside the ramp to compare spreads.
    let bulk_min_idx = AptImage::WIDTH * 2 + 50; // line 2, col 50 → value 50
    let bulk_max_idx = AptImage::WIDTH * 2 + 200; // line 2, col 200 → value 200
    let mm_spread = pixels_minmax[bulk_max_idx].saturating_sub(pixels_minmax[bulk_min_idx]);
    let pct_spread = pixels_pct[bulk_max_idx].saturating_sub(pixels_pct[bulk_min_idx]);
    assert!(
        pct_spread > mm_spread,
        "Percentile should preserve more bulk-pixel spread than MinMax-with-outliers: \
         pct_spread={pct_spread}, mm_spread={mm_spread}"
    );
}

#[test]
fn finalize_grayscale_telemetry_falls_back_when_unavailable() {
    // TelemetryCalibrated falls back to Percentile(0.98) when the
    // wedge calibration isn't available. Both should produce the
    // same output here (no telemetry decoded yet).
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    for _ in 0..3 {
        img.push_line(&synth_line(TEST_GOOD_QUALITY), Instant::now());
    }
    let tele = img.finalize_grayscale(BrightnessMode::TelemetryCalibrated);
    let pct = img.finalize_grayscale(BrightnessMode::Percentile(0.98));
    assert_eq!(tele, pct, "expected telemetry → percentile fallback");
}

#[test]
fn finalize_grayscale_histogram_spans_full_range() {
    // Histogram equalization stretches the input distribution
    // toward uniform across [0, 255]. The equalization LUT maps
    // the lowest input bin to a small value (>0 because the
    // CDF starts at the lowest-bin count, not 0) and the
    // highest to ~255. The synth_line ramp 0..=250 gives a
    // near-uniform input distribution — equalization is close
    // to identity but with the boundaries pinned to the full
    // byte range.
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    for _ in 0..10 {
        img.push_line(&synth_line(TEST_GOOD_QUALITY), Instant::now());
    }
    let pixels = img.finalize_grayscale(BrightnessMode::Histogram);
    let min_p = *pixels.iter().min().expect("non-empty image");
    let max_p = *pixels.iter().max().expect("non-empty image");
    // Min near 0 (a few % at most — the lowest-bin CDF entry).
    assert!(
        min_p < 16,
        "min should be near 0 after equalization, got {min_p}"
    );
    // Max should reach the upper end of the byte range.
    assert!(
        max_p >= 240,
        "histogram equalization should push max toward 255, got {max_p}"
    );
}

#[test]
fn rotate_180_per_channel_flips_video_regions_only() {
    // Build a 4-row image where each pixel encodes (row, col)
    // so we can assert exactly where each pixel ended up after
    // rotation. The sync (0..86), telemetry (995..1040), etc.
    // strips should be UNTOUCHED; only the two video regions
    // [86..995] and [1126..2035] should rotate 180°.
    let height = 4;
    let mut image = vec![0_u8; AptImage::WIDTH * height];
    // Tag each pixel: high nibble = row, low nibble = col_bucket
    // (col / 256, so we can stuff 8 buckets into low nibble).
    for row in 0..height {
        for col in 0..AptImage::WIDTH {
            let col_bucket = (col / 256) as u8 & 0x0F;
            #[allow(clippy::cast_possible_truncation)]
            let tag = ((row as u8) << 4) | col_bucket;
            image[row * AptImage::WIDTH + col] = tag;
        }
    }
    let pre_sync_a = image[0]; // (row 0, col 0)
    let pre_telem_a = image[995]; // (row 0, col 995, before rotation)
    let pre_sync_b = image[1040]; // (row 0, col 1040 — start of channel B half)

    rotate_180_per_channel(&mut image, height);

    // Sync A strip (0..86) untouched.
    let post_sync_a = image[0];
    assert_eq!(
        post_sync_a, pre_sync_a,
        "Sync A strip (col 0) should be untouched",
    );
    // Telemetry A strip (995..1040) untouched.
    let post_telem_a = image[995];
    assert_eq!(
        post_telem_a, pre_telem_a,
        "Telemetry A strip (col 995) should be untouched",
    );
    // Sync B strip (1040..1126) untouched.
    let post_sync_b = image[1040];
    assert_eq!(
        post_sync_b, pre_sync_b,
        "Sync B strip (col 1040) should be untouched",
    );

    // Channel A video [86..995]: row 0 col 86 ↔ row 3 col 994.
    // Tag was (0 << 4) | (86/256=0) = 0x00 originally at [0, 86].
    // After 180° rotation, that pixel ends up at [3, 994] which
    // had tag (3<<4) | (994/256=3) = 0x33 originally.
    let post_video_a_top_left = image[86]; // (row 0, col 86) post-rotate
    // Should now equal what was at (row 3, col 994) before.
    let pre_value_at_3_994 = (3_u8 << 4) | ((994 / 256) as u8 & 0x0F);
    assert_eq!(
        post_video_a_top_left, pre_value_at_3_994,
        "Channel A video [0,86] should now hold the pre-rotate value of [3,994]",
    );
}

#[test]
fn rotate_180_per_channel_self_inverse() {
    // Rotating twice returns to the original image — the
    // simplest invariant of any 180° rotation.
    let height = 5; // odd to exercise the middle-row path
    let mut image: Vec<u8> = (0..AptImage::WIDTH * height)
        .map(|i| (i & 0xFF) as u8)
        .collect();
    let original = image.clone();
    rotate_180_per_channel(&mut image, height);
    rotate_180_per_channel(&mut image, height);
    assert_eq!(
        image, original,
        "double 180° rotation should restore original image",
    );
}

#[test]
fn rotate_180_per_channel_handles_zero_height() {
    // Defensive: zero-height shouldn't panic (was an
    // off-by-one risk in the original implementation).
    let mut image = Vec::<u8>::new();
    rotate_180_per_channel(&mut image, 0);
    assert!(image.is_empty());
}

#[test]
fn finalize_grayscale_gap_lines_emit_black() {
    // A low-quality line in the middle of the image should be a
    // black row in the finalized grayscale output regardless of
    // mode — gap lines have no real signal to map.
    let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
    img.push_line(&synth_line(TEST_GOOD_QUALITY), Instant::now());
    img.push_line(&synth_line(TEST_BAD_QUALITY), Instant::now()); // gap
    img.push_line(&synth_line(TEST_GOOD_QUALITY), Instant::now());

    let pixels = img.finalize_grayscale(BrightnessMode::Percentile(0.98));
    // Row index 1 is the gap line. All its pixels must be 0.
    let gap_row = &pixels[AptImage::WIDTH..2 * AptImage::WIDTH];
    assert!(
        gap_row.iter().all(|&p| p == 0),
        "gap row not all-zero: first nonzero at {:?}",
        gap_row.iter().position(|&p| p != 0),
    );
}

/// #774 — the video band starts after the 39-px Sync A *field* and
/// the 47-px space, i.e. at column 86; rotating from column 85
/// (38-px template width + 47) dragged the last pre-video space
/// pixel into the video band and seamed the image.
#[test]
fn rotate_180_per_channel_starts_after_the_39px_sync_field() {
    /// Two rows are the smallest image in which a 180° rotation
    /// moves pixels across rows, so the boundary is observable.
    const HEIGHT: usize = 2;
    /// A small non-zero stride makes the two rows' patterns differ,
    /// so a moved pixel cannot be mistaken for an untouched one.
    const TEST_ROTATION_ROW_STRIDE: usize = 7;
    /// Expected APT geometry, written out independently of the
    /// production constants so boundary drift is detected:
    /// 39-px Sync A field + 47-px space → last non-video column 85,
    /// first video column 86, last video column 86 + 909 − 1.
    const EXPECTED_LAST_NON_VIDEO_COL: usize = 85;
    const EXPECTED_FIRST_VIDEO_COL: usize = 86;
    const EXPECTED_LAST_VIDEO_COL: usize = 994;
    let width = AptImage::WIDTH;
    let mut image = vec![0_u8; width * HEIGHT];
    for row in 0..HEIGHT {
        for col in 0..width {
            image[row * width + col] =
                ((row * TEST_ROTATION_ROW_STRIDE + col) % TEST_PIXEL_PATTERN_MODULUS) as u8;
        }
    }
    let original = image.clone();
    rotate_180_per_channel(&mut image, HEIGHT);
    assert_eq!(
        image[EXPECTED_LAST_NON_VIDEO_COL], original[EXPECTED_LAST_NON_VIDEO_COL],
        "column 85 is the last pre-video space px, untouched"
    );
    assert_eq!(
        image[EXPECTED_FIRST_VIDEO_COL],
        original[width + EXPECTED_LAST_VIDEO_COL],
        "column 86 is the first video px, rotated"
    );
}
