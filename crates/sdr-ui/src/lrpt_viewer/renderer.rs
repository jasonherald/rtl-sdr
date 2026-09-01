//! Pure Cairo renderer for the LRPT viewer (issue #819):
//! [`LrptImageRenderer`] with its per-APID [`ChannelSurface`]s,
//! the cached composite surface, the [`PushOutcome`] watermark
//! contract, and the shared aspect-preserving surface painter.
//! No GTK dependency — fully unit-testable. Split out of
//! `lrpt_viewer.rs` per the file-size pass.

use std::collections::HashMap;
use std::path::Path;

use gtk4::cairo;

use sdr_lrpt::image::IMAGE_WIDTH;
use sdr_radio::lrpt_image::LrptImage;

use super::composite::{CompositeRecipe, build_argb32_from_rgb};
use super::{BACKGROUND_RGB, BYTES_PER_PIXEL, MAX_LINES};
use crate::viewer::ViewerError;

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
    /// `pub(super)` (not private) solely so the sibling `tests`
    /// module can reach into per-channel state (`n_lines`,
    /// `surface`) to synthesize cap/full-surface fixtures —
    /// pre-#819-split the tests lived under the defining module
    /// and saw the field implicitly. Production code outside
    /// `renderer.rs` must keep going through the public API.
    pub(super) channels: HashMap<u16, ChannelSurface>,
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

pub(super) struct ChannelSurface {
    pub(super) surface: cairo::ImageSurface,
    pub(super) n_lines: usize,
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
