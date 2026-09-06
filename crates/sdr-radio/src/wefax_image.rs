//! Shared WEFAX (radiofax) image handle: bridges the WEFAX decode
//! tap (writer) and the live chart viewer (reader). Mirrors
//! [`crate::sstv_image::SstvImage`]'s `Arc<Mutex<Inner>>` shape —
//! a single growing greyscale image at a time — but drops the RGB
//! triple down to a single `u8` per pixel, matching WEFAX's
//! greyscale-only chart content.
//!
//! ```text
//!     WefaxDecoder ──[per-line events]──▶  WefaxImageHandle (writer)
//!                                               ▲
//!                                               │ (Arc<Mutex<Inner>>)
//!                                               │
//!                                         WefaxImageHandle
//!                                         │            │
//!                                         ▼            ▼
//!                                   live viewer    chart PNG export
//! ```
//!
//! One [`WefaxImage`] corresponds to **one received chart**. A
//! fresh chart = constructing a fresh handle (or calling
//! [`WefaxImage::clear`] / [`WefaxImageHandle::clear`]); finalizing
//! = draining the in-flight buffer via
//! [`WefaxImageHandle::take_completed`], which also resets the
//! buffer for the next chart.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing::warn;

/// Inner mutable state for the shared WEFAX image buffer.
struct Inner {
    /// Pixel width of the current image (set on first `write_line`).
    width: u32,
    /// Highest row index written + 1 — i.e. the buffer's current
    /// height. Grows on demand as later lines arrive; WEFAX has no
    /// fixed total-line-count header the way SSTV modes do.
    height: u32,
    /// Row-major greyscale pixels, one `u8` per pixel, length
    /// `width * height`.
    pixels: Vec<u8>,
}

impl Inner {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        }
    }

    /// True if no lines have been written yet.
    fn is_empty(&self) -> bool {
        self.height == 0
    }

    /// Reset to empty state, ready for a new chart.
    fn clear(&mut self) {
        self.width = 0;
        self.height = 0;
        self.pixels.clear();
    }

    /// Initialise the buffer width on the first line of a new
    /// image. Idempotent — later calls with the same width are a
    /// no-op; a differing width (shouldn't happen mid-chart) is
    /// also ignored rather than corrupting the existing buffer.
    fn init_if_needed(&mut self, width: u32) {
        if self.width == 0 {
            self.width = width;
        }
    }

    /// Grow the buffer so row `line_index` exists, filling any
    /// newly-created gap rows with zero (black). WEFAX lines can
    /// arrive out of order across a resync, so growth is driven by
    /// the highest row index seen rather than a running counter.
    fn grow_to(&mut self, line_index: u32) {
        let needed_height = line_index.saturating_add(1);
        if needed_height <= self.height {
            return;
        }
        let w = self.width as usize;
        self.pixels.resize((needed_height as usize) * w, 0);
        self.height = needed_height;
    }

    /// Copy one scan line's pixels into the buffer. Out-of-order
    /// and duplicate writes are safe: `grow_to` only ever extends
    /// the buffer, and copying the same row twice is idempotent on
    /// the pixel data.
    fn write_line(&mut self, line_index: u32, pixels: &[u8]) {
        if self.width == 0 {
            return; // `init_if_needed` wasn't called first; skip defensively.
        }
        self.grow_to(line_index);
        let w = self.width as usize;
        let row_start = (line_index as usize) * w;
        let row_end = row_start + w;
        if row_end <= self.pixels.len() && pixels.len() >= w {
            self.pixels[row_start..row_end].copy_from_slice(&pixels[..w]);
        }
    }
}

/// A completed WEFAX chart ready to be saved to disk.
///
/// Returned by [`WefaxImageHandle::take_completed`]. Owns the
/// pixel data out-right so the in-flight buffer can immediately
/// reset for the next chart without waiting for the save.
#[derive(Debug, Clone)]
pub struct CompletedWefaxImage {
    /// Pixel width of the chart.
    pub width: u32,
    /// Pixel height (total scan lines received).
    pub height: u32,
    /// Row-major greyscale pixels, length `width * height`.
    pub pixels: Vec<u8>,
}

impl CompletedWefaxImage {
    /// Flatten the pixels into a contiguous greyscale byte slice
    /// suitable for PNG encoding. Already flat (one `u8` per
    /// pixel) — this is a cheap clone, matching the shape of
    /// [`crate::sstv_image::CompletedSstvImage::to_flat_rgb`] so
    /// callers in `sdr-ui` can treat both encoders uniformly.
    #[must_use]
    pub fn to_flat_gray(&self) -> Vec<u8> {
        self.pixels.clone()
    }
}

/// A non-destructive snapshot of the in-flight WEFAX image for the
/// live viewer. Contains all rows written so far.
#[derive(Debug, Clone)]
pub struct WefaxSnapshot {
    /// Pixel width of the chart.
    pub width: u32,
    /// Number of rows currently in `pixels`.
    pub height: u32,
    /// Row-major greyscale pixels, length `width * height`.
    pub pixels: Vec<u8>,
}

/// Recover from a poisoned mutex, emitting a single warning.
fn lock_or_recover(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e: PoisonError<_>| {
        warn!("WefaxImage mutex poisoned, recovering — a decoder thread panicked");
        e.into_inner()
    })
}

/// Cloneable handle to the shared WEFAX image buffer.
///
/// All clones read and write the same underlying buffer. Clone the
/// handle to share it between the DSP tap and the UI viewer.
#[derive(Clone)]
pub struct WefaxImageHandle {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for WefaxImageHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WefaxImageHandle").finish_non_exhaustive()
    }
}

/// `WefaxImage` is the top-level constructor. Call
/// [`WefaxImage::handle`] to get a cloneable [`WefaxImageHandle`]
/// for sharing across threads.
pub struct WefaxImage {
    handle: WefaxImageHandle,
}

impl Default for WefaxImage {
    fn default() -> Self {
        Self::new()
    }
}

impl WefaxImage {
    /// Create a fresh, empty image (ready for a new chart).
    #[must_use]
    pub fn new() -> Self {
        Self {
            handle: WefaxImageHandle {
                inner: Arc::new(Mutex::new(Inner::new())),
            },
        }
    }

    /// Return a cloneable handle for sharing between the DSP tap
    /// and the UI viewer. Every clone reads/writes the same
    /// buffer.
    #[must_use]
    pub fn handle(&self) -> WefaxImageHandle {
        self.handle.clone()
    }

    /// Clear the buffer without returning the pixels. Convenience
    /// wrapper around [`WefaxImageHandle::clear`]; equivalent to
    /// `self.handle().clear()` but avoids cloning the Arc handle.
    pub fn clear(&self) {
        self.handle.clear();
    }
}

impl WefaxImageHandle {
    /// Write one decoded scan line into the buffer.
    ///
    /// Initialises the image width on the first call for a new
    /// image (i.e. after construction or after `take_completed` /
    /// `clear` reset the buffer). `width` is the chart's line
    /// width in pixels; `line_index` is the 0-based row being
    /// written; `pixels` is the decoded greyscale row (one `u8`
    /// per pixel).
    pub fn write_line(&self, line_index: u32, width: u32, pixels: &[u8]) {
        let mut g = lock_or_recover(&self.inner);
        g.init_if_needed(width);
        g.write_line(line_index, pixels);
    }

    /// Atomically swap out the completed in-flight image and reset
    /// the buffer for the next chart.
    ///
    /// Returns `None` when the buffer is empty (no lines written
    /// since last reset), so callers can skip saving empty images
    /// gracefully.
    #[must_use]
    pub fn take_completed(&self) -> Option<CompletedWefaxImage> {
        let mut g = lock_or_recover(&self.inner);
        if g.is_empty() {
            return None;
        }
        let completed = CompletedWefaxImage {
            width: g.width,
            height: g.height,
            pixels: std::mem::take(&mut g.pixels),
        };
        g.clear();
        Some(completed)
    }

    /// Non-destructive snapshot of the in-flight image for the
    /// live viewer. Clones the written rows so the caller doesn't
    /// hold the lock during a render.
    #[must_use]
    pub fn snapshot(&self) -> Option<WefaxSnapshot> {
        let g = lock_or_recover(&self.inner);
        if g.is_empty() {
            return None;
        }
        Some(WefaxSnapshot {
            width: g.width,
            height: g.height,
            pixels: g.pixels.clone(),
        })
    }

    /// Clear the buffer without returning the pixels. Use between
    /// charts when the previous image doesn't need to be saved.
    pub fn clear(&self) {
        let mut g = lock_or_recover(&self.inner);
        g.clear();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
