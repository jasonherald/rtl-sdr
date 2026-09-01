//! PNG export surface for the LRPT viewer (issue #819): the
//! standalone tightly-sized greyscale/RGB PNG writers shared with
//! the recorder's LOS save path, the [`ExportSnapshot`] contract
//! the Export button hands its worker thread, and the default
//! on-disk path builders. Split out of `lrpt_viewer.rs` per the
//! file-size pass.

use std::path::{Path, PathBuf};

use gtk4::{cairo, glib};

use super::BYTES_PER_PIXEL;
use super::composite::CompositeRecipe;
use crate::viewer::ViewerError;

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

/// Default path the Export PNG button writes to:
/// `~/sdr-recordings/lrpt-{apid}-YYYY-MM-DD-HHMMSS-uuuuuu.png`.
///
/// The microsecond suffix prevents collisions when the user
/// rapid-fires the export button on the same channel within the
/// same second — without it, the second export silently
/// overwrote the first via `File::create`'s truncate semantics.
/// Per `CodeRabbit` round 13 on PR #543.
pub(super) fn default_export_path(apid: Option<u16>) -> PathBuf {
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
pub(super) fn composite_export_path(recipe_name: &str) -> PathBuf {
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
