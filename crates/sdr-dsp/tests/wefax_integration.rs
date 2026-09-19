//! Hermetic WEFAX geometry integration test. No external fixtures so CI
//! stays self-contained: a synthetic white tone through the free-running
//! decoder must yield the expected line count and full-width scanlines,
//! pinning the pixel-clock geometry against regressions.

// Test harness: relax the workspace-pedantic numeric-cast lints — the
// synthetic-audio generation and the line-count assertion use plain casts.
#![allow(
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

use sdr_dsp::wefax::{PIXELS_PER_LINE, WefaxDecoder, WefaxLine};

/// Free-running decode of N seconds of white tone yields ≈ 2·N lines of
/// full width — locks the pixel-clock geometry against regressions.
#[test]
fn free_running_line_geometry() {
    let sr = 24_000u32;
    let mut dec = WefaxDecoder::new_free_running(sr);
    let secs = 10;
    let n = sr as usize * secs;
    let audio: Vec<f32> = (0..n)
        .map(|i| (2.0 * std::f64::consts::PI * 2300.0 * i as f64 / sr as f64).sin() as f32)
        .collect();
    let mut out = vec![WefaxLine::default(); 64];
    let mut lines = 0usize;
    for c in audio.chunks(8192) {
        lines += dec.process(c, &mut out).unwrap();
    }
    let expected = 2 * secs; // 120 lpm
    assert!(
        (lines as i64 - expected as i64).abs() <= 1,
        "≈{expected} lines, got {lines}"
    );
    let bright = out[0].pixels.iter().filter(|&&p| p > 200).count();
    assert!(
        bright > PIXELS_PER_LINE / 2,
        "white tone fills the scanline, got {bright} bright of {PIXELS_PER_LINE}"
    );
}
