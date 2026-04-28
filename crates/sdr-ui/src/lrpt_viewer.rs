//! Live Meteor-M LRPT image viewer + PNG export.
//!
//! LRPT counterpart to [`crate::apt_viewer`]. Displays the
//! per-APID scan-line buffers from a shared
//! [`sdr_radio::lrpt_image::LrptImage`] as they accumulate during
//! a satellite pass. Width is fixed at the LRPT scan width
//! ([`IMAGE_WIDTH`] = 1568 px); height grows downward as new
//! lines arrive from the FEC chain.
//!
//! Three pieces:
//!
//! * [`LrptImageRenderer`] — pure Cairo renderer. Owns a
//!   `HashMap<APID, ChannelSurface>` of ARGB32 surfaces, each
//!   sized for a full pass. Knows how to paint the active
//!   channel into a cairo context with auto-fit + aspect
//!   preservation. No GTK dependency, fully unit-testable.
//! * [`LrptImageView`] — GTK widget wrapping a renderer plus a
//!   poll timer that drains new scan lines from the shared
//!   [`sdr_radio::lrpt_image::LrptImage`] handle. Cloneable
//!   (all state is `Rc`-shared) so closures on toolbar buttons
//!   can hold their own handle. Polling — rather than
//!   message-pushing as APT does — matches LRPT's
//!   `Arc<Mutex<ImageAssembler>>` data-sharing model: the DSP
//!   thread mutates the shared buffer, the UI reads it.
//! * [`open_lrpt_viewer_window`] — opens the view in a
//!   non-modal transient window. Header bar carries a channel
//!   selector, Pause / Resume, and Export PNG.
//!
//! [`connect_lrpt_action`] wires the `app.lrpt-open` action
//! (`Ctrl+Shift+L`). Activating it opens a viewer window and
//! registers the shared `LrptImage` handle with the DSP
//! controller via `UiToDsp::SetLrptImage`. Closing the window
//! is **purely a UI teardown** — the DSP capture stays running
//! and the shared image keeps accumulating decoded rows so the
//! recorder's LOS save still produces a per-pass directory.
//! The decoder is gated by `current_mode == Lrpt` and the
//! source-stop cleanup path (an explicit detach via
//! `UiToDsp::ClearLrptImage` is reserved as future API surface
//! and never sent today). Per `CodeRabbit` rounds 7 + 8 on PR
//! #543.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{cairo, gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use sdr_lrpt::image::IMAGE_WIDTH;
use sdr_radio::lrpt_image::LrptImage;

use crate::messages::UiToDsp;
use crate::viewer::ViewerError;

// ─── False-colour composite catalog ────────────────────────────────────
//
// LRPT is multispectral — every Meteor-M pass usually decodes
// three or more AVHRR-style channels in parallel. The composite
// catalog below maps each user-facing recipe (chosen from the
// viewer's channel dropdown after the "Composite —" prefix) to a
// concrete R/G/B APID triple that
// [`sdr_lrpt::image::ImageAssembler::composite_rgb`] then renders
// into RGB pixels.
//
// Per #547. New recipes may only be appended, never inserted in
// the middle — the dropdown is rebuilt on every refresh tick and
// any reordering would silently shift the user's last selection
// (we don't persist a recipe, but the principle still applies if
// a future PR adds session memory).

/// A named R/G/B APID triple for false-colour rendering. Hard-
/// coded catalog entries cover the most common Meteor-M channel
/// combos — the user picks one from the dropdown and the
/// renderer composites the three named channels into RGB pixels.
///
/// Per #547. APID assignments follow Meteor-M N2-2's standard
/// channel layout. The User-facing walkthrough is at
/// `docs/guides/lrpt-reception.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositeRecipe {
    /// User-facing name. Shown in the dropdown after `Composite — `.
    pub name: &'static str,
    /// R, G, B APIDs in render order.
    pub r_apid: u16,
    pub g_apid: u16,
    pub b_apid: u16,
}

/// Hard-coded composite catalog. Each entry combines three AVHRR-
/// style channels into a single RGB image. Order matters — it's
/// the dropdown order users see. New entries must append, not
/// insert in the middle.
///
/// The three v1 entries pair the most-commonly-decoded Meteor
/// channel combos with their conventional false-colour roles:
///
/// 1. **Natural colour (123)** — visible R / visible G / visible B.
///    Rough true-colour for daylight passes.
/// 2. **False-colour IR (124)** — visible / visible / IR. Vegetation
///    reads bright red, water dark blue, snow white — the
///    classic "weather wash" composite.
/// 3. **Thermal IR (243)** — IR / IR / visible. Best for night
///    passes where the visible channels are dark but thermal
///    still discriminates land/sea/cloud.
///
/// APID values are the AVHRR slots Meteor-M N2-2 transmits on:
/// 64 = ch1, 65 = ch2, 66 = ch3, 68 = ch4 (ch4 thermal is the
/// commonly-active IR slot when operating in the standard
/// "three visible plus one IR" mode). If a future Meteor variant
/// ships a different APID assignment we'll add new recipes
/// alongside these rather than mutate the existing values —
/// composites that worked once must keep working as the catalog
/// grows.
pub const COMPOSITE_CATALOG: &[CompositeRecipe] = &[
    CompositeRecipe {
        name: "Natural colour (123)",
        r_apid: 66,
        g_apid: 65,
        b_apid: 64,
    },
    CompositeRecipe {
        name: "False-colour IR (124)",
        r_apid: 68,
        g_apid: 65,
        b_apid: 64,
    },
    CompositeRecipe {
        name: "Thermal IR (243)",
        // The "243" label denotes channels 2/4/3 in canonical
        // Meteor channel notation: R=channel-2 (APID 65),
        // G=channel-4 (APID 68), B=channel-3 (APID 66). The
        // earlier draft swapped the green and red slots — flagged
        // by CR round 1 on PR #575.
        r_apid: 65,
        g_apid: 68,
        b_apid: 66,
    },
];

/// Maximum lines we'll keep per channel. The MSU-MR scanner on
/// Meteor-M produces AVHRR-style imagery at ~6 scan lines per
/// second per channel; a long high-elevation pass (~15 min above
/// horizon) is therefore ~5400 lines, and a typical 10-min pass
/// is ~3600. 8192 gives ~2× headroom for the longest plausible
/// pass without ever growing the surface at runtime — the
/// previous 1024 cap clipped roughly the last 80 % of even a
/// nominal pass. Per `CodeRabbit` round 2 on PR #543.
///
/// Memory cost is lazy: the per-APID Cairo surface only
/// allocates when that channel first receives a line. At the
/// cap, one channel is `IMAGE_WIDTH × MAX_LINES × 4 B` ≈ 51 MB.
/// A typical pass with three active AVHRR channels therefore
/// peaks around 150 MB, which matches the rest of the SDR
/// pipeline's working-set budget.
pub const MAX_LINES: usize = 8_192;

/// Background colour painted before any LRPT data is received
/// (and behind the image when the widget is wider than the
/// image's aspect). Near-black so the eventual greyscale image
/// stands out, matching the APT viewer's palette.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

/// Bytes per pixel for Cairo's ARGB32 surface format —
/// `B`, `G`, `R`, `A` in little-endian byte order. Pulled out
/// of the hot-path pixel-copy loop in
/// [`LrptImageRenderer::push_line`] so a future format change
/// (e.g. RGB24 for the LRPT RGB-composite mode) is a one-line
/// edit. Per `CodeRabbit` round 4 on PR #543.
const BYTES_PER_PIXEL: usize = 4;

/// Default size for the viewer window. A typical Meteor MSU-MR
/// pass produces ~3600 lines × 1568 px (portrait, ~1:2 aspect)
/// at full duration. There's no scroll path — `DrawingArea`
/// sits directly under `ToolbarView` and `LrptImageRenderer::render`
/// scales the full image to fit the available area, preserving
/// aspect — so the visible pixels per scan-line drop as the
/// image grows tall. The default 900 × 600 landscape footprint
/// is chosen for ergonomics rather than aspect match: it sits
/// comfortably alongside the main radio window on a typical
/// 1080p+ desktop, fills well during the early-pass phase when
/// the image is still short and wide, and the user can resize
/// freely once they see how the pass is developing. (Pre-round-2
/// the comment claimed "wider than tall because typical pass
/// heights are ~600 lines" — that assumption was based on the
/// old 1024-line cap and stopped holding once `MAX_LINES`
/// bumped to 8192.) Per `CodeRabbit` rounds 14 + 15 on PR #543.
const VIEWER_WINDOW_WIDTH: i32 = 900;
const VIEWER_WINDOW_HEIGHT: i32 = 600;

/// Poll interval the view uses to drain new scan lines from
/// the shared `LrptImage` and queue redraws. MSU-MR produces
/// ~6 scan lines per second per channel; 250 ms (4 Hz) keeps
/// the viewer one tick behind the line arrival rate at most,
/// which feels responsive without burning CPU on a tight
/// loop. A faster cadence wouldn't pay off — multiple lines
/// land per tick anyway and `drain_new_lines` already batches
/// them. 60 FPS would be wasteful: there's no smooth-motion
/// content here, just discrete row appends. Per `CodeRabbit`
/// round 14 on PR #543 (refreshed from the older "~1 Hz" copy
/// that predated the round-2 MSU-MR rate research).
const POLL_INTERVAL_MS: u32 = 250;

/// Refresh interval for the channel-dropdown population tick.
/// Channel discovery on Meteor is rare (a handful of APIDs per
/// pass, all surfaced within the first minute), so 1 Hz is
/// plenty — anything faster would burn CPU on idle string
/// compares. Per `CodeRabbit` round 5 on PR #543.
const DROPDOWN_REFRESH_INTERVAL_MS: u32 = 1_000;

/// What [`LrptImageRenderer::push_line`] did with the row.
/// Drives the caller's per-APID watermark: rows that were
/// committed (or permanently dropped because they're either
/// malformed or past the channel's [`MAX_LINES`] cap) advance the
/// watermark; transient renderer failures leave the row in the
/// source so the next poll can retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Row was written into the per-APID Cairo surface.
    Pushed,
    /// Channel already at [`MAX_LINES`] — row intentionally
    /// dropped. Caller should advance its watermark; further
    /// data for this channel will keep hitting the cap no
    /// matter how many retries.
    Capped,
    /// Caller bug — pixel slice didn't match [`IMAGE_WIDTH`].
    /// Caller should advance its watermark; the data is
    /// malformed at the source and retrying won't help.
    InvalidLine,
    /// Transient renderer-side failure (surface allocation,
    /// stride conversion, or surface-data lock). Caller should
    /// NOT advance its watermark — the next poll might succeed
    /// (alloc relief, lock contention clearing).
    TransientFailure,
}

impl PushOutcome {
    /// `true` when the caller should advance its watermark past
    /// this row. `false` means "leave it in the source for the
    /// next poll to retry" — used only for [`Self::TransientFailure`].
    #[must_use]
    pub fn consumed(self) -> bool {
        !matches!(self, Self::TransientFailure)
    }
}

/// Pure Cairo renderer for a multi-channel LRPT image buffer.
///
/// Owns one persistent ARGB32 [`cairo::ImageSurface`] per APID,
/// each sized [`IMAGE_WIDTH`] × [`MAX_LINES`] and lazily
/// allocated on the first `push_line(apid, …)` for that APID.
/// Surfaces are kept across pushes so [`Self::render`] can paint
/// the latest state without copying — same alloc-free hot-path
/// guarantee the APT renderer offers.
pub struct LrptImageRenderer {
    channels: HashMap<u16, ChannelSurface>,
    /// What the renderer is currently showing. `Apid(_)` paints
    /// the per-channel greyscale surface for that APID;
    /// `Composite(_)` paints the cached ARGB32 surface in
    /// [`Self::composite_cache`] (or, if that cache hasn't been
    /// built yet, just the background — composite mode is
    /// authoritative, per CR round 4 on PR #575). `None` until
    /// the first per-APID line auto-selects via
    /// [`Self::push_line`]. Per CR round 5 on PR #575:
    /// previously two parallel `Option` fields lived here
    /// (`active` + `active_composite`); collapsing into one
    /// enum makes mode transitions atomic and removes the "did
    /// I remember to clear the other one" footgun the parallel
    /// fields invited.
    selection: Option<ActiveSelection>,
    /// Cached ARGB32 surface backing the active composite. Built
    /// off the GTK main thread by the View's `set_composite` —
    /// the worker calls [`Self::install_composite_cache`] when
    /// it returns. `None` until the first successful build for
    /// the current selection OR after [`Self::clear`] /
    /// [`Self::clear_composite`] / [`Self::mark_composite_pending`].
    /// The render code paints from this cached surface rather
    /// than re-running the composite math on every redraw —
    /// it's rebuilt on the dropdown-refresh tick when composite
    /// mode is active so new lines accrue at the dropdown
    /// cadence (~1 Hz). Per #547.
    composite_cache: Option<CompositeSurface>,
    /// `min(r_lines, g_lines, b_lines)` for the composite that
    /// is either currently cached OR currently being built by
    /// an in-flight worker. The dropdown-refresh tick reads
    /// this to skip a no-op rebuild when no source channel has
    /// advanced past the most recent build target (e.g., LOS
    /// reached, decoder stalled, or a non-limiting channel
    /// grew). Tracking the min — not each channel's height —
    /// is sufficient because the composite truncates to the
    /// shortest channel; advancing a non-limiting channel
    /// produces byte-identical output, so rebuilding it would
    /// be pure waste. Without this gate, composite mode burned
    /// ~5 ms × 3 memcpy + ~30 ms interleave + queued-redraw on
    /// every 1 Hz tick for the rest of the viewer's life.
    /// Per CR rounds 3 + 5 on PR #575: round 3 added the
    /// gate; round 5 extended it to also pin the in-flight
    /// build target so a long-running worker doesn't get
    /// duplicated by the next tick. Reset to `None` on
    /// `clear()`, `clear_composite()`, and
    /// `mark_composite_pending()`.
    composite_min_height: Option<usize>,
    /// Monotonic counter bumped by every selection-changing
    /// method (`set_active_apid`, `mark_composite_pending`,
    /// `prepare_composite_build`, `clear`, `clear_composite`).
    /// The View's async composite-build path captures this
    /// before spawning a worker; on completion, only installs
    /// the cache if the value still matches — otherwise the
    /// user has changed selection mid-flight and the worker's
    /// surface is stale. Wraps at `u64::MAX` (a billion ticks
    /// per second for half a millennium); not a concern. Per
    /// CR round 5 on PR #575.
    composite_gen: u64,
}

/// What the renderer is showing right now. Replaces the
/// previous parallel `active: Option<u16>` +
/// `active_composite: Option<CompositeRecipe>` fields, where
/// keeping both consistent across mode switches required the
/// caller to remember to clear the other one. Per CR round 5
/// on PR #575.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveSelection {
    /// Single per-APID greyscale channel.
    Apid(u16),
    /// Three-channel false-colour composite.
    Composite(CompositeRecipe),
}

struct ChannelSurface {
    surface: cairo::ImageSurface,
    n_lines: usize,
}

/// Pre-baked Cairo surface holding the active composite's pixels
/// as ARGB32 (B/G/R/A on little-endian hosts). Replaces a
/// composite-mode redraw's worst case from "iterate every pixel
/// of every source channel and pack RGB on each frame" with "blit
/// a single image surface" — same shape as the per-APID
/// `ChannelSurface` cache.
///
/// The owning recipe is tracked on `LrptImageRenderer.selection`
/// (as the `Composite(recipe)` variant) rather than here —
/// keeping recipe identity in one place avoids a "which one is
/// canonical" question. Per #547 + CR round 5 on PR #575.
struct CompositeSurface {
    surface: cairo::ImageSurface,
    /// Number of lines actually rendered. The composite
    /// assembler truncates to `min(r.lines, g.lines, b.lines)`
    /// so all three channels are valid for every painted row.
    height: usize,
}

impl ChannelSurface {
    /// Allocate a fresh full-pass-sized surface for one APID.
    /// Returns `None` if Cairo can't allocate the (~51 MB) ARGB32
    /// surface — practically unreachable on any desktop machine,
    /// but the library-crate "no panic" rule still applies.
    /// Per `CodeRabbit` round 1 on PR #543: the earlier draft
    /// panicked via `.expect()` even though `sdr-ui` is a
    /// library crate. Callers (`LrptImageRenderer::push_line`)
    /// degrade gracefully — log a warning and drop the line
    /// rather than killing the GTK main loop.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn new() -> Option<Self> {
        let surface = cairo::ImageSurface::create(
            cairo::Format::ARgb32,
            IMAGE_WIDTH as i32,
            MAX_LINES as i32,
        )
        .ok()?;
        Some(Self {
            surface,
            n_lines: 0,
        })
    }
}

impl Default for LrptImageRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl LrptImageRenderer {
    /// Build an empty renderer. No channels are allocated until
    /// the first `push_line` call for each APID.
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
            selection: None,
            composite_cache: None,
            composite_min_height: None,
            composite_gen: 0,
        }
    }

    /// All APIDs the renderer has seen at least one line for,
    /// in unspecified order. Used by the GTK widget to populate
    /// its channel dropdown.
    pub fn known_apids(&self) -> Vec<u16> {
        self.channels.keys().copied().collect()
    }

    /// APID currently selected for display, if any. Returns
    /// `None` when the renderer is in composite mode (or empty)
    /// — composite vs. per-APID is a single-source-of-truth
    /// enum now (per CR round 5 on PR #575), so this is just a
    /// pattern-match into the [`ActiveSelection::Apid`] variant.
    #[must_use]
    pub fn active_apid(&self) -> Option<u16> {
        match self.selection {
            Some(ActiveSelection::Apid(apid)) => Some(apid),
            _ => None,
        }
    }

    /// Set which APID's channel is shown. A no-op (returns
    /// `false`) if the renderer has never received a line for
    /// that APID — without a backing surface there's nothing to
    /// paint, and silently switching to a missing channel would
    /// leave the user staring at a blank canvas with no
    /// feedback. Successful switch atomically drops any active
    /// composite cache so the per-APID surface paints clean
    /// (no stale RGB pixels). Per CR round 5 on PR #575: the
    /// dropdown handler used to need a paired
    /// `clear_composite()` call before this; the enum-based
    /// selection makes that implicit, but the explicit call is
    /// still harmless and we leave it for readability.
    pub fn set_active_apid(&mut self, apid: u16) -> bool {
        if self.channels.contains_key(&apid) {
            self.selection = Some(ActiveSelection::Apid(apid));
            self.composite_cache = None;
            self.composite_min_height = None;
            self.composite_gen = self.composite_gen.wrapping_add(1);
            true
        } else {
            false
        }
    }

    /// Number of scan lines buffered for `apid`, or 0 if unknown.
    #[must_use]
    pub fn n_lines(&self, apid: u16) -> usize {
        self.channels.get(&apid).map_or(0, |c| c.n_lines)
    }

    /// `true` when no APID has any scan line yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.values().all(|c| c.n_lines == 0)
    }

    /// Append one scan line of width [`IMAGE_WIDTH`] to the
    /// surface for `apid`, lazy-allocating the surface on the
    /// first push for that APID. Greyscale values go into the
    /// surface's backing data as ARGB32 (B/G/R/A — Cairo's
    /// little-endian layout, alpha = `0xFF`).
    ///
    /// Returns a [`PushOutcome`] that callers (specifically
    /// [`LrptImageView::drain_new_lines`]) inspect to decide
    /// whether to advance their per-APID watermark. Pushed and
    /// permanently-dropped rows (cap reached, malformed input)
    /// advance the watermark; transient renderer failures
    /// (surface alloc, stride conversion, surface-data lock)
    /// leave the row in the source so the next poll can retry.
    /// Per `CodeRabbit` round 3 on PR #543.
    pub fn push_line(&mut self, apid: u16, pixels: &[u8]) -> PushOutcome {
        if pixels.len() != IMAGE_WIDTH {
            tracing::warn!(
                "LRPT renderer: ignoring line for APID {apid} with {} pixels (expected {IMAGE_WIDTH})",
                pixels.len(),
            );
            // Caller bug. Watermark should still advance —
            // retrying with the same malformed input will only
            // reproduce the same warn forever.
            return PushOutcome::InvalidLine;
        }
        // Lazy alloc; `ChannelSurface::new` returns `None` if
        // Cairo can't acquire the ~MAX-LINES-sized ARGB32
        // surface. Drop the line with a warn rather than
        // panicking — and report the failure as transient so
        // the next poll retries (alloc may succeed later under
        // memory pressure relief).
        let entry = match self.channels.entry(apid) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Some(surface) = ChannelSurface::new() else {
                    tracing::warn!(
                        "LRPT renderer: surface alloc failed for APID {apid}; dropping line",
                    );
                    return PushOutcome::TransientFailure;
                };
                e.insert(surface)
            }
        };
        if entry.n_lines >= MAX_LINES {
            // Surface full — advance watermark anyway. Further
            // data for this channel will keep hitting the cap
            // no matter how many times we retry.
            return PushOutcome::Capped;
        }
        let stride = match usize::try_from(entry.surface.stride()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("LRPT renderer: invalid surface stride: {e}");
                return PushOutcome::TransientFailure;
            }
        };
        let row_offset = entry.n_lines * stride;
        let mut data = match entry.surface.data() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("LRPT renderer: surface data lock failed: {e}");
                return PushOutcome::TransientFailure;
            }
        };
        for (i, &g) in pixels.iter().enumerate() {
            let pixel_offset = row_offset + i * BYTES_PER_PIXEL;
            data[pixel_offset] = g;
            data[pixel_offset + 1] = g;
            data[pixel_offset + 2] = g;
            data[pixel_offset + 3] = 0xFF;
        }
        // `data` guard drops here, flushing the surface for cairo.
        drop(data);
        entry.n_lines += 1;
        // First-ever push for any channel — auto-select it so
        // the user sees something the moment data starts
        // flowing, without having to discover the dropdown.
        // Skip when a composite is already selected; that mode
        // is authoritative (per CR round 4 on PR #575) so
        // shadow-tracking a per-APID would just be dead state.
        if self.selection.is_none() {
            self.selection = Some(ActiveSelection::Apid(apid));
            self.composite_gen = self.composite_gen.wrapping_add(1);
        }
        PushOutcome::Pushed
    }

    /// Drop all per-channel surfaces AND any cached composite.
    /// The `HashMap` allocation itself is preserved, but each
    /// ~51 MB surface is freed — callers (between-pass cleanup)
    /// typically rebuild from scratch as new channels reappear.
    /// The composite cache is also dropped so a fresh pass
    /// doesn't paint stale RGB pixels until the dropdown
    /// handler rebuilds against the new pass's per-APID
    /// surfaces. Per #547.
    pub fn clear(&mut self) {
        self.channels.clear();
        self.selection = None;
        self.composite_cache = None;
        self.composite_min_height = None;
        self.composite_gen = self.composite_gen.wrapping_add(1);
    }

    /// Mark composite mode as "selected but cache not yet
    /// built". Bumps the generation counter so any in-flight
    /// worker sees its captured generation no longer matches
    /// and discards its result. Used by the View's async path
    /// when the snapshot fails (one or more source APIDs
    /// missing/empty), and by [`Self::set_composite`] (the
    /// synchronous test path) on every error branch. Per CR
    /// round 5 on PR #575.
    pub fn mark_composite_pending(&mut self, recipe: CompositeRecipe) {
        self.selection = Some(ActiveSelection::Composite(recipe));
        self.composite_cache = None;
        self.composite_min_height = None;
        self.composite_gen = self.composite_gen.wrapping_add(1);
    }

    /// Mark composite mode as "selected, build for `target_height`
    /// is in flight". Same as [`Self::mark_composite_pending`]
    /// but pins `composite_min_height` to the snapshot's height
    /// so the 1 Hz refresh tick's `cached_min_height ==
    /// current_min_height` gate doesn't kick off a redundant
    /// worker for the same height while the first one is still
    /// running. Returns the new generation token the caller
    /// should pass to [`Self::install_composite_cache`] when
    /// the worker completes. Per CR round 5 on PR #575.
    pub fn prepare_composite_build(
        &mut self,
        recipe: CompositeRecipe,
        target_height: usize,
    ) -> u64 {
        self.selection = Some(ActiveSelection::Composite(recipe));
        self.composite_cache = None;
        self.composite_min_height = Some(target_height);
        self.composite_gen = self.composite_gen.wrapping_add(1);
        self.composite_gen
    }

    /// Install a freshly-built composite surface as the cache.
    /// Returns `true` if installed, `false` if the worker
    /// raced a selection change (selection moved away or a
    /// newer build for the same selection bumped the
    /// generation) and the surface should be dropped on the
    /// floor. Per CR round 5 on PR #575.
    pub fn install_composite_cache(
        &mut self,
        recipe: CompositeRecipe,
        expected_gen: u64,
        height: usize,
        surface: cairo::ImageSurface,
    ) -> bool {
        if self.composite_gen != expected_gen
            || self.selection != Some(ActiveSelection::Composite(recipe))
        {
            return false;
        }
        self.composite_cache = Some(CompositeSurface { surface, height });
        self.composite_min_height = Some(height);
        true
    }

    /// Synchronous one-shot composite build. Used by the test
    /// suite (which doesn't have a GTK main context) and as a
    /// fallback for callers that need the bool return value
    /// to know "did the build succeed end-to-end". Production
    /// code (the dropdown handler + 1 Hz tick) should use
    /// [`LrptImageView::set_composite`] instead, which does
    /// the same work but off the GTK thread via
    /// `gio::spawn_blocking`. Per CR rounds 1 + 5 on PR #575.
    ///
    /// On snapshot failure (any source APID missing/empty)
    /// returns `false` and leaves the renderer in
    /// composite-pending state ([`Self::mark_composite_pending`]).
    /// On Cairo allocation / surface-data lock failure, logs a
    /// warn and same — clears the cache without panicking.
    pub fn set_composite(&mut self, recipe: CompositeRecipe, image: &LrptImage) -> bool {
        // Hold the assembler lock only long enough to memcpy
        // the three source channel buffers; do the per-pixel
        // interleave + ARGB32 surface build OUTSIDE the lock so
        // the decoder thread doesn't get blocked behind a
        // full-frame walk. Per CR round 1 on PR #575.
        let snap = image.with_assembler(|a| {
            a.clone_channels_for_composite(recipe.r_apid, recipe.g_apid, recipe.b_apid)
        });
        let Some(snap) = snap else {
            tracing::debug!(
                ?recipe,
                "clone_channels_for_composite returned None — one or more source APIDs missing or empty",
            );
            self.mark_composite_pending(recipe);
            return false;
        };
        let rgb = sdr_lrpt::image::assemble_rgb_composite(
            &snap.r_pixels,
            &snap.g_pixels,
            &snap.b_pixels,
            snap.height,
        );
        let surface = match build_argb32_from_rgb(&rgb, IMAGE_WIDTH, snap.height) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?recipe, error = %e, "composite ARGB32 surface build failed");
                self.mark_composite_pending(recipe);
                return false;
            }
        };
        let generation = self.prepare_composite_build(recipe, snap.height);
        let installed = self.install_composite_cache(recipe, generation, snap.height, surface);
        debug_assert!(
            installed,
            "sync set_composite: install_composite_cache should always succeed \
             — no race window between prepare_composite_build and install",
        );
        true
    }

    /// Switch back to "nothing selected" — the next
    /// [`Self::render`] paints just the background. Per CR
    /// round 5 on PR #575: previously this only cleared
    /// composite-related fields and left a shadow-tracked
    /// `active: Option<u16>` to fall back to. With the
    /// [`ActiveSelection`] enum collapse there's no shadow
    /// state to fall back to; callers that want a per-APID
    /// fallback (the dropdown handler) call
    /// [`Self::set_active_apid`] explicitly afterwards.
    pub fn clear_composite(&mut self) {
        if matches!(self.selection, Some(ActiveSelection::Composite(_))) {
            self.selection = None;
        }
        self.composite_cache = None;
        self.composite_min_height = None;
        self.composite_gen = self.composite_gen.wrapping_add(1);
    }

    /// `true` when the renderer is currently in composite mode
    /// (a composite recipe has been activated, regardless of
    /// whether the cache is populated).
    #[must_use]
    pub fn is_composite_active(&self) -> bool {
        matches!(self.selection, Some(ActiveSelection::Composite(_)))
    }

    /// The currently-active composite recipe, if any. Used by
    /// the drain tick to re-issue
    /// [`LrptImageView::set_composite`] on every refresh tick
    /// so new lines accrue in near-real-time.
    #[must_use]
    pub fn active_composite(&self) -> Option<CompositeRecipe> {
        match self.selection {
            Some(ActiveSelection::Composite(recipe)) => Some(recipe),
            _ => None,
        }
    }

    /// `min(r_lines, g_lines, b_lines)` for the composite that
    /// is either currently cached OR currently being built by
    /// an in-flight worker. The dropdown-refresh tick reads
    /// this to skip a no-op rebuild when the current min height
    /// matches what we've already painted (or are painting).
    /// Per CR round 3 on PR #575, extended round 5 to cover
    /// the in-flight case.
    #[must_use]
    pub fn composite_min_height(&self) -> Option<usize> {
        self.composite_min_height
    }

    /// Current value of the composite generation counter. Used
    /// by the View's async path: capture before spawning a
    /// worker, pass back to [`Self::install_composite_cache`]
    /// to detect mid-flight selection changes. Per CR round 5
    /// on PR #575.
    #[must_use]
    pub fn composite_gen(&self) -> u64 {
        self.composite_gen
    }

    /// Paint the active channel's image into `cr`, scaled to fit
    /// `(width, height)` while preserving the
    /// `IMAGE_WIDTH : n_lines` aspect. Centred horizontally,
    /// top-aligned vertically.
    ///
    /// Composite mode (when [`Self::is_composite_active`] is
    /// `true` AND the cache is populated) takes precedence — the
    /// cached ARGB32 surface paints in place of any per-APID
    /// surface. Per #547.
    ///
    /// Returns `Ok(())` and paints just the background when no
    /// channel is active or the active channel has no lines —
    /// callers don't need to special-case the empty state.
    ///
    /// # Errors
    ///
    /// Returns [`ViewerError::Cairo`] on paint failure. Callers
    /// usually log and continue — drawing failures shouldn't
    /// kill the UI. Per issue #545.
    pub fn render(&self, cr: &cairo::Context, width: i32, height: i32) -> Result<(), ViewerError> {
        cr.set_source_rgb(BACKGROUND_RGB[0], BACKGROUND_RGB[1], BACKGROUND_RGB[2]);
        cr.paint().map_err(|e| ViewerError::Cairo {
            op: "background paint",
            source: e,
        })?;

        // Composite branch takes precedence when the cache is
        // populated. Per #547.
        if let Some(c) = &self.composite_cache {
            return paint_image_surface(cr, &c.surface, c.height, width, height);
        }

        // Composite is selected but the cache isn't built yet
        // (one or more source APIDs missing/empty, or the last
        // build failed / is still pending). Paint just the
        // background and stop — falling through to the per-APID
        // branch would render a single-channel greyscale image
        // while the dropdown still says "Composite — ...", which
        // the user would read as "this IS the composite". Per
        // CR round 4 on PR #575.
        let Some(ActiveSelection::Apid(apid)) = self.selection else {
            return Ok(());
        };
        let Some(channel) = self.channels.get(&apid) else {
            return Ok(());
        };
        if channel.n_lines == 0 || width <= 0 || height <= 0 {
            return Ok(());
        }
        paint_image_surface(cr, &channel.surface, channel.n_lines, width, height)
    }

    /// Save the currently-displayed image to a PNG file. Builds a
    /// one-shot tightly-sized export surface so the file doesn't
    /// carry padding rows past the real data.
    ///
    /// Composite-mode aware: when [`Self::is_composite_active`]
    /// AND the cache is populated, the cached ARGB32 composite
    /// surface is exported so the PNG matches what the user is
    /// looking at on screen. Otherwise falls back to the active
    /// per-APID greyscale surface. Per CR round 2 on PR #575.
    ///
    /// # Errors
    ///
    /// Returns [`ViewerError::NoActiveChannel`] when neither a
    /// composite nor a per-APID channel is selected,
    /// [`ViewerError::EmptyChannel`] when the active per-APID
    /// channel has no decoded rows yet,
    /// [`ViewerError::EmptyComposite`] when a composite recipe
    /// is selected but the cache isn't populated yet, or
    /// `Cairo` / `Io` / `PngEncode` on the failing step. Per
    /// issue #545.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    pub fn export_png(&self, path: &Path) -> Result<(), ViewerError> {
        // Composite branch wins when the cache is populated —
        // matches `Self::render`'s precedence so the PNG and the
        // on-screen pixels are bit-identical for the same
        // (recipe, line count) state. Per CR round 2 on PR #575.
        if let Some(c) = &self.composite_cache {
            return Self::export_png_from_surface(path, &c.surface, c.height, None);
        }
        // Composite selected but the cache isn't built yet (one
        // or more source APIDs missing/empty, the last build
        // failed, or a worker is still in flight). Surface this
        // as `EmptyComposite` rather than fall through to the
        // per-APID branch, which would silently export a
        // different image than the dropdown advertises. Per CR
        // round 4 on PR #575.
        let apid = match self.selection {
            Some(ActiveSelection::Apid(apid)) => apid,
            Some(ActiveSelection::Composite(recipe)) => {
                return Err(ViewerError::EmptyComposite {
                    recipe_name: recipe.name,
                });
            }
            None => return Err(ViewerError::NoActiveChannel),
        };
        let Some(channel) = self.channels.get(&apid) else {
            return Err(ViewerError::EmptyChannel { apid: Some(apid) });
        };
        if channel.n_lines == 0 {
            return Err(ViewerError::EmptyChannel { apid: Some(apid) });
        }
        Self::export_png_from_surface(path, &channel.surface, channel.n_lines, Some(apid))
    }

    /// Helper: write an in-memory Cairo surface to a tightly-sized
    /// PNG at `path`. Pulled out of [`Self::export_png`] so the
    /// per-APID and composite branches share the same one-shot
    /// surface + Cairo blit + `write_to_png` pipeline. The
    /// `apid` argument is informational — `None` for composite
    /// exports, `Some(apid)` for per-APID — and threads through to
    /// the success log line. Per CR round 2 on PR #575.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn export_png_from_surface(
        path: &Path,
        source: &cairo::ImageSurface,
        n_lines: usize,
        apid: Option<u16>,
    ) -> Result<(), ViewerError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
                op: "create_dir_all",
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let export_surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, IMAGE_WIDTH as i32, n_lines as i32)
                .map_err(|e| ViewerError::Cairo {
                    op: "export surface",
                    source: e,
                })?;
        let cr = cairo::Context::new(&export_surface).map_err(|e| ViewerError::Cairo {
            op: "export context",
            source: e,
        })?;
        cr.set_source_surface(source, 0.0, 0.0)
            .map_err(|e| ViewerError::Cairo {
                op: "export set_source_surface",
                source: e,
            })?;
        // IMAGE_WIDTH and n_lines are well under f64's mantissa
        // — bounded by MAX_LINES — so no real precision loss.
        #[allow(clippy::cast_precision_loss)]
        cr.rectangle(0.0, 0.0, IMAGE_WIDTH as f64, n_lines as f64);
        cr.fill().map_err(|e| ViewerError::Cairo {
            op: "export fill",
            source: e,
        })?;
        drop(cr);

        let mut file = std::fs::File::create(path).map_err(|e| ViewerError::Io {
            op: "file create",
            path: path.to_path_buf(),
            source: e,
        })?;
        export_surface.write_to_png(&mut file)?;
        tracing::info!(?path, ?apid, lines = n_lines, "LRPT image exported to PNG",);
        Ok(())
    }
}

// ─── Standalone PNG writer ─────────────────────────────────────────────

/// Write a tightly-sized PNG of greyscale `pixels` (one byte per
/// pixel, row-major, length `width * height`) to `path`.
///
/// Builds a one-shot ARGB32 surface — same Cairo path
/// `LrptImageRenderer::export_png` uses, but reading from a raw
/// pixel slice rather than a cached per-channel surface. Pulled
/// out as a free function so the LOS `SaveLrptPass` handler in
/// `window.rs` can write per-channel PNGs straight from
/// `state.lrpt_image` without going through a viewer renderer
/// — the recorder needs to save imagery whether or not the user
/// has the live viewer open. Per `CodeRabbit` round 7 on PR #543.
///
/// # Errors
///
/// Returns a [`ViewerError`] variant identifying the failing
/// step: `DimensionTooLarge` if `width` or `height` exceeds
/// `i32::MAX` (Cairo's API limit), `InvalidBuffer` if
/// `pixels.len()` doesn't match `width * height`, `ZeroSized`
/// if either dimension is 0, and `Io` / `Cairo` /
/// `SurfaceDataLock` / `InvalidStride` / `PngEncode` for the
/// downstream Cairo and filesystem failures. Per issue #545
/// (was `Result<(), String>` before).
pub fn write_greyscale_png(
    path: &Path,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> Result<(), ViewerError> {
    // Validate dimensions fit Cairo's `i32` API up front. The
    // earlier draft `as i32`-cast both, which silently wraps for
    // any usize > i32::MAX (2.1 G) into a negative or bogus
    // surface request. Practically unreachable for LRPT
    // (IMAGE_WIDTH = 1568, MAX_LINES = 8192) but
    // `write_greyscale_png` is a `pub` library function and the
    // `#[allow(cast_possible_wrap)]` would have hidden the
    // wrap, not prevented it. Per `CodeRabbit` round 9 on PR
    // #543.
    let width_i32 = i32::try_from(width).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "width",
        value: width,
    })?;
    let height_i32 = i32::try_from(height).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "height",
        value: height,
    })?;
    // Zero-size guard runs BEFORE buffer-shape validation so a
    // call like `write_greyscale_png(path, &[1], 0, 1)` reports
    // the dedicated `ZeroSized` discriminant rather than masking
    // it as a generic `InvalidBuffer`. Callers (and the user-
    // facing toast) match on these distinctly. Per CR on PR #550.
    if width == 0 || height == 0 {
        return Err(ViewerError::ZeroSized);
    }
    let expected = width
        .checked_mul(height)
        .ok_or(ViewerError::DimensionTooLarge {
            dim: "width × height",
            value: usize::MAX,
        })?;
    if pixels.len() != expected {
        return Err(ViewerError::InvalidBuffer(format!(
            "greyscale PNG pixel buffer length {} doesn't match width*height ({}*{} = {})",
            pixels.len(),
            width,
            height,
            expected,
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
            op: "create_dir_all",
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width_i32, height_i32)
        .map_err(|e| ViewerError::Cairo {
            op: "export surface",
            source: e,
        })?;
    {
        let stride = usize::try_from(surface.stride())?;
        let mut data = surface.data()?;
        for row in 0..height {
            let row_offset = row * stride;
            let pixel_row_offset = row * width;
            for col in 0..width {
                let g = pixels[pixel_row_offset + col];
                let pixel_offset = row_offset + col * BYTES_PER_PIXEL;
                data[pixel_offset] = g;
                data[pixel_offset + 1] = g;
                data[pixel_offset + 2] = g;
                data[pixel_offset + 3] = 0xFF;
            }
        }
    }
    let mut file = std::fs::File::create(path).map_err(|e| ViewerError::Io {
        op: "file create",
        path: path.to_path_buf(),
        source: e,
    })?;
    surface.write_to_png(&mut file)?;
    Ok(())
}

/// Write a tightly-sized PNG of interleaved RGB `pixels` (3 bytes
/// per pixel — R, G, B — row-major, length `width * height * 3`)
/// to `path`. Mirror of [`write_greyscale_png`] for the LRPT
/// composite LOS-save path: the recorder snapshots
/// `ImageAssembler::composite_rgb` output (which already returns
/// interleaved RGB) and hands it straight here. Same Cairo
/// `write_to_png` pipeline as the greyscale path so error
/// semantics line up across both writers. Per #547.
///
/// Cairo's PNG encoder emits the surface as RGBA when the format
/// is `ARgb32`; alpha is unconditionally `0xFF` in this writer
/// (no transparency in LRPT imagery), so consumers that only
/// understand RGB read the same pixels as if alpha were absent.
///
/// # Errors
///
/// Returns the same [`ViewerError`] variants as
/// [`write_greyscale_png`]: `DimensionTooLarge`, `ZeroSized`,
/// `InvalidBuffer`, `Io`, `Cairo`, `SurfaceDataLock`,
/// `InvalidStride`, `PngEncode`.
pub fn write_rgb_png(
    path: &Path,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> Result<(), ViewerError> {
    // Validate dimensions fit Cairo's `i32` API up front, same
    // shape as `write_greyscale_png`. Practically unreachable
    // for LRPT (IMAGE_WIDTH = 1568, MAX_LINES = 8192) but the
    // defensive try_from keeps `write_rgb_png` honest as a `pub`
    // library function — same rationale as the greyscale
    // writer's round-9 fix on PR #543.
    let width_i32 = i32::try_from(width).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "width",
        value: width,
    })?;
    let height_i32 = i32::try_from(height).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "height",
        value: height,
    })?;
    // Zero-size guard runs BEFORE buffer-shape validation so a
    // call with zero dimensions reports `ZeroSized` rather than
    // masking it as a generic `InvalidBuffer` length-mismatch —
    // same ordering as `write_greyscale_png` (per CR on PR
    // #550).
    if width == 0 || height == 0 {
        return Err(ViewerError::ZeroSized);
    }
    let expected = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(3))
        .ok_or(ViewerError::DimensionTooLarge {
            dim: "width × height × 3",
            value: usize::MAX,
        })?;
    if pixels.len() != expected {
        return Err(ViewerError::InvalidBuffer(format!(
            "RGB PNG pixel buffer length {} doesn't match width*height*3 ({}*{}*3 = {})",
            pixels.len(),
            width,
            height,
            expected,
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ViewerError::Io {
            op: "create_dir_all",
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width_i32, height_i32)
        .map_err(|e| ViewerError::Cairo {
            op: "export surface",
            source: e,
        })?;
    {
        let stride = usize::try_from(surface.stride())?;
        let mut data = surface.data()?;
        for row in 0..height {
            let row_offset = row * stride;
            let pixel_row_offset = row * width * 3;
            for col in 0..width {
                let r = pixels[pixel_row_offset + col * 3];
                let g = pixels[pixel_row_offset + col * 3 + 1];
                let b = pixels[pixel_row_offset + col * 3 + 2];
                let pixel_offset = row_offset + col * BYTES_PER_PIXEL;
                // Cairo ARGB32 little-endian byte order:
                //   data[0] = B, data[1] = G, data[2] = R, data[3] = A.
                data[pixel_offset] = b;
                data[pixel_offset + 1] = g;
                data[pixel_offset + 2] = r;
                data[pixel_offset + 3] = 0xFF;
            }
        }
    }
    let mut file = std::fs::File::create(path).map_err(|e| ViewerError::Io {
        op: "file create",
        path: path.to_path_buf(),
        source: e,
    })?;
    surface.write_to_png(&mut file)?;
    Ok(())
}

/// Paint `surface` (an `IMAGE_WIDTH × n_lines` Cairo image
/// surface) into `cr`, scaled to fit `(width, height)` while
/// preserving the `IMAGE_WIDTH : n_lines` aspect. Centred
/// horizontally, top-aligned vertically. Pulled out of
/// [`LrptImageRenderer::render`] so the per-APID and composite
/// paint paths share the same scale logic — only the source
/// surface differs. Per #547.
#[allow(clippy::cast_precision_loss)]
fn paint_image_surface(
    cr: &cairo::Context,
    surface: &cairo::ImageSurface,
    n_lines: usize,
    width: i32,
    height: i32,
) -> Result<(), ViewerError> {
    if n_lines == 0 || width <= 0 || height <= 0 {
        return Ok(());
    }
    let img_w = IMAGE_WIDTH as f64;
    let img_h = n_lines as f64;
    let scale = (f64::from(width) / img_w).min(f64::from(height) / img_h);
    let off_x = (f64::from(width) - img_w * scale) / 2.0;

    cr.save().map_err(|e| ViewerError::Cairo {
        op: "save",
        source: e,
    })?;
    cr.translate(off_x, 0.0);
    cr.scale(scale, scale);
    cr.set_source_surface(surface, 0.0, 0.0)
        .map_err(|e| ViewerError::Cairo {
            op: "set_source_surface",
            source: e,
        })?;
    cr.rectangle(0.0, 0.0, img_w, img_h);
    cr.fill().map_err(|e| ViewerError::Cairo {
        op: "image fill",
        source: e,
    })?;
    cr.restore().map_err(|e| ViewerError::Cairo {
        op: "restore",
        source: e,
    })?;
    Ok(())
}

/// Build a Cairo `ARgb32` surface from an interleaved RGB byte
/// buffer (3 bytes per pixel, row-major). Cairo's native ARGB32
/// on little-endian hosts is laid out as B, G, R, A in memory;
/// every supported sdr-rs platform (`x86_64`, `aarch64`) is
/// little-endian, so the byte rewrite below assumes that layout.
/// Per #547.
///
/// # Errors
///
/// Returns a [`ViewerError`] identifying the failing step —
/// `set_composite` logs and falls back gracefully on any non-`Ok`
/// outcome, so callers don't need to distinguish error cases.
/// Variants used: `DimensionTooLarge` (width / height exceeds
/// `i32::MAX` or `width * height * 3` overflows usize),
/// `InvalidBuffer` (RGB buffer length doesn't match
/// `width * height * 3`), `Cairo` (surface alloc),
/// `InvalidStride` (Cairo stride conversion), and
/// `SurfaceDataLock` (the surface's data borrow). Per CR round 2
/// on PR #575 — was `Result<_, String>` before, in violation of
/// the library-crate "no stringly-typed errors" rule.
fn build_argb32_from_rgb(
    rgb: &[u8],
    width: usize,
    height: usize,
) -> Result<cairo::ImageSurface, ViewerError> {
    let width_i32 = i32::try_from(width).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "composite width",
        value: width,
    })?;
    let height_i32 = i32::try_from(height).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "composite height",
        value: height,
    })?;
    let expected = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(3))
        .ok_or(ViewerError::DimensionTooLarge {
            dim: "composite width × height × 3",
            value: usize::MAX,
        })?;
    if rgb.len() != expected {
        return Err(ViewerError::InvalidBuffer(format!(
            "composite RGB buffer length {} doesn't match width*height*3 ({width} * {height} * 3 = {expected})",
            rgb.len(),
        )));
    }
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width_i32, height_i32)
        .map_err(|e| ViewerError::Cairo {
            op: "composite ARGB32 surface",
            source: e,
        })?;
    let stride = usize::try_from(surface.stride())?;
    {
        let mut data = surface.data()?;
        for y in 0..height {
            let src_row = y * width * 3;
            let dst_row = y * stride;
            for x in 0..width {
                let r = rgb[src_row + x * 3];
                let g = rgb[src_row + x * 3 + 1];
                let b = rgb[src_row + x * 3 + 2];
                let dst = dst_row + x * BYTES_PER_PIXEL;
                // Cairo ARGB32 little-endian byte order:
                //   data[0] = B, data[1] = G, data[2] = R, data[3] = A.
                data[dst] = b;
                data[dst + 1] = g;
                data[dst + 2] = r;
                data[dst + 3] = 0xFF;
            }
        }
    }
    surface.flush();
    Ok(surface)
}

/// Pack the three source channels of `snap` directly into a
/// flat `Vec<u8>` of BGRA bytes (Cairo's native ARGB32 little-
/// endian byte order: B / G / R / A per pixel, row-major,
/// stride = `width * 4`). Pure CPU; no Cairo state touched.
/// Used by [`LrptImageView::set_composite`]'s worker thread —
/// `cairo::ImageSurface` isn't `Send` so we can't build the
/// surface inside `gio::spawn_blocking`; instead we hand the
/// worker the snapshot, get back this byte buffer, and wrap
/// it via [`build_argb32_surface_from_bgra`] on the main
/// thread. Per CR round 5 on PR #575.
///
/// # Panics
///
/// Asserts (debug builds only) that each source channel has
/// exactly `IMAGE_WIDTH * snap.height` bytes — that's the
/// `clone_channels_for_composite` contract (truncated
/// `CompositeSnapshot`). The release path silently does the
/// wrong thing if a caller violates the contract; the assert
/// catches the bug in CI / dev builds.
fn build_bgra_composite_bytes(snap: &sdr_lrpt::image::CompositeSnapshot) -> Vec<u8> {
    let width = IMAGE_WIDTH;
    let height = snap.height;
    let n = width * height;
    debug_assert_eq!(
        snap.r_pixels.len(),
        n,
        "r_pixels length doesn't match width*height",
    );
    debug_assert_eq!(
        snap.g_pixels.len(),
        n,
        "g_pixels length doesn't match width*height",
    );
    debug_assert_eq!(
        snap.b_pixels.len(),
        n,
        "b_pixels length doesn't match width*height",
    );
    let mut bgra = Vec::with_capacity(n * BYTES_PER_PIXEL);
    for i in 0..n {
        bgra.push(snap.b_pixels[i]);
        bgra.push(snap.g_pixels[i]);
        bgra.push(snap.r_pixels[i]);
        bgra.push(0xFF);
    }
    bgra
}

/// Wrap a `width * height * 4`-byte BGRA buffer (Cairo's
/// ARGB32 native byte order on little-endian hosts) in a
/// `cairo::ImageSurface` via `create_for_data`. Re-packs to
/// Cairo's required stride if `stride_for_width(ARgb32, width)`
/// differs from `width * 4` (in practice never, for ARGB32 +
/// the LRPT 1568-pixel width — but the re-pack guards against
/// platforms or future widths where Cairo demands extra
/// padding). Per CR round 5 on PR #575.
///
/// Used by [`LrptImageView::set_composite`]'s post-await
/// callback to install the worker's BGRA bytes as the
/// composite cache surface.
///
/// # Errors
///
/// Same set as [`build_argb32_from_rgb`]: `DimensionTooLarge`,
/// `InvalidBuffer`, `Cairo`. Callers (the View's worker
/// callback) log and reset the in-flight build — they don't
/// need to distinguish.
#[allow(clippy::cast_sign_loss)]
fn build_argb32_surface_from_bgra(
    bgra: Vec<u8>,
    width: usize,
    height: usize,
) -> Result<cairo::ImageSurface, ViewerError> {
    let width_i32 = i32::try_from(width).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "composite width",
        value: width,
    })?;
    let height_i32 = i32::try_from(height).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "composite height",
        value: height,
    })?;
    let expected = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
        .ok_or(ViewerError::DimensionTooLarge {
            dim: "composite width × height × 4",
            value: usize::MAX,
        })?;
    if bgra.len() != expected {
        return Err(ViewerError::InvalidBuffer(format!(
            "composite BGRA buffer length {} doesn't match width*height*4 ({width} * {height} * 4 = {expected})",
            bgra.len(),
        )));
    }
    let cairo_width = u32::try_from(width).map_err(|_| ViewerError::DimensionTooLarge {
        dim: "composite width",
        value: width,
    })?;
    let stride = cairo::Format::ARgb32
        .stride_for_width(cairo_width)
        .map_err(|e| ViewerError::Cairo {
            op: "composite ARGB32 stride",
            source: e,
        })?;
    let packed_stride = i32::try_from(width.checked_mul(BYTES_PER_PIXEL).ok_or(
        ViewerError::DimensionTooLarge {
            dim: "composite width × 4",
            value: usize::MAX,
        },
    )?)
    .map_err(|_| ViewerError::DimensionTooLarge {
        dim: "composite packed stride",
        value: width * BYTES_PER_PIXEL,
    })?;
    // Common case (ARGB32 at any reasonable width): Cairo's
    // stride matches our packed layout — hand the buffer over
    // verbatim. Otherwise re-pack with the padding Cairo wants.
    let buf = if stride == packed_stride {
        bgra
    } else {
        let stride_usize = stride as usize;
        let row_bytes = width * BYTES_PER_PIXEL;
        let mut padded = vec![0u8; stride_usize * height];
        for row in 0..height {
            let src = row * row_bytes;
            let dst = row * stride_usize;
            padded[dst..dst + row_bytes].copy_from_slice(&bgra[src..src + row_bytes]);
        }
        padded
    };
    cairo::ImageSurface::create_for_data(buf, cairo::Format::ARgb32, width_i32, height_i32, stride)
        .map_err(|e| ViewerError::Cairo {
            op: "composite ARGB32 surface (create_for_data)",
            source: e,
        })
}

// ─── GTK widget ────────────────────────────────────────────────────────

/// Live Meteor LRPT image viewer widget.
///
/// Holds a `DrawingArea`, a renderer, the shared
/// [`LrptImage`] handle the DSP thread is writing to, and a
/// poll-tick `glib` source. The poll tick drains any new scan
/// lines from the shared image into the renderer and queues a
/// redraw.
///
/// `Clone` is derived (existing pattern) so toolbar callbacks
/// and the channel dropdown can hold their own handles. Every
/// field is internally `Rc`-shared, so cloning is a refcount
/// bump.
#[derive(Clone)]
pub struct LrptImageView {
    drawing_area: gtk4::DrawingArea,
    renderer: Rc<RefCell<LrptImageRenderer>>,
    image: LrptImage,
    paused: Rc<Cell<bool>>,
    /// Per-APID watermark: how many lines have already been
    /// pulled from the shared image into the renderer. Mirrors
    /// the watermark map in the DSP-side `LrptDecoder` — both
    /// sides need it so the same line isn't pushed twice (and
    /// so the viewer's poll tick is O(new lines), not O(total
    /// lines)).
    last_seen_lines: Rc<RefCell<HashMap<u16, usize>>>,
    /// `glib` source IDs of timeouts spawned by the view (the
    /// drain tick) and by `open_lrpt_viewer_window` (the
    /// channel-dropdown refresh tick). [`Self::shutdown`]
    /// removes them all so the closures' `Rc` chains drop and
    /// the view + ~51 MB-per-channel surfaces don't leak past
    /// the window's close-request. Per `CodeRabbit` round 1 on
    /// PR #543.
    timeout_ids: Rc<RefCell<Vec<glib::SourceId>>>,
}

impl LrptImageView {
    /// Build a view bound to the given shared image. Spawns a
    /// poll tick on the GTK main context that drains new lines
    /// every [`POLL_INTERVAL_MS`].
    #[must_use]
    pub fn new(image: LrptImage) -> Self {
        let renderer = Rc::new(RefCell::new(LrptImageRenderer::new()));
        let paused = Rc::new(Cell::new(false));
        let last_seen_lines: Rc<RefCell<HashMap<u16, usize>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let timeout_ids: Rc<RefCell<Vec<glib::SourceId>>> = Rc::new(RefCell::new(Vec::new()));

        let drawing_area = gtk4::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        let renderer_for_draw = Rc::clone(&renderer);
        drawing_area.set_draw_func(move |_area, cr, w, h| {
            if let Err(e) = renderer_for_draw.borrow().render(cr, w, h) {
                tracing::warn!("LRPT render failed: {e}");
            }
        });

        let view = Self {
            drawing_area,
            renderer,
            image,
            paused,
            last_seen_lines,
            timeout_ids,
        };

        // Poll tick: drain new lines + queue redraw on change.
        let view_for_tick = view.clone();
        let drain_id = glib::timeout_add_local(
            std::time::Duration::from_millis(u64::from(POLL_INTERVAL_MS)),
            move || {
                view_for_tick.drain_new_lines();
                glib::ControlFlow::Continue
            },
        );
        view.timeout_ids.borrow_mut().push(drain_id);

        view
    }

    /// Register an external `glib` source (e.g. the
    /// channel-dropdown refresh tick spawned by
    /// [`open_lrpt_viewer_window`]) so it gets cleaned up by
    /// [`Self::shutdown`] alongside the internal drain tick.
    pub fn register_source(&self, id: glib::SourceId) {
        self.timeout_ids.borrow_mut().push(id);
    }

    /// Cancel every registered `glib` source. Called on the
    /// viewer window's `close-request` so the timeout closures
    /// drop their `Rc` clones of the view's inner state — without
    /// this, the view + ~51 MB-per-channel surfaces stay alive in
    /// the main context until the application exits. Safe to
    /// call more than once (subsequent calls are no-ops because
    /// the `Vec` is drained).
    pub fn shutdown(&self) {
        for id in std::mem::take(&mut *self.timeout_ids.borrow_mut()) {
            id.remove();
        }
    }

    /// The underlying `GtkDrawingArea`. Pack into a layout
    /// container, wrap in a `ScrolledWindow`, etc.
    #[must_use]
    pub fn drawing_area(&self) -> &gtk4::DrawingArea {
        &self.drawing_area
    }

    /// All APIDs the renderer has seen at least one line for.
    /// Wraps the renderer's `known_apids` for callers that hold
    /// only a `LrptImageView` (the dropdown updater).
    #[must_use]
    pub fn known_apids(&self) -> Vec<u16> {
        self.renderer.borrow().known_apids()
    }

    /// Switch which APID's channel is displayed. Returns `false`
    /// (no-op) if the renderer has never seen a line for that
    /// APID — see [`LrptImageRenderer::set_active_apid`] for
    /// rationale. Queues a redraw on success.
    pub fn set_active_apid(&self, apid: u16) -> bool {
        let ok = self.renderer.borrow_mut().set_active_apid(apid);
        if ok {
            self.drawing_area.queue_draw();
        }
        ok
    }

    /// Currently-displayed APID, if any.
    #[must_use]
    pub fn active_apid(&self) -> Option<u16> {
        self.renderer.borrow().active_apid()
    }

    /// Switch the viewer to composite mode for `recipe` and
    /// kick off an off-thread rebuild of the cached ARGB32
    /// surface. Returns `true` if the snapshot succeeded (all
    /// three source APIDs have data — the worker will install
    /// the cache when it returns); `false` if the snapshot
    /// failed (one or more source APIDs missing/empty). The
    /// canvas always queues a redraw immediately so background-
    /// vs-stale-pixels stays consistent.
    ///
    /// Two-phase work split:
    ///
    /// - **GTK main thread (synchronous, fast):** clone the
    ///   three source channel buffers under the assembler lock
    ///   (3 memcpy of `IMAGE_WIDTH * height` bytes), then mark
    ///   the renderer's selection as "composite, build for
    ///   `snap.height` in flight" and capture the generation
    ///   token. Tens of milliseconds at most, even on a full
    ///   pass.
    /// - **Worker thread (`gio::spawn_blocking`):** per-pixel
    ///   R/G/B interleave (`assemble_rgb_composite`) + ARGB32
    ///   surface build (`build_argb32_from_rgb`). Tens of
    ///   milliseconds on a full pass — that's what we're
    ///   moving off the GTK main loop. Returns the freshly-
    ///   built `cairo::ImageSurface`.
    /// - **Main thread (post-await):** if the renderer's
    ///   generation still matches the captured token (no
    ///   selection change mid-flight), call
    ///   [`LrptImageRenderer::install_composite_cache`] to
    ///   adopt the surface and queue a redraw. Otherwise drop
    ///   the surface; the user has moved on.
    ///
    /// Per CR round 5 on PR #575: previously the per-pixel
    /// interleave + Cairo surface build ran synchronously on
    /// the GTK main thread on every dropdown click + every
    /// 1 Hz refresh tick. Composite mode could hitch the UI on
    /// a full pass.
    pub fn set_composite(&self, recipe: CompositeRecipe) -> bool {
        // Phase 1 — under the assembler lock: snapshot the
        // three source channels into owned `Vec<u8>`s. Lock
        // released as soon as the closure returns.
        let snap = self.image.with_assembler(|a| {
            a.clone_channels_for_composite(recipe.r_apid, recipe.g_apid, recipe.b_apid)
        });
        let Some(snap) = snap else {
            tracing::debug!(
                ?recipe,
                "clone_channels_for_composite returned None — one or more source APIDs missing or empty",
            );
            self.renderer.borrow_mut().mark_composite_pending(recipe);
            self.drawing_area.queue_draw();
            return false;
        };
        // Pin the in-flight build height in the renderer so
        // the 1 Hz refresh-tick gate (`cached_min_height ==
        // current_min_height`) doesn't kick off a duplicate
        // worker for the same height while this one runs.
        // Capture the generation token at the same time;
        // mismatch on completion = stale = drop on the floor.
        let target_height = snap.height;
        let captured_gen = self
            .renderer
            .borrow_mut()
            .prepare_composite_build(recipe, target_height);
        // Always queue a redraw — even before the worker
        // returns, the background paint covers any previous
        // render's pixels so the user sees the canvas reset
        // rather than stale composite data hanging around
        // until the new surface lands.
        self.drawing_area.queue_draw();
        // Phase 2 — off the main thread: assemble RGB +
        // ARGB32. Phase 3 (post-await) runs back on the main
        // thread.
        let renderer = Rc::clone(&self.renderer);
        let drawing_area = self.drawing_area.clone();
        glib::spawn_future_local(async move {
            // Worker: pure CPU. Reads three source channels'
            // owned `Vec<u8>`s from the captured `snap` (no
            // mutex), packs into a flat BGRA `Vec<u8>` ready
            // for Cairo. Returns the bytes — `cairo::ImageSurface`
            // isn't `Send` so the surface itself is built on
            // the main thread post-await.
            let bytes_result = gio::spawn_blocking(move || build_bgra_composite_bytes(&snap)).await;
            // Main thread: wrap the worker's BGRA bytes in a
            // Cairo surface via `create_for_data`. Cheap (no
            // memcpy in the common stride case) — the heavy
            // per-pixel pack already ran on the worker.
            let surface_result = match bytes_result {
                Ok(bgra) => build_argb32_surface_from_bgra(bgra, IMAGE_WIDTH, target_height),
                Err(panic) => {
                    tracing::warn!(?recipe, "composite worker panicked: {panic:?}");
                    let mut r = renderer.borrow_mut();
                    if r.composite_gen() == captured_gen {
                        r.mark_composite_pending(recipe);
                    }
                    return;
                }
            };
            match surface_result {
                Ok(surface) => {
                    let installed = renderer.borrow_mut().install_composite_cache(
                        recipe,
                        captured_gen,
                        target_height,
                        surface,
                    );
                    if installed {
                        drawing_area.queue_draw();
                    } else {
                        tracing::debug!(
                            ?recipe,
                            captured_gen,
                            "stale composite worker — selection changed mid-flight, dropping built surface",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(?recipe, error = %e, "composite ARGB32 surface build failed");
                    // Reset the in-flight target so the next
                    // 1 Hz tick retries — only if the user
                    // hasn't moved on. Gate on generation match
                    // to avoid clobbering a newer build that
                    // started while ours was running.
                    let mut r = renderer.borrow_mut();
                    if r.composite_gen() == captured_gen {
                        r.mark_composite_pending(recipe);
                    }
                }
            }
        });
        true
    }

    /// Drop composite mode; subsequent renders fall back to the
    /// active per-APID channel. Queues a redraw so the canvas
    /// updates immediately. Per #547.
    pub fn clear_composite(&self) {
        self.renderer.borrow_mut().clear_composite();
        self.drawing_area.queue_draw();
    }

    /// `true` when composite mode is currently active.
    #[must_use]
    pub fn is_composite_active(&self) -> bool {
        self.renderer.borrow().is_composite_active()
    }

    /// The active composite recipe, if any.
    #[must_use]
    pub fn active_composite(&self) -> Option<CompositeRecipe> {
        self.renderer.borrow().active_composite()
    }

    /// `min(r_lines, g_lines, b_lines)` for `recipe` queried
    /// against the live shared image *right now*. Returns `None`
    /// if any source APID is missing or empty — matches
    /// [`sdr_lrpt::image::ImageAssembler::clone_channels_for_composite`]'s
    /// contract so the dropdown-refresh tick can compare this
    /// directly to [`Self::cached_composite_min_height`] and skip
    /// a no-op rebuild. Per CR round 3 on PR #575.
    #[must_use]
    pub fn current_composite_min_height(&self, recipe: CompositeRecipe) -> Option<usize> {
        self.image.with_assembler(|a| {
            let r = a.channel(recipe.r_apid)?.lines;
            let g = a.channel(recipe.g_apid)?.lines;
            let b = a.channel(recipe.b_apid)?.lines;
            let m = r.min(g).min(b);
            if m == 0 { None } else { Some(m) }
        })
    }

    /// The min height the renderer's cached composite surface
    /// was built against, or `None` if there's no current cache.
    /// Used by the dropdown-refresh tick to decide whether a
    /// rebuild would change anything. Per CR round 3 on PR #575.
    #[must_use]
    pub fn cached_composite_min_height(&self) -> Option<usize> {
        self.renderer.borrow().composite_min_height()
    }

    /// Pull every scan line that's new since the last call out
    /// of the shared [`LrptImage`] and into the per-APID
    /// renderer surfaces. Queues a single redraw if anything
    /// changed and the view isn't paused.
    ///
    /// `with_assembler` holds the shared mutex while the line
    /// copy runs, so we keep the closure minimal — just memcpy
    /// the row slices, no rendering work — to avoid blocking
    /// the DSP thread on the lock for any longer than the
    /// strict copy time.
    pub fn drain_new_lines(&self) {
        // Two-phase to keep the shared `LrptImage` mutex hold
        // bounded. Phase 1 (under lock): walk the assembler and
        // copy out the new rows per APID into owned `Vec<u8>`s.
        // Phase 2 (lock released): hand the rows to the renderer,
        // which may lazy-alloc a ~51 MB Cairo surface and acquire
        // its surface-data lock — neither operation is fast
        // enough to hold the assembler mutex across, since that
        // would stall the DSP-thread writer behind it. Per
        // `CodeRabbit` round 12 on PR #543.
        struct PendingChannel {
            apid: u16,
            already: usize,
            /// Flat tail of the channel's pixel buffer — every
            /// row from `already` to `min(channel.lines,
            /// available_lines)` packed contiguously, ready for
            /// `chunks_exact(IMAGE_WIDTH)` in phase 2. One heap
            /// alloc per APID per drain instead of one per row;
            /// matters on viewer reopen mid-pass when there can
            /// be thousands of unseen rows for a single APID
            /// and the per-row alloc would churn the allocator
            /// at 4 Hz under the shared-image mutex. Per
            /// `CodeRabbit` round 17 on PR #543.
            pixels: Vec<u8>,
        }

        // Phase 1 — under shared-image lock.
        let pending: Vec<PendingChannel> = {
            let last_seen = self.last_seen_lines.borrow();
            let mut acc: Vec<PendingChannel> = Vec::new();
            self.image.with_assembler(|a| {
                for (&apid, channel) in a.channels() {
                    let already = last_seen.get(&apid).copied().unwrap_or(0);
                    if channel.lines <= already {
                        continue;
                    }
                    // Defensive — see lrpt_decoder::harvest_new_lines
                    // for the parallel guard. Structurally
                    // unreachable; the warn protects against
                    // a future refactor of the assembler buffer
                    // that drops the "pixels grows by full-line
                    // increments" invariant.
                    let available_lines = channel.pixels.len() / IMAGE_WIDTH;
                    if available_lines < channel.lines {
                        tracing::warn!(
                            "LRPT view: channel {apid} pixel buffer shorter than expected; truncating at line {available_lines} (claimed lines = {})",
                            channel.lines,
                        );
                    }
                    let end_line = channel.lines.min(available_lines);
                    if end_line <= already {
                        continue;
                    }
                    let start = already * IMAGE_WIDTH;
                    let end = end_line * IMAGE_WIDTH;
                    acc.push(PendingChannel {
                        apid,
                        already,
                        pixels: channel.pixels[start..end].to_vec(),
                    });
                }
            });
            acc
        };

        // Phase 2 — outside the shared-image lock.
        //
        // Only the renderer's currently-active APID is painted
        // by `LrptImageRenderer::render`, so the redraw should
        // fire ONLY when that channel got a row this tick.
        // Hidden APIDs that just gained rows are off-screen —
        // their data lands in the per-channel surface but isn't
        // visible until the user picks them in the dropdown,
        // and the dropdown's own selected_notify handler will
        // queue a redraw when that happens. Per `CodeRabbit`
        // round 16 on PR #543.
        //
        // The auto-select transition (active was None, first
        // ever push promotes it to Some(apid)) is covered by
        // the per-channel comparison below: after `push_line`
        // the renderer's `active_apid()` matches `p.apid`, so
        // the same `painted_any && active == Some(p.apid)` gate
        // catches the auto-select case naturally.
        let mut visible_dirty = false;
        let mut last_seen = self.last_seen_lines.borrow_mut();
        let mut renderer = self.renderer.borrow_mut();
        for p in pending {
            // Track lines actually consumed so the watermark
            // doesn't advance past either the bounds-guard
            // skip path OR a transient renderer failure
            // (surface alloc / stride / lock). Same shape as
            // `lrpt_decoder::harvest_new_lines` on the DSP
            // side, plus `PushOutcome::consumed()` for the
            // renderer-side failure case. Per `CodeRabbit`
            // rounds 2 + 3 on PR #543.
            //
            // `painted_any` only flips on `PushOutcome::Pushed`.
            // `Capped` / `InvalidLine` advance the watermark (so
            // the row is "consumed" — see `PushOutcome::consumed`)
            // but don't change the visible canvas, and
            // `TransientFailure` doesn't even advance. Without
            // this distinction, a channel parked at MAX_LINES
            // would queue a redraw every 250 ms tick forever —
            // wasted GPU work for an unchanged image. Per
            // `CodeRabbit` round 9 on PR #543.
            let mut painted_any = false;
            let mut pushed = p.already;
            // `chunks_exact` views the flat tail buffer as
            // per-row slices without further allocation. Per
            // `CodeRabbit` round 17 on PR #543.
            for (offset, row) in p.pixels.chunks_exact(IMAGE_WIDTH).enumerate() {
                let outcome = renderer.push_line(p.apid, row);
                if !outcome.consumed() {
                    // Transient failure — leave this row in the
                    // source so the next poll retries.
                    break;
                }
                if matches!(outcome, PushOutcome::Pushed) {
                    painted_any = true;
                }
                pushed = p.already + offset + 1;
            }
            last_seen.insert(p.apid, pushed);
            if painted_any && renderer.active_apid() == Some(p.apid) {
                visible_dirty = true;
            }
        }
        drop(renderer);
        drop(last_seen);

        if visible_dirty && !self.paused.get() {
            self.drawing_area.queue_draw();
        }
    }

    /// Clear all buffered lines and reset the watermark map,
    /// AND clear the backing shared `LrptImage` so the next
    /// drain tick can't replay any rows that were still in the
    /// shared assembler at the time of the clear. Without that,
    /// reopening the viewer mid-pass — or starting a new pass
    /// while the wiring layer hasn't yet cleared the shared
    /// image itself — would repopulate the canvas with the
    /// previous pass's pixels and contaminate later exports.
    /// Per `CodeRabbit` round 1 on PR #543.
    ///
    /// Between-pass cleanup; the next pass starts on a clean
    /// canvas. Idempotent — calling twice is harmless.
    pub fn clear(&self) {
        self.image.clear();
        self.renderer.borrow_mut().clear();
        self.last_seen_lines.borrow_mut().clear();
        self.drawing_area.queue_draw();
    }

    /// Toggle pause / resume. Pausing freezes the visible
    /// canvas; new lines pulled while paused still accumulate
    /// in the renderer (so nothing is lost) and become visible
    /// on resume via a forced single redraw.
    pub fn set_paused(&self, paused: bool) {
        let was_paused = self.paused.replace(paused);
        if was_paused && !paused {
            self.drawing_area.queue_draw();
        }
    }

    /// `true` if the view is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Save the active channel's image to a PNG. Same error
    /// semantics as [`LrptImageRenderer::export_png`].
    ///
    /// Drains any pending rows from the shared `LrptImage`
    /// into the renderer first, so the export captures the tail
    /// of the pass even if it arrived after the most recent
    /// poll tick. Without this, an immediate-export flow would
    /// systematically miss the last fraction-of-a-second of
    /// decoded data. Per `CodeRabbit` round 1 on PR #543.
    ///
    /// **Main-thread only.** `drain_new_lines` invokes
    /// `DrawingArea::queue_draw`, which GTK4 requires on the
    /// main thread, so this method cannot be moved to
    /// `gio::spawn_blocking` directly. It also performs
    /// synchronous Cairo PNG encoding + filesystem I/O — large
    /// images (~50 MB cap) will freeze the GTK main loop while
    /// it runs.
    ///
    /// For off-main-thread use the production paths take two
    /// different routes:
    ///
    /// - The manual Export PNG button in
    ///   [`open_lrpt_viewer_window`] calls
    ///   [`Self::snapshot_active_channel`] on the main thread
    ///   (cheap mutex-clone, also drains rows + queues the
    ///   redraw), then writes the PNG inside
    ///   `gio::spawn_blocking` via [`write_greyscale_png`].
    /// - The recorder's `RecorderAction::SaveLrptPass` handler
    ///   in `window.rs` snapshots per-APID `ChannelBuffer`s
    ///   directly from `AppState::lrpt_image` (it doesn't go
    ///   through the viewer at all — the LOS save needs to
    ///   work even when the user has closed the window
    ///   mid-pass), then writes one PNG per channel inside
    ///   `gio::spawn_blocking` via the same
    ///   [`write_greyscale_png`].
    ///
    /// Kept as a convenience for any future caller that
    /// genuinely wants the synchronous main-thread path (small
    /// test exports, scripted batch flows). Per `CodeRabbit`
    /// rounds 15 + 16 on PR #543.
    ///
    /// # Errors
    ///
    /// Propagates any [`ViewerError`] from the underlying
    /// renderer (per issue #545 — was `Result<(), String>`
    /// before).
    pub fn export_png(&self, path: &Path) -> Result<(), ViewerError> {
        self.drain_new_lines();
        self.renderer.borrow().export_png(path)
    }

    /// Snapshot the currently-active channel's pixel data into
    /// an owned `(apid, ChannelBuffer)` pair. Used by callers that
    /// only ever need the per-APID greyscale path (the composite-
    /// aware export button uses [`Self::snapshot_for_export`]
    /// instead). Drains pending rows from the shared `LrptImage`
    /// first so the snapshot captures the tail of the pass.
    ///
    /// Returns `None` if no APID is currently selected, or if
    /// the active APID has no decoded rows in the shared image.
    pub fn snapshot_active_channel(&self) -> Option<(u16, sdr_lrpt::image::ChannelBuffer)> {
        self.drain_new_lines();
        let apid = self.renderer.borrow().active_apid()?;
        let snap = self.image.snapshot_channel(apid)?;
        if snap.lines == 0 {
            return None;
        }
        Some((apid, snap))
    }

    /// Snapshot the viewer's current display state for off-main-
    /// thread PNG export. Returns either an [`ExportSnapshot::Channel`]
    /// (per-APID greyscale path, same as
    /// [`Self::snapshot_active_channel`]) or an
    /// [`ExportSnapshot::Composite`] (RGB composite path) depending
    /// on whether the user has a composite recipe selected. Drains
    /// pending rows from the shared `LrptImage` first so the
    /// snapshot captures the tail of the pass.
    ///
    /// Returns `None` if there's nothing to export — either no
    /// channel/composite selected, an active per-APID channel
    /// with no decoded lines, or a composite recipe whose
    /// source APIDs aren't all populated yet. Per CR round 2
    /// on PR #575.
    pub fn snapshot_for_export(&self) -> Option<ExportSnapshot> {
        self.drain_new_lines();
        if let Some(recipe) = self.renderer.borrow().active_composite() {
            // Composite selection is authoritative. If
            // `clone_channels_for_composite` returns `None` (one
            // or more source APIDs missing/empty), return `None`
            // — never fall back to the per-APID path. The
            // dropdown still says "Composite — ..." and exporting
            // the last greyscale APID under that label would
            // silently mislead the user (the on-screen canvas
            // doesn't fall back either; both stay consistent
            // until the composite is buildable). The export-
            // button toast handler surfaces the resulting `None`
            // as "No LRPT image data to export yet". Per CR
            // round 4 on PR #575.
            let snap = self.image.with_assembler(|a| {
                a.clone_channels_for_composite(recipe.r_apid, recipe.g_apid, recipe.b_apid)
            });
            return snap.map(|snapshot| ExportSnapshot::Composite { recipe, snapshot });
        }
        let (apid, buffer) = self.snapshot_active_channel()?;
        Some(ExportSnapshot::Channel { apid, buffer })
    }
}

/// Tagged snapshot returned by [`LrptImageView::snapshot_for_export`]
/// — what the worker thread needs to write the PNG that matches
/// the viewer's current on-screen state.
///
/// The two variants split per-APID greyscale exports from
/// composite RGB exports because each path has its own writer
/// (`write_greyscale_png` vs the assemble-then-`write_rgb_png`
/// pair). Without the variant tag the export button would have to
/// reach back into the renderer from the worker, which would race
/// the live drain tick. Per CR round 2 on PR #575.
#[derive(Debug)]
pub enum ExportSnapshot {
    /// Single per-APID greyscale channel — the previous-only
    /// path, kept for the no-composite case.
    Channel {
        /// AVHRR channel ID.
        apid: u16,
        /// Cloned per-channel pixel buffer + line count.
        buffer: sdr_lrpt::image::ChannelBuffer,
    },
    /// Three-channel RGB composite. The worker calls
    /// [`sdr_lrpt::image::assemble_rgb_composite`] on the snapshot
    /// to interleave R/G/B bytes, then [`write_rgb_png`] to write
    /// the file. Both calls run inside `gio::spawn_blocking` so
    /// the GTK main loop isn't blocked on either the per-pixel
    /// walk or the Cairo PNG encode.
    Composite {
        /// Recipe identifying which AVHRR channels are R/G/B.
        recipe: CompositeRecipe,
        /// Cloned source-channel pixels + truncated height.
        snapshot: sdr_lrpt::image::CompositeSnapshot,
    },
}

// ─── Non-modal viewer window ───────────────────────────────────────────

/// One row in the dropdown — either a single APID, or a
/// composite recipe. Pulled out as a tagged enum (rather than
/// the previous parallel `Vec<u16>`) so the
/// `connect_selected_notify` handler can dispatch straight off
/// the index without index-arithmetic against a "where do
/// composites start" boundary that drifted any time the APID
/// list changed. Per #547.
#[derive(Clone, Copy, Debug)]
enum DropdownEntry {
    Apid(u16),
    Composite(CompositeRecipe),
}

/// Build the dynamic channel-picker dropdown for the viewer
/// header. APIDs aren't known at open time, so the dropdown
/// starts dimmed-but-visible and a 1 Hz `glib` timer rebuilds
/// its model whenever new APIDs appear in `view`. A parallel
/// `Vec<DropdownEntry>` lets us decode the dropdown's numeric
/// `selected` index back into either an APID or a composite
/// recipe without parsing the display string.
///
/// The model is laid out as: per-APID entries first (sorted),
/// then every recipe in [`COMPOSITE_CATALOG`] in catalog order.
/// Composite rows are listed unconditionally even when the
/// underlying APIDs aren't all present yet — picking one in
/// that state shows a black canvas with a debug log, and the
/// dropdown's drain tick re-issues `set_composite` on every
/// poll so the image populates the moment the missing channel
/// arrives. Per #547.
#[allow(
    clippy::too_many_lines,
    reason = "the refresh tick is one logical block — building the desired \
              entries list, detecting changes, optionally rebuilding the \
              composite cache, then syncing the dropdown's selected index. \
              Splitting it would force the borrow-scoping comments and the \
              re-entrance-safety invariants out of the body that depends on \
              them"
)]
fn build_channel_dropdown(view: &LrptImageView) -> gtk4::DropDown {
    let model = gtk4::StringList::new(&[]);
    let dropdown = gtk4::DropDown::builder()
        .model(&model)
        .tooltip_text("Which AVHRR channel (APID) or composite to display")
        .sensitive(false)
        .build();
    dropdown.update_property(&[gtk4::accessible::Property::Label("LRPT channel selector")]);
    let dropdown_entries: Rc<RefCell<Vec<DropdownEntry>>> = Rc::new(RefCell::new(Vec::new()));

    // Selection → renderer. Per-APID picks route to
    // `set_active_apid` and clear any active composite so the
    // single-channel canvas paints; composite picks call
    // `set_composite`, which builds the cached ARGB32 surface
    // from the named source APIDs. Per #547.
    {
        let view = view.clone();
        let dropdown_entries = Rc::clone(&dropdown_entries);
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            let entries = dropdown_entries.borrow();
            let Some(&entry) = entries.get(idx) else {
                return;
            };
            // Drop the borrow before any view mutation that
            // might re-enter the dropdown handler (e.g. via
            // a future `set_selected` call inside the view).
            drop(entries);
            match entry {
                DropdownEntry::Apid(apid) => {
                    view.clear_composite();
                    let _ = view.set_active_apid(apid);
                }
                DropdownEntry::Composite(recipe) => {
                    let _ = view.set_composite(recipe);
                }
            }
        });
    }

    // Refresh tick — runs at 1 Hz (channel discovery is rare;
    // a faster cadence would burn CPU on idle string compares).
    // Register the source on the view so `LrptImageView::shutdown`
    // can cancel it when the window closes; otherwise the closure's
    // `view.clone()` would keep the view + ~51 MB-per-channel
    // surfaces alive forever.
    //
    // The tick has three jobs:
    //   1. Rebuild the entries list when the APID set changes
    //      (composite rows are always appended; per-APID rows
    //      are sorted).
    //   2. Re-sync the dropdown's `selected` to whichever APID
    //      the renderer thinks is active (or the first APID if
    //      the renderer has no selection yet).
    //   3. When composite mode is active, re-issue
    //      `view.set_composite(recipe)` so newly-decoded lines
    //      from the source APIDs land in the cached composite
    //      surface. The user sees lines accrue at the same
    //      cadence the dropdown refreshes (~1 Hz). Per #547.
    //
    // **Borrow scoping:** GTK4's `gtk4::DropDown::set_selected`
    // emits `notify::selected` SYNCHRONOUSLY inside the setter,
    // which means the `connect_selected_notify` handler above
    // re-enters this same `dropdown_entries` `RefCell` to look
    // up the entry for the new index. If we held a `borrow_mut()`
    // across `set_selected(...)`, that re-entrance would panic
    // with "already borrowed". Per `CodeRabbit` round 3 on PR
    // #543. The borrows below are kept tight: an immutable
    // `borrow()` for the equality compare, a fresh
    // `borrow_mut()` for the `clone_from`, and zero borrows
    // held during the `set_selected` calls.
    let view_for_tick = view.clone();
    let dropdown_clone = dropdown.clone();
    let refresh_id = glib::timeout_add_local(
        std::time::Duration::from_millis(u64::from(DROPDOWN_REFRESH_INTERVAL_MS)),
        move || {
            let mut current_apids = view_for_tick.known_apids();
            current_apids.sort_unstable();

            // Build the desired full entries list: per-APID
            // entries first (sorted), then catalog composites.
            let mut desired: Vec<DropdownEntry> = current_apids
                .iter()
                .copied()
                .map(DropdownEntry::Apid)
                .collect();
            desired.extend(
                COMPOSITE_CATALOG
                    .iter()
                    .copied()
                    .map(DropdownEntry::Composite),
            );

            let entries_unchanged = {
                let cur = dropdown_entries.borrow();
                cur.len() == desired.len()
                    && cur.iter().zip(desired.iter()).all(|(a, b)| match (a, b) {
                        (DropdownEntry::Apid(x), DropdownEntry::Apid(y)) => x == y,
                        (DropdownEntry::Composite(x), DropdownEntry::Composite(y)) => x == y,
                        _ => false,
                    })
            };

            // Rebuild the composite cache when composite mode is
            // active so newly-decoded lines accrue in near-real-
            // time. Skip the rebuild while the user has the viewer
            // paused — `set_composite` always queues a redraw, and
            // the pause contract says the canvas should freeze
            // until Resume is clicked. The next non-paused tick
            // catches up. Per #547 + CR round 1 on PR #575.
            //
            // Also skip when the source channels' min height
            // hasn't advanced since the last build (e.g. LOS
            // reached, decoder stalled, only a non-limiting
            // channel grew). The composite truncates to
            // `min(r, g, b)`, so a non-limiting channel growing
            // produces byte-identical output — rebuilding then is
            // pure waste (~3 memcpys + per-pixel interleave +
            // queued redraw, every tick, forever). Per CR round 3
            // on PR #575.
            if let Some(recipe) = view_for_tick.active_composite()
                && !view_for_tick.is_paused()
            {
                let current = view_for_tick.current_composite_min_height(recipe);
                let cached = view_for_tick.cached_composite_min_height();
                if current != cached {
                    let _ = view_for_tick.set_composite(recipe);
                }
            }

            // If the entries match AND the dropdown's selected
            // entry still aligns with the renderer's active
            // channel, there's nothing else to do this tick.
            let active_apid = view_for_tick.active_apid();
            let active_composite = view_for_tick.active_composite();
            #[allow(clippy::cast_possible_truncation)]
            let selected_entry = {
                let entries = dropdown_entries.borrow();
                entries.get(dropdown_clone.selected() as usize).copied()
            };
            let selected_aligned = match (selected_entry, active_composite, active_apid) {
                (Some(DropdownEntry::Composite(s)), Some(a), _) => s == a,
                (Some(DropdownEntry::Apid(s)), None, Some(a)) => s == a,
                _ => false,
            };
            if entries_unchanged && selected_aligned {
                return glib::ControlFlow::Continue;
            }

            if !entries_unchanged {
                model.splice(0, model.n_items(), &[]);
                for entry in &desired {
                    match entry {
                        DropdownEntry::Apid(apid) => model.append(&format!("APID {apid}")),
                        DropdownEntry::Composite(recipe) => {
                            model.append(&format!("Composite — {}", recipe.name));
                        }
                    }
                }
                dropdown_entries.borrow_mut().clone_from(&desired);
            }
            // Always sensitive — composite catalog entries are
            // present even before any APID arrives. Picking one
            // pre-decode logs and falls through to the
            // background-painted canvas; the next refresh tick
            // rebuilds once data shows up. Per #547.
            dropdown_clone.set_sensitive(!desired.is_empty());

            // Sync the selected index to the renderer's active
            // state. Composite mode wins over per-APID active.
            if let Some(recipe) = active_composite {
                if let Some(pos) = desired.iter().position(|e| match e {
                    DropdownEntry::Composite(r) => *r == recipe,
                    DropdownEntry::Apid(_) => false,
                }) {
                    #[allow(clippy::cast_possible_truncation)]
                    dropdown_clone.set_selected(pos as u32);
                }
            } else if let Some(active) = active_apid {
                if let Some(pos) = desired.iter().position(|e| match e {
                    DropdownEntry::Apid(a) => *a == active,
                    DropdownEntry::Composite(_) => false,
                }) {
                    #[allow(clippy::cast_possible_truncation)]
                    dropdown_clone.set_selected(pos as u32);
                }
            } else if !current_apids.is_empty() {
                // No previous selection — pick the first APID
                // (sorted) so the user sees something the moment
                // data arrives. The `selected_notify` handler above
                // will route the choice into the renderer.
                dropdown_clone.set_selected(0);
            }
            glib::ControlFlow::Continue
        },
    );
    view.register_source(refresh_id);

    dropdown
}

/// Build the Pause / Resume toggle for the viewer header.
/// Pull-out so [`open_lrpt_viewer_window`] stays under the
/// 100-line clippy threshold.
fn build_pause_button(view: &LrptImageView) -> gtk4::ToggleButton {
    let btn = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause / resume the live image update")
        .build();
    btn.update_property(&[gtk4::accessible::Property::Label(
        "Pause or resume live image update",
    )]);
    let view = view.clone();
    btn.connect_toggled(move |b| {
        view.set_paused(b.is_active());
    });
    btn
}

/// Open the LRPT viewer in a non-modal transient window. The
/// window holds a header bar with a channel dropdown,
/// Pause / Resume, and Export PNG, plus the drawing-area
/// canvas underneath.
///
/// Non-modal so the user can keep tuning, recording, or
/// otherwise interacting with the main radio window while the
/// LRPT image builds up alongside.
pub fn open_lrpt_viewer_window<W: gtk4::prelude::IsA<gtk4::Window>>(
    parent: &W,
    title: &str,
    image: LrptImage,
) -> (LrptImageView, adw::Window) {
    let view = LrptImageView::new(image);

    let window = adw::Window::builder()
        .title(title)
        .default_width(VIEWER_WINDOW_WIDTH)
        .default_height(VIEWER_WINDOW_HEIGHT)
        .transient_for(parent)
        .modal(false)
        .build();

    let header = adw::HeaderBar::new();

    let channel_dropdown = build_channel_dropdown(&view);
    header.pack_start(&channel_dropdown);

    let pause_btn = build_pause_button(&view);
    header.pack_start(&pause_btn);

    // ── Export PNG ────────────────────────────────────────
    let export_btn = gtk4::Button::builder()
        .icon_name("document-save-symbolic")
        // Wording covers both per-APID and composite exports — the
        // button writes whatever the dropdown has selected
        // (per-channel greyscale OR a false-colour composite). Per
        // CR round 3 on PR #575.
        .tooltip_text("Export the current LRPT image to PNG")
        .build();
    export_btn.update_property(&[gtk4::accessible::Property::Label(
        "Export current LRPT image to PNG",
    )]);
    let export_view = view.clone();
    let window_for_export = window.downgrade();
    export_btn.connect_clicked(move |_| {
        let Some(window_now) = window_for_export.upgrade() else {
            return;
        };
        // Snapshot the viewer's current state on the GTK main
        // thread (drains pending rows + clones either the per-
        // channel Vec<u8> or the three composite source channels
        // under a brief mutex hold), then off-main-thread does
        // the heavy PNG encoding + filesystem I/O via
        // `gio::spawn_blocking`. Same pattern the LOS
        // `SaveLrptPass` handler uses; before this round, the
        // manual Export PNG button froze the GTK main loop on
        // any large channel because Cairo PNG encoding is
        // O(width × n_lines) and not negligible at the
        // ≤8192-line cap. Per `CodeRabbit` round 10 on PR #543.
        //
        // Composite-mode aware: `snapshot_for_export` returns an
        // [`ExportSnapshot::Composite`] when the user has a
        // composite recipe active so the manual Export PNG matches
        // what's on screen. Per CR round 2 on PR #575 — before
        // this, exporting while a composite was displayed wrote
        // out the last greyscale APID's surface instead of the
        // composite the user was looking at.
        let Some(snapshot) = export_view.snapshot_for_export() else {
            // Either nothing is selected, or the active channel /
            // composite has no decoded rows yet. Surface as a
            // clear toast rather than an opaque "no active
            // channel" error.
            show_toast_in(
                &window_now,
                adw::Toast::builder()
                    .title("No LRPT image data to export yet")
                    .build(),
            );
            return;
        };
        // Filename is derived AFTER the snapshot so the resolved
        // APID (or composite recipe slug) lands in it. See
        // `default_export_path` / `composite_export_path` for the
        // disk-layout convention.
        let path = match &snapshot {
            ExportSnapshot::Channel { apid, .. } => default_export_path(Some(*apid)),
            ExportSnapshot::Composite { recipe, .. } => composite_export_path(recipe.name),
        };
        let window_weak = window_now.downgrade();
        glib::spawn_future_local(async move {
            let path_for_msg = path.clone();
            let result = gio::spawn_blocking(move || match snapshot {
                ExportSnapshot::Channel { buffer, .. } => {
                    write_greyscale_png(&path, &buffer.pixels, IMAGE_WIDTH, buffer.lines)
                }
                ExportSnapshot::Composite { snapshot, .. } => {
                    let rgb = sdr_lrpt::image::assemble_rgb_composite(
                        &snapshot.r_pixels,
                        &snapshot.g_pixels,
                        &snapshot.b_pixels,
                        snapshot.height,
                    );
                    write_rgb_png(&path, &rgb, IMAGE_WIDTH, snapshot.height)
                }
            })
            .await;
            let toast = match result {
                Ok(Ok(())) => adw::Toast::builder()
                    .title(format!("Saved {}", path_for_msg.display()))
                    .build(),
                Ok(Err(e)) => adw::Toast::builder()
                    .title(format!("PNG export failed: {e}"))
                    .build(),
                Err(e) => {
                    // Worker thread panicked. `Box<dyn Any>`
                    // doesn't implement Display — log via Debug,
                    // surface a generic message.
                    tracing::warn!("manual LRPT export worker panicked: {e:?}");
                    adw::Toast::builder()
                        .title("PNG export worker panicked")
                        .build()
                }
            };
            if let Some(window) = window_weak.upgrade() {
                show_toast_in(&window, toast);
            }
        });
    });
    header.pack_end(&export_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(view.drawing_area()));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar));

    window.set_content(Some(&toast_overlay));
    window.present();

    (view, window)
}

/// Default path the Export PNG button writes to:
/// `~/sdr-recordings/lrpt-{apid}-YYYY-MM-DD-HHMMSS-uuuuuu.png`.
///
/// The microsecond suffix prevents collisions when the user
/// rapid-fires the export button on the same channel within the
/// same second — without it, the second export silently
/// overwrote the first via `File::create`'s truncate semantics.
/// Per `CodeRabbit` round 13 on PR #543.
fn default_export_path(apid: Option<u16>) -> PathBuf {
    let timestamp = glib::DateTime::now_local()
        .as_ref()
        .ok()
        .and_then(|dt| {
            let stamp = dt.format("%Y-%m-%d-%H%M%S").ok()?;
            // glib's `microsecond()` is 0..=999_999, zero-padded
            // to 6 digits keeps lexical-sort matching wall-clock.
            Some(format!("{stamp}-{usec:06}", usec = dt.microsecond()))
        })
        .unwrap_or_else(|| "unknown".to_string());
    let apid_part = apid.map_or_else(|| "unknown".to_string(), |a| format!("apid{a}"));
    glib::home_dir()
        .join("sdr-recordings")
        .join(format!("lrpt-{apid_part}-{timestamp}.png"))
}

/// Default path the composite-mode Export PNG button writes to:
/// `~/sdr-recordings/lrpt-composite-{slug}-YYYY-MM-DD-HHMMSS-uuuuuu.png`.
///
/// Same microsecond-suffix collision protection as
/// [`default_export_path`]. The recipe `name` is sanitized via
/// the same slug rules used for the LOS-side composite filenames
/// in `window.rs::SaveLrptPass` so the manual and auto-record
/// paths share a disk-layout convention. Per CR round 2 on PR
/// #575.
fn composite_export_path(recipe_name: &str) -> PathBuf {
    let timestamp = glib::DateTime::now_local()
        .as_ref()
        .ok()
        .and_then(|dt| {
            let stamp = dt.format("%Y-%m-%d-%H%M%S").ok()?;
            Some(format!("{stamp}-{usec:06}", usec = dt.microsecond()))
        })
        .unwrap_or_else(|| "unknown".to_string());
    let slug = recipe_name.replace(' ', "-").replace(['/', '\\'], "_");
    glib::home_dir()
        .join("sdr-recordings")
        .join(format!("lrpt-composite-{slug}-{timestamp}.png"))
}

/// Walk the window content tree looking for a [`adw::ToastOverlay`]
/// to display `toast` in. Falls through silently if the layout
/// changes — toasts are best-effort feedback, not load-bearing UI.
fn show_toast_in<W: gtk4::prelude::IsA<gtk4::Window>>(window: &W, toast: adw::Toast) {
    if let Some(child) = window.as_ref().child()
        && let Some(overlay) = child.downcast_ref::<adw::ToastOverlay>()
    {
        overlay.add_toast(toast);
    }
}

// ─── Live viewer action ────────────────────────────────────────────────

/// Wire the `app.lrpt-open` action onto `app`. Activating it
/// (via the app menu, the `Ctrl+Shift+L` accelerator, or future
/// activity-bar entry) opens a non-modal LRPT viewer window
/// and informs the DSP controller about the shared image
/// handle so the LRPT decoder tap starts pushing scan lines
/// into it. Closing the window clears the `AppState` slot
/// (the GTK widget tree drops with the window) but leaves the
/// DSP-side decoder + shared image attached so an in-flight
/// auto-record pass keeps capturing — see the close-request
/// comment in [`open_lrpt_viewer_if_needed`] and the
/// module-level docs above for the lifecycle rationale.
pub fn connect_lrpt_action(
    app: &adw::Application,
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    let action = gio::SimpleAction::new("lrpt-open", None);
    let parent_provider = Rc::clone(parent_provider);
    let state_for_action = Rc::clone(state);
    action.connect_activate(move |_, _| {
        open_lrpt_viewer_if_needed(&parent_provider, &state_for_action);
    });
    app.add_action(&action);
    app.set_accels_for_action("app.lrpt-open", &["<Ctrl><Shift>l"]);
}

/// Open the LRPT viewer window if it isn't already open.
/// Registers the new view in `state.lrpt_viewer`, hands the
/// shared image to the DSP thread, and wires `close-request`
/// to cancel the view's `glib` timers + drop the `AppState` slot.
/// The DSP capture (decoder + shared image) intentionally
/// outlives the window — see the close-request body for why.
///
/// Pulled out of [`connect_lrpt_action`] so the auto-record
/// path (Task 7.5) can fire the same open flow at AOS without
/// going through the GIO action system. Mirrors the APT
/// viewer's [`crate::apt_viewer::open_apt_viewer_if_needed`].
pub fn open_lrpt_viewer_if_needed(
    parent_provider: &Rc<dyn Fn() -> Option<gtk4::Window>>,
    state: &Rc<crate::state::AppState>,
) {
    if state.lrpt_viewer.borrow().is_some() {
        // Defensive re-attach: if a future code path ever
        // detaches the DSP-side image (today nothing sends
        // `ClearLrptImage`, but a future refactor might), the
        // existing-viewer fast-path would silently leave the
        // tap muted. Re-sending `SetLrptImage` is idempotent
        // — the controller's handler no longer drops the
        // decoder on attach (round 11 paired change), so
        // mid-pass decoder state survives the round-trip. Per
        // `CodeRabbit` round 11 on PR #543.
        state.send_dsp(UiToDsp::SetLrptImage(state.lrpt_image.clone()));
        // Raise the existing window so `Ctrl+Shift+L` actually
        // surfaces a buried / minimised viewer instead of being
        // a silent no-op. Weak-ref upgrade fails closed: if the
        // window is gone but the AppState slot wasn't cleared
        // yet (close-request race), we just skip — the slot
        // will clear momentarily anyway. Per `CodeRabbit`
        // round 13 on PR #543.
        if let Some(window) = state
            .lrpt_viewer_window
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
        {
            window.present();
        }
        return;
    }
    let Some(parent) = parent_provider() else {
        tracing::warn!("lrpt-open invoked with no main window available");
        return;
    };
    let image = state.lrpt_image.clone();
    let (view, window) = open_lrpt_viewer_window(&parent, "Meteor-M LRPT", image.clone());
    *state.lrpt_viewer.borrow_mut() = Some(view);
    *state.lrpt_viewer_window.borrow_mut() = Some(window.downgrade());
    state.send_dsp(UiToDsp::SetLrptImage(image));

    let state_for_close = Rc::clone(state);
    window.connect_close_request(move |_| {
        // Cancel the view's drain + dropdown-refresh timeouts
        // BEFORE we drop the AppState slot; otherwise their
        // closures' `Rc<view>` clones keep the view + ~51 MB-
        // per-channel surfaces alive until the application
        // exits. Per `CodeRabbit` round 1 on PR #543.
        if let Some(view) = state_for_close.lrpt_viewer.borrow().as_ref() {
            view.shutdown();
        }
        *state_for_close.lrpt_viewer.borrow_mut() = None;
        *state_for_close.lrpt_viewer_window.borrow_mut() = None;
        // Deliberately NOT sending `UiToDsp::ClearLrptImage`
        // here — the DSP-side decoder + shared image stay
        // attached so the DSP keeps decoding into the shared
        // image regardless of viewer presence. Closing the
        // viewer mid-pass used to drop all subsequent rows
        // and break the LOS `SaveLrptPass` save (the recorder
        // would post "no image saved" even though decoding
        // was still feasible). Now the recorder reads the
        // shared image directly at LOS, so viewer close is
        // purely a UI teardown. The decoder remains gated by
        // `current_mode == Lrpt` and the source-stop cleanup
        // path, so closing the viewer in manual LRPT mode
        // doesn't burn CPU forever — switching demod or
        // stopping the source still tears it down. Per
        // `CodeRabbit` round 7 on PR #543.
        glib::Propagation::Proceed
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// APID used in renderer tests. AVHRR convention: 64 = ch1.
    /// Same value the rest of the LRPT test suite uses for
    /// single-channel cases.
    const APID_TEST: u16 = 64;
    /// Secondary APID for multi-channel checks.
    const APID_TEST_2: u16 = 65;
    /// Pixel marker — distinct from 0/0xFF so a regression that
    /// returned a default-allocated surface would fail loudly.
    const TEST_PIXEL: u8 = 0x42;
    const TEST_PIXEL_2: u8 = 0xC0;

    fn synth_line(value: u8) -> Vec<u8> {
        vec![value; IMAGE_WIDTH]
    }

    #[test]
    fn renderer_starts_empty_with_no_active_channel() {
        let r = LrptImageRenderer::new();
        assert!(r.is_empty());
        assert!(r.active_apid().is_none());
        assert!(r.known_apids().is_empty());
    }

    #[test]
    fn push_line_lazily_allocates_surface_per_apid() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        assert_eq!(r.n_lines(APID_TEST), 1);
        // The other APID has never been pushed — n_lines returns 0
        // and the channel doesn't exist yet.
        assert_eq!(r.n_lines(APID_TEST_2), 0);
        assert_eq!(r.known_apids(), vec![APID_TEST]);
    }

    #[test]
    fn first_push_auto_selects_that_apid() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        // The user shouldn't have to manually pick a channel
        // before any data is visible — pushing the first line
        // for any APID auto-selects it.
        assert_eq!(r.active_apid(), Some(APID_TEST));
    }

    #[test]
    fn subsequent_push_for_different_apid_doesnt_steal_active() {
        // First-push auto-select shouldn't keep firing — the
        // user's pick (or the initial pick) stays sticky as
        // additional channels appear.
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        r.push_line(APID_TEST_2, &synth_line(TEST_PIXEL_2));
        assert_eq!(r.active_apid(), Some(APID_TEST));
    }

    #[test]
    fn push_line_caps_at_max_lines_per_channel() {
        let mut r = LrptImageRenderer::new();
        for _ in 0..MAX_LINES {
            assert_eq!(
                r.push_line(APID_TEST, &synth_line(TEST_PIXEL)),
                PushOutcome::Pushed,
            );
        }
        assert_eq!(r.n_lines(APID_TEST), MAX_LINES);
        // One more push past the cap reports `Capped` — caller's
        // watermark should still advance (further pushes won't
        // succeed no matter how many retries).
        assert_eq!(
            r.push_line(APID_TEST, &synth_line(TEST_PIXEL)),
            PushOutcome::Capped,
        );
        assert_eq!(r.n_lines(APID_TEST), MAX_LINES);
    }

    #[test]
    fn push_line_with_wrong_width_is_dropped() {
        let mut r = LrptImageRenderer::new();
        // IMAGE_WIDTH is 1568; deliberately pass a short slice.
        assert_eq!(
            r.push_line(APID_TEST, &[TEST_PIXEL; 16]),
            PushOutcome::InvalidLine,
        );
        // No surface allocated, no line counted.
        assert_eq!(r.n_lines(APID_TEST), 0);
        assert!(r.known_apids().is_empty());
    }

    #[test]
    fn push_outcome_consumed_pins_watermark_semantics() {
        // Pin the contract `LrptImageView::drain_new_lines`
        // depends on: only `TransientFailure` leaves the row
        // in the source for retry. Per `CodeRabbit` round 3
        // on PR #543.
        assert!(PushOutcome::Pushed.consumed());
        assert!(PushOutcome::Capped.consumed());
        assert!(PushOutcome::InvalidLine.consumed());
        assert!(!PushOutcome::TransientFailure.consumed());
    }

    #[test]
    fn set_active_apid_only_succeeds_for_known_channels() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        // Existing APID — switch succeeds.
        assert!(r.set_active_apid(APID_TEST));
        assert_eq!(r.active_apid(), Some(APID_TEST));
        // Unknown APID — switch refused, active stays put.
        assert!(!r.set_active_apid(APID_TEST_2));
        assert_eq!(r.active_apid(), Some(APID_TEST));
    }

    #[test]
    fn clear_drops_all_channels_and_active_selection() {
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        r.push_line(APID_TEST_2, &synth_line(TEST_PIXEL_2));
        r.clear();
        assert!(r.is_empty());
        assert!(r.active_apid().is_none());
        assert!(r.known_apids().is_empty());
    }

    #[test]
    fn pixel_layout_is_argb32_with_grayscale_in_bgr_channels() {
        // Same invariant as the APT renderer test: Cairo's
        // ARGB32 little-endian layout = B, G, R, A. Every
        // channel of the input greyscale value goes into all
        // three colour bytes; alpha is opaque.
        let mut r = LrptImageRenderer::new();
        let mut line = vec![0_u8; IMAGE_WIDTH];
        line[0] = 0x80;
        line[1] = 0xC0;
        r.push_line(APID_TEST, &line);
        let surface = &mut r.channels.get_mut(&APID_TEST).unwrap().surface;
        let data = surface.data().unwrap();
        assert_eq!(&data[0..4], &[0x80, 0x80, 0x80, 0xFF]);
        assert_eq!(&data[4..8], &[0xC0, 0xC0, 0xC0, 0xFF]);
    }

    #[test]
    fn export_png_refuses_when_no_active_channel() {
        let r = LrptImageRenderer::new();
        let path = std::env::temp_dir().join("lrpt-test-no-active-should-not-be-written.png");
        let result = r.export_png(&path);
        assert!(result.is_err());
        assert!(!path.exists(), "no file should be created on empty export");
    }

    #[test]
    fn export_png_refuses_when_active_channel_has_no_data() {
        // Force-set active to an APID we never pushed to (via
        // the test-only path: renderer's HashMap entry exists
        // because we push one line then... wait, no — we need
        // a way to test "active set, but channel empty". Push
        // then clear partway: clear() drops active too, so
        // that's not it. Instead use the renderer's contract:
        // set_active_apid can't succeed for an unknown channel
        // either, so the only reachable "active set, n_lines==0"
        // case is "freshly pushed once, then..." — actually
        // n_lines becomes 1 the moment we push. So the first
        // branch (no active) is the only reachable empty error.
        // We test the second branch by directly mutating the
        // channel's n_lines back to 0 via the test-only access
        // below.
        let mut r = LrptImageRenderer::new();
        r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        r.channels.get_mut(&APID_TEST).unwrap().n_lines = 0;
        let path = std::env::temp_dir().join("lrpt-test-empty-channel-should-not-be-written.png");
        let result = r.export_png(&path);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn export_png_round_trips_to_a_real_file() {
        use std::io::Read;
        let mut r = LrptImageRenderer::new();
        for _ in 0..16 {
            r.push_line(APID_TEST, &synth_line(TEST_PIXEL));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-test-{nanos}.png"));
        r.export_png(&path).expect("export per-APID PNG");
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert!(metadata.len() > 0, "PNG file shouldn't be empty");
        let mut header = [0_u8; 8];
        let mut f = std::fs::File::open(&path).expect("open");
        f.read_exact(&mut header).expect("read_exact");
        assert_eq!(
            &header, b"\x89PNG\r\n\x1a\n",
            "exported file isn't a valid PNG (header mismatch)",
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_png_refuses_when_composite_active_but_cache_empty() {
        // Per CR round 4 on PR #575: composite mode is
        // authoritative — when a recipe is active but the
        // cache hasn't built yet (source APIDs missing/empty),
        // `export_png` must fail loudly with `EmptyComposite`
        // rather than silently fall through to whatever
        // single-APID was last selected. The dropdown still
        // says "Composite — ..." in that state, so a per-APID
        // export would mislead.
        let mut r = LrptImageRenderer::new();
        // Activate a composite without any source data — this
        // sets active_composite + leaves cache None.
        let recipe = COMPOSITE_CATALOG[0];
        let empty = LrptImage::new();
        assert!(!r.set_composite(recipe, &empty));
        // Also feed one line into a per-APID surface that
        // ISN'T part of the recipe — the bug we're guarding
        // against is "fall through and export this one".
        r.push_line(99, &synth_line(TEST_PIXEL));
        let path = std::env::temp_dir().join("lrpt-test-empty-composite-should-not-be-written.png");
        let result = r.export_png(&path);
        assert!(matches!(
            result,
            Err(crate::viewer::ViewerError::EmptyComposite { recipe_name })
                if recipe_name == recipe.name
        ));
        assert!(
            !path.exists(),
            "no file should be created on EmptyComposite",
        );
    }

    #[test]
    fn render_paints_only_background_when_composite_active_but_cache_empty() {
        // Sibling guarantee to `export_png_refuses_when_composite_active_but_cache_empty`:
        // the on-screen render path must also stay on
        // background-only paint when composite is active but
        // unbuilt — never fall through to per-APID. We can't
        // easily inspect Cairo's surface state in a unit test,
        // but the function returns `Ok(())` without panicking
        // along the no-fall-through path, and the no-image-
        // surface-blit branch is the only one that reaches
        // that state with both `composite_cache: None` and
        // `active_composite: Some(_)`. Per CR round 4 on
        // PR #575.
        let mut r = LrptImageRenderer::new();
        let recipe = COMPOSITE_CATALOG[0];
        let empty = LrptImage::new();
        assert!(!r.set_composite(recipe, &empty));
        // Per-APID surface for an unrelated APID — what the
        // pre-fix render would have painted.
        r.push_line(99, &synth_line(TEST_PIXEL));
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, 32, 32).expect("test surface alloc");
        let cr = cairo::Context::new(&surface).expect("cairo ctx");
        r.render(&cr, 32, 32).expect("render");
    }

    #[test]
    fn export_png_uses_composite_cache_when_active() {
        // Per CR round 2 on PR #575: when composite mode is
        // active and the cache is populated, `export_png` must
        // export the composite surface — not the active per-APID
        // surface. Without this, exporting while a composite was
        // on screen wrote out the last greyscale APID instead.
        use std::io::Read;
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        // Push one line into each source APID so the composite
        // cache populates. The recipe is from the catalog, so
        // those APIDs are well-defined.
        image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
        image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
        image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
        // Also feed one line to a per-APID surface that ISN'T in
        // the recipe — this is what the previous greyscale fallback
        // would have written. If the export silently wrote that
        // surface instead of the composite, the test below would
        // still pass the PNG header check; we only confirm here
        // that export succeeds end-to-end, not which pixels it
        // wrote (the byte-level guarantee is covered by
        // `build_argb32_from_rgb_writes_bgra_byte_order`).
        assert!(r.set_composite(recipe, &image));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-comp-{nanos}.png"));
        r.export_png(&path).expect("export composite PNG");
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert!(metadata.len() > 0, "PNG file shouldn't be empty");
        let mut header = [0_u8; 8];
        let mut f = std::fs::File::open(&path).expect("open");
        f.read_exact(&mut header).expect("read_exact");
        assert_eq!(&header, b"\x89PNG\r\n\x1a\n", "not a PNG");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_greyscale_png_round_trips_to_a_real_file() {
        // Pin the new free-function path used by the LOS
        // `SaveLrptPass` handler in `window.rs`. Per
        // `CodeRabbit` round 7 on PR #543.
        use std::io::Read;
        const W: usize = 32;
        const H: usize = 8;
        let pixels: Vec<u8> = (0..W * H)
            .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
            .collect();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-bare-{nanos}.png"));
        write_greyscale_png(&path, &pixels, W, H).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() > 0);
        let mut header = [0_u8; 8];
        let mut f = std::fs::File::open(&path).unwrap();
        f.read_exact(&mut header).unwrap();
        assert_eq!(&header, b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_greyscale_png_rejects_size_mismatch() {
        let path = std::env::temp_dir().join("sdr-ui-lrpt-bare-mismatch.png");
        let result = write_greyscale_png(&path, &[0_u8; 10], 4, 4);
        assert!(result.is_err());
        assert!(
            !path.exists(),
            "no file should be written on size-mismatch error"
        );
    }

    #[test]
    fn write_greyscale_png_rejects_zero_size() {
        let path = std::env::temp_dir().join("sdr-ui-lrpt-bare-zero.png");
        let result = write_greyscale_png(&path, &[], 0, 0);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn write_greyscale_png_zero_dim_with_pixels_reports_zero_sized() {
        // Pin the CR-requested ordering: a zero-dim call with a
        // non-empty pixel buffer must surface as `ZeroSized`, not
        // mask as the generic `InvalidBuffer` length-mismatch.
        // Per CR on PR #550.
        let path = std::env::temp_dir().join("sdr-ui-lrpt-bare-zero-dim-pixels.png");
        let result = write_greyscale_png(&path, &[1_u8], 0, 1);
        assert!(matches!(result, Err(crate::viewer::ViewerError::ZeroSized)));
        assert!(!path.exists());
    }

    // ─── Composite catalog (#547) ───────────────────────────

    #[test]
    fn composite_catalog_is_non_empty() {
        // Defensive — if a future maintainer ever empties the
        // catalog the dropdown silently loses every composite
        // option. Catch that loud-and-early.
        assert!(!COMPOSITE_CATALOG.is_empty());
    }

    #[test]
    fn composite_catalog_has_unique_names() {
        // Names show up in the dropdown with a `Composite — `
        // prefix; duplicates would render two indistinguishable
        // entries. Pin uniqueness so a copy-paste typo can't
        // ship.
        let names: Vec<&str> = COMPOSITE_CATALOG.iter().map(|r| r.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names.len(),
            sorted.len(),
            "duplicate composite name in catalog",
        );
    }

    #[test]
    fn composite_catalog_apid_triples_are_distinct_per_entry() {
        // A recipe with R == G (or any pair equal) collapses to a
        // 2-channel composite — almost certainly a typo. The
        // assembler still renders, but the result is misleading
        // (one channel painted into two RGB slots). Pin
        // distinctness as a sanity guard.
        for r in COMPOSITE_CATALOG {
            assert_ne!(r.r_apid, r.g_apid, "{}: r and g APIDs are the same", r.name);
            assert_ne!(r.g_apid, r.b_apid, "{}: g and b APIDs are the same", r.name);
            assert_ne!(r.r_apid, r.b_apid, "{}: r and b APIDs are the same", r.name);
        }
    }

    #[test]
    fn renderer_starts_in_single_channel_mode() {
        let r = LrptImageRenderer::new();
        assert!(!r.is_composite_active());
        assert!(r.active_composite().is_none());
    }

    #[test]
    fn set_composite_returns_false_when_source_apids_missing() {
        // No data pushed yet — every recipe's source APIDs are
        // missing, so `composite_rgb` returns None and
        // `set_composite` reports false. The recipe is still
        // remembered as the active composite (so the drain
        // tick will retry every poll), but the cache stays
        // empty.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        assert!(!r.set_composite(recipe, &image));
        assert!(r.is_composite_active());
        assert_eq!(r.active_composite(), Some(recipe));
    }

    #[test]
    fn set_composite_succeeds_when_all_three_apids_have_data() {
        // Push one line per source APID for the first catalog
        // recipe, then activate it. The cache should populate
        // and `is_composite_active` stays true.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
        image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
        image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
        assert!(r.set_composite(recipe, &image));
        assert!(r.is_composite_active());
        assert_eq!(r.active_composite(), Some(recipe));
    }

    #[test]
    fn clear_composite_drops_recipe_and_cache() {
        // Activate composite, then clear. Both the recipe and
        // the cache must be gone so the next render falls back
        // to single-channel mode.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
        image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
        image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
        r.set_composite(recipe, &image);
        r.clear_composite();
        assert!(!r.is_composite_active());
        assert!(r.active_composite().is_none());
    }

    #[test]
    fn composite_min_height_tracks_min_of_three_channels() {
        // Pin the gate the dropdown tick uses to skip no-op
        // composite rebuilds. Build with channels at lines
        // (3, 5, 4); the renderer should remember 3 — only
        // advancing the limiting channel can change the output.
        // Per CR round 3 on PR #575.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        for _ in 0..3 {
            image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
        }
        for _ in 0..5 {
            image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
        }
        for _ in 0..4 {
            image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
        }
        assert!(r.set_composite(recipe, &image));
        assert_eq!(r.composite_min_height(), Some(3));

        // Clearing the composite must drop the cached min so the
        // tick treats the next activation as a fresh build.
        r.clear_composite();
        assert_eq!(r.composite_min_height(), None);

        // Re-activating, then `clear()` (between-pass cleanup)
        // also drops the cached min.
        r.set_composite(recipe, &image);
        assert_eq!(r.composite_min_height(), Some(3));
        r.clear();
        assert_eq!(r.composite_min_height(), None);
    }

    #[test]
    fn composite_min_height_is_none_when_source_apids_missing() {
        // Empty image — `set_composite` returns false and the
        // cached min stays `None` so the tick keeps retrying
        // (compares as `None != current_min`) until source
        // channels appear. Per CR round 3 on PR #575.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        assert!(!r.set_composite(recipe, &image));
        assert_eq!(r.composite_min_height(), None);
    }

    #[test]
    fn selection_enum_makes_apid_and_composite_mutually_exclusive() {
        // Per CR round 5 on PR #575: collapsing the parallel
        // `active`/`active_composite` fields into one enum
        // means switching modes is atomic — picking an APID
        // implicitly drops the composite cache, picking a
        // composite implicitly hides any per-APID. Pin both
        // directions so a future field-split-and-add doesn't
        // silently regress.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        // Seed three source channels + a non-recipe APID.
        image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
        image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
        image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
        r.push_line(99, &synth_line(TEST_PIXEL));

        // APID first → set composite. Composite mode wins;
        // active_apid() must return None (not the prior APID).
        assert!(r.set_active_apid(99));
        assert_eq!(r.active_apid(), Some(99));
        assert_eq!(r.active_composite(), None);
        assert!(r.set_composite(recipe, &image));
        assert_eq!(r.active_apid(), None);
        assert_eq!(r.active_composite(), Some(recipe));

        // Composite first → set APID. APID wins; cache + min
        // are dropped atomically (no leftover composite state).
        assert!(r.set_active_apid(99));
        assert_eq!(r.active_apid(), Some(99));
        assert_eq!(r.active_composite(), None);
        assert!(!r.is_composite_active());
        assert_eq!(r.composite_min_height(), None);
    }

    #[test]
    fn composite_gen_bumps_on_every_selection_change() {
        // Pin the generation-counter contract: every
        // selection-changing method bumps it so an in-flight
        // worker's captured token mismatches and its result
        // gets dropped on the floor instead of installed.
        // Per CR round 5 on PR #575.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
        image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
        image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
        r.push_line(99, &synth_line(TEST_PIXEL));

        let g0 = r.composite_gen();
        r.set_active_apid(99);
        let g1 = r.composite_gen();
        assert_ne!(g0, g1, "set_active_apid must bump");

        r.mark_composite_pending(recipe);
        let g2 = r.composite_gen();
        assert_ne!(g1, g2, "mark_composite_pending must bump");

        let g3 = r.prepare_composite_build(recipe, 5);
        assert_ne!(g2, g3, "prepare_composite_build must bump");
        assert_eq!(g3, r.composite_gen(), "returned gen matches stored");

        r.clear_composite();
        let g4 = r.composite_gen();
        assert_ne!(g3, g4, "clear_composite must bump");

        r.clear();
        let g5 = r.composite_gen();
        assert_ne!(g4, g5, "clear must bump");
    }

    #[test]
    fn install_composite_cache_drops_stale_workers() {
        // Pin the async-path stale-result guard: a worker that
        // captures gen=N then returns after a selection change
        // (gen=N+1) must not install its surface. Per CR round
        // 5 on PR #575.
        let mut r = LrptImageRenderer::new();
        let recipe = COMPOSITE_CATALOG[0];
        let captured_gen = r.prepare_composite_build(recipe, 5);
        // Simulate the user picking a different recipe before
        // the worker returns — bumps gen.
        let other = COMPOSITE_CATALOG[1];
        r.mark_composite_pending(other);
        // Build a dummy 5-line surface (the worker's output).
        let bytes = vec![0xFFu8; IMAGE_WIDTH * 5 * BYTES_PER_PIXEL];
        let surface = build_argb32_surface_from_bgra(bytes, IMAGE_WIDTH, 5)
            .expect("surface build for stale-worker test");
        // Old gen no longer matches; install must refuse.
        assert!(!r.install_composite_cache(recipe, captured_gen, 5, surface));
    }

    #[test]
    fn build_bgra_composite_bytes_matches_build_argb32_from_rgb() {
        // Cross-check the two paths produce byte-identical
        // pixels: the worker BGRA path (build_bgra_composite_bytes)
        // and the legacy sync path (assemble_rgb_composite +
        // build_argb32_from_rgb). Per CR round 5 on PR #575.
        let height = 4;
        let n = IMAGE_WIDTH * height;
        // Synthetic gradient — three different patterns per
        // channel so any byte mix-up shows.
        let r_pixels: Vec<u8> = (0..n)
            .map(|i| u8::try_from(i & 0xFF).unwrap_or(0))
            .collect();
        let g_pixels: Vec<u8> = (0..n)
            .map(|i| u8::try_from((i.wrapping_mul(3)) & 0xFF).unwrap_or(0))
            .collect();
        let b_pixels: Vec<u8> = (0..n)
            .map(|i| u8::try_from((i.wrapping_mul(7)) & 0xFF).unwrap_or(0))
            .collect();
        let snap = sdr_lrpt::image::CompositeSnapshot {
            r_pixels: r_pixels.clone(),
            g_pixels: g_pixels.clone(),
            b_pixels: b_pixels.clone(),
            height,
        };

        // Worker path: build BGRA bytes directly from the snap.
        let bgra_worker = build_bgra_composite_bytes(&snap);

        // Legacy path: assemble_rgb_composite → build_argb32_from_rgb,
        // then read BGRA out of the surface for compare.
        let rgb = sdr_lrpt::image::assemble_rgb_composite(&r_pixels, &g_pixels, &b_pixels, height);
        let mut surface_legacy =
            build_argb32_from_rgb(&rgb, IMAGE_WIDTH, height).expect("legacy surface build");
        let stride_legacy =
            usize::try_from(surface_legacy.stride()).expect("legacy stride non-negative");
        let data_legacy = surface_legacy.data().expect("legacy surface data");

        // Worker output uses packed stride (width * 4); the
        // legacy surface uses Cairo's stride which is also
        // width * 4 for ARGB32. Compare row-by-row to be
        // robust against future stride drift.
        let row_bytes = IMAGE_WIDTH * BYTES_PER_PIXEL;
        for row in 0..height {
            let worker_row = &bgra_worker[row * row_bytes..row * row_bytes + row_bytes];
            let legacy_row = &data_legacy[row * stride_legacy..row * stride_legacy + row_bytes];
            assert_eq!(
                worker_row, legacy_row,
                "BGRA mismatch on row {row}: worker path vs legacy path",
            );
        }
    }

    #[test]
    fn renderer_clear_drops_composite_state() {
        // `clear()` is between-pass cleanup; it must drop the
        // composite alongside the per-APID surfaces so a fresh
        // pass doesn't paint stale RGB pixels until the
        // dropdown handler rebuilds.
        let mut r = LrptImageRenderer::new();
        let image = LrptImage::new();
        let recipe = COMPOSITE_CATALOG[0];
        image.push_line(recipe.r_apid, &vec![0x10; IMAGE_WIDTH]);
        image.push_line(recipe.g_apid, &vec![0x20; IMAGE_WIDTH]);
        image.push_line(recipe.b_apid, &vec![0x30; IMAGE_WIDTH]);
        r.set_composite(recipe, &image);
        assert!(r.is_composite_active());
        r.clear();
        assert!(!r.is_composite_active());
        assert!(r.active_composite().is_none());
    }

    #[test]
    fn build_argb32_from_rgb_writes_bgra_byte_order() {
        // Pin Cairo's ARGB32 little-endian byte order — the
        // composite cache and the test assertion below would
        // both flip in lockstep otherwise. R/G/B input bytes
        // land at offsets +2 / +1 / +0 in the surface data;
        // alpha is opaque.
        let rgb = vec![0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33];
        let mut surface = build_argb32_from_rgb(&rgb, 2, 1).expect("argb32 build");
        let data = surface.data().expect("surface data");
        assert_eq!(&data[0..4], &[0xCC, 0xBB, 0xAA, 0xFF]);
        assert_eq!(&data[4..8], &[0x33, 0x22, 0x11, 0xFF]);
    }

    #[test]
    fn build_argb32_from_rgb_rejects_size_mismatch() {
        // Buffer length must equal width*height*3 — anything
        // else is a caller bug. The error string is matched
        // loosely; we just want to confirm the path doesn't
        // build a malformed surface.
        let rgb = vec![0; 10];
        assert!(build_argb32_from_rgb(&rgb, 4, 4).is_err());
    }

    // ─── write_rgb_png (#547) ───────────────────────────────

    #[test]
    fn write_rgb_png_round_trips_to_a_real_file() {
        // Pin the new RGB writer used by the LRPT composite
        // LOS-save path. Per #547.
        use std::io::Read;
        const W: usize = 32;
        const H: usize = 8;
        let pixels: Vec<u8> = (0..W * H * 3)
            .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
            .collect();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("sdr-ui-lrpt-rgb-{nanos}.png"));
        write_rgb_png(&path, &pixels, W, H).expect("write_rgb_png");
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert!(metadata.len() > 0);
        let mut header = [0_u8; 8];
        let mut f = std::fs::File::open(&path).expect("open");
        f.read_exact(&mut header).expect("read_exact");
        assert_eq!(&header, b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_rgb_png_rejects_size_mismatch() {
        let path = std::env::temp_dir().join("sdr-ui-lrpt-rgb-mismatch.png");
        // 10 bytes can't equal 4*4*3 = 48 — should fail without
        // creating the file.
        let result = write_rgb_png(&path, &[0_u8; 10], 4, 4);
        assert!(result.is_err());
        assert!(
            !path.exists(),
            "no file should be written on size-mismatch error",
        );
    }

    #[test]
    fn write_rgb_png_rejects_zero_size() {
        let path = std::env::temp_dir().join("sdr-ui-lrpt-rgb-zero.png");
        let result = write_rgb_png(&path, &[], 0, 0);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn write_rgb_png_zero_dim_with_pixels_reports_zero_sized() {
        // Same ordering invariant `write_greyscale_png` has: a
        // zero-dim call with a non-empty pixel buffer surfaces as
        // `ZeroSized`, not as the generic `InvalidBuffer`
        // length-mismatch.
        let path = std::env::temp_dir().join("sdr-ui-lrpt-rgb-zero-dim.png");
        let result = write_rgb_png(&path, &[1_u8, 2, 3], 0, 1);
        assert!(matches!(result, Err(crate::viewer::ViewerError::ZeroSized)));
        assert!(!path.exists());
    }
}
