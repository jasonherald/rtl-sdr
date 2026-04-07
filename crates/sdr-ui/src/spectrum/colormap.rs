//! Colormap generation for waterfall display.
//!
//! Provides multiple colormaps mapping dB values (0..255) to RGBA,
//! suitable for RF spectrum visualization.

/// Number of entries in the colormap lookup table.
pub const COLORMAP_SIZE: usize = 256;

/// Available colormap styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColormapStyle {
    /// Turbo — black → blue → cyan → green → yellow → red → white.
    Turbo,
    /// Viridis — perceptually uniform, purple → teal → yellow.
    Viridis,
    /// Plasma — perceptually uniform, purple → pink → orange → yellow.
    Plasma,
    /// Inferno — perceptually uniform, black → purple → orange → yellow.
    Inferno,
}

/// Generate a 256-entry RGBA colormap for waterfall display.
#[allow(clippy::cast_precision_loss)]
pub fn generate_colormap(style: ColormapStyle) -> Vec<[u8; 4]> {
    let mut map = Vec::with_capacity(COLORMAP_SIZE);

    for i in 0..COLORMAP_SIZE {
        let t = i as f32 / (COLORMAP_SIZE - 1) as f32;
        let (r, g, b) = match style {
            ColormapStyle::Turbo => piecewise_color(t, &TURBO_STOPS),
            ColormapStyle::Viridis => piecewise_color(t, &VIRIDIS_STOPS),
            ColormapStyle::Plasma => piecewise_color(t, &PLASMA_STOPS),
            ColormapStyle::Inferno => piecewise_color(t, &INFERNO_STOPS),
        };
        map.push([r, g, b, 255]);
    }

    map
}

// Color stop tables: (position, red, green, blue) — sampled from matplotlib.

const TURBO_STOPS: [(f32, u8, u8, u8); 8] = [
    (0.00, 0, 0, 0),
    (0.10, 10, 10, 80),
    (0.25, 20, 40, 200),
    (0.40, 0, 180, 220),
    (0.55, 20, 200, 40),
    (0.70, 240, 220, 10),
    (0.85, 240, 40, 10),
    (1.00, 255, 255, 255),
];

const VIRIDIS_STOPS: [(f32, u8, u8, u8); 8] = [
    (0.00, 68, 1, 84),
    (0.14, 72, 35, 116),
    (0.28, 64, 67, 135),
    (0.42, 52, 95, 141),
    (0.57, 33, 145, 140),
    (0.71, 53, 183, 121),
    (0.85, 143, 215, 68),
    (1.00, 253, 231, 37),
];

const PLASMA_STOPS: [(f32, u8, u8, u8); 8] = [
    (0.00, 13, 8, 135),
    (0.14, 84, 2, 163),
    (0.28, 139, 10, 165),
    (0.42, 185, 50, 137),
    (0.57, 219, 92, 104),
    (0.71, 244, 136, 73),
    (0.85, 254, 188, 43),
    (1.00, 240, 249, 33),
];

const INFERNO_STOPS: [(f32, u8, u8, u8); 8] = [
    (0.00, 0, 0, 4),
    (0.14, 31, 12, 72),
    (0.28, 85, 15, 109),
    (0.42, 136, 34, 106),
    (0.57, 186, 54, 85),
    (0.71, 227, 89, 51),
    (0.85, 249, 149, 21),
    (1.00, 252, 255, 164),
];

/// Compute a piecewise-linear interpolated color from a stop table.
fn piecewise_color(t: f32, stops: &[(f32, u8, u8, u8)]) -> (u8, u8, u8) {
    let mut lower = 0;
    for (i, &(pos, _, _, _)) in stops.iter().enumerate().skip(1) {
        if pos >= t {
            lower = i - 1;
            break;
        }
        if i == stops.len() - 1 {
            lower = i - 1;
        }
    }

    let (t0, r0, g0, b0) = stops[lower];
    let (t1, r1, g1, b1) = stops[lower + 1];

    let span = t1 - t0;
    let frac = if span > 0.0 { (t - t0) / span } else { 0.0 };
    let frac = frac.clamp(0.0, 1.0);

    (
        lerp_u8(r0, r1, frac),
        lerp_u8(g0, g1, frac),
        lerp_u8(b0, b1, frac),
    )
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
        for style in [
            ColormapStyle::Turbo,
            ColormapStyle::Viridis,
            ColormapStyle::Plasma,
            ColormapStyle::Inferno,
        ] {
            let map = generate_colormap(style);
            assert_eq!(
                map.len(),
                COLORMAP_SIZE,
                "{style:?} should have {COLORMAP_SIZE} entries"
            );
        }
    }

    #[test]
    fn colormap_all_entries_fully_opaque() {
        for style in [
            ColormapStyle::Turbo,
            ColormapStyle::Viridis,
            ColormapStyle::Plasma,
            ColormapStyle::Inferno,
        ] {
            let map = generate_colormap(style);
            for (i, &[_, _, _, a]) in map.iter().enumerate() {
                assert_eq!(a, 255, "{style:?} entry {i} alpha should be 255");
            }
        }
    }

    #[test]
    fn colormap_midpoint_is_colored() {
        for style in [
            ColormapStyle::Turbo,
            ColormapStyle::Viridis,
            ColormapStyle::Plasma,
            ColormapStyle::Inferno,
        ] {
            let map = generate_colormap(style);
            let [r, g, b, _] = map[128];
            let total = u16::from(r) + u16::from(g) + u16::from(b);
            assert!(
                total > 50,
                "{style:?} midpoint should have visible color, total={total}"
            );
        }
    }
}
