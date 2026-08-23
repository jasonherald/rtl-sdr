use super::*;

#[test]
fn clear_zeros_pixel_buf_and_resets_top_row() {
    // Per #646: toggling the waterfall off must wipe the
    // surface so the user doesn't see a frozen pre-disable
    // snapshot. Fresh `new()` state is all-zeros + top_row=0;
    // `clear()` must restore that exact state regardless of
    // how many lines were pushed first.
    let mut r = WaterfallRenderer::new(64);
    for _ in 0..10 {
        r.push_line(&vec![-30.0_f32; 64]);
    }
    assert!(
        r.pixel_buf.iter().any(|b| *b != 0),
        "push_line must have written non-zero pixels for the test premise to hold"
    );
    // top_row may have advanced (it walks backwards as new
    // rows arrive — the ring-buffer wrap shipped in PR #458).
    // Whichever value it landed on, `clear()` should reset it.

    r.clear();

    assert!(
        r.pixel_buf.iter().all(|b| *b == 0),
        "clear must zero every byte of pixel_buf"
    );
    assert_eq!(
        r.top_row, 0,
        "clear must reset top_row so the next push_line starts at the top"
    );
}

#[test]
fn downsample_preserves_peak() {
    let data = [0.0, 5.0, 1.0, 3.0, 2.0, 8.0, 4.0, 1.0];
    let mut buf = Vec::new();
    downsample_to(&data, &mut buf, 4);
    assert_eq!(buf.len(), 4);
    assert!((buf[0] - 5.0).abs() < f32::EPSILON);
    assert!((buf[1] - 3.0).abs() < f32::EPSILON);
    assert!((buf[2] - 8.0).abs() < f32::EPSILON);
    assert!((buf[3] - 4.0).abs() < f32::EPSILON);
}

#[test]
fn downsample_non_divisible() {
    // 7 bins -> 3: ratio 2.333, buckets [0..3), [2..5), [4..7)
    let data = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
    let mut buf = Vec::new();
    downsample_to(&data, &mut buf, 3);
    assert_eq!(buf.len(), 3);
    assert!((buf[0] - 3.0).abs() < f32::EPSILON); // max(1, 2, 3)
    assert!((buf[1] - 5.0).abs() < f32::EPSILON); // max(3, 4, 5)
    assert!((buf[2] - 7.0).abs() < f32::EPSILON); // max(5, 6, 7)
}

#[test]
fn downsample_single_output() {
    let data = [1.0, 9.0, 3.0, 2.0];
    let mut buf = Vec::new();
    downsample_to(&data, &mut buf, 1);
    assert_eq!(buf.len(), 1);
    assert!((buf[0] - 9.0).abs() < f32::EPSILON);
}

#[test]
fn downsample_same_size_passthrough() {
    let data = [1.0, 2.0, 3.0];
    let mut buf = Vec::new();
    downsample_to(&data, &mut buf, 3);
    assert_eq!(buf.len(), 3);
    assert!((buf[0] - 1.0).abs() < f32::EPSILON);
    assert!((buf[1] - 2.0).abs() < f32::EPSILON);
    assert!((buf[2] - 3.0).abs() < f32::EPSILON);
}

#[test]
fn rgba_to_bgra_converts_correctly() {
    let rgba = vec![[255, 128, 64, 255]];
    let bgra = rgba_to_bgra(&rgba);
    assert_eq!(bgra[0], [64, 128, 255, 255]);
}

#[test]
fn supported_width_clamped() {
    assert_eq!(supported_texture_width(8192), MAX_TEXTURE_WIDTH);
    assert_eq!(supported_texture_width(1024), 1024);
}

/// Ring-buffer test fixtures — named so the ring-invariant
/// tests read at the level of intent, not at the level of
/// specific byte values.
const WIDTH_SMALL: usize = 4;
const WIDTH_LARGE: usize = 8;
const DB_MIN: f32 = 0.0;
const DB_MAX: f32 = 100.0;
/// Normalizes to `(50 - 0) / 100 = 0.5 → byte 128` — a
/// mid-range value that's cleanly distinguishable from both
/// the floor (byte 0) and the saturation (byte 255) in the
/// ring-buffer placement tests.
const DB_MID: f32 = 50.0;
/// Normalizes to `1.0 → byte 255` — the saturation end. Used
/// by the narrow-FFT test to make the "stale tail" regression
/// visually obvious if it recurs.
const DB_HIGH: f32 = 100.0;
/// Low dB anchor for the linearization ordering test. Paired
/// with `DB_HIGH` to make newest-row vs oldest-row easy to
/// identify in the linearized output.
const DB_LOW: f32 = 0.0;

/// Helper: read the BGRA pixel at the physical row / column out
/// of the renderer's internal buffer. Tests use this to verify
/// that the ring-buffer `top_row` advances correctly.
fn physical_pixel(r: &WaterfallRenderer, row: usize, col: usize) -> [u8; 4] {
    let idx = (row * r.display_width + col) * 4;
    [
        r.pixel_buf[idx],
        r.pixel_buf[idx + 1],
        r.pixel_buf[idx + 2],
        r.pixel_buf[idx + 3],
    ]
}

/// Helper: read one BGRA pixel from a linearized buffer.
fn linear_pixel(buf: &[u8], width: usize, row: usize, col: usize) -> [u8; 4] {
    let idx = (row * width + col) * 4;
    [buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3]]
}

#[test]
fn ring_buffer_starts_at_zero() {
    let r = WaterfallRenderer::new(WIDTH_LARGE);
    assert_eq!(r.top_row, 0);
    assert!(r.pixel_buf.iter().all(|&b| b == 0));
}

#[test]
fn ring_buffer_advances_backwards_on_push() {
    let mut r = WaterfallRenderer::new(WIDTH_SMALL);
    r.set_db_range(DB_MIN, DB_MAX);
    // One push advances `top_row` from 0 to HISTORY_LINES - 1.
    r.push_line(&[DB_MID; WIDTH_SMALL]);
    assert_eq!(r.top_row, HISTORY_LINES - 1);
    // Second push advances to HISTORY_LINES - 2.
    r.push_line(&[DB_MID; WIDTH_SMALL]);
    assert_eq!(r.top_row, HISTORY_LINES - 2);
}

#[test]
fn ring_buffer_wraps_after_full_cycle() {
    let mut r = WaterfallRenderer::new(WIDTH_SMALL);
    r.set_db_range(DB_MIN, DB_MAX);
    for _ in 0..HISTORY_LINES {
        r.push_line(&[DB_MID; WIDTH_SMALL]);
    }
    // After exactly HISTORY_LINES pushes we wrap back to 0.
    assert_eq!(r.top_row, 0);
    // One more push: back to HISTORY_LINES - 1.
    r.push_line(&[DB_MID; WIDTH_SMALL]);
    assert_eq!(r.top_row, HISTORY_LINES - 1);
}

#[test]
fn pushed_row_lands_at_top_row_offset() {
    let mut r = WaterfallRenderer::new(WIDTH_SMALL);
    r.set_db_range(DB_MIN, DB_MAX);

    // Push a distinctive line: four distinct magnitudes so the
    // colormap lookup produces distinct per-column outputs.
    // With the default Turbo colormap we don't care about exact
    // RGB — we care that the row was written at the current
    // `top_row`, and the row BEFORE it (physical row `top_row
    // + 1`, still zeroed) is untouched.
    r.push_line(&[0.0, 25.0, DB_MID, DB_HIGH]);
    let first_top = r.top_row;
    assert_eq!(first_top, HISTORY_LINES - 1);
    // The new row is non-zero — check the fourth pixel, which
    // uses colormap[255] and is definitely non-zero for Turbo.
    let p = physical_pixel(&r, first_top, 3);
    assert!(p != [0, 0, 0, 0], "new row pixel should be non-zero");
    // And the row ADJACENT to top_row going the other way
    // (physical row 0, which will be overwritten last) is still
    // zero.
    assert_eq!(physical_pixel(&r, 0, 3), [0, 0, 0, 0]);

    // Push a second line and confirm it lands one physical row
    // up, not on top of the previous row.
    r.push_line(&[0.0, 25.0, DB_MID, DB_HIGH]);
    assert_eq!(r.top_row, HISTORY_LINES - 2);
    // Previous row unchanged.
    let p_prev = physical_pixel(&r, first_top, 3);
    assert_eq!(
        p, p_prev,
        "previous row pixel must not be touched by next push"
    );
}

#[test]
fn narrow_fft_does_not_leak_stale_tail() {
    // Simulates the ring-buffer hazard: after many rows of a
    // full-width FFT, switch to a narrow FFT. The recycled
    // physical slot previously held a fully-populated row with
    // non-zero tail pixels — if `push_line` only writes
    // `bin_count` pixels, those stale tail pixels bleed into
    // the new row.
    let mut r = WaterfallRenderer::new(WIDTH_SMALL);
    r.set_db_range(DB_MIN, DB_MAX);

    // Fill every slot with a fully-saturated full-width row so
    // the tail positions are non-zero across the whole ring.
    for _ in 0..HISTORY_LINES {
        r.push_line(&[DB_HIGH; WIDTH_SMALL]);
    }

    // Now push a half-width frame — the other two positions
    // are "past the FFT width" and MUST render as `colormap[0]`,
    // not the previous row's saturated tail.
    r.push_line(&[DB_HIGH; WIDTH_SMALL / 2]);
    let top = r.top_row;

    // Positions 0..WIDTH_SMALL/2: high-level color.
    let high = physical_pixel(&r, top, 0);
    // Positions past the FFT width: "no data" color.
    let no_data = physical_pixel(&r, top, WIDTH_SMALL / 2);

    assert_ne!(
        high, no_data,
        "narrow-FFT tail should differ from the data region"
    );
    // And no-data must match the colormap's zero entry (the
    // "floor" color), not the saturated color the old row had.
    let floor = r.colormap_bgra[0];
    assert_eq!(
        no_data, floor,
        "tail pixels must be colormap[0], not stale ring-buffer content"
    );
}

#[test]
fn resize_resets_ring_index() {
    let mut r = WaterfallRenderer::new(WIDTH_SMALL);
    r.set_db_range(DB_MIN, DB_MAX);
    r.push_line(&[DB_MID; WIDTH_SMALL]);
    assert_eq!(r.top_row, HISTORY_LINES - 1);
    r.resize(WIDTH_LARGE);
    assert_eq!(r.top_row, 0);
    assert!(r.pixel_buf.iter().all(|&b| b == 0));
}

#[test]
fn linearize_places_newest_row_on_top() {
    // Exercises `linearized_pixel_buf` — the function used by
    // `export_png` to turn a ring-ordered pixel_buf into
    // newest-on-top visual order. This is the test CR asked for:
    // it verifies correct ordering WITHOUT the PNG round-trip,
    // and works past the ring wrap where ordering is most
    // likely to break.
    let mut r = WaterfallRenderer::new(WIDTH_SMALL);
    r.set_db_range(DB_MIN, DB_MAX);

    // Push HISTORY_LINES+5 frames so we've wrapped once. The
    // final five frames alternate LOW / HIGH so the newest row
    // is identifiable by color.
    for _ in 0..HISTORY_LINES {
        r.push_line(&[DB_LOW; WIDTH_SMALL]);
    }
    // Four low rows then one high row. After these pushes, the
    // most recent row (which should end up at display row 0) is
    // the high one.
    r.push_line(&[DB_LOW; WIDTH_SMALL]);
    r.push_line(&[DB_LOW; WIDTH_SMALL]);
    r.push_line(&[DB_LOW; WIDTH_SMALL]);
    r.push_line(&[DB_LOW; WIDTH_SMALL]);
    r.push_line(&[DB_HIGH; WIDTH_SMALL]);

    let floor = r.colormap_bgra[0];
    let saturation = r.colormap_bgra[255];

    let linear = r.linearized_pixel_buf();

    // Display row 0: the newest push — the saturated row.
    assert_eq!(
        linear_pixel(&linear, WIDTH_SMALL, 0, 0),
        saturation,
        "display row 0 must be the newest (saturated) line"
    );
    // Display rows 1..5: the four `DB_LOW` pushes immediately
    // before the saturated one.
    for visual_row in 1..5 {
        assert_eq!(
            linear_pixel(&linear, WIDTH_SMALL, visual_row, 0),
            floor,
            "display row {visual_row} must be the DB_LOW line"
        );
    }
}

/// Per #516: scanner-axis sparse fill. With a 4 MHz locked
/// range and a 250 kHz FFT centred at 146 MHz, only the
/// pixel slice corresponding to `[145.875, 146.125] MHz`
/// should carry signal-coloured pixels; the rest of the row
/// must read `colormap[0]` ("no signal" floor).
#[test]
fn push_line_locked_writes_only_active_slice() {
    const WIDTH: usize = 1024;
    let mut r = WaterfallRenderer::new(WIDTH);
    r.set_db_range(DEFAULT_MIN_DB, DEFAULT_MAX_DB);
    let floor = r.colormap_bgra[0];
    let saturation = r.colormap_bgra[255];

    // 250 kHz FFT, all bins at saturation. Channel sits in
    // the middle of a 4 MHz scanner range.
    let fft = vec![0.0_f32; 256]; // 0 dB == DEFAULT_MAX_DB == sat
    let lock = ScannerAxisLock {
        min_hz: 144_000_000.0,
        max_hz: 148_000_000.0,
        active_channel_hz: Some(146_000_000.0),
        active_channel_bw_hz: Some(25_000.0),
    };
    r.push_line_locked(&fft, 250_000.0, &lock);

    let linear = r.linearized_pixel_buf();
    // Active channel + FFT span maps to absolute frequency
    // [145.875M, 146.125M]. Within the [144M, 148M] range
    // and 1024 px width, the FFT's first bin lands at
    // pixel 480 and the last bin lands at pixel 544
    // (inclusive — `bin_to_locked_x` puts bin N-1 at
    // exactly active + bw/2, which yields `x = 544.0` for
    // these inputs). Outside `[480, 544]`, every pixel
    // must be `colormap[0]`.
    for px in 0..WIDTH {
        let bgra = linear_pixel(&linear, WIDTH, 0, px);
        if (480..=544).contains(&px) {
            assert_eq!(
                bgra, saturation,
                "px {px} (inside active slice) must carry signal colour"
            );
        } else {
            assert_eq!(
                bgra, floor,
                "px {px} (outside active slice) must carry colormap[0]"
            );
        }
    }
}

/// Per #516: when the scanner has engaged but no channel is
/// active yet (between `enter_scanner_mode` and the first
/// `set_scanner_active_channel`), the push must still
/// advance the ring and produce a uniform `colormap[0]` row
/// — historical rows scroll downward at the normal cadence,
/// just dark.
#[test]
fn push_line_locked_with_no_active_channel_fills_dark() {
    const WIDTH: usize = 1024;
    let mut r = WaterfallRenderer::new(WIDTH);
    r.set_db_range(DEFAULT_MIN_DB, DEFAULT_MAX_DB);
    let floor = r.colormap_bgra[0];

    let fft = vec![0.0_f32; 256];
    let lock = ScannerAxisLock {
        min_hz: 144_000_000.0,
        max_hz: 148_000_000.0,
        active_channel_hz: None,
        active_channel_bw_hz: None,
    };
    r.push_line_locked(&fft, 250_000.0, &lock);

    let linear = r.linearized_pixel_buf();
    for px in 0..WIDTH {
        assert_eq!(
            linear_pixel(&linear, WIDTH, 0, px),
            floor,
            "px {px}: with no active channel, the entire row must be colormap[0]",
        );
    }
}
