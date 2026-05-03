//! Tray icon byte buffers (multi-size).
//!
//! ksni accepts ARGB32 raw bytes plus width/height. We ship the
//! tray icon at four pre-rasterized sizes (16/22/32/48) generated
//! from `data/com.sdr.rs.svg` by `scripts/regen-tray-icon.sh` and
//! committed to the repo. ksni's `Tray::icon_pixmap` returns a
//! `Vec<Icon>` and the tray host picks the closest match at draw
//! time — so `HiDPI` displays get the 32 or 48 px buffer, standard
//! tray slots get 22, and legacy hosts that ask for 16 get a
//! purpose-rendered 16. Per #573.
//!
//! No runtime SVG dep, no `librsvg` transitive tree, no `cairo`
//! initialisation cost.
//!
//! Trade-off: re-run `scripts/regen-tray-icon.sh` whenever the SVG
//! source changes. For an app icon that ships once and stays put,
//! that's a vastly better deal than a dependency cluster pulling
//! `nalgebra` / `paste` / `fxhash` (the last two unmaintained per
//! RUSTSEC) just to render the icon at startup. Per #512.
//!
//! Total committed bytes across all four sizes: 16 272 (1024 +
//! 1936 + 4096 + 9216).

/// One pre-baked tray-icon size: ksni-ready ARGB32 bytes plus the
/// width/height ksni's `Icon` constructor takes verbatim. Bytes
/// are layed out row-major, network-byte-order ARGB32 — the SNI
/// wire format.
struct PreBakedIcon {
    width: i32,
    height: i32,
    bytes: &'static [u8],
}

impl PreBakedIcon {
    /// Compile-time assertion that `bytes.len() == width * height *
    /// 4`. A const-fn returning the icon (rather than a const
    /// `PreBakedIcon` literal) lets us run the assertion at the
    /// expression site and produce a clear error message if the
    /// SVG regen ever drops bytes.
    #[allow(
        clippy::cast_sign_loss,
        reason = "width and height are positive icon dimensions \
                  hand-passed at every call site (16, 22, 32, 48); \
                  the cast to usize is safe by construction"
    )]
    const fn new(width: i32, height: i32, bytes: &'static [u8]) -> Self {
        assert!(
            bytes.len() == (width * height * 4) as usize,
            "tray icon byte length must equal width*height*4 — \
             regenerate via scripts/regen-tray-icon.sh",
        );
        Self {
            width,
            height,
            bytes,
        }
    }
}

/// Pre-baked 16x16 tray icon — low-DPI legacy tray hosts (`LXQt`,
/// older Plasma, some `XFCE` configurations).
const TRAY_ICON_16: PreBakedIcon = PreBakedIcon::new(
    16,
    16,
    include_bytes!("../../../data/com.sdr.rs.tray16.argb32"),
);

/// Pre-baked 22x22 tray icon — the `StatusNotifierItem` default
/// size, what most current Linux trays request first.
const TRAY_ICON_22: PreBakedIcon = PreBakedIcon::new(
    22,
    22,
    include_bytes!("../../../data/com.sdr.rs.tray22.argb32"),
);

/// Pre-baked 32x32 tray icon — `HiDPI` / 2x display scaling.
const TRAY_ICON_32: PreBakedIcon = PreBakedIcon::new(
    32,
    32,
    include_bytes!("../../../data/com.sdr.rs.tray32.argb32"),
);

/// Pre-baked 48x48 tray icon — large-tray hosts (some KDE
/// custom panels, accessibility large-icon mode).
const TRAY_ICON_48: PreBakedIcon = PreBakedIcon::new(
    48,
    48,
    include_bytes!("../../../data/com.sdr.rs.tray48.argb32"),
);

/// All four pre-baked sizes in ksni-preferred order. ksni passes
/// every entry through the `StatusNotifierItem` `IconPixmap` D-Bus
/// property; the tray host scans the array and renders whichever
/// matches its requested size best (typically by closest exact
/// match, with upscaling on a miss). Smallest-first is the SNI
/// convention.
const TRAY_ICONS: [&PreBakedIcon; 4] =
    [&TRAY_ICON_16, &TRAY_ICON_22, &TRAY_ICON_32, &TRAY_ICON_48];

/// Returns every pre-baked tray-icon size as `(width, height,
/// owned ARGB32 bytes)`. Each `Vec<u8>` is a fresh copy per call
/// — ksni's `Icon` type wants owned bytes.
pub(crate) fn current_icons() -> Vec<(i32, i32, Vec<u8>)> {
    TRAY_ICONS
        .iter()
        .map(|icon| (icon.width, icon.height, icon.bytes.to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each pre-baked buffer must be exactly width × height × 4
    /// bytes. Compile-time `const_assert` covers this in
    /// `PreBakedIcon::new`, but a runtime test makes the failure
    /// mode legible if the const ever lands wrong. Cheap.
    #[test]
    fn buffer_lengths_match_dimensions() {
        for icon in TRAY_ICONS {
            let expected = usize::try_from(icon.width)
                .and_then(|w| usize::try_from(icon.height).map(|h| w * h * 4))
                .expect("positive icon dimensions fit in usize");
            assert_eq!(
                icon.bytes.len(),
                expected,
                "icon {}x{} byte length mismatch",
                icon.width,
                icon.height,
            );
        }
    }

    /// Every icon must have at least one opaque pixel — a
    /// fully-transparent icon would draw a hole in the tray and
    /// signals a botched SVG regen.
    #[test]
    fn every_icon_has_at_least_one_opaque_pixel() {
        for icon in TRAY_ICONS {
            assert!(
                icon.bytes.chunks(4).any(|p| p[0] != 0),
                "icon {}x{} has zero alpha everywhere — \
                 regenerate via scripts/regen-tray-icon.sh",
                icon.width,
                icon.height,
            );
        }
    }

    /// `current_icons` must return one entry per pre-baked size,
    /// in `TRAY_ICONS` order, with matching dimensions.
    #[test]
    fn current_icons_returns_all_sizes_in_order() {
        let icons = current_icons();
        assert_eq!(icons.len(), TRAY_ICONS.len());
        for ((w, h, bytes), expected) in icons.iter().zip(TRAY_ICONS.iter()) {
            assert_eq!(*w, expected.width);
            assert_eq!(*h, expected.height);
            assert_eq!(bytes.len(), expected.bytes.len());
        }
    }

    /// SNI wants smallest-first. Pin the order so a future refactor
    /// that reshuffles the array doesn't silently break tray hosts
    /// that depend on the convention.
    #[test]
    fn icons_are_smallest_first() {
        for window in TRAY_ICONS.windows(2) {
            assert!(
                window[0].width < window[1].width,
                "TRAY_ICONS must be smallest-first for SNI compliance \
                 (got {} before {})",
                window[0].width,
                window[1].width,
            );
        }
    }
}
