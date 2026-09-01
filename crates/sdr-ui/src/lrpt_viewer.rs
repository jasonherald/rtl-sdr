//! Live Meteor-M LRPT image viewer + PNG export.
//!
//! LRPT counterpart to [`crate::apt_viewer`]. Displays the
//! per-APID scan-line buffers from a shared
//! [`sdr_radio::lrpt_image::LrptImage`] as they accumulate during
//! a satellite pass. Width is fixed at the LRPT scan width
//! ([`IMAGE_WIDTH`] = 1568 px); height grows downward as new
//! lines arrive from the FEC chain.
//!
//! Three pieces:
//!
//! * [`LrptImageRenderer`] — pure Cairo renderer. Owns a
//!   `HashMap<APID, ChannelSurface>` of ARGB32 surfaces, each
//!   sized for a full pass. Knows how to paint the active
//!   channel into a cairo context with auto-fit + aspect
//!   preservation. No GTK dependency, fully unit-testable.
//! * [`LrptImageView`] — GTK widget wrapping a renderer plus a
//!   poll timer that drains new scan lines from the shared
//!   [`sdr_radio::lrpt_image::LrptImage`] handle. Cloneable
//!   (all state is `Rc`-shared) so closures on toolbar buttons
//!   can hold their own handle. Polling — rather than
//!   message-pushing as APT does — matches LRPT's
//!   `Arc<Mutex<ImageAssembler>>` data-sharing model: the DSP
//!   thread mutates the shared buffer, the UI reads it.
//! * [`open_lrpt_viewer_window`] — opens the view in a
//!   non-modal transient window. Header bar carries a channel
//!   selector, Pause / Resume, and Export PNG.
//!
//! [`connect_lrpt_action`] wires the `app.lrpt-open` action
//! (`Ctrl+Shift+L`). Activating it opens a viewer window and
//! registers the shared `LrptImage` handle with the DSP
//! controller via `UiToDsp::SetLrptImage`. Closing the window
//! is **purely a UI teardown** — the DSP capture stays running
//! and the shared image keeps accumulating decoded rows so the
//! recorder's LOS save still produces a per-pass directory.
//! The decoder is gated by `current_mode == Lrpt` and the
//! source-stop cleanup path (an explicit detach via
//! `UiToDsp::ClearLrptImage` is reserved as future API surface
//! and never sent today). Per `CodeRabbit` rounds 7 + 8 on PR
//! #543.

mod composite;
mod export;
mod renderer;
mod view;
mod window;

pub use composite::{COMPOSITE_CATALOG, CompositeRecipe};
pub use export::{ExportSnapshot, write_greyscale_png, write_rgb_png};
pub use renderer::{ActiveSelection, LrptImageRenderer, PushOutcome};
pub use view::LrptImageView;
pub use window::{connect_lrpt_action, open_lrpt_viewer_if_needed, open_lrpt_viewer_window};

/// Maximum lines we'll keep per channel. The MSU-MR scanner on
/// Meteor-M produces AVHRR-style imagery at ~6 scan lines per
/// second per channel; a long high-elevation pass (~15 min above
/// horizon) is therefore ~5400 lines, and a typical 10-min pass
/// is ~3600. 8192 gives ~2× headroom for the longest plausible
/// pass without ever growing the surface at runtime — the
/// previous 1024 cap clipped roughly the last 80 % of even a
/// nominal pass. Per `CodeRabbit` round 2 on PR #543.
///
/// Memory cost is lazy: the per-APID Cairo surface only
/// allocates when that channel first receives a line. At the
/// cap, one channel is `IMAGE_WIDTH × MAX_LINES × 4 B` ≈ 51 MB.
/// A typical pass with three active AVHRR channels therefore
/// peaks around 150 MB, which matches the rest of the SDR
/// pipeline's working-set budget.
pub const MAX_LINES: usize = 8_192;

/// Background colour painted before any LRPT data is received
/// (and behind the image when the widget is wider than the
/// image's aspect). Near-black so the eventual greyscale image
/// stands out, matching the APT viewer's palette.
const BACKGROUND_RGB: [f64; 3] = [0.05, 0.05, 0.06];

/// Bytes per pixel for Cairo's ARGB32 surface format —
/// `B`, `G`, `R`, `A` in little-endian byte order. Pulled out
/// of the hot-path pixel-copy loop in
/// [`LrptImageRenderer::push_line`] so a future format change
/// (e.g. RGB24 for the LRPT RGB-composite mode) is a one-line
/// edit. Per `CodeRabbit` round 4 on PR #543.
const BYTES_PER_PIXEL: usize = 4;

/// Default size for the viewer window. A typical Meteor MSU-MR
/// pass produces ~3600 lines × 1568 px (portrait, ~1:2 aspect)
/// at full duration. There's no scroll path — `DrawingArea`
/// sits directly under `ToolbarView` and `LrptImageRenderer::render`
/// scales the full image to fit the available area, preserving
/// aspect — so the visible pixels per scan-line drop as the
/// image grows tall. The default 900 × 600 landscape footprint
/// is chosen for ergonomics rather than aspect match: it sits
/// comfortably alongside the main radio window on a typical
/// 1080p+ desktop, fills well during the early-pass phase when
/// the image is still short and wide, and the user can resize
/// freely once they see how the pass is developing. (Pre-round-2
/// the comment claimed "wider than tall because typical pass
/// heights are ~600 lines" — that assumption was based on the
/// old 1024-line cap and stopped holding once `MAX_LINES`
/// bumped to 8192.) Per `CodeRabbit` rounds 14 + 15 on PR #543.
const VIEWER_WINDOW_WIDTH: i32 = 900;
const VIEWER_WINDOW_HEIGHT: i32 = 600;

/// Poll interval the view uses to drain new scan lines from
/// the shared `LrptImage` and queue redraws. MSU-MR produces
/// ~6 scan lines per second per channel; 250 ms (4 Hz) keeps
/// the viewer one tick behind the line arrival rate at most,
/// which feels responsive without burning CPU on a tight
/// loop. A faster cadence wouldn't pay off — multiple lines
/// land per tick anyway and `drain_new_lines` already batches
/// them. 60 FPS would be wasteful: there's no smooth-motion
/// content here, just discrete row appends. Per `CodeRabbit`
/// round 14 on PR #543 (refreshed from the older "~1 Hz" copy
/// that predated the round-2 MSU-MR rate research).
const POLL_INTERVAL_MS: u32 = 250;

/// Refresh interval for the channel-dropdown population tick.
/// Channel discovery on Meteor is rare (a handful of APIDs per
/// pass, all surfaced within the first minute), so 1 Hz is
/// plenty — anything faster would burn CPU on idle string
/// compares. Per `CodeRabbit` round 5 on PR #543.
const DROPDOWN_REFRESH_INTERVAL_MS: u32 = 1_000;

/// Downlink profile the DSP thread should decode `norad_id` with,
/// from the `KnownSatellite` catalog (`lrpt_modulation` +
/// `lrpt_differential`). An uncatalogued satellite (or a catalog
/// entry with no LRPT modulation) falls back to plain QPSK without
/// differential decoding — the standards-default LRPT profile, so
/// an unknown bird is more likely standard-spec than Meteor-style
/// OQPSK (CR round 1 on PR #663, #730). `lrpt_differential` is only
/// honoured alongside an explicit modulation.
#[must_use]
pub fn lrpt_downlink_for(norad_id: u32) -> sdr_radio::lrpt_decoder::LrptDownlink {
    let lrpt_entry = sdr_sat::KNOWN_SATELLITES
        .iter()
        .find(|s| s.norad_id == norad_id)
        .and_then(|s| {
            s.lrpt_modulation
                .map(|modulation| (modulation, s.lrpt_differential))
        });
    let Some((modulation, differential)) = lrpt_entry else {
        return sdr_radio::lrpt_decoder::LrptDownlink::new(sdr_dsp::lrpt::LrptMode::Qpsk, false);
    };
    let mode = match modulation {
        sdr_sat::LrptModulation::Qpsk => sdr_dsp::lrpt::LrptMode::Qpsk,
        sdr_sat::LrptModulation::Oqpsk => sdr_dsp::lrpt::LrptMode::Oqpsk,
    };
    sdr_radio::lrpt_decoder::LrptDownlink::new(mode, differential)
}

/// The DSP commands an LRPT pass start sends, in the order they must
/// be queued: the downlink profile first (a changed profile flushes
/// the old decoder's held-back row group into `image`), then the
/// canvas wipe. Clearing from the UI thread instead raced that flush
/// (CR on PR #806).
#[must_use]
pub fn lrpt_pass_start_commands(
    norad_id: u32,
    image: &sdr_radio::lrpt_image::LrptImage,
) -> [sdr_core::messages::UiToDsp; 2] {
    [
        sdr_core::messages::UiToDsp::SetLrptDownlink(lrpt_downlink_for(norad_id)),
        sdr_core::messages::UiToDsp::ClearLrptImageContents(image.clone()),
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
