//! Assembled NOAA APT scan-line image.
//!
//! Sits one stage downstream of [`sdr_dsp::apt::AptDecoder`]: takes the
//! stream of `AptLine` events the decoder emits and stitches them into a
//! growing 2D greyscale image (the actual picture).
//!
//! Pure data structure — no threading, no audio I/O, no DSP. The caller
//! (typically the radio or UI layer) owns the lifecycle:
//!
//! ```text
//!     AptDecoder ──[AptLine]──▶  AptImage::push_line  ──▶  AptImage::lines()
//!                                                          │
//!                                                          ▼
//!                                              live viewer / PNG export
//! ```
//!
//! One `AptImage` corresponds to **one satellite pass**. Starting a fresh
//! pass = constructing a fresh `AptImage`; finalizing = stop pushing
//! lines and use the existing snapshot for export. Pass detection itself
//! (auto-record on overhead pass) lives in #482; this module is just the
//! buffer.

use std::time::Instant;

use sdr_dsp::apt::{AptLine, LINE_PIXELS};

/// Sync-quality threshold at or above which a decoded line is treated as
/// trustworthy and shown verbatim. Anything below this is replaced with
/// a black row so dropouts read as obvious gaps in the resulting image
/// instead of corrupting it with noise.
///
/// Empirically the sync detector scores clean templates near 1.0, pure
/// noise at 0.3–0.4, and partial sync at 0.6–0.8. 0.5 sits comfortably
/// in the "definitely better than noise" band without being so strict
/// that fading edges drop out unnecessarily.
pub const MIN_VALID_SYNC_QUALITY: f32 = 0.5;

/// Default number of lines to pre-reserve in the image buffer. NOAA APT
/// passes are typically 12–15 minutes; at 2 lines/sec, 1800 lines covers
/// the upper end without reallocating mid-pass. Longer passes still
/// work — Vec just resizes — but the common case is alloc-free.
pub const DEFAULT_MAX_LINES: usize = 1_800;

/// AVHRR channel identifier as decoded from the wedge-16 telemetry slot.
///
/// NOAA APT transmits two of the AVHRR's six channels at any given
/// moment — typically Ch2 (visible / near-IR) on the A side and Ch4
/// (thermal IR) on the B side, but the exact pair varies with day /
/// night and operational mode. Wedge-16 of each scan line's telemetry
/// strip identifies which channel is on which side.
///
/// Decoding the wedge into this enum is the job of #479; this module
/// just provides the slot. Channels stay `None` until that ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvhrrChannel {
    /// Channel 1: visible (0.58–0.68 µm). Daytime visible-light imagery.
    Ch1Visible,
    /// Channel 2: near-IR (0.725–1.00 µm). Daytime, vegetation-sensitive.
    Ch2NearIr,
    /// Channel 3A: shortwave IR (1.58–1.64 µm). Daytime cloud / snow.
    Ch3aShortwaveIr,
    /// Channel 3B: thermal IR (3.55–3.93 µm). Nighttime cloud / fire.
    Ch3bThermalIr,
    /// Channel 4: thermal IR (10.30–11.30 µm). Sea-surface temperature.
    Ch4ThermalIr,
    /// Channel 5: thermal IR (11.50–12.50 µm). Cloud-top temperature.
    Ch5ThermalIr,
}

/// One scan line in an [`AptImage`], with the timing + quality metadata
/// the renderer needs.
#[derive(Debug, Clone)]
pub struct AptImageLine {
    /// 2080 8-bit greyscale pixels in transmission order. For sub-threshold
    /// quality lines, this is all zeros (gap fill).
    pub pixels: [u8; LINE_PIXELS],
    /// The decoder's normalized cross-correlation score for this line's
    /// sync burst (`[0.0, 1.0]`). Preserved verbatim from the source
    /// `AptLine` even when `pixels` was gap-filled, so a UI overlay can
    /// still surface a quality strip alongside the image.
    pub sync_quality: f32,
    /// Wall-clock instant the line was added to the image. Set by the
    /// caller (typically `Instant::now()`); supplied as a parameter
    /// rather than read internally so the type stays test-friendly.
    pub received_at: Instant,
}

/// 2D image being assembled from a single NOAA APT satellite pass.
///
/// Lines accumulate in transmission order; the buffer never compresses
/// or rewrites past entries, so `lines()` is a stable view callers can
/// snapshot at any point during a live pass.
#[derive(Debug)]
pub struct AptImage {
    lines: Vec<AptImageLine>,
    pass_start: Instant,
    channel_a_id: Option<AvhrrChannel>,
    channel_b_id: Option<AvhrrChannel>,
}

impl AptImage {
    /// Number of pixels per line — the full APT scan width. Both halves
    /// (Channel A + Channel B + their telemetry strips) are included.
    pub const WIDTH: usize = LINE_PIXELS;

    /// Start a fresh pass with the default ~15-minute capacity. Use
    /// [`AptImage::with_capacity`] if you want to tune the upper bound
    /// (e.g. for a higher-elevation pass or for tests).
    #[must_use]
    pub fn new(pass_start: Instant) -> Self {
        Self::with_capacity(pass_start, DEFAULT_MAX_LINES)
    }

    /// Start a fresh pass, pre-reserving room for `max_lines` scan lines.
    /// Pushing past `max_lines` still works, but additional growth may
    /// trigger one or more Vec reallocations as it doubles to fit.
    #[must_use]
    pub fn with_capacity(pass_start: Instant, max_lines: usize) -> Self {
        Self {
            lines: Vec::with_capacity(max_lines),
            pass_start,
            channel_a_id: None,
            channel_b_id: None,
        }
    }

    /// Append a decoded scan line.
    ///
    /// If the source line's `sync_quality` is below
    /// [`MIN_VALID_SYNC_QUALITY`], the stored pixels are replaced with
    /// a black row — the original quality score is preserved verbatim
    /// so the renderer can still overlay a quality strip if it wants.
    /// This keeps a flapping squelch / sync-loss visible as a clean dark
    /// horizontal band rather than a strip of meaningless pixel noise.
    pub fn push_line(&mut self, line: &AptLine, received_at: Instant) {
        let pixels = if line.sync_quality >= MIN_VALID_SYNC_QUALITY {
            line.pixels
        } else {
            [0_u8; LINE_PIXELS]
        };
        self.lines.push(AptImageLine {
            pixels,
            sync_quality: line.sync_quality,
            received_at,
        });
    }

    /// All lines in transmission order. Cheap reference — the renderer
    /// can snapshot at any time without copying.
    #[must_use]
    pub fn lines(&self) -> &[AptImageLine] {
        &self.lines
    }

    /// Number of lines currently in the image.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// `true` if no lines have been pushed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Wall-clock instant the pass began (set at construction).
    #[must_use]
    pub fn pass_start(&self) -> Instant {
        self.pass_start
    }

    /// Identifier of the channel transmitted on the A side of the line,
    /// or `None` until telemetry decode (#479) fills it in.
    #[must_use]
    pub fn channel_a_id(&self) -> Option<AvhrrChannel> {
        self.channel_a_id
    }

    /// Identifier of the channel transmitted on the B side of the line,
    /// or `None` until telemetry decode (#479) fills it in.
    #[must_use]
    pub fn channel_b_id(&self) -> Option<AvhrrChannel> {
        self.channel_b_id
    }

    /// Set / overwrite the channel A identifier. Called by telemetry
    /// decode (#479) once it has accumulated enough wedge-16 frames.
    pub fn set_channel_a_id(&mut self, channel: AvhrrChannel) {
        self.channel_a_id = Some(channel);
    }

    /// Set / overwrite the channel B identifier. Called by telemetry
    /// decode (#479) once it has accumulated enough wedge-16 frames.
    pub fn set_channel_b_id(&mut self, channel: AvhrrChannel) {
        self.channel_b_id = Some(channel);
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::cast_possible_truncation)]
mod tests {
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
    fn synth_line(quality: f32) -> AptLine {
        let mut line = AptLine {
            sync_quality: quality,
            ..AptLine::default()
        };
        for (i, p) in line.pixels.iter_mut().enumerate() {
            *p = (i % TEST_PIXEL_PATTERN_MODULUS) as u8;
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
}
