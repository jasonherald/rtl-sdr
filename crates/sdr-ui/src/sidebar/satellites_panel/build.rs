//! Widget construction for the Satellites scheduler panel —
//! [`build_satellites_panel`] lays out the ground-station, TLE,
//! notifications, recording, and upcoming-passes groups. Split out of
//! `satellites_panel.rs` per the file-size pass (issue #819). (The
//! former Heard-via-Orbcomm group moved to the dedicated Orbcomm
//! activity panel in epic #867.)

use libadwaita as adw;
use libadwaita::prelude::*;

use super::{
    ALT_DIGITS, ALT_MAX_M, ALT_MIN_M, ALT_STEP_M, AutoRecordQuality, DEFAULT_PASS_DISPLAY_COUNT,
    DEFAULT_STATION_ALT_M, DEFAULT_STATION_LAT_DEG, DEFAULT_STATION_LON_DEG, LAT_MAX_DEG,
    LAT_MIN_DEG, LATLON_DIGITS, LATLON_STEP_DEG, LON_MAX_DEG, LON_MIN_DEG, MIN_PASS_ELEVATION_DEG,
    PASS_LOOKAHEAD_HOURS, SatellitesPanel,
};

/// Build the Satellites scheduler panel widgets with first-run
/// defaults. Persisted values are restored later by
/// `window.rs::connect_satellites_panel` *before* it wires the
/// change-notify handlers, matching the scanner-panel pattern
/// (avoids spurious save-on-restore feedback during window
/// construction).
#[must_use]
pub fn build_satellites_panel() -> SatellitesPanel {
    let page = adw::PreferencesPage::new();

    // Attachment order == call order (each builder adds its group
    // to the page): Ground Station → TLE Data → Notifications →
    // Recording → Upcoming Passes — the same top-to-bottom order the
    // pre-split monolithic builder used. (The former "Heard via
    // Orbcomm" group moved to the dedicated Orbcomm activity panel in
    // epic #867.)
    let station = build_station_group(&page);
    let (last_refresh_row, refresh_button, refresh_spinner) = build_tle_group(&page);
    let notify_lead_row = build_notify_group(&page);
    let recording = build_recording_group(&page);
    let (passes_group, passes_status_row) = build_passes_group(&page);

    SatellitesPanel {
        widget: page,
        lat_row: station.lat_row,
        lon_row: station.lon_row,
        alt_row: station.alt_row,
        zip_row: station.zip_row,
        zip_status_row: station.zip_status_row,
        last_refresh_row,
        refresh_button,
        refresh_spinner,
        notify_lead_row,
        auto_record_switch: recording.auto_record_switch,
        auto_record_audio_switch: recording.auto_record_audio_switch,
        auto_record_composites_switch: recording.auto_record_composites_switch,
        auto_record_quality_row: recording.auto_record_quality_row,
        doppler_switch: recording.doppler_switch,
        passes_group,
        passes_status_row,
    }
}

/// Ground-station group — lat / lon / alt spin rows plus the ZIP
/// shortcut and its status row. Split out of
/// [`build_satellites_panel`] per the 50-NLOC gate (#819).
/// The Ground Station group's five rows, named because latitude /
/// longitude / altitude share the `SpinRow` type — a reordered
/// positional return would swap them silently, and these values
/// feed pass prediction, so the failure would be wrong pass times
/// rather than a compile error (`CodeRabbit` round 1 on PR #887;
/// same shape as [`RecordingRows`]).
#[allow(
    clippy::struct_field_names,
    reason = "field names deliberately mirror the SatellitesPanel fields they \
              feed so the orchestrator's struct literal moves them 1:1 \
              (same call as RecordingRows and PR #886's SharedRows)"
)]
struct StationRows {
    lat_row: adw::SpinRow,
    lon_row: adw::SpinRow,
    alt_row: adw::SpinRow,
    zip_row: adw::EntryRow,
    zip_status_row: adw::ActionRow,
}

fn build_station_group(page: &adw::PreferencesPage) -> StationRows {
    // ─── Ground station ────────────────────────────────────────
    let station_group = adw::PreferencesGroup::builder()
        .title("Ground Station")
        .description("Used to compute pass times for your location.")
        .build();

    let lat_row = adw::SpinRow::with_range(LAT_MIN_DEG, LAT_MAX_DEG, LATLON_STEP_DEG);
    lat_row.set_title("Latitude");
    lat_row.set_subtitle("Degrees north (negative = south)");
    lat_row.set_digits(LATLON_DIGITS);
    lat_row.set_value(DEFAULT_STATION_LAT_DEG);
    station_group.add(&lat_row);

    let lon_row = adw::SpinRow::with_range(LON_MIN_DEG, LON_MAX_DEG, LATLON_STEP_DEG);
    lon_row.set_title("Longitude");
    lon_row.set_subtitle("Degrees east (negative = west)");
    lon_row.set_digits(LATLON_DIGITS);
    lon_row.set_value(DEFAULT_STATION_LON_DEG);
    station_group.add(&lon_row);

    let alt_row = adw::SpinRow::with_range(ALT_MIN_M, ALT_MAX_M, ALT_STEP_M);
    alt_row.set_title("Altitude");
    alt_row.set_subtitle("Metres above WGS84 ellipsoid (≈ sea level)");
    alt_row.set_digits(ALT_DIGITS);
    alt_row.set_value(DEFAULT_STATION_ALT_M);
    station_group.add(&alt_row);

    // ZIP-code shortcut: the apply button (Enter / click) fires the
    // `apply` signal that `connect_satellites_panel` listens on. The
    // wiring layer runs the network lookup off-thread and writes
    // back to `lat_row`/`lon_row` on success.
    //
    // Deliberately NO custom suffix widget: AdwEntryRow's built-in
    // apply button lives in the suffix slot, and an earlier attempt
    // to pack a `gtk4::Spinner` next to it broke the apply button —
    // the user's Enter / click stopped firing the `apply` signal,
    // presumably because the layout pushed the apply button out of
    // its normal position. The status row below carries
    // "Looking up…" / "Resolved: …" text instead, which is plenty
    // of feedback for a sub-second lookup.
    let zip_row = adw::EntryRow::builder()
        .title("US ZIP code")
        .show_apply_button(true)
        .input_purpose(gtk4::InputPurpose::Digits)
        .build();
    station_group.add(&zip_row);

    // Always visible — toggling `visible(false) → set_visible(true)`
    // on rows packed into an `AdwPreferencesGroup` doesn't always
    // surface the row reliably (the group's internal listbox caches
    // child measurements at construction). Always-visible with a
    // hint title is also better UX: the user sees the affordance
    // without having to attempt a lookup first.
    let zip_status_row = adw::ActionRow::builder()
        .title("Type a US ZIP code and press ↵")
        .css_classes(["dim-label"])
        .build();
    station_group.add(&zip_status_row);

    page.add(&station_group);

    StationRows {
        lat_row,
        lon_row,
        alt_row,
        zip_row,
        zip_status_row,
    }
}

/// TLE group — last-refreshed row with the refresh button +
/// spinner suffix. Split out per the 50-NLOC gate (#819).
fn build_tle_group(page: &adw::PreferencesPage) -> (adw::ActionRow, gtk4::Button, gtk4::Spinner) {
    // ─── TLE Data ──────────────────────────────────────────────
    let tle_group = adw::PreferencesGroup::builder()
        .title("TLE Data")
        .description("Two-line element sets fetched from celestrak.org.")
        .build();

    let last_refresh_row = adw::ActionRow::builder()
        .title("Last refreshed")
        .subtitle("Never")
        .build();

    let refresh_spinner = gtk4::Spinner::builder().visible(false).build();

    let refresh_button = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Re-download TLE data from celestrak.org")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    // Tooltips aren't read by screen readers — set the accessible
    // label too, matching the project rule for icon-only buttons.
    refresh_button.update_property(&[gtk4::accessible::Property::Label("Refresh TLE data")]);

    let refresh_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    refresh_box.append(&refresh_spinner);
    refresh_box.append(&refresh_button);
    last_refresh_row.add_suffix(&refresh_box);

    tle_group.add(&last_refresh_row);
    page.add(&tle_group);

    (last_refresh_row, refresh_button, refresh_spinner)
}

/// Notifications group — the pre-pass lead-time spin row. Split
/// out per the 50-NLOC gate (#819).
fn build_notify_group(page: &adw::PreferencesPage) -> adw::SpinRow {
    // ─── Notifications ─────────────────────────────────────────
    let notify_group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .description(
            "Click the bell next to a pass to subscribe; you'll get a desktop \
             alert this many minutes before the satellite comes overhead.",
        )
        .build();

    let notify_lead_row = adw::SpinRow::with_range(
        f64::from(crate::sidebar::satellites_notify::NOTIFY_LEAD_MIN_LOWER),
        f64::from(crate::sidebar::satellites_notify::NOTIFY_LEAD_MIN_UPPER),
        1.0,
    );
    notify_lead_row.set_title("Lead time");
    notify_lead_row.set_subtitle("Minutes before AOS to fire the alert");
    notify_lead_row.set_digits(0);
    notify_lead_row.set_value(f64::from(
        crate::sidebar::satellites_notify::DEFAULT_NOTIFY_LEAD_MIN,
    ));
    notify_group.add(&notify_lead_row);
    page.add(&notify_group);

    notify_lead_row
}

/// The Recording group's five behavior toggles, named so the
/// orchestrator's struct literal can move them by field instead of
/// positionally. Split out per the 50-NLOC gate (#819).
struct RecordingRows {
    auto_record_switch: adw::SwitchRow,
    auto_record_audio_switch: adw::SwitchRow,
    auto_record_composites_switch: adw::SwitchRow,
    auto_record_quality_row: adw::ComboRow,
    doppler_switch: adw::SwitchRow,
}

/// Recording group — auto-record master switch, quality tier (with
/// its sensitivity sync), audio + composites toggles, Doppler
/// tracking. Split out per the 50-NLOC gate (#819).
fn build_recording_group(page: &adw::PreferencesPage) -> RecordingRows {
    // ─── Recording ─────────────────────────────────────────────
    let recording_group = adw::PreferencesGroup::builder()
        .title("Recording")
        .description("Pick what happens when a pass becomes active.")
        .build();

    let auto_record_switch = adw::SwitchRow::builder()
        .title("Auto-record satellite passes")
        .subtitle("Tune to the satellite, start the decoder, save the imagery at LOS. Works for Meteor-M LRPT and ARISS SSTV from the ISS.")
        .active(false)
        .build();
    recording_group.add(&auto_record_switch);

    let auto_record_quality_row = build_quality_row(&recording_group, &auto_record_switch);

    // Pairs with auto-record. Only takes effect when both this
    // and `auto_record_switch` are on; sampled exclusively at
    // AOS so a mid-pass toggle can't leave a half-stopped writer.
    // Per #533. Also depends on #532 (pre-volume WAV writer) to
    // capture usable audio when speakers are muted.
    //
    // LRPT passes ignore this toggle even when it's on — the
    // demod is silent (the imagery is the artifact) so a 10-min
    // pass would write ~115 MB of 48 kHz stereo silence
    // (`AUDIO_SAMPLE_RATE × AUDIO_CHANNELS × 2 B × 600 s`) for
    // no value. The recorder enforces this in `tick_idle`. Per
    // epic #469 task 7.4.
    let auto_record_audio_switch = adw::SwitchRow::builder()
        .title("Also save audio (.wav) — SSTV only")
        .subtitle(
            "Capture the demodulated audio alongside the imagery (paired by filename). \
             Has no effect on Meteor-M LRPT passes — those have a silent demod and \
             are skipped at the recorder regardless of this switch.",
        )
        .active(false)
        .build();
    recording_group.add(&auto_record_audio_switch);

    // False-colour composites — LRPT only. Off by default —
    // composites add a few PNG files per pass (one per recipe in
    // `lrpt_viewer::COMPOSITE_CATALOG`, currently 3) and not
    // every user wants the extras. SSTV passes ignore this toggle
    // by nature: SSTV is a single-channel format and the recorder
    // branches on `RecorderAction::SaveSstvPass` vs.
    // `RecorderAction::SaveLrptPass` (per-pass directory) before
    // reading this switch. Per #547.
    let auto_record_composites_switch = adw::SwitchRow::builder()
        .title("Save false-colour composites — LRPT only")
        .subtitle(
            "Write RGB composite PNGs (Natural colour, False-colour IR, Thermal IR) \
             alongside the per-APID files. SSTV passes ignore this — they're \
             single-channel by nature.",
        )
        .active(false)
        .build();
    recording_group.add(&auto_record_composites_switch);

    let doppler_switch = adw::SwitchRow::builder()
        .title("Doppler tracking")
        .subtitle("Auto-correct frequency drift during satellite passes")
        // Default ON — matches the persisted-default contract in
        // `load_doppler_tracking_enabled`. The wiring layer in
        // `window.rs::restore_doppler_switch` overrides this with
        // the persisted value, but if that wiring is ever skipped
        // the widget should still reflect "default ON" rather than
        // a misleading `false`. Per CR round 1 on PR #554.
        .active(true)
        .build();
    // Same group as the auto-record switches — these are all
    // "behavior toggles for the satellites workflow". Per #521.
    recording_group.add(&doppler_switch);
    page.add(&recording_group);

    RecordingRows {
        auto_record_switch,
        auto_record_audio_switch,
        auto_record_composites_switch,
        auto_record_quality_row,
        doppler_switch,
    }
}

/// Quality-threshold combo for the Recording group, added to the
/// group and kept sensitivity-synced to the auto-record switch.
/// Split out of [`build_recording_group`] per the 50-NLOC gate
/// (#819).
fn build_quality_row(
    recording_group: &adw::PreferencesGroup,
    auto_record_switch: &adw::SwitchRow,
) -> adw::ComboRow {
    // Quality threshold combo row — gates which passes are
    // worth auto-recording. The `AdwComboRow.string_list` indices
    // match `AutoRecordQuality::ALL` order; if the order ever
    // changes the persisted u32 indices in the user's config will
    // silently drift, so don't reorder. Per #511.
    let quality_strings = gtk4::StringList::new(
        &AutoRecordQuality::ALL
            .iter()
            .map(|q| q.display_label())
            .collect::<Vec<_>>(),
    );
    let auto_record_quality_row = adw::ComboRow::builder()
        .title("Quality threshold")
        .subtitle("Only passes with peak elevation at or above the selected tier auto-record.")
        .model(&quality_strings)
        .selected(AutoRecordQuality::DEFAULT.to_index())
        // Combo is only useful when auto-record is on; sensitivity
        // tracks the switch state below.
        .sensitive(false)
        .build();
    recording_group.add(&auto_record_quality_row);

    // Sync the combo's sensitivity to the auto-record switch.
    {
        let combo_clone = auto_record_quality_row.clone();
        auto_record_switch.connect_active_notify(move |row| {
            combo_clone.set_sensitive(row.is_active());
        });
        // Initial sync — the switch builder above defaulted to
        // `false`, so this just keeps the two in lockstep if a
        // future builder change flips the default.
        auto_record_quality_row.set_sensitive(auto_record_switch.is_active());
    }

    auto_record_quality_row
}

/// Upcoming-passes group with its empty-state row. Split out per
/// the 50-NLOC gate (#819).
fn build_passes_group(page: &adw::PreferencesPage) -> (adw::PreferencesGroup, adw::ActionRow) {
    // ─── Upcoming Passes ──────────────────────────────────────
    let passes_group = adw::PreferencesGroup::builder()
        .title("Upcoming Passes")
        .description(format!(
            "Next {DEFAULT_PASS_DISPLAY_COUNT} passes above \
             {MIN_PASS_ELEVATION_DEG:.0}° in the coming {PASS_LOOKAHEAD_HOURS} h."
        ))
        .build();

    let passes_status_row = adw::ActionRow::builder()
        .title("No passes yet")
        .subtitle("Click ↻ to fetch fresh TLE data, or adjust your ground station above.")
        .build();
    passes_group.add(&passes_status_row);
    page.add(&passes_group);

    (passes_group, passes_status_row)
}
