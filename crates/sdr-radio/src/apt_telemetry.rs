//! NOAA APT telemetry-strip decode.
//!
//! Each APT scan line carries a 45-pixel telemetry strip on each side
//! (one for Channel A, one for Channel B). Stacking those strips
//! vertically across 128 consecutive lines gives a 16-wedge × 8-line
//! repeating pattern:
//!
//! ```text
//!   wedges 1..=8   — calibration grayscale ramp (dark → white)
//!   wedges 9..=15  — spacecraft thermal telemetry
//!   wedge   16     — channel-ID: indicates which AVHRR channel
//!                    is currently transmitted on this side
//! ```
//!
//! This module turns an [`AptImage`] (assembled scan lines from the
//! decoder) into a decoded [`AptTelemetry`] result. We only consume
//! wedges 1–8 (calibration) and wedge 16 (channel-ID); the spacecraft
//! thermal telemetry wedges 9–15 stay unparsed for now (out-of-scope
//! for #479; would need radiometric calibration to be useful).
//!
//! # Algorithm
//!
//! 1. For each scan line, average the 45 horizontal pixels of the
//!    telemetry strip on each side → one u8 per line per side.
//! 2. With ≥ [`FRAME_LINES`] (128) per-line averages buffered, scan
//!    every candidate frame-start offset and Pearson-correlate the
//!    candidate's first 64 line-averages against a hard-coded template
//!    of the canonical calibration ramp (8 lines of `8`, then 8 of
//!    `31`, …, 8 of `255`). Templating against the line-by-line spec
//!    pattern (rather than an idealized linear ramp) gives a sharp
//!    `1.0` correlation at the true frame boundary while penalizing
//!    every off-by-one offset decisively, since at any shift the
//!    wedge transitions land in the wrong slots of the template.
//! 3. With sync locked, average each of the 16 wedges as the mean of
//!    its 8 line averages.
//! 4. Wedge 16 is classified by nearest-match against the decoded
//!    wedges 1–6 — the AVHRR spec uses wedges 1..=6 of the calibration
//!    ramp as the channel-ID encoding, in this exact order:
//!
//!    ```text
//!     wedge 1 ↔ Channel 1 (visible)
//!     wedge 2 ↔ Channel 2 (near-IR)
//!     wedge 3 ↔ Channel 3A (shortwave IR, daytime)
//!     wedge 4 ↔ Channel 3B (thermal IR, nighttime)
//!     wedge 5 ↔ Channel 4 (thermal IR, sea-surface temp)
//!     wedge 6 ↔ Channel 5 (thermal IR, cloud-top temp)
//!    ```
//!
//! Note: pixel-position numbers in the original ticket (909..954,
//! 1989..2034) were off-by-86 — they treated indices as "from start
//! of video A" instead of "from start of line". The values here
//! (995..1040 and 2035..2080) match the NOAA KLM User's Guide and
//! every open-source APT decoder I cross-checked.

use sdr_dsp::apt::LINE_PIXELS;

use crate::apt_image::{AptImage, AptImageLine, AvhrrChannel};

/// Width of one telemetry strip in pixels (per APT spec).
pub const TELEMETRY_WIDTH: usize = 45;

/// First pixel of the Channel A telemetry strip in a 2080-pixel line.
/// The Channel A layout is `Sync(39) + Space(47) + Video(909) + Telem(45)`,
/// so telemetry starts at `39 + 47 + 909 = 995`.
pub const TELEMETRY_A_START: usize = 995;
/// One past the last pixel of Channel A telemetry (`995 + 45 = 1040`).
pub const TELEMETRY_A_END: usize = TELEMETRY_A_START + TELEMETRY_WIDTH;

/// First pixel of the Channel B telemetry strip in a 2080-pixel line.
/// Channel B is laid out the same way but starts at the line midpoint
/// 1040, so its telemetry starts at `1040 + 39 + 47 + 909 = 2035`.
pub const TELEMETRY_B_START: usize = 2035;
/// One past the last pixel of Channel B telemetry (`2035 + 45 = 2080`).
pub const TELEMETRY_B_END: usize = TELEMETRY_B_START + TELEMETRY_WIDTH;

/// Number of wedges in one telemetry frame (per APT spec).
pub const WEDGES_PER_FRAME: usize = 16;
/// Lines per wedge — vertically each wedge is repeated 8 times.
pub const LINES_PER_WEDGE: usize = 8;
/// Total lines in one full telemetry frame (`16 × 8 = 128`).
pub const FRAME_LINES: usize = WEDGES_PER_FRAME * LINES_PER_WEDGE;

/// Canonical 8-step calibration ramp brightness values from the APT
/// spec (wedges 1..=8, dark → white). Used both as the frame-sync
/// correlation template and as the channel-ID classification reference.
pub const SPEC_GRAYSCALE_RAMP: [u8; 8] = [8, 31, 63, 95, 127, 159, 191, 255];

/// Number of lines covered by the calibration-ramp portion of a frame
/// (`8 wedges × 8 lines = 64 lines`). The frame-sync correlator
/// templates against just this portion since wedges 9–15 carry
/// unknown spacecraft data and wedge 16 carries the unknown channel ID.
const RAMP_LINES: usize = 8 * LINES_PER_WEDGE;

// Compile-time invariants — if any of these trip, the constants drifted
// out of sync with the layout the docs above describe.
const _: () = assert!(TELEMETRY_A_END == 1040);
const _: () = assert!(TELEMETRY_B_END == LINE_PIXELS);
const _: () = assert!(TELEMETRY_B_START - TELEMETRY_A_END == 995);
const _: () = assert!(FRAME_LINES == 128);

/// Decoded telemetry for one APT pass — both sides of the line.
#[derive(Debug, Clone)]
pub struct AptTelemetry {
    /// Telemetry decoded from the Channel A side (left half of line).
    pub side_a: AptTelemetrySide,
    /// Telemetry decoded from the Channel B side (right half of line).
    pub side_b: AptTelemetrySide,
}

/// Decoded telemetry from a single side of the scan line.
#[derive(Debug, Clone)]
pub struct AptTelemetrySide {
    /// The 8-step grayscale calibration ramp (wedges 1–8). Should be
    /// roughly monotonically increasing from dark to bright.
    pub grayscale_ramp: [u8; 8],
    /// AVHRR channel encoded in wedge 16, or `None` if the wedge value
    /// matched none of wedges 1–6 closely enough to classify.
    pub channel_id: Option<AvhrrChannel>,
    /// Quality of the frame-sync lock for this side, in `[0.0, 1.0]`.
    /// Pearson correlation between the candidate's first 64 line
    /// averages and the canonical calibration-ramp line template,
    /// re-mapped from `[-1, 1]` to `[0, 1]`. `1.0` = perfect alignment,
    /// `0.5` ≈ no correlation, anything below ~0.6 is effectively noise.
    pub frame_sync_quality: f32,
}

/// Average the 45-pixel Channel A telemetry strip of one scan line into
/// a single u8.
#[must_use]
pub fn line_telemetry_a(pixels: &[u8; LINE_PIXELS]) -> u8 {
    average_strip(&pixels[TELEMETRY_A_START..TELEMETRY_A_END])
}

/// Average the 45-pixel Channel B telemetry strip of one scan line into
/// a single u8.
#[must_use]
pub fn line_telemetry_b(pixels: &[u8; LINE_PIXELS]) -> u8 {
    average_strip(&pixels[TELEMETRY_B_START..TELEMETRY_B_END])
}

/// Decode telemetry for both sides of an [`AptImage`].
///
/// Returns `None` if the image has fewer than [`FRAME_LINES`] (128) scan
/// lines — the frame-sync algorithm needs a full cycle to lock.
#[must_use]
pub fn decode_telemetry(image: &AptImage) -> Option<AptTelemetry> {
    if image.len() < FRAME_LINES {
        return None;
    }

    let lines = image.lines();
    let avgs_a: Vec<u8> = lines.iter().map(image_line_avg_a).collect();
    let avgs_b: Vec<u8> = lines.iter().map(image_line_avg_b).collect();

    Some(AptTelemetry {
        side_a: decode_side(&avgs_a)?,
        side_b: decode_side(&avgs_b)?,
    })
}

/// Decode telemetry from one side's per-line averages.
///
/// Returns `None` if `line_avgs` has fewer than [`FRAME_LINES`] entries.
#[must_use]
pub fn decode_side(line_avgs: &[u8]) -> Option<AptTelemetrySide> {
    if line_avgs.len() < FRAME_LINES {
        return None;
    }

    let (frame_offset, frame_sync_quality) = find_frame_start(line_avgs);
    let wedges = extract_wedges(line_avgs, frame_offset);
    let mut grayscale_ramp = [0_u8; 8];
    grayscale_ramp.copy_from_slice(&wedges[0..8]);
    // Wedge 16 lives at index 15 (zero-based).
    let channel_id = classify_channel_wedge(wedges[15], grayscale_ramp);

    Some(AptTelemetrySide {
        grayscale_ramp,
        channel_id,
        frame_sync_quality,
    })
}

// ─── Internals ────────────────────────────────────────────────────────

fn image_line_avg_a(line: &AptImageLine) -> u8 {
    line_telemetry_a(&line.pixels)
}

fn image_line_avg_b(line: &AptImageLine) -> u8 {
    line_telemetry_b(&line.pixels)
}

fn average_strip(strip: &[u8]) -> u8 {
    debug_assert_eq!(strip.len(), TELEMETRY_WIDTH);
    let sum: u32 = strip.iter().copied().map(u32::from).sum();
    // u32 fits 255 × 45 = 11475 trivially; never overflows.
    #[allow(clippy::cast_possible_truncation)]
    {
        (sum / TELEMETRY_WIDTH as u32) as u8
    }
}

/// Scan every candidate frame-start offset and return `(offset, quality)`
/// for the best-matching one.
///
/// "Quality" is the Pearson correlation between
/// `line_avgs[offset..offset + RAMP_LINES]` and the canonical 64-line
/// calibration-ramp template (8 lines of `8`, then 8 of `31`, …, 8 of
/// `255`), mapped from `[-1, 1]` to `[0, 1]`. Comparing line-by-line
/// against the spec ramp (rather than against a generic linear template)
/// gives a sharp `1.0` at the true frame boundary and decisive penalties
/// at off-by-one offsets, where the wedge transitions land in the wrong
/// slot of the template.
///
/// We only scan offsets where a full [`FRAME_LINES`]-line frame fits
/// past the offset, so the caller can safely follow up with
/// [`extract_wedges`] using the returned offset.
fn find_frame_start(line_avgs: &[u8]) -> (usize, f32) {
    debug_assert!(line_avgs.len() >= FRAME_LINES);

    // We need a full frame past the chosen offset for wedge extraction,
    // so cap the scan range to `len - FRAME_LINES`. Within a single
    // frame's worth of slack (FRAME_LINES candidate offsets) is enough —
    // beyond that we'd just be re-scoring identical patterns from
    // subsequent cycles.
    let max_offset = line_avgs.len().saturating_sub(FRAME_LINES);
    let scan_range = max_offset.min(FRAME_LINES - 1) + 1;

    let mut best = (0_usize, f32::NEG_INFINITY);
    for offset in 0..scan_range {
        let score = ramp_template_correlation(line_avgs, offset);
        if score > best.1 {
            best = (offset, score);
        }
    }
    // Map [-1, 1] correlation to [0, 1] for the quality metric.
    let quality = (best.1 + 1.0) * 0.5;
    (best.0, quality.clamp(0.0, 1.0))
}

/// Pearson correlation of `line_avgs[offset..offset + RAMP_LINES]`
/// against the canonical line-by-line calibration template (each spec
/// ramp value repeated [`LINES_PER_WEDGE`] times). Returns `0.0` if the
/// window has zero variance, otherwise `[-1.0, 1.0]`. Aligned frame
/// starts return exactly `1.0`; even a one-line shift drops well below
/// because every wedge boundary now misaligns with the template.
#[allow(clippy::cast_precision_loss)]
fn ramp_template_correlation(line_avgs: &[u8], offset: usize) -> f32 {
    debug_assert!(offset + RAMP_LINES <= line_avgs.len());

    let n = RAMP_LINES as f32;
    let mut window_sum = 0.0_f32;
    let mut template_sum = 0.0_f32;
    let mut cross_sum = 0.0_f32;
    let mut window_sq_sum = 0.0_f32;
    let mut template_sq_sum = 0.0_f32;
    for i in 0..RAMP_LINES {
        let window = f32::from(line_avgs[offset + i]);
        let template = f32::from(SPEC_GRAYSCALE_RAMP[i / LINES_PER_WEDGE]);
        window_sum += window;
        template_sum += template;
        cross_sum += window * template;
        window_sq_sum += window * window;
        template_sq_sum += template * template;
    }
    let cov = cross_sum - window_sum * template_sum / n;
    let var_w = window_sq_sum - window_sum * window_sum / n;
    let var_t = template_sq_sum - template_sum * template_sum / n;
    let denom = (var_w * var_t).sqrt();
    if denom < f32::EPSILON {
        return 0.0;
    }
    cov / denom
}

/// Extract 16 wedge averages from `line_avgs` starting at `frame_offset`.
///
/// Each wedge averages [`LINES_PER_WEDGE`] (8) consecutive line averages.
/// If the buffer doesn't have a full 128 lines past `frame_offset`,
/// missing wedges are zero-filled — the caller controls the pre-check.
fn extract_wedges(line_avgs: &[u8], frame_offset: usize) -> [u8; WEDGES_PER_FRAME] {
    let mut wedges = [0_u8; WEDGES_PER_FRAME];
    for (w, dst) in wedges.iter_mut().enumerate() {
        let start = frame_offset + w * LINES_PER_WEDGE;
        let end = start + LINES_PER_WEDGE;
        if end > line_avgs.len() {
            break;
        }
        let sum: u32 = line_avgs[start..end].iter().copied().map(u32::from).sum();
        #[allow(clippy::cast_possible_truncation)]
        {
            *dst = (sum / LINES_PER_WEDGE as u32) as u8;
        }
    }
    wedges
}

/// Channels are encoded by matching wedge 16's brightness against the
/// calibration ramp's wedges 1–6, in this specific order.
const CHANNEL_ID_MAPPING: [AvhrrChannel; 6] = [
    AvhrrChannel::Ch1Visible,
    AvhrrChannel::Ch2NearIr,
    AvhrrChannel::Ch3aShortwaveIr,
    AvhrrChannel::Ch3bThermalIr,
    AvhrrChannel::Ch4ThermalIr,
    AvhrrChannel::Ch5ThermalIr,
];

/// Map a wedge-16 brightness value to an AVHRR channel by finding which
/// of wedges 1–6 of the calibration ramp it most closely matches.
///
/// Returns `None` if the calibration ramp's range is too narrow to
/// confidently classify (e.g. the side has nothing but noise) — protects
/// against false positives when the image isn't really showing telemetry.
fn classify_channel_wedge(wedge16: u8, grayscale_ramp: [u8; 8]) -> Option<AvhrrChannel> {
    // If the ramp's dynamic range is tiny, it's not a real telemetry
    // strip — bail rather than emit a noise classification.
    let min = *grayscale_ramp.iter().min()?;
    let max = *grayscale_ramp.iter().max()?;
    if max.saturating_sub(min) < MIN_RAMP_RANGE {
        return None;
    }

    let mut best_idx = 0_usize;
    let mut best_distance = u16::MAX;
    for (i, &ramp_value) in grayscale_ramp.iter().take(6).enumerate() {
        let distance = u16::from(wedge16.abs_diff(ramp_value));
        if distance < best_distance {
            best_distance = distance;
            best_idx = i;
        }
    }
    Some(CHANNEL_ID_MAPPING[best_idx])
}

/// Minimum dark-to-bright range (in raw u8 units) the calibration ramp
/// must span to be considered a real telemetry signal. A narrower range
/// means the channel is either flat black, flat white, or noise — none
/// of which can reliably classify wedge 16.
const MIN_RAMP_RANGE: u8 = 32;

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    clippy::cast_possible_truncation,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use sdr_dsp::apt::AptLine;
    use std::time::Instant;

    /// Tight pre-allocation for [`AptImage`] in tests — well under
    /// [`crate::apt_image::DEFAULT_MAX_LINES`].
    const TEST_MAX_LINES: usize = 256;

    /// Quality value high enough to clear the [`AptImage`] gap-fill threshold.
    const TEST_GOOD_QUALITY: f32 = 0.92;

    /// Build a synthetic 2080-pixel scan line with the given wedge value
    /// painted across both telemetry strips and zeros elsewhere. Lets us
    /// hand-craft an image whose telemetry decodes to a known result.
    fn line_with_wedge(wedge_value_a: u8, wedge_value_b: u8) -> [u8; LINE_PIXELS] {
        let mut pixels = [0_u8; LINE_PIXELS];
        for p in &mut pixels[TELEMETRY_A_START..TELEMETRY_A_END] {
            *p = wedge_value_a;
        }
        for p in &mut pixels[TELEMETRY_B_START..TELEMETRY_B_END] {
            *p = wedge_value_b;
        }
        pixels
    }

    /// Build a synthetic [`AptImage`] whose telemetry strips repeat the
    /// canonical 16-wedge frame `cycles` times, with the given channel-ID
    /// brightness on wedge 16 of each side. `frame_offset` shifts the
    /// frame start so we can verify sync detection at non-zero offsets.
    fn synth_image_with_frame(
        cycles: usize,
        wedge16_a: u8,
        wedge16_b: u8,
        frame_offset: usize,
    ) -> AptImage {
        let mut image = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);

        // Pre-roll: first `frame_offset` lines carry the wedge values that
        // *would* have come from the back of the previous frame, so the
        // sync detector sees a wrap-around it can lock onto.
        let line_total = cycles * FRAME_LINES + frame_offset;
        for i in 0..line_total {
            // Position within the conceptual frame (counting from frame
            // start, with the offset applied).
            let frame_pos = (i + (FRAME_LINES - frame_offset)) % FRAME_LINES;
            let wedge_idx = frame_pos / LINES_PER_WEDGE;
            let val_a = if wedge_idx < 8 {
                SPEC_GRAYSCALE_RAMP[wedge_idx]
            } else if wedge_idx == 15 {
                wedge16_a
            } else {
                // Wedges 9–15 (spacecraft telemetry): mid-grey, irrelevant
                // to the channel-ID test.
                128
            };
            let val_b = if wedge_idx < 8 {
                SPEC_GRAYSCALE_RAMP[wedge_idx]
            } else if wedge_idx == 15 {
                wedge16_b
            } else {
                128
            };
            let mut apt_line = AptLine {
                sync_quality: TEST_GOOD_QUALITY,
                ..AptLine::default()
            };
            apt_line.pixels = line_with_wedge(val_a, val_b);
            image.push_line(&apt_line, Instant::now());
        }
        image
    }

    #[test]
    fn line_telemetry_extracts_correct_pixel_ranges() {
        let mut pixels = [0_u8; LINE_PIXELS];
        // Paint a unique value into A only.
        for p in &mut pixels[TELEMETRY_A_START..TELEMETRY_A_END] {
            *p = 200;
        }
        // Paint a different value into B only.
        for p in &mut pixels[TELEMETRY_B_START..TELEMETRY_B_END] {
            *p = 50;
        }
        // Paint nonsense everywhere else.
        for p in &mut pixels[..TELEMETRY_A_START] {
            *p = 99;
        }
        for p in &mut pixels[TELEMETRY_A_END..TELEMETRY_B_START] {
            *p = 17;
        }
        assert_eq!(line_telemetry_a(&pixels), 200);
        assert_eq!(line_telemetry_b(&pixels), 50);
    }

    #[test]
    fn decode_telemetry_returns_none_for_short_image() {
        let image = synth_image_with_frame(0, 0, 0, 0); // 0 lines
        assert!(decode_telemetry(&image).is_none());

        // Even with 127 lines (one short of FRAME_LINES) we should refuse.
        let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
        for _ in 0..(FRAME_LINES - 1) {
            let mut line = AptLine {
                sync_quality: TEST_GOOD_QUALITY,
                ..AptLine::default()
            };
            line.pixels = line_with_wedge(31, 31);
            img.push_line(&line, Instant::now());
        }
        assert!(decode_telemetry(&img).is_none());
    }

    #[test]
    fn decode_telemetry_recovers_grayscale_ramp() {
        // 2 cycles of clean telemetry, no offset.
        let image = synth_image_with_frame(
            2,
            SPEC_GRAYSCALE_RAMP[1], // wedge 16 = wedge 2 → Channel 2 (Near-IR)
            SPEC_GRAYSCALE_RAMP[4], // wedge 16 = wedge 5 → Channel 4 (Thermal IR)
            0,
        );
        let result = decode_telemetry(&image).expect("two clean cycles is enough");

        // The decoded ramps should match the spec values within rounding.
        for (i, (&got, &expected)) in result
            .side_a
            .grayscale_ramp
            .iter()
            .zip(SPEC_GRAYSCALE_RAMP.iter())
            .enumerate()
        {
            assert_eq!(
                got, expected,
                "side_a wedge {i}: got {got}, expected {expected}"
            );
        }
        for (i, (&got, &expected)) in result
            .side_b
            .grayscale_ramp
            .iter()
            .zip(SPEC_GRAYSCALE_RAMP.iter())
            .enumerate()
        {
            assert_eq!(
                got, expected,
                "side_b wedge {i}: got {got}, expected {expected}"
            );
        }

        assert_eq!(result.side_a.channel_id, Some(AvhrrChannel::Ch2NearIr));
        assert_eq!(result.side_b.channel_id, Some(AvhrrChannel::Ch4ThermalIr));

        assert!(
            result.side_a.frame_sync_quality > 0.95,
            "expected near-perfect sync, got {:.3}",
            result.side_a.frame_sync_quality,
        );
        assert!(
            result.side_b.frame_sync_quality > 0.95,
            "expected near-perfect sync, got {:.3}",
            result.side_b.frame_sync_quality,
        );
    }

    #[test]
    fn frame_sync_locks_at_arbitrary_offset() {
        // Shift frame start by 37 lines. Decoder must still lock onto the
        // ramp and return the right channel ID even though "line 0" of
        // the buffer isn't the start of a frame.
        let image = synth_image_with_frame(3, SPEC_GRAYSCALE_RAMP[0], SPEC_GRAYSCALE_RAMP[5], 37);
        let result = decode_telemetry(&image).unwrap();
        assert_eq!(result.side_a.channel_id, Some(AvhrrChannel::Ch1Visible));
        assert_eq!(result.side_b.channel_id, Some(AvhrrChannel::Ch5ThermalIr));
        assert!(result.side_a.frame_sync_quality > 0.95);
    }

    #[test]
    fn channel_id_covers_all_six_avhrr_channels() {
        let cases = [
            (0, AvhrrChannel::Ch1Visible),
            (1, AvhrrChannel::Ch2NearIr),
            (2, AvhrrChannel::Ch3aShortwaveIr),
            (3, AvhrrChannel::Ch3bThermalIr),
            (4, AvhrrChannel::Ch4ThermalIr),
            (5, AvhrrChannel::Ch5ThermalIr),
        ];
        for (ramp_idx, expected) in cases {
            let wedge16 = SPEC_GRAYSCALE_RAMP[ramp_idx];
            let image = synth_image_with_frame(2, wedge16, wedge16, 0);
            let result = decode_telemetry(&image).unwrap();
            assert_eq!(
                result.side_a.channel_id,
                Some(expected),
                "wedge16={wedge16} (ramp idx {ramp_idx}) should map to {expected:?}",
            );
        }
    }

    #[test]
    fn channel_id_returns_none_when_ramp_is_flat() {
        // Flat-grey image: all telemetry pixels = same value, no ramp.
        // Classification must refuse rather than emit a bogus channel.
        let mut img = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);
        for _ in 0..(FRAME_LINES * 2) {
            let mut line = AptLine {
                sync_quality: TEST_GOOD_QUALITY,
                ..AptLine::default()
            };
            line.pixels = line_with_wedge(120, 120);
            img.push_line(&line, Instant::now());
        }
        let result = decode_telemetry(&img).unwrap();
        assert!(result.side_a.channel_id.is_none());
        assert!(result.side_b.channel_id.is_none());
    }

    #[test]
    fn frame_sync_quality_is_near_zero_for_random_input() {
        // Build a buffer of pseudo-random per-line telemetry averages
        // and confirm the correlation-based quality stays low — the
        // monotonic ramp template shouldn't lock onto noise.
        let mut state: u32 = 0xDEAD_BEEF;
        let mut avgs = vec![0_u8; FRAME_LINES * 2];
        for v in &mut avgs {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *v = ((state >> 16) & 0xff) as u8;
        }
        let side = decode_side(&avgs).unwrap();
        assert!(
            side.frame_sync_quality < 0.85,
            "noise should not yield strong sync, got {:.3}",
            side.frame_sync_quality,
        );
    }

    #[test]
    fn decode_side_returns_none_for_short_input() {
        let avgs = vec![0_u8; FRAME_LINES - 1];
        assert!(decode_side(&avgs).is_none());
    }

    #[test]
    fn pixel_position_constants_match_apt_spec() {
        // 45-pixel telemetry strips at the standard APT positions.
        assert_eq!(TELEMETRY_WIDTH, 45);
        assert_eq!(TELEMETRY_A_START, 995);
        assert_eq!(TELEMETRY_A_END, 1040);
        assert_eq!(TELEMETRY_B_START, 2035);
        assert_eq!(TELEMETRY_B_END, 2080);
        assert_eq!(FRAME_LINES, 128);
        assert_eq!(WEDGES_PER_FRAME * LINES_PER_WEDGE, FRAME_LINES);
    }
}
