//! PNG export for assembled LRPT imagery.
//!
//! Thin layer over the [`image`] crate. Two entry points:
//! per-channel grayscale PNG via [`save_channel`], and
//! multi-channel false-color RGB composite via [`save_composite`].

use std::path::Path;

use super::composite::{ChannelBuffer, IMAGE_WIDTH, ImageAssembler};

/// Save one channel's accumulated image to a grayscale PNG.
///
/// # Errors
///
/// Returns an error string if the channel is empty, the buffer
/// dimensions don't match the expected `lines × IMAGE_WIDTH`
/// shape, or the PNG write itself fails.
pub fn save_channel(path: &Path, channel: &ChannelBuffer) -> Result<(), String> {
    if channel.lines == 0 {
        return Err("channel has no scan lines".into());
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
        .ok_or_else(|| "buffer size mismatch".to_string())?;
    img.save(path).map_err(|e| format!("png save: {e}"))
}

/// Save the RGB composite to a PNG.
///
/// # Errors
///
/// Returns an error string if any of the three channels are
/// missing or empty, the buffer dimensions don't match, or the
/// PNG write fails.
pub fn save_composite(
    path: &Path,
    assembler: &ImageAssembler,
    r_apid: u16,
    g_apid: u16,
    b_apid: u16,
) -> Result<(), String> {
    let (w, h, rgb) = assembler
        .composite_rgb(r_apid, g_apid, b_apid)
        .ok_or_else(|| "composite unavailable: missing or empty channels".to_string())?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "image dimensions are bounded by Meteor's 1568 px width and pass-length lines"
    )]
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb)
        .ok_or_else(|| "composite buffer size mismatch".to_string())?;
    img.save(path).map_err(|e| format!("png save: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_channel_writes_png_signature() {
        let mut cb = ChannelBuffer::new();
        for _ in 0..10 {
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
        assert!(result.is_err(), "empty buffer must fail to save");
    }

    #[test]
    fn save_composite_writes_png_signature() {
        let mut a = ImageAssembler::new();
        for _ in 0..5 {
            a.push_line(64, &vec![100; IMAGE_WIDTH]);
            a.push_line(65, &vec![150; IMAGE_WIDTH]);
            a.push_line(66, &vec![200; IMAGE_WIDTH]);
        }
        let tmp = std::env::temp_dir().join("test_lrpt_save_composite.png");
        save_composite(&tmp, &a, 64, 65, 66).expect("save");
        let bytes = std::fs::read(&tmp).expect("read back");
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
        let _ = std::fs::remove_file(&tmp);
    }
}
