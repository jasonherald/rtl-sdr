//! Real decoded WEFAX rows (1809 bytes each, one byte per pixel) cut from
//! overnight NMF Boston 4235 kHz captures — provenance in
//! `tests/data/README.md`. Shared by the phasing and sync unit tests.

use super::PIXELS_PER_LINE;

/// A strong real phasing band (pulse leading edge ~1411-1417). Stray bright
/// specks left of the pulse on most lines defeat a first-bright-pixel column.
pub(crate) const PHASING_STRONG: &[u8] = include_bytes!("../../tests/data/wefax_phasing_strong.u8");

/// A weak real phasing band (pulse ~1591-1598): heavy speckle, so the pulse
/// is only ~25-45% of each line's bright pixels.
pub(crate) const PHASING_WEAK: &[u8] = include_bytes!("../../tests/data/wefax_phasing_weak.u8");

/// Real chart content with no phasing in it: a surface forecast, satellite
/// IR imagery, and an analysis chart (400 rows each).
pub(crate) const CONTENT: [&[u8]; 3] = [
    include_bytes!("../../tests/data/wefax_content_surface.u8"),
    include_bytes!("../../tests/data/wefax_content_satellite.u8"),
    include_bytes!("../../tests/data/wefax_content_analysis.u8"),
];

/// Split a fixture into lines.
pub(crate) fn lines(bytes: &[u8]) -> Vec<[u8; PIXELS_PER_LINE]> {
    let (rows, rest) = bytes.as_chunks::<PIXELS_PER_LINE>();
    assert!(rest.is_empty(), "fixture is whole PIXELS_PER_LINE rows");
    rows.to_vec()
}
