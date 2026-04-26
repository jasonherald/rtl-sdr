//! Live Meteor LRPT image handle — sdr-radio surface over the
//! `sdr-lrpt::ImageAssembler`.
//!
//! Sits one stage downstream of [`sdr_lrpt::LrptPipeline`]: the
//! pipeline accumulates per-channel scan lines as VCDUs arrive,
//! and this wrapper exposes that accumulation to other threads
//! (the live viewer in `sdr-ui`, the LOS PNG saver) through an
//! `Arc<Mutex<...>>`.
//!
//! Pure handle — no decoding, no threading control. Clone the
//! handle to share the underlying assembler across threads;
//! every clone reads / writes the same buffer.
//!
//! ```text
//!     LrptPipeline ──[VCDU → MCU placement]──▶  ImageAssembler
//!                                                  ▲
//!                                                  │ (Arc<Mutex<>>)
//!                                                  │
//!                                              LrptImage
//!                                              │      │
//!                                              ▼      ▼
//!                                  live viewer    LOS PNG export
//! ```
//!
//! One [`LrptImage`] corresponds to **one satellite pass**.
//! Starting a fresh pass = constructing a fresh handle (or
//! calling [`LrptImage::clear`]); finalizing = stopping the
//! decoder feed and using the existing snapshot for export.

use sdr_lrpt::image::{ChannelBuffer, ImageAssembler};
use std::sync::{Arc, Mutex};

/// Cloneable handle over a shared [`ImageAssembler`]. All clones
/// read and write the same underlying buffer.
#[derive(Clone)]
pub struct LrptImage {
    inner: Arc<Mutex<ImageAssembler>>,
}

impl Default for LrptImage {
    fn default() -> Self {
        Self::new()
    }
}

impl LrptImage {
    /// Start a fresh pass with an empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ImageAssembler::new())),
        }
    }

    /// Push a complete scan line for `apid`. Used by callers
    /// that already have a row buffered.
    pub fn push_line(&self, apid: u16, line: &[u8]) {
        if let Ok(mut a) = self.inner.lock() {
            a.push_line(apid, line);
        }
    }

    /// Snapshot of channel `apid` as it stands right now, or
    /// `None` if the channel hasn't received any data yet.
    /// Returns a clone — the caller doesn't hold the lock during
    /// long renders.
    #[must_use]
    pub fn snapshot_channel(&self, apid: u16) -> Option<ChannelBuffer> {
        let a = self.inner.lock().ok()?;
        a.channel(apid).cloned()
    }

    /// All channel APIDs the assembler has seen at least one
    /// MCU / line for, in unspecified order.
    #[must_use]
    pub fn channel_apids(&self) -> Vec<u16> {
        let Ok(a) = self.inner.lock() else {
            return Vec::new();
        };
        a.channels().map(|(&apid, _)| apid).collect()
    }

    /// Borrow the assembler under lock for save / composite ops
    /// at LOS. Caller is expected to keep the closure short —
    /// other threads block on the mutex while it runs.
    pub fn with_assembler<R>(&self, f: impl FnOnce(&ImageAssembler) -> R) -> Option<R> {
        let a = self.inner.lock().ok()?;
        Some(f(&a))
    }

    /// Clear all accumulated channels — call between passes.
    pub fn clear(&self) {
        if let Ok(mut a) = self.inner.lock() {
            a.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_lrpt::image::IMAGE_WIDTH;

    /// APID used in single-channel persistence checks. Any value
    /// in the AVHRR range works (64–69); 64 is the conventional
    /// "first channel" used elsewhere in the test suite.
    const APID_TEST: u16 = 64;

    /// Marker pixel value pushed in `push_then_snapshot_round_trip`.
    /// Distinct from 0 (empty buffer fill) and 0xFF (saturation)
    /// so a regression that returned a default-constructed buffer
    /// would fail loudly instead of silently matching.
    const TEST_PIXEL: u8 = 42;

    #[test]
    fn push_then_snapshot_round_trip() {
        let img = LrptImage::new();
        img.push_line(APID_TEST, &vec![TEST_PIXEL; IMAGE_WIDTH]);
        let snap = img.snapshot_channel(APID_TEST).expect("channel present");
        assert_eq!(snap.lines, 1);
        assert_eq!(snap.pixels[0], TEST_PIXEL);
    }

    #[test]
    fn snapshot_unknown_channel_returns_none() {
        let img = LrptImage::new();
        assert!(img.snapshot_channel(APID_TEST).is_none());
    }

    #[test]
    fn channel_apids_lists_pushed_channels() {
        let img = LrptImage::new();
        img.push_line(64, &vec![1; IMAGE_WIDTH]);
        img.push_line(65, &vec![2; IMAGE_WIDTH]);
        let mut apids = img.channel_apids();
        apids.sort_unstable();
        assert_eq!(apids, vec![64, 65]);
    }

    #[test]
    fn clear_drops_all_channels() {
        let img = LrptImage::new();
        img.push_line(APID_TEST, &vec![TEST_PIXEL; IMAGE_WIDTH]);
        img.clear();
        assert!(img.snapshot_channel(APID_TEST).is_none());
        assert!(img.channel_apids().is_empty());
    }

    #[test]
    fn handle_clones_share_assembler() {
        // Both clones must observe the same underlying buffer —
        // that's the whole point of the Arc<Mutex<...>> handle.
        // A regression that accidentally cloned the inner state
        // (e.g. by deriving Clone on a non-Arc inner type) would
        // surface as the second handle reading None.
        let a = LrptImage::new();
        let b = a.clone();
        a.push_line(APID_TEST, &vec![TEST_PIXEL; IMAGE_WIDTH]);
        let snap = b.snapshot_channel(APID_TEST).expect("clone sees write");
        assert_eq!(snap.pixels[0], TEST_PIXEL);
    }

    #[test]
    fn with_assembler_runs_closure_under_lock() {
        let img = LrptImage::new();
        img.push_line(APID_TEST, &vec![TEST_PIXEL; IMAGE_WIDTH]);
        let lines = img
            .with_assembler(|a| a.channel(APID_TEST).map(|c| c.lines))
            .expect("lock acquired");
        assert_eq!(lines, Some(1));
    }
}
