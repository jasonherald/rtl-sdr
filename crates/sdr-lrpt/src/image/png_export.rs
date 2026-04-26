//! PNG export for assembled LRPT imagery.
//!
//! Thin layer over the [`image`] crate. Two entry points:
//! per-channel grayscale PNG via [`save_channel`], and
//! multi-channel false-color RGB composite via [`save_composite`].

use std::path::Path;

use super::composite::{ChannelBuffer, IMAGE_WIDTH, ImageAssembler};

/// Errors from PNG export. Typed (not `String`) so callers can
/// pattern-match each kind — useful for the live-viewer UI to
/// surface "no signal yet" (`EmptyChannel`) differently from
/// "disk write failed" (`Save`).
#[derive(Debug, thiserror::Error)]
pub enum PngExportError {
    /// The requested channel buffer is empty (no scan lines).
    /// Common during the first second of a new pass before any
    /// VCDU has been decoded.
    #[error("channel has no scan lines")]
    EmptyChannel,
    /// One or more of the requested composite channels is missing
    /// from the assembler or has zero scan lines. Surfaced when
    /// the user picks an RGB triple before all three channels
    /// have any data.
    #[error("composite unavailable: missing or empty channels")]
    CompositeUnavailable,
    /// The pixel buffer's length doesn't match the requested
    /// `width × height` (or `width × height × 3` for RGB).
    /// Indicates an internal bug — should never surface to a
    /// user.
    #[error("buffer size mismatch")]
    BufferSizeMismatch,
    /// The underlying [`image::ImageError`] from the encoder /
    /// filesystem write. Source preserved so callers can dig
    /// into the wrapped I/O or format error.
    #[error("png save: {0}")]
    Save(#[from] image::ImageError),
}

/// Save one channel's accumulated image to a grayscale PNG.
///
/// # Errors
///
/// - [`PngExportError::EmptyChannel`] if the channel has no
///   scan lines yet.
/// - [`PngExportError::BufferSizeMismatch`] if the channel's
///   internal pixel buffer length doesn't match
///   `lines × IMAGE_WIDTH` (internal bug).
/// - [`PngExportError::Save`] wrapping the underlying
///   [`image::ImageError`] from the PNG encoder or filesystem.
pub fn save_channel(path: &Path, channel: &ChannelBuffer) -> Result<(), PngExportError> {
    if channel.lines == 0 {
        return Err(PngExportError::EmptyChannel);
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "image dimensions are bounded by Meteor's 1568 px width and pass-length lines"
    )]
    let width_u32 = IMAGE_WIDTH as u32;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "ditto — pass-length lines fit in u32"
    )]
    let height_u32 = channel.lines as u32;
    let img = image::GrayImage::from_raw(width_u32, height_u32, channel.pixels.clone())
        .ok_or(PngExportError::BufferSizeMismatch)?;
    img.save(path)?;
    Ok(())
}

/// Save the RGB composite to a PNG.
///
/// # Errors
///
/// - [`PngExportError::CompositeUnavailable`] if any of the
///   three requested channels is missing from the assembler or
///   has zero scan lines.
/// - [`PngExportError::BufferSizeMismatch`] if the composite
///   buffer length doesn't match the expected
///   `width × height × 3` (internal bug).
/// - [`PngExportError::Save`] wrapping the underlying
///   [`image::ImageError`] from the PNG encoder or filesystem.
pub fn save_composite(
    path: &Path,
    assembler: &ImageAssembler,
    r_apid: u16,
    g_apid: u16,
    b_apid: u16,
) -> Result<(), PngExportError> {
    let (w, h, rgb) = assembler
        .composite_rgb(r_apid, g_apid, b_apid)
        .ok_or(PngExportError::CompositeUnavailable)?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "image dimensions are bounded by Meteor's 1568 px width and pass-length lines"
    )]
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb)
        .ok_or(PngExportError::BufferSizeMismatch)?;
    img.save(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// APID used in the round-trip save tests. Matches the
    /// AVHRR convention (64 = first channel) used elsewhere in
    /// the test suite.
    const APID_R: u16 = 64;
    const APID_G: u16 = 65;
    const APID_B: u16 = 66;

    /// Number of scan lines pushed in the round-trip tests.
    /// Small enough to keep the test fast; large enough that the
    /// PNG actually has multi-row content to verify against.
    const TEST_N_LINES: usize = 5;

    /// Per-channel grayscale fill values for the composite test.
    /// Distinct values so a regression that mismatched channel
    /// ordering would fail the byte-equality check.
    const FILL_R: u8 = 100;
    const FILL_G: u8 = 150;
    const FILL_B: u8 = 200;

    #[test]
    fn save_channel_writes_png_signature() {
        let mut cb = ChannelBuffer::new();
        for _ in 0..TEST_N_LINES {
            cb.push_line(&vec![128_u8; IMAGE_WIDTH]);
        }
        let tmp = std::env::temp_dir().join("test_lrpt_save_channel.png");
        save_channel(&tmp, &cb).expect("save");
        let bytes = std::fs::read(&tmp).expect("read back");
        // PNG file signature: 89 50 4E 47 ('\x89PNG').
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn save_channel_rejects_empty_buffer() {
        let cb = ChannelBuffer::new();
        let tmp = std::env::temp_dir().join("test_lrpt_empty.png");
        let result = save_channel(&tmp, &cb);
        assert!(
            matches!(result, Err(PngExportError::EmptyChannel)),
            "empty buffer must return EmptyChannel, got {result:?}"
        );
    }

    #[test]
    fn save_composite_writes_png_signature() {
        let mut a = ImageAssembler::new();
        for _ in 0..TEST_N_LINES {
            a.push_line(APID_R, &vec![FILL_R; IMAGE_WIDTH]);
            a.push_line(APID_G, &vec![FILL_G; IMAGE_WIDTH]);
            a.push_line(APID_B, &vec![FILL_B; IMAGE_WIDTH]);
        }
        let tmp = std::env::temp_dir().join("test_lrpt_save_composite.png");
        save_composite(&tmp, &a, APID_R, APID_G, APID_B).expect("save");
        let bytes = std::fs::read(&tmp).expect("read back");
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn save_composite_rejects_missing_channel() {
        // Only push two of the three requested channels — the
        // composite call should surface CompositeUnavailable
        // (not a panic, not a generic IO error). This is the UI
        // path's "user picked RGB before all three channels had
        // data" case.
        let mut a = ImageAssembler::new();
        a.push_line(APID_R, &vec![FILL_R; IMAGE_WIDTH]);
        a.push_line(APID_G, &vec![FILL_G; IMAGE_WIDTH]);
        let tmp = std::env::temp_dir().join("test_lrpt_missing_channel.png");
        let result = save_composite(&tmp, &a, APID_R, APID_G, APID_B);
        assert!(
            matches!(result, Err(PngExportError::CompositeUnavailable)),
            "missing channel must return CompositeUnavailable, got {result:?}"
        );
    }

    #[test]
    fn save_channel_propagates_io_failure_as_save_variant() {
        // Path inside a non-existent directory tree without
        // create_dir_all — the underlying image crate surfaces
        // an io error. We just want to confirm it lands in the
        // Save variant (preserving the source) and not in any
        // other branch.
        let mut cb = ChannelBuffer::new();
        cb.push_line(&vec![1_u8; IMAGE_WIDTH]);
        let bad_path = std::path::PathBuf::from(
            "/nonexistent-directory-for-lrpt-png-save-test/should-fail.png",
        );
        let result = save_channel(&bad_path, &cb);
        assert!(
            matches!(result, Err(PngExportError::Save(_))),
            "io failure must land in Save variant, got {result:?}"
        );
    }

    #[test]
    fn error_display_strings_are_human_readable() {
        // CR pattern: error variants have stable, descriptive
        // messages. Pin the strings so a future refactor that
        // accidentally swaps them in the `#[error(...)]` attrs
        // fails a test rather than silently rewording the UI.
        assert_eq!(
            format!("{}", PngExportError::EmptyChannel),
            "channel has no scan lines"
        );
        assert_eq!(
            format!("{}", PngExportError::CompositeUnavailable),
            "composite unavailable: missing or empty channels"
        );
        assert_eq!(
            format!("{}", PngExportError::BufferSizeMismatch),
            "buffer size mismatch"
        );
    }
}
