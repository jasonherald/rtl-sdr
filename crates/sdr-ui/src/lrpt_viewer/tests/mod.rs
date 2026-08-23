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

// ─── Composite catalog (#547) ───────────────────────────

// ─── write_rgb_png (#547) ───────────────────────────────

// --- #730 ---

mod composite;
mod downlink;
mod export;
mod renderer;
