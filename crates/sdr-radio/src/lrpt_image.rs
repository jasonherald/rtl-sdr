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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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

// Manual `Debug` so callers can put `LrptImage` in derived-`Debug`
// enums (e.g. `UiToDsp::SetLrptImage`) without forcing
// `ImageAssembler` to expose its internals through derived
// `Debug`. The handle is opaque from the outside — the only
// observable identity is its existence — so a placeholder print
// is faithful and avoids accidentally locking the mutex from a
// `Debug` formatter (which would otherwise let a panic'd holder
// poison logging too).
impl std::fmt::Debug for LrptImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LrptImage").finish_non_exhaustive()
    }
}

/// Acquire the assembler lock, recovering from a poisoned mutex
/// by logging a warning and returning the inner guard via
/// [`PoisonError::into_inner`].
///
/// Poisoning here means a previous decoder thread panicked while
/// holding the lock — the in-flight VCDU placement may have left
/// the buffer mid-mutation, but `ImageAssembler` operations are
/// individually atomic (they don't span across calls), so the
/// next read/write still observes consistent per-channel state.
/// Silently swallowing the lock as the previous code did meant
/// a single panic anywhere in the decoder would permanently mute
/// the live viewer; recovering keeps the pipeline alive at the
/// cost of one warn log.
fn lock_or_recover(inner: &Mutex<ImageAssembler>) -> MutexGuard<'_, ImageAssembler> {
    inner.lock().unwrap_or_else(|e: PoisonError<_>| {
        tracing::warn!("LrptImage mutex poisoned, recovering — a decoder thread panicked");
        e.into_inner()
    })
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
        let mut a = lock_or_recover(&self.inner);
        a.push_line(apid, line);
    }

    /// Snapshot of channel `apid` as it stands right now, or
    /// `None` if the channel hasn't received any data yet.
    /// Returns a clone — the caller doesn't hold the lock during
    /// long renders.
    #[must_use]
    pub fn snapshot_channel(&self, apid: u16) -> Option<ChannelBuffer> {
        let a = lock_or_recover(&self.inner);
        a.channel(apid).cloned()
    }

    /// All channel APIDs the assembler has seen at least one
    /// MCU / line for, in unspecified order.
    #[must_use]
    pub fn channel_apids(&self) -> Vec<u16> {
        let a = lock_or_recover(&self.inner);
        a.channels().map(|(&apid, _)| apid).collect()
    }

    /// Borrow the assembler under lock for save / composite ops
    /// at LOS. Caller is expected to keep the closure short —
    /// other threads block on the mutex while it runs.
    pub fn with_assembler<R>(&self, f: impl FnOnce(&ImageAssembler) -> R) -> R {
        let a = lock_or_recover(&self.inner);
        f(&a)
    }

    /// Clear all accumulated channels — call between passes.
    pub fn clear(&self) {
        let mut a = lock_or_recover(&self.inner);
        a.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_lrpt::image::IMAGE_WIDTH;

    /// Primary APID used in single-channel persistence checks.
    /// Any value in the AVHRR range works (64–69); 64 is the
    /// conventional "first channel" used elsewhere in the test
    /// suite.
    const APID_TEST: u16 = 64;
    /// Secondary APID for multi-channel listing checks. Distinct
    /// from `APID_TEST` so the sort-and-compare assertion in
    /// `channel_apids_lists_pushed_channels` actually exercises
    /// the multi-channel path.
    const APID_TEST_2: u16 = 65;

    /// Marker pixel value pushed in `push_then_snapshot_round_trip`.
    /// Distinct from 0 (empty buffer fill) and 0xFF (saturation)
    /// so a regression that returned a default-constructed buffer
    /// would fail loudly instead of silently matching.
    const TEST_PIXEL: u8 = 42;
    /// Marker pixel for the second channel in the listing test.
    /// Distinct from `TEST_PIXEL` so a swapped-channel bug would
    /// surface in any future per-channel content assertion.
    const TEST_PIXEL_2: u8 = 99;

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
        img.push_line(APID_TEST, &vec![TEST_PIXEL; IMAGE_WIDTH]);
        img.push_line(APID_TEST_2, &vec![TEST_PIXEL_2; IMAGE_WIDTH]);
        let mut apids = img.channel_apids();
        apids.sort_unstable();
        assert_eq!(apids, vec![APID_TEST, APID_TEST_2]);
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
        let lines = img.with_assembler(|a| a.channel(APID_TEST).map(|c| c.lines));
        assert_eq!(lines, Some(1));
    }

    #[test]
    #[allow(
        clippy::panic,
        reason = "test deliberately panics on a worker thread to poison the mutex; the panic is the test fixture"
    )]
    fn recovers_from_poisoned_mutex() {
        // Per CR round 1: a panic on a decoder thread used to
        // permanently mute the live viewer because every
        // subsequent `inner.lock()` would return Err and the
        // silent-swallow code path skipped the operation. With
        // poison recovery, the next push/snapshot still works —
        // we just emit a warn log.
        use std::sync::Arc;
        use std::thread;

        let img = LrptImage::new();
        img.push_line(APID_TEST, &vec![TEST_PIXEL; IMAGE_WIDTH]);

        // Poison the mutex: spawn a thread that locks then
        // panics. The Arc/Mutex we share with `img` will be
        // marked poisoned when the thread unwinds.
        let inner = Arc::clone(&img.inner);
        let _ = thread::spawn(move || {
            let _guard = inner.lock().expect("first lock");
            panic!("intentional panic to poison the mutex");
        })
        .join();
        assert!(
            img.inner.is_poisoned(),
            "test setup: mutex must be poisoned"
        );

        // The pre-poison line must still be readable, AND new
        // writes must still land. Both routes go through
        // `lock_or_recover`.
        let snap = img
            .snapshot_channel(APID_TEST)
            .expect("snapshot post-poison");
        assert_eq!(snap.pixels[0], TEST_PIXEL);
        img.push_line(APID_TEST_2, &vec![TEST_PIXEL_2; IMAGE_WIDTH]);
        let snap2 = img
            .snapshot_channel(APID_TEST_2)
            .expect("write post-poison");
        assert_eq!(snap2.pixels[0], TEST_PIXEL_2);

        // channel_apids and clear must also recover, not return
        // empty / no-op.
        let mut apids = img.channel_apids();
        apids.sort_unstable();
        assert_eq!(apids, vec![APID_TEST, APID_TEST_2]);
        img.clear();
        assert!(
            img.channel_apids().is_empty(),
            "clear must work post-poison"
        );
    }
}
