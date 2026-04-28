//! Tray icon byte buffers. Task 3 expands this with librsvg
//! rasterization; for now only the static fallback exists so the
//! Task 2 ksni wiring has something to draw.

#![allow(dead_code, reason = "stubs consumed by sdr-tray::lib in Task 2")]

pub(crate) const TRAY_ICON_SIZE: i32 = 22;

pub(crate) const FALLBACK_ICON_22X22_ARGB32: [u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] =
    [0; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize];

pub(crate) fn current_icon() -> (i32, i32, Vec<u8>) {
    (
        TRAY_ICON_SIZE,
        TRAY_ICON_SIZE,
        FALLBACK_ICON_22X22_ARGB32.to_vec(),
    )
}
