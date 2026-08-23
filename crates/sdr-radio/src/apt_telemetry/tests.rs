use super::*;
use sdr_dsp::apt::AptLine;
use std::time::Instant;

// ─── Fixture constants ────────────────────────────────────────────
//
// Hoisted so the same load-bearing values can be retuned in one
// place if upstream design parameters change, and so future readers
// don't have to re-derive what e.g. "0.95" means in context.

/// Tight pre-allocation for [`AptImage`] in tests — well under
/// [`crate::apt_image::DEFAULT_MAX_LINES`].
const TEST_MAX_LINES: usize = 256;

/// Quality value high enough to clear the [`AptImage`] gap-fill threshold.
const TEST_GOOD_QUALITY: f32 = 0.92;

/// Frame-start offset used by the arbitrary-offset sync test. Picked
/// to be relatively prime to [`LINES_PER_WEDGE`] (8) so the offset
/// can't accidentally align with a wedge boundary and pass for
/// trivial reasons.
const TEST_FRAME_OFFSET: usize = 37;

/// Mid-grey value painted across an entire image to exercise the
/// "flat ramp, refuse to classify" branch. Anything in the middle
/// of the u8 range works — 120 just keeps it visibly distinct from
/// the spec ramp's actual values.
const TEST_FLAT_GREY: u8 = 120;

/// Mid-grey wedge value used as a placeholder for spacecraft-
/// telemetry wedges 9–15 in the synthetic-frame builder. The
/// channel-ID test is insensitive to this exact value.
const TEST_PLACEHOLDER_WEDGE: u8 = 128;

/// Quality threshold for "near-perfect frame sync" assertions.
/// At this threshold the decoded ramp matches the spec template
/// to within line-rounding noise.
const TEST_GOOD_SYNC_QUALITY: f32 = 0.95;

/// Upper bound on `frame_sync_quality` for pseudo-random input.
/// Random data shouldn't be able to fake the spec ramp's specific
/// 8-step shape past this threshold.
const TEST_NOISE_SYNC_CEILING: f32 = 0.85;

/// LCG seed for the noise-cycle test. Picked so the resulting noise
/// pattern doesn't accidentally correlate with the spec ramp.
const LCG_SEED_NOISE_CYCLE: u32 = 0x00C0_FFEE;
/// LCG seed for the random-input frame-sync ceiling test.
const LCG_SEED_RANDOM: u32 = 0xDEAD_BEEF;
/// BSD libc's well-known LCG multiplier and increment. Notoriously
/// poor as a real RNG but plenty unstructured for "no-pattern" test
/// inputs, and pulling in a `rand` dep just for these tests would
/// be overkill.
const LCG_MULTIPLIER: u32 = 1_103_515_245;
const LCG_INCREMENT: u32 = 12_345;

/// Tiny LCG step used by the noise tests. Returns a u8 sample by
/// taking the middle bits of the new state — same byte distribution
/// as `(state >> 16) & 0xff`. Shared between the noise-cycle test
/// and the random-input ceiling test so we don't dup the prime
/// constants in two places.
fn lcg_step(state: &mut u32) -> u8 {
    *state = state
        .wrapping_mul(LCG_MULTIPLIER)
        .wrapping_add(LCG_INCREMENT);
    ((*state >> 16) & 0xff) as u8
}

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
            TEST_PLACEHOLDER_WEDGE
        };
        let val_b = if wedge_idx < 8 {
            SPEC_GRAYSCALE_RAMP[wedge_idx]
        } else if wedge_idx == 15 {
            wedge16_b
        } else {
            TEST_PLACEHOLDER_WEDGE
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
    assert_eq!(result.side_b.channel_id, Some(AvhrrChannel::Ch5ThermalIr));

    assert!(
        result.side_a.frame_sync_quality > TEST_GOOD_SYNC_QUALITY,
        "expected near-perfect sync, got {:.3}",
        result.side_a.frame_sync_quality,
    );
    assert!(
        result.side_b.frame_sync_quality > TEST_GOOD_SYNC_QUALITY,
        "expected near-perfect sync, got {:.3}",
        result.side_b.frame_sync_quality,
    );
}

#[test]
fn frame_sync_locks_at_arbitrary_offset() {
    // Shift frame start by TEST_FRAME_OFFSET lines. Decoder must still lock
    // onto the ramp and return the right channel ID even though "line 0"
    // of the buffer isn't the start of a frame.
    let image = synth_image_with_frame(
        3,
        SPEC_GRAYSCALE_RAMP[0],
        SPEC_GRAYSCALE_RAMP[5],
        TEST_FRAME_OFFSET,
    );
    let result = decode_telemetry(&image).unwrap();
    assert_eq!(result.side_a.channel_id, Some(AvhrrChannel::Ch1Visible));
    assert_eq!(result.side_b.channel_id, Some(AvhrrChannel::Ch3bThermalIr));
    assert!(result.side_a.frame_sync_quality > TEST_GOOD_SYNC_QUALITY);
}

#[test]
fn frame_sync_prefers_clean_cycle_over_noisy_earlier_cycle() {
    // Build a 3-cycle synthetic image, then deliberately corrupt the
    // first cycle's calibration ramp by overwriting the per-line
    // pixels with a pseudo-random pattern. The decoder should still
    // emit the right channel ID — it must scan past the noisy first
    // cycle and lock onto the clean second cycle, not just pick the
    // best phase within the first 128 lines.
    let mut image = synth_image_with_frame(
        3,
        SPEC_GRAYSCALE_RAMP[2], // wedge16 = wedge3 → Ch3a Shortwave IR
        SPEC_GRAYSCALE_RAMP[2],
        0,
    );

    // Corrupt the first cycle's lines in place: replace each line's
    // pixels with a deterministic noise pattern. We can't mutate
    // `AptImage`'s lines() through its public API (sealed by design),
    // so reconstruct: take the second/third cycles verbatim and
    // prepend a fresh noise cycle.
    let clean_lines: Vec<_> = image.lines().iter().skip(FRAME_LINES).cloned().collect();
    image = AptImage::with_capacity(Instant::now(), TEST_MAX_LINES);

    let mut state: u32 = LCG_SEED_NOISE_CYCLE;
    for _ in 0..FRAME_LINES {
        let noise_byte = lcg_step(&mut state);
        let mut noisy_pixels = [0_u8; LINE_PIXELS];
        for p in &mut noisy_pixels[TELEMETRY_A_START..TELEMETRY_A_END] {
            *p = noise_byte;
        }
        for p in &mut noisy_pixels[TELEMETRY_B_START..TELEMETRY_B_END] {
            *p = noise_byte;
        }
        let mut line = AptLine {
            sync_quality: TEST_GOOD_QUALITY,
            ..AptLine::default()
        };
        line.pixels = noisy_pixels;
        image.push_line(&line, Instant::now());
    }
    for clean in clean_lines {
        let mut line = AptLine {
            sync_quality: clean.sync_quality,
            ..AptLine::default()
        };
        line.pixels = clean.pixels;
        image.push_line(&line, Instant::now());
    }

    let result = decode_telemetry(&image).expect("two clean cycles is enough");
    assert_eq!(
        result.side_a.channel_id,
        Some(AvhrrChannel::Ch3aShortwaveIr)
    );
    assert_eq!(
        result.side_b.channel_id,
        Some(AvhrrChannel::Ch3aShortwaveIr)
    );
    assert!(
        result.side_a.frame_sync_quality > TEST_GOOD_SYNC_QUALITY,
        "should lock onto the clean cycle past the noisy one, got {:.3}",
        result.side_a.frame_sync_quality,
    );
}

#[test]
fn channel_id_covers_all_six_avhrr_channels() {
    let cases = [
        (0, AvhrrChannel::Ch1Visible),
        (1, AvhrrChannel::Ch2NearIr),
        (2, AvhrrChannel::Ch3aShortwaveIr),
        (3, AvhrrChannel::Ch4ThermalIr),
        (4, AvhrrChannel::Ch5ThermalIr),
        (5, AvhrrChannel::Ch3bThermalIr),
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
        line.pixels = line_with_wedge(TEST_FLAT_GREY, TEST_FLAT_GREY);
        img.push_line(&line, Instant::now());
    }
    let result = decode_telemetry(&img).unwrap();
    assert!(result.side_a.channel_id.is_none());
    assert!(result.side_b.channel_id.is_none());
}

#[test]
fn channel_id_returns_none_for_wedge16_off_the_ramp() {
    // Calibration ramp decodes correctly, but wedge 16 lands in
    // off-the-ramp territory: no spec channel encodes a value >
    // wedge[5]=159, so a wedge16 of 250 is ~91 units from the
    // nearest channel-bearing wedge — way past
    // MAX_CHANNEL_MATCH_DISTANCE. Classification must refuse
    // rather than guess at the closest wedge in range. (Adjacent
    // wedges are only 23–32 apart, so values strictly *between*
    // wedges still fall within the threshold and do classify;
    // the threshold is specifically a guard against off-end /
    // non-spec wedge16 values.)
    let off_ramp_value = 250_u8;
    let image = synth_image_with_frame(2, off_ramp_value, off_ramp_value, 0);
    let result = decode_telemetry(&image).unwrap();
    assert!(
        result.side_a.channel_id.is_none(),
        "wedge16={off_ramp_value} is past the channel range, must not classify",
    );
    assert!(result.side_b.channel_id.is_none());
    // But the ramp itself decoded fine — sync quality is high.
    assert!(result.side_a.frame_sync_quality > TEST_GOOD_SYNC_QUALITY);
}

#[test]
fn frame_sync_quality_is_near_zero_for_random_input() {
    // Build a buffer of pseudo-random per-line telemetry averages
    // and confirm the correlation-based quality stays low — the
    // monotonic ramp template shouldn't lock onto noise.
    let mut state: u32 = LCG_SEED_RANDOM;
    let mut avgs = vec![0_u8; FRAME_LINES * 2];
    for v in &mut avgs {
        *v = lcg_step(&mut state);
    }
    let side = decode_side(&avgs).unwrap();
    assert!(
        side.frame_sync_quality < TEST_NOISE_SYNC_CEILING,
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

/// #735 — NOAA KLM wedge-16 channel IDs: 1 = Ch1, 2 = Ch2, 3 = `Ch3A`,
/// 4 = Ch4, 5 = Ch5, 6 = `Ch3B`. The old table had 4→3B, 5→4, 6→5,
/// mislabelling every night pass.
#[test]
fn wedge_channel_ids_follow_noaa_klm() {
    let expected = [
        AvhrrChannel::Ch1Visible,
        AvhrrChannel::Ch2NearIr,
        AvhrrChannel::Ch3aShortwaveIr,
        AvhrrChannel::Ch4ThermalIr,
        AvhrrChannel::Ch5ThermalIr,
        AvhrrChannel::Ch3bThermalIr,
    ];
    for (wedge_idx, channel) in expected.iter().enumerate() {
        assert_eq!(
            classify_channel_wedge(SPEC_GRAYSCALE_RAMP[wedge_idx], SPEC_GRAYSCALE_RAMP),
            Some(*channel),
            "wedge {}",
            wedge_idx + 1
        );
    }
}
