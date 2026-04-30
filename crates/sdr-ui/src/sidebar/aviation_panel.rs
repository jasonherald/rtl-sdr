//! Aviation sidebar activity panel (epic #474, sub-project 3).
//!
//! Pure widget construction — no `AppState` references, no
//! signal wiring. The connect-up logic (switch-row → DSP
//! command, status-row live refresh, channel-row refresh from
//! `DspToUi::AcarsChannelStats`) lives in
//! `crate::window::connect_aviation_panel`. Same separation
//! the other sidebar panels use.

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_core::acars_airband_lock::US_SIX_CHANNEL_COUNT;

/// Per-channel row glyphs for the lock-state column. Per spec
/// section "Group 2 — Channels":
///
/// - `LOCKED` ●  — receiving valid frames within the recent window
/// - `IDLE`   ○  — no signal detected
/// - `SIGNAL` ⚠  — RF energy present but no valid frames
pub const GLYPH_LOCKED: &str = "●";
pub const GLYPH_IDLE: &str = "○";
pub const GLYPH_SIGNAL: &str = "⚠";

/// Sidebar status-row refresh cadence (per spec section
/// "`AcarsPanel` structure" — subtitle live-updated, ~4 Hz).
/// Drives the `glib::timeout_add_local` tick in
/// `crate::window::connect_aviation_panel`.
pub const SIDEBAR_STATUS_REFRESH_MS: u64 = 250;

/// Aviation activity panel built widgets. Returned to
/// `build_window` so signal handlers can wire to specific
/// rows; the module itself does no wiring.
pub struct AviationPanel {
    /// Root `AdwPreferencesPage` to install in the activity-bar
    /// stack.
    pub widget: adw::PreferencesPage,
    /// "Enable ACARS" switch — drives `UiToDsp::SetAcarsEnabled`.
    pub enable_switch: adw::SwitchRow,
    /// Status row showing "Decoded N · Last: Ts ago" subtitle.
    /// Subtitle is live-updated at ~4 Hz from
    /// `crate::window::connect_aviation_panel`.
    pub status_row: adw::ActionRow,
    /// "Open ACARS Window" button — drives
    /// `crate::acars_viewer::open_acars_viewer_if_needed`.
    pub open_viewer_button: gtk4::Button,
    /// Per-channel rows (one per US-6 channel). Width sourced
    /// from `sdr_core::acars_airband_lock::US_SIX_CHANNEL_COUNT`
    /// so it stays in lock-step with the DSP-side channel array.
    /// Subtitles are live-updated from
    /// `DspToUi::AcarsChannelStats` arrivals (~1 Hz cadence per
    /// the DSP-side throttle).
    pub channel_rows: [adw::ActionRow; US_SIX_CHANNEL_COUNT],
}

/// Build the Aviation activity panel. Pure widget assembly.
#[must_use]
pub fn build_aviation_panel() -> AviationPanel {
    let page = adw::PreferencesPage::new();

    // ─── Group 1: ACARS toggle + status + open-window ───
    let acars_group = adw::PreferencesGroup::builder()
        .title("ACARS")
        .description(
            "Decode aircraft text-message broadcasts (130 MHz US airband). \
             Forces 2.5 MSps source rate and disables the VFO while on.",
        )
        .build();

    let enable_switch = adw::SwitchRow::builder()
        .title("Enable ACARS")
        .subtitle("Locks airband geometry and starts the 6-channel decoder")
        .build();
    acars_group.add(&enable_switch);

    let status_row = adw::ActionRow::builder()
        .title("Status")
        .subtitle("Disabled")
        .build();
    acars_group.add(&status_row);

    let open_viewer_row = adw::ActionRow::builder()
        .title("ACARS messages window")
        .subtitle("Live log of decoded aircraft messages")
        .build();
    let open_viewer_button = gtk4::Button::builder()
        .label("Open")
        .valign(gtk4::Align::Center)
        .build();
    open_viewer_row.add_suffix(&open_viewer_button);
    open_viewer_row.set_activatable_widget(Some(&open_viewer_button));
    acars_group.add(&open_viewer_row);

    page.add(&acars_group);

    // ─── Group 2: per-channel status rows ───
    let channels_group = adw::PreferencesGroup::builder()
        .title("Channels (US-6)")
        .description(format!(
            "{GLYPH_LOCKED} Locked   {GLYPH_IDLE} Idle   {GLYPH_SIGNAL} Signal-no-decode"
        ))
        .build();

    let channel_rows: [adw::ActionRow; US_SIX_CHANNEL_COUNT] = std::array::from_fn(|_| {
        let row = adw::ActionRow::builder().title("—").subtitle("—").build();
        channels_group.add(&row);
        row
    });

    page.add(&channels_group);

    AviationPanel {
        widget: page,
        enable_switch,
        status_row,
        open_viewer_button,
        channel_rows,
    }
}
