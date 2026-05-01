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
use sdr_core::acars_airband_lock::AcarsRegion;

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
    /// Per-channel rows (one per region channel). Width is the
    /// `channel_count` argument passed to `build_aviation_panel`
    /// so it stays in lock-step with the active region's channel
    /// list (US-6 / Europe = 6; Custom is variable up to
    /// `MAX_CUSTOM_CHANNELS`). Subtitles are live-updated from
    /// `DspToUi::AcarsChannelStats` arrivals (~1 Hz cadence per
    /// the DSP-side throttle). Wrapped in `Rc<RefCell<…>>` so the
    /// region-change rebuild in
    /// `crate::window::connect_aviation_panel` can swap the row
    /// list while the 4 Hz tick still reads a live view (issue
    /// #592).
    pub channel_rows: std::rc::Rc<std::cell::RefCell<Vec<adw::ActionRow>>>,
    /// Per-channel rows' container group. Exposed so the
    /// region-change rebuild can `add` / `remove` rows when the
    /// active region's channel count changes (issue #592).
    pub channels_group: adw::PreferencesGroup,
    /// Region selector (issue #581). Switches the channel set
    /// and source center frequency between US-6, Europe, and
    /// user-defined Custom (#592). Wired up in
    /// `crate::window::connect_aviation_panel` to dispatch
    /// `UiToDsp::SetAcarsRegion` + persist the choice.
    pub region_row: adw::ComboRow,
    /// User-defined channel CSV editor. Visible only when the
    /// region combo's selected slot is the Custom slot. Wired
    /// in `crate::window::connect_aviation_panel`'s
    /// `connect_apply` handler to parse + validate + persist
    /// + dispatch. Issue #592.
    pub custom_channels_row: adw::EntryRow,
    /// Operator station ID — embedded in JSON's
    /// `station_id` field. Issue #578.
    pub station_id_row: adw::EntryRow,
    /// Toggle for the JSONL log writer. Issue #578.
    pub jsonl_enable_row: adw::SwitchRow,
    /// Path entry for the JSONL log. Visible only when
    /// `jsonl_enable_row` is on. Issue #578.
    pub jsonl_path_row: adw::EntryRow,
    /// Toggle for the UDP JSON feeder. Issue #578.
    pub network_enable_row: adw::SwitchRow,
    /// host:port entry for the feeder. Visible only when
    /// `network_enable_row` is on. Issue #578.
    pub network_addr_row: adw::EntryRow,
}

/// Region combo-row entries. Index → `(config_id, display_label)`.
/// The ordering here is the source of truth for both the model
/// and the persistence round-trip; bumping a new region means
/// appending here, plus the matching `AcarsRegion` variant and
/// `from_config_id`/`config_id` arms. Issue #592 added the
/// `"custom"` slot at index 2.
pub const REGION_OPTIONS: &[(&str, &str)] = &[
    ("us-6", "United States (US-6)"),
    ("europe", "Europe"),
    ("custom", "Custom"),
];

/// Combo-row slot index for the user-defined `Custom` region.
/// Used by the visibility binding for the custom-channels
/// `EntryRow`.
pub const CUSTOM_REGION_COMBO_INDEX: u32 = 2;

/// Map a `0..REGION_OPTIONS.len()` combo-row index back to the
/// matching region. Falls back to the default when the model
/// changes shape under us (e.g. transient null state during
/// rebuilds). The `Custom` arm returns
/// `AcarsRegion::Custom(Box::new([]))` as a placeholder; the
/// caller is responsible for populating the actual frequencies
/// from `acars_custom_channels` before dispatching.
#[must_use]
pub fn region_from_combo_index(idx: u32) -> AcarsRegion {
    REGION_OPTIONS
        .get(idx as usize)
        .map_or_else(AcarsRegion::default, |(id, _)| {
            AcarsRegion::from_config_id(id)
        })
}

/// Inverse of `region_from_combo_index`: locate the region's
/// index in the combo's model. Falls back to `0` (default
/// region) on misses, which mirrors the seeding behaviour at
/// startup when a stale config string can't be matched.
#[must_use]
pub fn region_combo_index(region: &AcarsRegion) -> u32 {
    let id = region.config_id();
    REGION_OPTIONS
        .iter()
        .position(|(slot_id, _)| *slot_id == id)
        .map_or(0, |i| u32::try_from(i).unwrap_or(0))
}

/// Build the Aviation activity panel. Pure widget assembly.
///
/// `channel_count` sizes the per-channel row list. Predefined
/// regions (US-6 / Europe) are 6; the Custom variant is variable
/// up to `MAX_CUSTOM_CHANNELS`. Callers source this from the
/// active region's `channels().len()`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_aviation_panel(channel_count: usize) -> AviationPanel {
    let page = adw::PreferencesPage::new();

    // ─── Group 1: ACARS toggle + status + open-window ───
    let acars_group = adw::PreferencesGroup::builder()
        .title("ACARS")
        .description(
            "Decode aircraft text-message broadcasts on ACARS airband channels. \
             Forces 2.5 MSps source rate and disables the VFO while on.",
        )
        .build();

    let enable_switch = adw::SwitchRow::builder()
        .title("Enable ACARS")
        .subtitle("Locks airband geometry and starts the 6-channel decoder")
        .build();
    acars_group.add(&enable_switch);

    // Region selector (issue #581). Predefined channel sets
    // (US-6 / Europe) plus user-defined Custom (issue #592).
    let region_model = gtk4::StringList::new(
        &REGION_OPTIONS
            .iter()
            .map(|(_, label)| *label)
            .collect::<Vec<_>>(),
    );
    let region_row = adw::ComboRow::builder()
        .title("Region")
        .subtitle("ACARS channel set + source center frequency")
        .model(&region_model)
        .build();
    acars_group.add(&region_row);

    // Custom-channels editor (issue #592). Visible only when the
    // region combo's selected slot is `CUSTOM_REGION_COMBO_INDEX`.
    // CSV of MHz values, parsed + validated by the apply handler
    // wired in `crate::window::connect_aviation_panel`.
    let custom_channels_row = adw::EntryRow::builder()
        .title("Custom channels (MHz, comma-separated)")
        .build();
    custom_channels_row.set_visible(false);
    acars_group.add(&custom_channels_row);

    // Bind visibility: visible only when the region combo's
    // selected slot is the Custom slot. Wired here in the
    // pure-widget builder because it's a self-contained
    // visibility mirror — no AppState / config touching.
    {
        let custom_row = custom_channels_row.clone();
        region_row.connect_selected_notify(move |row| {
            custom_row.set_visible(row.selected() == CUSTOM_REGION_COMBO_INDEX);
        });
    }

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
    // Title kept region-neutral — the same group hosts both US-6
    // and Europe channels, just relabeled per-row from the active
    // region's `channels()` array. Embedding the region name in
    // the group title would go stale on every region swap (the
    // builder doesn't return the group widget, so there's no
    // later hook to retitle it). CR round 1 on PR #593.
    let channels_group = adw::PreferencesGroup::builder()
        .title("Channels")
        .description(format!(
            "{GLYPH_LOCKED} Locked   {GLYPH_IDLE} Idle   {GLYPH_SIGNAL} Signal-no-decode"
        ))
        .build();

    let channel_rows: Vec<adw::ActionRow> = (0..channel_count)
        .map(|_| {
            let row = adw::ActionRow::builder().title("—").subtitle("—").build();
            channels_group.add(&row);
            row
        })
        .collect();

    page.add(&channels_group);

    // Output preferences group — JSONL log + UDP feeder +
    // station ID. Issue #578.
    let output_group = adw::PreferencesGroup::builder()
        .title("Output")
        .description("Log decoded messages to disk and forward them to external feeders (e.g. airframes.io).")
        .build();

    let station_id_row = adw::EntryRow::builder().title("Station ID").build();
    // 8-char cap matches acarsdec's `idstation` field width
    // (output.c uses an 8-byte char array for the station_id
    // embedded in JSON output). `AdwEntryRow` doesn't expose
    // `set_max_length` directly, so we truncate on `changed`.
    // CR round 2 on PR #595.
    station_id_row.connect_changed(|row| {
        let text = row.text();
        if text.chars().count() > 8 {
            let truncated: String = text.chars().take(8).collect();
            row.set_text(&truncated);
        }
    });
    output_group.add(&station_id_row);

    let jsonl_enable_row = adw::SwitchRow::builder().title("Write JSON log").build();
    output_group.add(&jsonl_enable_row);

    let jsonl_path_row = adw::EntryRow::builder().title("Log file path").build();
    jsonl_path_row.set_visible(false);
    output_group.add(&jsonl_path_row);

    let network_enable_row = adw::SwitchRow::builder()
        .title("Forward to network feeder")
        .build();
    output_group.add(&network_enable_row);

    let network_addr_row = adw::EntryRow::builder().title("Feeder address").build();
    network_addr_row.set_visible(false);
    output_group.add(&network_addr_row);

    // Visibility binding: path/addr rows visible only when
    // their toggle is on.
    jsonl_enable_row
        .bind_property("active", &jsonl_path_row, "visible")
        .sync_create()
        .build();
    network_enable_row
        .bind_property("active", &network_addr_row, "visible")
        .sync_create()
        .build();

    page.add(&output_group);

    AviationPanel {
        widget: page,
        enable_switch,
        status_row,
        open_viewer_button,
        channel_rows: std::rc::Rc::new(std::cell::RefCell::new(channel_rows)),
        channels_group,
        region_row,
        custom_channels_row,
        station_id_row,
        jsonl_enable_row,
        jsonl_path_row,
        network_enable_row,
        network_addr_row,
    }
}

/// Rebuild the per-channel rows in `panel.channels_group` to
/// match `channel_count`. Removes any existing rows first, then
/// appends `channel_count` fresh placeholder rows. Used by
/// `crate::window::connect_aviation_panel` on region change so
/// the row list stays in lock-step with
/// `region.channels().len()` (predefined regions are 6; Custom
/// is variable, including 0 when the user hasn't entered a CSV
/// yet). Issue #592.
pub fn rebuild_channel_rows(panel: &AviationPanel, channel_count: usize) {
    let mut rows = panel.channel_rows.borrow_mut();
    for row in rows.iter() {
        panel.channels_group.remove(row);
    }
    rows.clear();
    for _ in 0..channel_count {
        let row = adw::ActionRow::builder().title("—").subtitle("—").build();
        panel.channels_group.add(&row);
        rows.push(row);
    }
}
