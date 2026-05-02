//! Shared SSTV image handle: bridges `sstv_decode_tap` (writer)
//! and `sstv_viewer` (reader). Mirrors [`crate::lrpt_image::LrptImage`]'s
//! `Arc<Mutex<Inner>>` shape but holds a single growing image at a time.
//!
//! slowrx emits one image per VIS detection; ARISS events typically
//! send ~12 images per pass, so the consumer copies + clears between
//! detections via [`SstvImageHandle::take_completed`].
//!
//! ```text
//!     SstvDecoder ──[per-line events]──▶  SstvImageHandle (writer)
//!                                               ▲
//!                                               │ (Arc<Mutex<Inner>>)
//!                                               │
//!                                         SstvImageHandle
//!                                         │            │
//!                                         ▼            ▼
//!                                   live viewer    LOS PNG export
//! ```
//!
//! One [`SstvImage`] corresponds to **one satellite pass**. A fresh pass =
//! constructing a fresh handle (or calling the existing one's clear path
//! via [`SstvImageHandle::take_completed`]); the inner buffer automatically
//! resets after each [`SstvEvent::ImageComplete`].

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing::warn;

/// Per-line SSTV mode information carried alongside pixel data.
/// Stored with the in-flight image so the viewer and save path can
/// read it without going back to the decoder.
#[derive(Debug, Clone, Copy)]
pub struct SstvModeInfo {
    /// Pixel width of the image this mode produces.
    pub width: u32,
    /// Total number of scan lines the mode produces.
    pub height: u32,
}

/// Inner mutable state for the shared SSTV image buffer.
struct Inner {
    /// Pixel width of the current image (set on first `write_line`).
    width: u32,
    /// Total number of expected scan lines (set on first `write_line`).
    height: u32,
    /// Row-major RGB pixels, length = `width * height` when complete.
    /// Partially-filled during a live pass.
    pixels: Vec<[u8; 3]>,
    /// Number of lines written so far.
    lines_written: u32,
}

impl Inner {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            lines_written: 0,
        }
    }

    /// True if no lines have been written yet.
    fn is_empty(&self) -> bool {
        self.lines_written == 0
    }

    /// Reset to empty state, ready for a new image.
    fn clear(&mut self) {
        self.width = 0;
        self.height = 0;
        self.pixels.clear();
        self.lines_written = 0;
    }

    /// Initialise buffer dimensions on the first line of a new image.
    fn init_if_needed(&mut self, width: u32, height: u32) {
        if self.width == 0 || self.height == 0 {
            self.width = width;
            self.height = height;
            self.pixels = vec![[0, 0, 0]; (width * height) as usize];
        }
    }

    /// Copy one scan line's pixels into the buffer.
    ///
    /// Duplicate or out-of-order writes are idempotent on the
    /// pixel data and do not advance the counter past
    /// `line_index + 1`, preventing `lines_written` from
    /// drifting above `height` even when slowrx re-emits a
    /// row (rare edge case observed on noisy signals).
    fn write_line(&mut self, line_index: u32, pixels: &[[u8; 3]]) {
        if self.width == 0 {
            return; // `init_if_needed` wasn't called first; skip defensively.
        }
        if line_index >= self.height {
            return; // out-of-bounds row — silently drop.
        }
        let w = self.width as usize;
        let row_start = (line_index as usize) * w;
        let row_end = row_start + w;
        if row_end <= self.pixels.len() && pixels.len() >= w {
            self.pixels[row_start..row_end].copy_from_slice(&pixels[..w]);
            // Track the highest written row+1, not a count of
            // writes — makes duplicate and out-of-order writes
            // idempotent and prevents overflow past `height`.
            self.lines_written = self.lines_written.max(line_index.saturating_add(1));
        }
    }
}

/// A completed SSTV image ready to be saved to disk.
///
/// Returned by [`SstvImageHandle::take_completed`] at an
/// [`SstvEvent::ImageComplete`] event. Owns the pixel data
/// out-right so the in-flight buffer can immediately reset
/// for the next VIS detection without waiting for the save.
#[derive(Debug, Clone)]
pub struct CompletedSstvImage {
    /// Pixel width (e.g. 640 for PD120/PD180).
    pub width: u32,
    /// Pixel height (total scan lines).
    pub height: u32,
    /// Row-major RGB triples, length = `width * height`.
    pub pixels: Vec<[u8; 3]>,
}

impl CompletedSstvImage {
    /// Flatten the pixels into a contiguous RGB byte slice suitable
    /// for PNG encoding. Each pixel is 3 bytes (R, G, B) in row-major
    /// order; the returned `Vec<u8>` has length `width * height * 3`.
    ///
    /// This method is the single serialisation entry point — callers
    /// in `sdr-ui` (which have Cairo available) convert this flat
    /// buffer into an ARGB32 surface and call `write_to_png`, mirroring
    /// the LRPT viewer's `write_rgb_png` helper.
    #[must_use]
    pub fn to_flat_rgb(&self) -> Vec<u8> {
        self.pixels
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b])
            .collect()
    }
}

/// A non-destructive snapshot of the in-flight SSTV image for the
/// live viewer. Contains only the rows written so far.
#[derive(Debug, Clone)]
pub struct SstvSnapshot {
    pub width: u32,
    pub height: u32,
    /// Rows 0 … `lines_written - 1`, each `width` RGB triples.
    pub pixels: Vec<[u8; 3]>,
    /// Number of complete scan lines in `pixels`.
    pub lines_written: u32,
}

/// Recover from a poisoned mutex, emitting a single warning.
fn lock_or_recover(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e: PoisonError<_>| {
        warn!("SstvImage mutex poisoned, recovering — a decoder thread panicked");
        e.into_inner()
    })
}

/// Cloneable handle to the shared SSTV image buffer.
///
/// All clones read and write the same underlying buffer. Clone the
/// handle to share it between the DSP tap and the UI viewer.
#[derive(Clone)]
pub struct SstvImageHandle {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for SstvImageHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SstvImageHandle").finish_non_exhaustive()
    }
}

/// `SstvImage` is the top-level constructor. Call [`SstvImage::handle`]
/// to get a cloneable [`SstvImageHandle`] for sharing across threads.
pub struct SstvImage {
    handle: SstvImageHandle,
}

impl Default for SstvImage {
    fn default() -> Self {
        Self::new()
    }
}

impl SstvImage {
    /// Create a fresh, empty image (ready for a new pass).
    #[must_use]
    pub fn new() -> Self {
        Self {
            handle: SstvImageHandle {
                inner: Arc::new(Mutex::new(Inner::new())),
            },
        }
    }

    /// Return a cloneable handle for sharing between the DSP tap
    /// and the UI viewer. Every clone reads/writes the same buffer.
    #[must_use]
    pub fn handle(&self) -> SstvImageHandle {
        self.handle.clone()
    }

    /// Clear the buffer without returning the pixels. Convenience
    /// wrapper around [`SstvImageHandle::clear`]; equivalent to
    /// `self.handle().clear()` but avoids cloning the Arc handle.
    pub fn clear(&self) {
        self.handle.clear();
    }
}

impl SstvImageHandle {
    /// Write one decoded scan line into the buffer.
    ///
    /// Initialises the image dimensions on the first call for a new
    /// image (i.e. after construction or after `take_completed` reset
    /// the buffer). `width` and `height` are the mode's full-image
    /// dimensions; `line_index` is the 0-based row being written;
    /// `pixels` is the decoded row from `SstvEvent::LineDecoded`.
    pub fn write_line(&self, line_index: u32, width: u32, height: u32, pixels: &[[u8; 3]]) {
        let mut g = lock_or_recover(&self.inner);
        g.init_if_needed(width, height);
        g.write_line(line_index, pixels);
    }

    /// Atomically swap out the completed in-flight image and reset the
    /// buffer for the next VIS detection.
    ///
    /// Returns `None` when the buffer is empty (no lines written since
    /// last reset), so callers can skip saving empty images gracefully.
    #[must_use]
    pub fn take_completed(&self) -> Option<CompletedSstvImage> {
        let mut g = lock_or_recover(&self.inner);
        if g.is_empty() {
            return None;
        }
        let completed = CompletedSstvImage {
            width: g.width,
            height: g.height,
            pixels: std::mem::take(&mut g.pixels),
        };
        g.clear();
        Some(completed)
    }

    /// Non-destructive snapshot of the in-flight image for the live
    /// viewer. Clones only the written rows, so a 50-line snapshot
    /// from a 496-line PD120 image copies ~50 × 640 × 3 ≈ 96 KB —
    /// cheap compared to the full-image clone.
    #[must_use]
    pub fn snapshot(&self) -> Option<SstvSnapshot> {
        let g = lock_or_recover(&self.inner);
        if g.is_empty() {
            return None;
        }
        // Clamp defensively: `lines_written` should never exceed
        // `height` after the `write_line` fix, but belt-and-
        // suspenders keeps `snapshot` panic-safe even if the
        // accounting somehow drifted.
        let lines = g.lines_written.min(g.height) as usize;
        let w = g.width as usize;
        Some(SstvSnapshot {
            width: g.width,
            height: g.height,
            pixels: g.pixels[..lines * w].to_vec(),
            lines_written: g.lines_written,
        })
    }

    /// Clear the buffer without returning the pixels. Use between
    /// passes when the previous image doesn't need to be saved.
    pub fn clear(&self) {
        let mut g = lock_or_recover(&self.inner);
        g.clear();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Width + height used by all tests. Matches PD120's spec (640 × 496).
    const W: u32 = 640;
    const H: u32 = 496;

    fn blank_row() -> Vec<[u8; 3]> {
        vec![[0, 0, 0]; W as usize]
    }

    fn red_row() -> Vec<[u8; 3]> {
        vec![[255, 0, 0]; W as usize]
    }

    #[test]
    fn snapshot_returns_none_before_any_write() {
        let img = SstvImage::new();
        let h = img.handle();
        assert!(h.snapshot().is_none());
    }

    #[test]
    fn take_completed_returns_none_before_any_write() {
        let img = SstvImage::new();
        let h = img.handle();
        assert!(h.take_completed().is_none());
    }

    #[test]
    fn write_line_then_snapshot_round_trip() {
        let img = SstvImage::new();
        let h = img.handle();
        h.write_line(0, W, H, &red_row());
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.width, W);
        assert_eq!(snap.height, H);
        assert_eq!(snap.lines_written, 1);
        assert_eq!(snap.pixels.len(), W as usize);
        assert_eq!(snap.pixels[0], [255, 0, 0]);
    }

    #[test]
    fn take_completed_drains_and_resets() {
        let img = SstvImage::new();
        let h = img.handle();
        h.write_line(0, W, H, &red_row());
        h.write_line(1, W, H, &blank_row());

        let completed = h.take_completed().unwrap();
        assert_eq!(completed.width, W);
        assert_eq!(completed.height, H);
        assert_eq!(completed.pixels.len(), (W * H) as usize);
        // First row is red.
        assert_eq!(completed.pixels[0], [255, 0, 0]);
        // Second row is black.
        assert_eq!(completed.pixels[W as usize], [0, 0, 0]);

        // Buffer should be reset.
        assert!(h.snapshot().is_none());
        assert!(h.take_completed().is_none());
    }

    #[test]
    fn clones_share_same_buffer() {
        let img = SstvImage::new();
        let a = img.handle();
        let b = a.clone();
        a.write_line(0, W, H, &red_row());
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.pixels[0], [255, 0, 0]);
    }

    #[test]
    fn clear_resets_without_returning_pixels() {
        let img = SstvImage::new();
        let h = img.handle();
        h.write_line(0, W, H, &red_row());
        h.clear();
        assert!(h.snapshot().is_none());
    }

    #[test]
    fn multiple_images_can_be_cycled() {
        // Simulate two successive VIS detections (two images in a pass).
        let img = SstvImage::new();
        let h = img.handle();

        // Image 1: one red row.
        h.write_line(0, W, H, &red_row());
        let first = h.take_completed().unwrap();
        assert_eq!(first.pixels[0], [255, 0, 0]);

        // Image 2: one blank row.
        h.write_line(0, W, H, &blank_row());
        let second = h.take_completed().unwrap();
        assert_eq!(second.pixels[0], [0, 0, 0]);
    }

    #[test]
    #[allow(clippy::panic)]
    fn recovers_from_poisoned_mutex() {
        use std::thread;
        let img = SstvImage::new();
        let h = img.handle();
        h.write_line(0, W, H, &red_row());

        let inner_clone = Arc::clone(&h.inner);
        let _ = thread::spawn(move || {
            let _guard = inner_clone.lock().expect("first lock");
            panic!("intentional panic to poison the mutex");
        })
        .join();

        assert!(h.inner.is_poisoned(), "mutex must be poisoned for test");
        // Snapshot and write both go through `lock_or_recover` and must work.
        let snap = h.snapshot().unwrap();
        assert_eq!(snap.lines_written, 1);
        h.write_line(1, W, H, &blank_row());
        let snap2 = h.snapshot().unwrap();
        assert_eq!(snap2.lines_written, 2);
    }

    #[test]
    fn to_flat_rgb_has_expected_length() {
        let img = SstvImage::new();
        let h = img.handle();
        for i in 0_u32..4 {
            h.write_line(i, W, H, &red_row());
        }
        let completed = h.take_completed().unwrap();
        let flat = completed.to_flat_rgb();
        // Full-image flat RGB is width * height * 3 bytes.
        assert_eq!(
            flat.len(),
            (W * H) as usize * 3,
            "flat RGB must be width * height * 3 bytes"
        );
        // First pixel should be [255, 0, 0].
        assert_eq!(&flat[0..3], &[255_u8, 0, 0]);
    }

    /// Duplicate `write_line` calls for the same row must not
    /// advance `lines_written` — idempotent pixel write.
    /// Regression for the CR #5 finding (PR #599): the old
    /// `+= 1` counter drifted above `height` on re-sends.
    #[test]
    fn duplicate_write_does_not_advance_counter() {
        let img = SstvImage::new();
        let h = img.handle();
        h.write_line(0, W, H, &red_row());
        h.write_line(0, W, H, &red_row()); // duplicate
        h.write_line(0, W, H, &red_row()); // duplicate again
        let snap = h.snapshot().unwrap();
        assert_eq!(
            snap.lines_written, 1,
            "duplicate writes must not advance lines_written past 1"
        );
    }

    /// Out-of-order writes must not inflate `lines_written`
    /// beyond the highest row index seen.
    #[test]
    fn out_of_order_writes_track_highest_row() {
        let img = SstvImage::new();
        let h = img.handle();
        h.write_line(5, W, H, &red_row()); // write row 5 first
        h.write_line(2, W, H, &blank_row()); // then row 2
        let snap = h.snapshot().unwrap();
        // lines_written should be 6 (highest row index 5 + 1),
        // not 2 (a naive count of calls).
        assert_eq!(
            snap.lines_written, 6,
            "out-of-order write: lines_written must be max(row_index)+1"
        );
    }

    /// An out-of-bounds row index (>= height) must be silently
    /// dropped; neither the pixel buffer nor `lines_written`
    /// should be affected.
    #[test]
    fn out_of_bounds_row_is_silently_dropped() {
        let img = SstvImage::new();
        let h = img.handle();
        h.write_line(0, W, H, &red_row()); // valid
        h.write_line(H, W, H, &red_row()); // OOB: index == height
        h.write_line(H + 99, W, H, &red_row()); // OOB: way past
        let snap = h.snapshot().unwrap();
        assert_eq!(
            snap.lines_written, 1,
            "OOB row must not advance lines_written"
        );
    }
}
