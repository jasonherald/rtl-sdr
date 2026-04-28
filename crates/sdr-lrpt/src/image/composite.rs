//! Per-channel scan-line buffer + RGB composite renderer.
//!
//! Meteor LRPT can transmit up to 6 AVHRR imaging channels per
//! pass. Each channel arrives as a stream of decoded 8×8 pixel
//! blocks (one per Meteor JPEG MCU); we stitch the blocks into a
//! 2D image per channel, indexed by APID (channel ID).
//!
//! The RGB compositor takes three channel selections and
//! produces a false-color image — what the user sees in the
//! live viewer.

use std::collections::HashMap;

use super::jpeg::{Block8x8, MCU_SIDE};

/// MCUs per Meteor LRPT scan line. Per medet's `mcu_per_line`.
pub const MCUS_PER_LINE: usize = 196;

/// Width of one Meteor scan line in pixels.
/// (= [`MCUS_PER_LINE`] × [`MCU_SIDE`] = 196 × 8 = 1568 px.)
pub const IMAGE_WIDTH: usize = MCUS_PER_LINE * MCU_SIDE;

/// One channel's accumulated grayscale image.
#[derive(Clone, Debug, Default)]
pub struct ChannelBuffer {
    /// Row-major 8-bit pixel data. Length = `lines * IMAGE_WIDTH`.
    pub pixels: Vec<u8>,
    /// Number of complete scan lines accumulated so far.
    pub lines: usize,
}

impl ChannelBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one full scan line ([`IMAGE_WIDTH`] pixels). Pads
    /// with zero if the input is short, truncates if too long.
    /// Used by callers that have a complete scanline ready.
    pub fn push_line(&mut self, line: &[u8]) {
        let mut padded = vec![0_u8; IMAGE_WIDTH];
        let n = line.len().min(IMAGE_WIDTH);
        padded[..n].copy_from_slice(&line[..n]);
        self.pixels.extend_from_slice(&padded);
        self.lines += 1;
    }

    /// Place one decoded 8×8 MCU at line index `mcu_row` and
    /// column index `mcu_col` (0-indexed in MCUs, not pixels).
    /// Auto-grows the underlying buffer to hold up to row
    /// `mcu_row + 1` of MCUs (= `(mcu_row + 1) * MCU_SIDE` lines
    /// of pixels).
    ///
    /// This is the per-MCU placement entry point used by the
    /// LRPT pipeline as JPEG decoding emits blocks.
    pub fn place_mcu(&mut self, mcu_row: usize, mcu_col: usize, block: &Block8x8) {
        let needed_lines = (mcu_row + 1) * MCU_SIDE;
        if needed_lines > self.lines {
            let new_pixels = (needed_lines - self.lines) * IMAGE_WIDTH;
            self.pixels.extend(std::iter::repeat_n(0_u8, new_pixels));
            self.lines = needed_lines;
        }
        if mcu_col >= MCUS_PER_LINE {
            return; // out of bounds — silently drop
        }
        let px_top = mcu_row * MCU_SIDE;
        let px_left = mcu_col * MCU_SIDE;
        for (dy, row) in block.iter().enumerate() {
            let dst_y = px_top + dy;
            let dst_off = dst_y * IMAGE_WIDTH + px_left;
            self.pixels[dst_off..dst_off + MCU_SIDE].copy_from_slice(row);
        }
    }

    pub fn clear(&mut self) {
        self.pixels.clear();
        self.lines = 0;
    }
}

/// Multi-channel image accumulator. Maps APID → channel buffer.
///
/// Uses the Meteor APID-as-channel-key convention: APID 64 / 65
/// / 66 / 67 / 68 / 69 are the AVHRR channels (the actual
/// channel set transmitted on a given pass varies; only the
/// active ones populate).
pub struct ImageAssembler {
    channels: HashMap<u16, ChannelBuffer>,
}

impl Default for ImageAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    /// Push one decoded 8×8 MCU for `apid` at the given MCU row
    /// + column. Creates the channel buffer on first sight.
    pub fn place_mcu(&mut self, apid: u16, mcu_row: usize, mcu_col: usize, block: &Block8x8) {
        self.channels
            .entry(apid)
            .or_default()
            .place_mcu(mcu_row, mcu_col, block);
    }

    /// Push one full scan line for `apid` (used by callers that
    /// already have a row buffered, e.g. APT-style consumers).
    pub fn push_line(&mut self, apid: u16, line: &[u8]) {
        self.channels.entry(apid).or_default().push_line(line);
    }

    /// Iterate channels by APID in unspecified order.
    pub fn channels(&self) -> impl Iterator<Item = (&u16, &ChannelBuffer)> {
        self.channels.iter()
    }

    /// Borrow the buffer for `apid`.
    #[must_use]
    pub fn channel(&self, apid: u16) -> Option<&ChannelBuffer> {
        self.channels.get(&apid)
    }

    /// Snapshot of three channels' pixel buffers + their truncated
    /// height, ready to be handed off to a worker thread for
    /// composite assembly without holding the assembler lock.
    /// Returns `None` if any APID is missing or any channel is
    /// empty. Per CR round 1 on PR #575.
    ///
    /// Width is implicit ([`IMAGE_WIDTH`]); height is
    /// `min(r.lines, g.lines, b.lines)` — same shortest-wins rule
    /// `composite_rgb` uses, so every output row has data from all
    /// three channels.
    ///
    /// The clone is a triple memcpy of the channels' `Vec<u8>`
    /// buffers (~3 MB each for a full pass). Pure memory-bandwidth
    /// work — no per-pixel logic — so the lock hold is short
    /// enough not to back up the decoder thread. Compose the RGB
    /// bytes via [`assemble_rgb_composite`] outside the lock.
    #[must_use]
    pub fn clone_channels_for_composite(
        &self,
        r_apid: u16,
        g_apid: u16,
        b_apid: u16,
    ) -> Option<CompositeSnapshot> {
        let r = self.channels.get(&r_apid)?;
        let g = self.channels.get(&g_apid)?;
        let b = self.channels.get(&b_apid)?;
        if r.lines == 0 || g.lines == 0 || b.lines == 0 {
            return None;
        }
        let height = r.lines.min(g.lines).min(b.lines);
        Some(CompositeSnapshot {
            r_pixels: r.pixels.clone(),
            g_pixels: g.pixels.clone(),
            b_pixels: b.pixels.clone(),
            height,
        })
    }

    /// Build an RGB composite from three channels. Returns
    /// `(width, height, RGB bytes)` or `None` if any of the
    /// three channels is missing or empty.
    ///
    /// Composite height is `min(r.lines, g.lines, b.lines)` — we
    /// stop at the shortest channel so every output row has
    /// data from all three.
    ///
    /// Performs both the channel snapshot AND the RGB interleave
    /// while holding the assembler's lock. For background-thread
    /// callers (the LRPT viewer's composite mode + the recorder's
    /// LOS save), prefer [`Self::clone_channels_for_composite`]
    /// followed by [`assemble_rgb_composite`] — that pattern
    /// holds the lock only long enough to memcpy the source
    /// buffers, then runs the per-pixel interleave outside.
    /// Per CR round 1 on PR #575.
    #[must_use]
    pub fn composite_rgb(
        &self,
        r_apid: u16,
        g_apid: u16,
        b_apid: u16,
    ) -> Option<(usize, usize, Vec<u8>)> {
        let snap = self.clone_channels_for_composite(r_apid, g_apid, b_apid)?;
        let rgb =
            assemble_rgb_composite(&snap.r_pixels, &snap.g_pixels, &snap.b_pixels, snap.height);
        Some((IMAGE_WIDTH, snap.height, rgb))
    }

    pub fn clear(&mut self) {
        self.channels.clear();
    }
}

/// Cloned channel pixels for one (r, g, b) composite recipe,
/// produced by [`ImageAssembler::clone_channels_for_composite`]
/// under the assembler's lock. Hand off to a worker thread (or
/// run inline) and assemble via [`assemble_rgb_composite`]
/// without holding the lock. Per CR round 1 on PR #575.
#[derive(Clone, Debug)]
pub struct CompositeSnapshot {
    /// Cloned pixel buffer for the R channel (greyscale, row-major,
    /// width = [`IMAGE_WIDTH`]). Length must be `IMAGE_WIDTH * height`.
    pub r_pixels: Vec<u8>,
    /// Cloned pixel buffer for the G channel.
    pub g_pixels: Vec<u8>,
    /// Cloned pixel buffer for the B channel.
    pub b_pixels: Vec<u8>,
    /// `min(r.lines, g.lines, b.lines)` — every row in the output
    /// has data from all three channels.
    pub height: usize,
}

/// Interleave three same-shape greyscale channel buffers into an
/// RGB byte buffer ([R, G, B, R, G, B, …], row-major, width =
/// [`IMAGE_WIDTH`]). Pure CPU work — call this OUTSIDE the
/// assembler lock, on the snapshot returned by
/// [`ImageAssembler::clone_channels_for_composite`]. Per CR round
/// 1 on PR #575.
///
/// Out-of-bounds reads are panic-prevented by truncating to
/// `min(r.len(), g.len(), b.len(), IMAGE_WIDTH * height)` —
/// callers passing a [`CompositeSnapshot`] always satisfy the
/// shape contract, but defensive bounds keep a corrupted
/// snapshot from panicking inside a `gio::spawn_blocking` worker
/// where the panic payload is harder to surface.
#[must_use]
pub fn assemble_rgb_composite(r: &[u8], g: &[u8], b: &[u8], height: usize) -> Vec<u8> {
    let total_pixels = IMAGE_WIDTH.saturating_mul(height);
    let bound = total_pixels.min(r.len()).min(g.len()).min(b.len());
    let mut rgb = Vec::with_capacity(bound * 3);
    for idx in 0..bound {
        rgb.push(r[idx]);
        rgb.push(g[idx]);
        rgb.push(b[idx]);
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_block(value: u8) -> Block8x8 {
        [[value; MCU_SIDE]; MCU_SIDE]
    }

    #[test]
    fn channel_buffer_pads_short_lines() {
        let mut cb = ChannelBuffer::new();
        cb.push_line(&[1, 2, 3]);
        assert_eq!(cb.lines, 1);
        assert_eq!(cb.pixels.len(), IMAGE_WIDTH);
        assert_eq!(&cb.pixels[..3], &[1, 2, 3]);
        assert_eq!(cb.pixels[3], 0, "should be padded with 0");
    }

    #[test]
    fn channel_buffer_truncates_long_lines() {
        let mut cb = ChannelBuffer::new();
        let huge = vec![5_u8; IMAGE_WIDTH * 2];
        cb.push_line(&huge);
        assert_eq!(cb.pixels.len(), IMAGE_WIDTH);
    }

    #[test]
    fn place_mcu_grows_buffer_and_places_block() {
        let mut cb = ChannelBuffer::new();
        // Place a block at MCU row 2, col 3.
        let block = fill_block(42);
        cb.place_mcu(2, 3, &block);
        // Buffer should now have (2+1)*8 = 24 lines.
        assert_eq!(cb.lines, 24);
        assert_eq!(cb.pixels.len(), 24 * IMAGE_WIDTH);
        // Every pixel inside the placed block should be 42.
        let px_top = 2 * MCU_SIDE;
        let px_left = 3 * MCU_SIDE;
        for dy in 0..MCU_SIDE {
            for dx in 0..MCU_SIDE {
                let idx = (px_top + dy) * IMAGE_WIDTH + (px_left + dx);
                assert_eq!(cb.pixels[idx], 42, "block pixel ({dy}, {dx}) wrong");
            }
        }
        // A pixel outside the block should still be 0.
        assert_eq!(cb.pixels[0], 0);
    }

    #[test]
    fn place_mcu_skips_out_of_bounds_columns() {
        let mut cb = ChannelBuffer::new();
        let block = fill_block(99);
        // MCUS_PER_LINE = 196; col 200 is past the line.
        cb.place_mcu(0, 200, &block);
        // Buffer grew but the block wasn't placed.
        assert_eq!(cb.lines, MCU_SIDE);
        assert!(cb.pixels.iter().all(|&p| p == 0), "no pixel should be 99");
    }

    #[test]
    fn assembler_routes_mcus_by_apid() {
        let mut a = ImageAssembler::new();
        a.place_mcu(64, 0, 0, &fill_block(11));
        a.place_mcu(65, 0, 0, &fill_block(22));
        let ch64 = a.channel(64).expect("channel 64");
        let ch65 = a.channel(65).expect("channel 65");
        assert_eq!(ch64.pixels[0], 11);
        assert_eq!(ch65.pixels[0], 22);
    }

    #[test]
    fn composite_requires_all_three_channels() {
        let mut a = ImageAssembler::new();
        a.push_line(64, &vec![100; IMAGE_WIDTH]);
        a.push_line(65, &vec![150; IMAGE_WIDTH]);
        // No channel 66.
        assert!(a.composite_rgb(64, 65, 66).is_none());
        a.push_line(66, &vec![200; IMAGE_WIDTH]);
        let (w, h, rgb) = a.composite_rgb(64, 65, 66).expect("composite");
        assert_eq!(w, IMAGE_WIDTH);
        assert_eq!(h, 1);
        // First pixel: (R, G, B) = (100, 150, 200).
        assert_eq!(&rgb[..3], &[100, 150, 200]);
    }

    #[test]
    fn composite_truncates_to_shortest_channel() {
        let mut a = ImageAssembler::new();
        a.push_line(64, &vec![1; IMAGE_WIDTH]);
        a.push_line(64, &vec![2; IMAGE_WIDTH]);
        a.push_line(65, &vec![3; IMAGE_WIDTH]);
        a.push_line(66, &vec![4; IMAGE_WIDTH]);
        a.push_line(66, &vec![5; IMAGE_WIDTH]);
        // R has 2 lines, G has 1, B has 2 → composite height = 1.
        let (_, h, _) = a.composite_rgb(64, 65, 66).expect("composite");
        assert_eq!(h, 1);
    }
}
