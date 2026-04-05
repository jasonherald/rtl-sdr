//! Colormap generation for waterfall display.
//!
//! Provides a turbo/jet-style colormap mapping dB values (0..255) to RGBA,
//! suitable for RF spectrum visualization.

/// Number of entries in the colormap lookup table.
pub const COLORMAP_SIZE: usize = 256;

/// Generate a 256-entry RGBA colormap for waterfall display.
///
/// Maps indices 0..255 through a perceptually-motivated gradient:
/// black -> dark blue -> blue -> cyan -> green -> yellow -> red -> white.
/// This provides good contrast across the full dynamic range typical
/// of SDR waterfall displays (-120 dB to 0 dB).
#[allow(clippy::cast_precision_loss)]
pub fn generate_colormap() -> Vec<[u8; 4]> {
    let mut map = Vec::with_capacity(COLORMAP_SIZE);

    for i in 0..COLORMAP_SIZE {
        let t = i as f32 / (COLORMAP_SIZE - 1) as f32;
        let (r, g, b) = turbo_color(t);
        map.push([r, g, b, 255]);
    }

    map
}

/// Compute an RGB color from a turbo-style colormap at position `t` in [0, 1].
///
/// Uses a piecewise-linear interpolation through key color stops:
/// - 0.00 black
/// - 0.10 dark blue
/// - 0.25 blue
/// - 0.40 cyan
/// - 0.55 green
/// - 0.70 yellow
/// - 0.85 red
/// - 1.00 white
fn turbo_color(t: f32) -> (u8, u8, u8) {
    /// Color stop: (position, red, green, blue) all in 0..255.
    const STOPS: &[(f32, u8, u8, u8)] = &[
        (0.00, 0, 0, 0),       // black
        (0.10, 10, 10, 80),    // dark blue
        (0.25, 20, 40, 200),   // blue
        (0.40, 0, 180, 220),   // cyan
        (0.55, 20, 200, 40),   // green
        (0.70, 240, 220, 10),  // yellow
        (0.85, 240, 40, 10),   // red
        (1.00, 255, 255, 255), // white
    ];

    // Find the two stops that bracket `t`.
    let mut lower = 0;
    for (i, &(pos, _, _, _)) in STOPS.iter().enumerate().skip(1) {
        if pos >= t {
            lower = i - 1;
            break;
        }
        // If we've gone past all stops, clamp to last segment.
        if i == STOPS.len() - 1 {
            lower = i - 1;
        }
    }

    let (t0, r0, g0, b0) = STOPS[lower];
    let (t1, r1, g1, b1) = STOPS[lower + 1];

    let span = t1 - t0;
    let frac = if span > 0.0 { (t - t0) / span } else { 0.0 };
    let frac = frac.clamp(0.0, 1.0);

    let r = lerp_u8(r0, r1, frac);
    let g = lerp_u8(g0, g1, frac);
    let b = lerp_u8(b0, b1, frac);

    (r, g, b)
}

/// Linearly interpolate between two `u8` values.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let result = f32::from(a) + (f32::from(b) - f32::from(a)) * t;
    result.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn colormap_has_correct_size() {
        let map = generate_colormap();
        assert_eq!(map.len(), COLORMAP_SIZE);
    }

    #[test]
    fn colormap_starts_dark() {
        let map = generate_colormap();
        let [r, g, b, a] = map[0];
        // First entry should be near-black.
        assert!(r < 5, "red channel at 0 should be near 0, got {r}");
        assert!(g < 5, "green channel at 0 should be near 0, got {g}");
        assert!(b < 5, "blue channel at 0 should be near 0, got {b}");
        assert_eq!(a, 255);
    }

    #[test]
    fn colormap_ends_bright() {
        let map = generate_colormap();
        let [r, g, b, a] = map[COLORMAP_SIZE - 1];
        // Last entry should be near-white.
        assert!(r > 250, "red channel at 255 should be near 255, got {r}");
        assert!(g > 250, "green channel at 255 should be near 255, got {g}");
        assert!(b > 250, "blue channel at 255 should be near 255, got {b}");
        assert_eq!(a, 255);
    }

    #[test]
    fn colormap_all_entries_fully_opaque() {
        let map = generate_colormap();
        for (i, &[_, _, _, a]) in map.iter().enumerate() {
            assert_eq!(a, 255, "entry {i} alpha should be 255");
        }
    }

    #[test]
    fn colormap_midpoint_is_colored() {
        let map = generate_colormap();
        let [r, g, b, _] = map[128];
        // Midpoint should be somewhere in the green/cyan range — not black or white.
        let total = u16::from(r) + u16::from(g) + u16::from(b);
        assert!(
            total > 50,
            "midpoint should have visible color, total={total}"
        );
        assert!(total < 700, "midpoint should not be white, total={total}");
    }
}
