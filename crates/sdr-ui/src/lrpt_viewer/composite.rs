//! False-colour composite machinery for the LRPT viewer (issue
//! #819): the [`CompositeRecipe`] catalog mapping user-facing
//! recipes to R/G/B APID triples, plus the pixel-format helpers
//! that turn composite RGB/BGRA byte buffers into Cairo ARGB32
//! surfaces for the renderer and the view's off-thread build
//! path. Split out of `lrpt_viewer.rs` per the file-size pass.

use gtk4::cairo;

use sdr_lrpt::image::IMAGE_WIDTH;

use super::BYTES_PER_PIXEL;
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
/// APID values are the AVHRR slots: 64 = ch1, 65 = ch2, 66 = ch3,
/// 68 = ch4 (thermal IR).
///
/// **Per-satellite, per-season availability.** Roscosmos schedules
/// each Meteor-M bird's broadcast set independently and rotates
/// IR availability with the season. As of May 2026:
///
/// - **METEOR-M2 4** transmits the standard set 64/65/68
///   (visible/visible/IR) — all three composites have full coverage.
/// - **METEOR-M2 3** is in summer mode broadcasting 64/65/66 (three
///   visible channels, no IR) — only Natural colour (123) covers
///   this set; the IR-based composites are silently unavailable
///   (`save_composite` returns `CompositeUnavailable` and the
///   recipe is skipped at LOS).
///
/// `sdr_sat::KnownSatellite::expected_lrpt_apids` carries each
/// satellite's current expected set, and the auto-record LOS path
/// emits a warning if the live transmission diverges from it (e.g.,
/// Roscosmos flips M2-3 back to standard mode and we start receiving
/// APID 68 again). Per #645.
///
/// If a future Meteor variant ships a different APID assignment
/// we'll add new recipes alongside these rather than mutate the
/// existing values — composites that worked once must keep working
/// as the catalog grows.
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

/// Shared dimension / buffer-shape validation for the composite
/// surface builders: `width` / `height` must fit Cairo's `i32`
/// API, and the byte buffer must be exactly
/// `width * height * bytes_per_pixel` long. Returns the
/// `(width, height)` pair as `i32` for the surface constructors.
/// `mul_dim` / `kind` thread through so the error text stays
/// byte-identical to the pre-split messages. Split out of
/// [`build_argb32_from_rgb`] / [`build_argb32_surface_from_bgra`]
/// per the 50-NLOC gate (#819, PR #880 Codacy precedent).
fn validated_composite_dims(
    buf_len: usize,
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    mul_dim: &'static str,
    kind: &str,
) -> Result<(i32, i32), ViewerError> {
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
        .and_then(|n| n.checked_mul(bytes_per_pixel))
        .ok_or(ViewerError::DimensionTooLarge {
            dim: mul_dim,
            value: usize::MAX,
        })?;
    if buf_len != expected {
        return Err(ViewerError::InvalidBuffer(format!(
            "composite {kind} buffer length {buf_len} doesn't match width*height*{bytes_per_pixel} ({width} * {height} * {bytes_per_pixel} = {expected})",
        )));
    }
    Ok((width_i32, height_i32))
}

/// Cairo's required ARGB32 stride for `width`, paired with our
/// packed `width * 4` stride, both as `i32`. The caller compares
/// them to decide whether the worker's tightly-packed BGRA buffer
/// can be handed to `create_for_data` verbatim or needs a padded
/// re-pack. Split out of [`build_argb32_surface_from_bgra`] per
/// the 50-NLOC gate (#819).
fn argb32_stride_pair(width: usize) -> Result<(i32, i32), ViewerError> {
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
    Ok((stride, packed_stride))
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
pub(super) fn build_argb32_from_rgb(
    rgb: &[u8],
    width: usize,
    height: usize,
) -> Result<cairo::ImageSurface, ViewerError> {
    let (width_i32, height_i32) = validated_composite_dims(
        rgb.len(),
        width,
        height,
        3,
        "composite width × height × 3",
        "RGB",
    )?;
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
pub(super) fn build_bgra_composite_bytes(snap: &sdr_lrpt::image::CompositeSnapshot) -> Vec<u8> {
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
pub(super) fn build_argb32_surface_from_bgra(
    bgra: Vec<u8>,
    width: usize,
    height: usize,
) -> Result<cairo::ImageSurface, ViewerError> {
    let (width_i32, height_i32) = validated_composite_dims(
        bgra.len(),
        width,
        height,
        BYTES_PER_PIXEL,
        "composite width × height × 4",
        "BGRA",
    )?;
    let (stride, packed_stride) = argb32_stride_pair(width)?;
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
