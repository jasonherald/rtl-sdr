use super::*;

// ============================================================
// Import-path adjustments for the #819 module split. Before the
// split every name below reached the test files through the
// `use super::*` glob against the monolithic `lrpt_viewer.rs`;
// the production items now live in the `composite` / `export` /
// `renderer` / `view` / `window` submodules (re-exported at the
// root for the `pub` surface), and the external-crate imports
// that used to sit at the old root are re-imported here. These
// `use` declarations are visible to the child test modules via
// their own `use super::*` globs, so the individual test files
// stay untouched.
// ============================================================
use sdr_lrpt::image::IMAGE_WIDTH;
use sdr_radio::lrpt_image::LrptImage;

use super::composite::{
    build_argb32_from_rgb, build_argb32_surface_from_bgra, build_bgra_composite_bytes,
};

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
