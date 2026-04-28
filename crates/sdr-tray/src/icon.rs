//! Tray icon byte buffers.
//!
//! ksni accepts ARGB32 raw bytes plus width/height. We rasterize the
//! app's SVG icon at startup; if rasterization fails for any reason
//! (missing file, librsvg parse error, Cairo allocation error) we
//! fall back to a built-in solid-color 22x22 buffer so the tray
//! always has *something* to draw — failure here must never block
//! tray spawn.

use std::path::Path;
use std::sync::OnceLock;

pub(crate) const TRAY_ICON_SIZE: i32 = 22;

pub(crate) const FALLBACK_ICON_22X22_ARGB32: [u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] =
    fallback_argb32();

const fn fallback_argb32() -> [u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] {
    let mut out = [0u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize];
    let mut i = 0;
    while i < out.len() {
        out[i] = 0xFF; // A
        out[i + 1] = 0x21; // R
        out[i + 2] = 0x6F; // G
        out[i + 3] = 0xB6; // B
        i += 4;
    }
    out
}

pub(crate) fn rasterize_svg_to_argb32(
    path: &Path,
    size: i32,
) -> Result<(i32, i32, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let handle = rsvg::Loader::new().read_path(path)?;
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, size, size)?;
    {
        let cr = cairo::Context::new(&surface)?;
        let renderer = rsvg::CairoRenderer::new(&handle);
        let viewport = cairo::Rectangle::new(0.0, 0.0, f64::from(size), f64::from(size));
        renderer.render_document(&cr, &viewport)?;
    }
    surface.flush();
    let stride = surface.stride();
    let data = surface.data()?;
    // Cairo ARGB32 is native-endian; SNI wants network byte order
    // (A, R, G, B). On little-endian hosts the in-memory order is
    // BGRA — swap.
    let size_u = usize::try_from(size).unwrap_or(0);
    let mut out = Vec::with_capacity(size_u * size_u * 4);
    let stride_u = usize::try_from(stride).unwrap_or(0);
    for y in 0..size_u {
        let row_start = y * stride_u;
        for x in 0..size_u {
            let px = row_start + (x * 4);
            let b = data[px];
            let g = data[px + 1];
            let r = data[px + 2];
            let a = data[px + 3];
            out.extend_from_slice(&[a, r, g, b]);
        }
    }
    Ok((size, size, out))
}

static CACHED_ICON: OnceLock<(i32, i32, Vec<u8>)> = OnceLock::new();

pub(crate) fn current_icon() -> (i32, i32, Vec<u8>) {
    CACHED_ICON
        .get_or_init(|| {
            let svg_path = locate_app_icon();
            match rasterize_svg_to_argb32(&svg_path, TRAY_ICON_SIZE) {
                Ok(triple) => triple,
                Err(e) => {
                    tracing::warn!(
                        path = %svg_path.display(),
                        error = %e,
                        "tray icon rasterization failed, using fallback bytes",
                    );
                    (
                        TRAY_ICON_SIZE,
                        TRAY_ICON_SIZE,
                        FALLBACK_ICON_22X22_ARGB32.to_vec(),
                    )
                }
            }
        })
        .clone()
}

/// Search order: `$XDG_DATA_HOME` → `~/.local/share` → workspace `data/`.
fn locate_app_icon() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        let p = std::path::PathBuf::from(home).join("icons/hicolor/scalable/apps/com.sdr.rs.svg");
        if p.exists() {
            return p;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::PathBuf::from(home)
            .join(".local/share/icons/hicolor/scalable/apps/com.sdr.rs.svg");
        if p.exists() {
            return p;
        }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/com.sdr.rs.svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterize_svg_returns_argb32_at_requested_size() {
        let svg_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/com.sdr.rs.svg");
        let (w, h, bytes) =
            rasterize_svg_to_argb32(&svg_path, 22).expect("rasterize known-good SVG");
        assert_eq!(w, 22);
        assert_eq!(h, 22);
        assert_eq!(bytes.len(), 22 * 22 * 4);
        // `out` is laid out as `[A, R, G, B]` per pixel — see the
        // byte-swap loop in `rasterize_svg_to_argb32`. So `p[0]` is
        // the alpha channel; checking `!= 0` confirms at least one
        // non-transparent pixel landed in the buffer (otherwise SNI
        // would just draw a hole).
        assert!(
            bytes.chunks(4).any(|p| p[0] != 0),
            "rasterized icon has zero alpha everywhere",
        );
    }

    #[test]
    fn rasterize_svg_missing_file_returns_err() {
        let result =
            rasterize_svg_to_argb32(std::path::Path::new("/nonexistent/never-here.svg"), 22);
        assert!(result.is_err());
    }
}
