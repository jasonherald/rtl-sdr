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
    /// 2080 8-bit greyscale pixels in transmission order, with
    /// per-line min/max normalization. For sub-threshold quality
    /// lines, this is all zeros (gap fill). Used by the live image
    /// viewer; PNG export ignores this in favor of `raw_samples`
    /// for image-wide brightness modes (per [`BrightnessMode`]).
    pub pixels: [u8; LINE_PIXELS],
    /// Raw f32 envelope samples — one per pixel, in the demodulator's
    /// native float scale (no normalization). The PNG-export pipeline
    /// (per `AptImage::finalize_grayscale`) re-normalizes these
    /// across the entire image at write time, with the brightness
    /// reference range chosen by [`BrightnessMode`].
    /// Sub-threshold quality lines get zero-filled here too, matching
    /// the gap-fill semantics of `pixels`.
    pub raw_samples: [f32; LINE_PIXELS],
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

/// Brightness-mapping mode used when rendering the assembled image
/// (typically at PNG-export time, when the entire pass's dynamic
/// range is known). Inspired by noaa-apt's `Contrast` enum;
/// reimplemented from first principles in our streaming model.
#[derive(Debug, Clone, Copy)]
pub enum BrightnessMode {
    /// Map the lowest sample in the image to 0 and the highest to 255.
    /// Naive — a single hot pixel from a noise spike sets the white
    /// level for the whole image and dims everything real. Useful as
    /// a fallback when nothing else is available.
    MinMax,
    /// Take a percentile band as the reference range, clipping the
    /// outermost `(1 − p) / 2` of samples on each side. The default
    /// `0.98` matches noaa-apt's `Contrast::Percent(0.98)` and
    /// rejects the kind of impulsive noise that breaks `MinMax`
    /// while preserving the image's full real dynamic range.
    Percentile(f32),
    /// Use the calibration grayscale ramp from the telemetry strips
    /// for absolute brightness mapping: telemetry wedge 9 = pure
    /// black (0), wedge 8 = pure white (255). Per noaa-apt's
    /// `Contrast::Telemetry`. Best fidelity when at least a couple
    /// of full telemetry frames have decoded; falls back to
    /// `Percentile(0.98)` when the calibration ramp is unavailable
    /// (short pass, telemetry not yet locked, or the wedge values
    /// aren't monotonic which would indicate a misaligned decode).
    TelemetryCalibrated,
    /// Per-channel histogram equalization (separately on Channel A
    /// and Channel B half-lines). Maximizes contrast at the cost of
    /// not preserving absolute brightness — features get pushed to
    /// the available 0..255 range regardless of their real
    /// radiometric value. Per noaa-apt's `Contrast::Histogram`.
    Histogram,
}

impl Default for BrightnessMode {
    /// Default to the 98% percentile, matching noaa-apt's standard
    /// profile and the most useful general-case mode (rejects
    /// impulsive noise while preserving the bulk dynamic range).
    fn default() -> Self {
        Self::Percentile(0.98)
    }
}

/// 2D image being assembled from a single NOAA APT satellite pass.
///
/// Lines accumulate in transmission order; the buffer never compresses
/// or rewrites past entries, so `lines()` is a stable view callers can
/// snapshot at any point during a live pass.
///
/// `Clone` is derived so the PNG-export path can snapshot the image
/// quickly on the GTK main thread, then move the snapshot into a
/// `gio::spawn_blocking` worker for the CPU-heavy
/// finalize/rotate/encode without holding the UI. The clone copies
/// every line's `pixels` + `raw_samples` (~10 KB/line), so a 1500-line
/// pass is ~15 MB — bigger than ideal but cheaper than freezing GTK
/// for the duration of the encode. Per CR round 1 on PR #571.
#[derive(Debug, Clone)]
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
    /// [`MIN_VALID_SYNC_QUALITY`], the stored pixels and `raw_samples`
    /// are replaced with zero rows — the original quality score is
    /// preserved verbatim so the renderer can still overlay a quality
    /// strip if it wants. This keeps a flapping squelch / sync-loss
    /// visible as a clean dark horizontal band rather than a strip of
    /// meaningless pixel noise. Both fields go to zero in lock-step
    /// so PNG export's image-wide normalization (per
    /// [`BrightnessMode`]) doesn't pull from a "gap" line's
    /// pre-gap-fill raw values.
    pub fn push_line(&mut self, line: &AptLine, received_at: Instant) {
        let valid = line.sync_quality >= MIN_VALID_SYNC_QUALITY;
        let pixels = if valid {
            line.pixels
        } else {
            [0_u8; LINE_PIXELS]
        };
        let raw_samples = if valid {
            line.raw_samples
        } else {
            [0.0_f32; LINE_PIXELS]
        };
        self.lines.push(AptImageLine {
            pixels,
            raw_samples,
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

    /// Render the assembled image to a flat row-major u8 buffer of
    /// dimensions `WIDTH × len()`, applying image-wide brightness
    /// mapping per the requested [`BrightnessMode`]. Used at PNG
    /// export time to produce a properly-calibrated final image
    /// rather than the per-line-normalized live preview.
    ///
    /// The returned `Vec<u8>` has length `WIDTH * len()` with each
    /// row of `WIDTH` pixels in transmission order. Rows for
    /// gap-filled lines (`sync_quality` < `MIN_VALID_SYNC_QUALITY`)
    /// are emitted as black regardless of mode — gap lines have no
    /// real signal to map.
    #[must_use]
    pub fn finalize_grayscale(&self, mode: BrightnessMode) -> Vec<u8> {
        let pass_through_quality = self
            .lines
            .iter()
            .map(|l| l.sync_quality >= MIN_VALID_SYNC_QUALITY);

        // Compute the (low, high) brightness reference range
        // applicable to the whole image. Each mode picks a different
        // (low, high) pair; the actual u8 mapping below is identical.
        // Histogram equalization is special-cased because it doesn't
        // map by a single (low, high) range.
        let mut out = Vec::with_capacity(Self::WIDTH * self.lines.len());
        match mode {
            BrightnessMode::MinMax => {
                let (low, high) = self.signal_min_max();
                self.append_with_range(&mut out, low, high, pass_through_quality);
            }
            BrightnessMode::Percentile(p) => {
                let (low, high) = self.signal_percentile(p);
                self.append_with_range(&mut out, low, high, pass_through_quality);
            }
            BrightnessMode::TelemetryCalibrated => {
                if let Some((low, high)) = self.telemetry_calibration_range() {
                    self.append_with_range(&mut out, low, high, pass_through_quality);
                } else {
                    // Telemetry not available / not monotonic — fall
                    // back to the 98% percentile (the sensible
                    // general-case mode and what noaa-apt's standard
                    // profile uses pre-color).
                    let (low, high) = self.signal_percentile(0.98);
                    self.append_with_range(&mut out, low, high, pass_through_quality);
                }
            }
            BrightnessMode::Histogram => {
                self.append_histogram_equalized(&mut out);
            }
        }
        out
    }

    /// Min and max across all valid (above-threshold) raw samples.
    /// Returns `(0.0, 0.0)` if the image has no valid lines —
    /// caller's downstream divide-by-range handles that with a
    /// `max(EPS)` guard.
    fn signal_min_max(&self) -> (f32, f32) {
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for line in &self.lines {
            if line.sync_quality < MIN_VALID_SYNC_QUALITY {
                continue;
            }
            for &v in &line.raw_samples {
                if v < lo {
                    lo = v;
                }
                if v > hi {
                    hi = v;
                }
            }
        }
        if lo.is_infinite() {
            (0.0, 0.0)
        } else {
            (lo, hi)
        }
    }

    /// Percentile-based reference range.
    ///
    /// Takes the central `p` fraction of valid samples — clipping
    /// `(1 − p) / 2` of the lowest and highest. Reimplemented from
    /// noaa-apt's `misc::percent`; uses `select_nth_unstable` for
    /// O(n) average-case selection (vs. their full-sort O(n log n)).
    fn signal_percentile(&self, p: f32) -> (f32, f32) {
        let p = p.clamp(0.0, 1.0);
        // Gather all valid samples. ~16 MB for a typical 15-min pass
        // — large but transient (drops as soon as we return).
        let mut samples: Vec<f32> = Vec::new();
        for line in &self.lines {
            if line.sync_quality < MIN_VALID_SYNC_QUALITY {
                continue;
            }
            samples.extend_from_slice(&line.raw_samples);
        }
        if samples.is_empty() {
            return (0.0, 0.0);
        }
        let n = samples.len();
        // (1 - p) / 2 fraction trimmed off each end.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            reason = "n is usize, p in [0,1] → tail in [0, n/2], fits usize"
        )]
        let tail = (((1.0 - p) / 2.0) * n as f32) as usize;
        let lo_idx = tail.min(n.saturating_sub(1));
        let hi_idx = (n - 1).saturating_sub(tail);
        let (_, lo, _) = samples.select_nth_unstable_by(lo_idx, |a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        let lo_v = *lo;
        let (_, hi, _) = samples.select_nth_unstable_by(hi_idx, |a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        (lo_v, *hi)
    }

    /// Telemetry-calibrated reference range. Reads the calibration
    /// grayscale ramp from the telemetry strips: wedge 9 = black,
    /// wedge 8 = white. Returns `None` if telemetry hasn't been
    /// decoded (channel ids unset) or if the implied range is
    /// degenerate (high <= low, indicating a misaligned decode).
    ///
    /// Currently a placeholder returning `None` — the wedge-value
    /// extraction lives in `apt_telemetry.rs`, but exposing it
    /// requires plumbing the per-frame wedge values through the
    /// `AptTelemetry` result. Tracked as a B1 follow-up; for now the
    /// `TelemetryCalibrated` mode falls back to `Percentile(0.98)`,
    /// which is what noaa-apt does when telemetry decode fails too.
    #[allow(
        clippy::unused_self,
        reason = "placeholder — full impl wires telemetry decode through `&self`"
    )]
    fn telemetry_calibration_range(&self) -> Option<(f32, f32)> {
        // TODO(#566 follow-up): decode telemetry on this image's
        // `raw_samples` + line layout, return wedge-9 / wedge-8 means
        // as the (low, high) reference. The current
        // `apt_telemetry::decode_apt_telemetry` API operates on the
        // u8 `pixels` and returns an `AptTelemetry` with classified
        // channel ids — it doesn't expose the calibration float
        // values. Wiring is straightforward but out of scope for
        // the initial parity PR; falling back to percentile
        // matches noaa-apt's behavior when telemetry isn't usable.
        None
    }

    /// Append rows to `out`, mapping `raw_samples` from `[low, high]`
    /// to `[0, 255]`. Gap lines (sync below threshold) emit as black.
    /// Implementation factored so all the (low, high)-driven modes
    /// share the same hot loop.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn append_with_range(
        &self,
        out: &mut Vec<u8>,
        low: f32,
        high: f32,
        valid_iter: impl IntoIterator<Item = bool>,
    ) {
        let range = (high - low).max(1e-9);
        for (line, valid) in self.lines.iter().zip(valid_iter) {
            if !valid {
                out.extend(core::iter::repeat_n(0_u8, Self::WIDTH));
                continue;
            }
            for &v in &line.raw_samples {
                let norm = ((v - low) / range).clamp(0.0, 1.0);
                out.push((norm * 255.0).round() as u8);
            }
        }
    }

    /// Histogram-equalize per-channel (Channel A + Channel B
    /// separately, matching noaa-apt's `processing::histogram_equalization`).
    /// First runs a 98% percentile clip to map `raw_samples` to u8,
    /// then equalizes the u8 histogram of each half independently.
    /// The `pixels` and `raw_samples` aren't used directly here —
    /// we re-derive a baseline u8 image from the percentile-clipped
    /// `raw_samples` to ensure consistency with the other modes.
    fn append_histogram_equalized(&self, out: &mut Vec<u8>) {
        // Step 1: build a baseline u8 image from percentile-clipped
        // `raw_samples`. This is the same starting point as
        // `BrightnessMode::Percentile(0.98)`.
        let (low, high) = self.signal_percentile(0.98);
        let pass_through_quality: Vec<bool> = self
            .lines
            .iter()
            .map(|l| l.sync_quality >= MIN_VALID_SYNC_QUALITY)
            .collect();
        let mut baseline = Vec::with_capacity(Self::WIDTH * self.lines.len());
        self.append_with_range(
            &mut baseline,
            low,
            high,
            pass_through_quality.iter().copied(),
        );

        // Step 2: equalize each half separately. Channel A is the
        // first WIDTH/2 pixels of each row; Channel B is the
        // remainder. Compute per-channel histogram across all rows,
        // build the equalization LUT, apply.
        //
        // Pass `pass_through_quality` so the histogram + LUT are
        // built from VALID rows only — gap-filled rows would
        // otherwise dump a giant pile of 0s into bin 0, dragging
        // the CDF and remapping `lut[0]` to a non-zero gray. Skipped
        // rows are explicitly re-zeroed after the LUT pass to
        // preserve the `finalize_grayscale` contract that gap rows
        // stay black regardless of mode. Per CR round 1 on PR #571.
        let height = self.lines.len();
        let half = Self::WIDTH / 2;
        equalize_channel_in_place(&mut baseline, height, 0, half, &pass_through_quality);
        equalize_channel_in_place(
            &mut baseline,
            height,
            half,
            Self::WIDTH,
            &pass_through_quality,
        );

        out.extend(baseline);
    }
}

/// Rotate the two video-data sub-rectangles of a finalized APT
/// grayscale buffer 180° in place. Use for ascending passes (heading
/// north), which transmit lines south-to-north so the assembled
/// image is upside-down AND mirrored east-west.
///
/// **Layout assumption:** the input buffer is row-major with
/// `AptImage::WIDTH` (= 2080) pixels per row, exactly as produced by
/// [`AptImage::finalize_grayscale`]. The two video sub-rectangles
/// rotated are:
///
/// * Channel A video: cols `[86, 86+909)` (= post-Sync-A,
///   post-Space-A, pre-Telemetry-A)
/// * Channel B video: cols `[86+1040, 86+1040+909)` (same offset
///   into the Channel B half)
///
/// Sync / Space / Telemetry strips are **left untouched** so the
/// telemetry-wedge calibration stays in its original position. This
/// matches noaa-apt's `processing::rotate` behavior: rotating the
/// whole image would scramble the wedge order, breaking
/// telemetry-based contrast calibration.
/// Pixels of "space" between Sync A and Channel A video data.
/// 47 px per noaa-apt's `PX_SPACE_DATA` + spec.
const PX_SPACE_DATA: usize = 47;
/// Pixels of channel video data per channel (909 px) — between
/// the space and telemetry strips.
const PX_CHANNEL_IMAGE_DATA: usize = 909;
/// Half-line stride: 2080 / 2 = 1040 px (= `Sync_A` + `Space_A` +
/// `Video_A` + `Telemetry_A` = 39 + 47 + 909 + 45).
const PX_PER_CHANNEL: usize = 1040;

pub fn rotate_180_per_channel(image: &mut [u8], height: usize) {
    use sdr_dsp::apt::SYNC_A_TOTAL_PX;

    if image.len() != AptImage::WIDTH * height {
        return; // defensive — caller violated the layout contract
    }
    let video_start_a = SYNC_A_TOTAL_PX + PX_SPACE_DATA; // 39 + 47 = 86
    rotate_rectangle_180_in_place(
        image,
        height,
        video_start_a,
        video_start_a + PX_CHANNEL_IMAGE_DATA,
    );
    let video_start_b = video_start_a + PX_PER_CHANNEL;
    rotate_rectangle_180_in_place(
        image,
        height,
        video_start_b,
        video_start_b + PX_CHANNEL_IMAGE_DATA,
    );
}

/// Rotate a column-band sub-rectangle of a row-major image 180° in
/// place. Used by [`rotate_180_per_channel`].
///
/// `image` is row-major with `AptImage::WIDTH` pixels per row.
/// Pixels in `[col_start, col_end)` of every row are rotated 180°
/// (= vertical flip + horizontal mirror within the band). Other
/// columns are untouched.
fn rotate_rectangle_180_in_place(
    image: &mut [u8],
    height: usize,
    col_start: usize,
    col_end: usize,
) {
    let width = AptImage::WIDTH;
    let n_cols = col_end - col_start;
    if n_cols == 0 || height == 0 {
        return;
    }
    // Pair up rows from the outside in, swapping mirrored columns.
    // The middle row (when height is odd) gets reversed in place
    // separately below.
    for row_a in 0..(height / 2) {
        let row_b = height - 1 - row_a;
        for off in 0..n_cols {
            let col_a = col_start + off;
            let col_b = col_start + (n_cols - 1 - off);
            let i_a = row_a * width + col_a;
            let i_b = row_b * width + col_b;
            image.swap(i_a, i_b);
        }
    }
    if height.is_multiple_of(2) {
        return;
    }
    // Odd height: middle row needs an in-place reverse of the band.
    let mid_row = height / 2;
    let row_start = mid_row * width;
    let mut left = col_start;
    let mut right = col_end - 1;
    while left < right {
        image.swap(row_start + left, row_start + right);
        left += 1;
        right -= 1;
    }
}

/// Histogram-equalize one half of the image in place, ignoring
/// gap-filled rows.
///
/// `image` is row-major with `WIDTH` pixels per row, `height` rows.
/// Pixels in columns `[col_start, col_end)` are equalized; the rest
/// of the row is untouched. Standard CDF-based equalization.
///
/// `valid_row[i]` is true for rows whose `sync_quality` cleared
/// [`MIN_VALID_SYNC_QUALITY`]. Rows where it's false are excluded
/// from the histogram (so the CDF reflects real signal only) AND
/// re-zeroed after equalization (so a non-zero `lut[0]` doesn't
/// turn a gap row gray). Per CR round 1 on PR #571.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn equalize_channel_in_place(
    image: &mut [u8],
    height: usize,
    col_start: usize,
    col_end: usize,
    valid_row: &[bool],
) {
    let width = AptImage::WIDTH;
    debug_assert_eq!(valid_row.len(), height);
    // Histogram of u8 values across the half-image, valid rows only.
    let mut hist = [0_u32; 256];
    let mut valid_pixel_count = 0_usize;
    for row in 0..height {
        if !valid_row.get(row).copied().unwrap_or(false) {
            continue;
        }
        let row_start = row * width;
        for col in col_start..col_end {
            hist[image[row_start + col] as usize] += 1;
        }
        valid_pixel_count += col_end - col_start;
    }
    if valid_pixel_count == 0 {
        // No valid rows in this half — leave the half untouched.
        // Gap rows already came in zeroed; there's nothing to remap.
        return;
    }
    // Cumulative distribution, then equalization LUT.
    let mut lut = [0_u8; 256];
    let mut cumulative = 0_u32;
    #[allow(
        clippy::cast_precision_loss,
        reason = "valid_pixel_count = (col_end - col_start) × valid_row_count — \
                  bounded by WIDTH × MAX_LINES (~3 M), well inside f32 mantissa."
    )]
    let denom = valid_pixel_count as f32;
    for (i, &count) in hist.iter().enumerate() {
        cumulative += count;
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "CDF · 255 is in [0, 255]; clamp pins it before the cast"
        )]
        let v = ((cumulative as f32 / denom) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
        lut[i] = v;
    }
    // Apply on valid rows only; re-zero invalid rows so a non-zero
    // `lut[0]` doesn't turn the gap-fill gray.
    for row in 0..height {
        let row_start = row * width;
        if valid_row.get(row).copied().unwrap_or(false) {
            for col in col_start..col_end {
                let idx = row_start + col;
                image[idx] = lut[image[idx] as usize];
            }
        } else {
            for col in col_start..col_end {
                image[row_start + col] = 0;
            }
        }
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
}
