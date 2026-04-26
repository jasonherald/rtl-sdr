//! Satellite pass scheduler panel — ground station settings, TLE
//! refresh status, upcoming-passes list, auto-record toggle.
//!
//! Pure view module: this file builds the widgets and exposes them
//! through [`SatellitesPanel`]. Signal-handler wiring (persisting
//! station coordinates on edit, kicking off TLE refreshes on a
//! worker thread, the 1 Hz countdown timer, recomputing the pass
//! list when ground station or TLEs change) lives in
//! `window.rs::connect_satellites_panel` — same separation as
//! `scanner_panel`.
//!
//! ## Layout
//!
//! Three flat `AdwPreferencesGroup`s on a single
//! `AdwPreferencesPage` (no `AdwExpanderRow`s — see CLAUDE.md
//! "Sidebar architecture" for the rationale):
//!
//! 1. **Ground Station** — latitude / longitude / altitude
//!    `AdwSpinRow`s plus a "ZIP code" entry that resolves to a
//!    centroid via [`sdr_sat::lookup_us_zip`]. First-run defaults
//!    to the geographic centre of the contiguous US so passes show
//!    up immediately and the user can tweak (or paste a ZIP) from
//!    a sensible starting point. ZIP lookup is US-only by design;
//!    international users enter lat/lon by hand.
//! 2. **TLE Data** — last-refreshed timestamp + a refresh button
//!    that re-downloads each entry in [`KNOWN_SATELLITES`] via
//!    Celestrak's per-NORAD `gp.php?CATNR=…` endpoint, through
//!    [`sdr_sat::TleCache`]. The button uses an `AdwSpinner` to
//!    show progress; refresh runs on a worker thread so the UI
//!    stays responsive.
//! 3. **Recording** — auto-record `AdwSwitchRow` (drives #482's
//!    auto-record-on-pass wiring once it lands).
//! 4. **Upcoming Passes** — one `AdwActionRow` per pass; titles
//!    carry "Sat name — countdown", subtitles carry max elevation
//!    plus start/end direction. Empty state shows a placeholder
//!    row pointing the user at the Refresh button.

use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use gtk4::glib;
use gtk4::prelude::ObjectExt;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_config::ConfigManager;
use sdr_sat::{
    GroundStation, KNOWN_SATELLITES, Pass, Satellite, TleCache, TleCacheError, upcoming_passes,
};

// ─── First-run defaults ────────────────────────────────────────────────

/// Geographic center of the contiguous US (≈ Lebanon, Kansas).
/// First-run default so passes appear immediately and the user
/// adjusts from a known-OK starting point rather than (0, 0) ocean.
pub const DEFAULT_STATION_LAT_DEG: f64 = 39.8283;
/// Geographic center of the contiguous US — longitude.
pub const DEFAULT_STATION_LON_DEG: f64 = -98.5795;
/// Default altitude — sea level. Most users are within a few hundred
/// metres of this; pass timings are mostly insensitive to altitude
/// at LEO ranges.
pub const DEFAULT_STATION_ALT_M: f64 = 0.0;

// ─── SpinRow bounds ────────────────────────────────────────────────────

/// Latitude range, degrees north (negative = south).
pub const LAT_MIN_DEG: f64 = -90.0;
pub const LAT_MAX_DEG: f64 = 90.0;
/// Longitude range, degrees east (negative = west).
pub const LON_MIN_DEG: f64 = -180.0;
pub const LON_MAX_DEG: f64 = 180.0;
/// Altitude range, metres above the WGS84 ellipsoid. Lower bound
/// covers the Dead Sea (-430 m); upper bound covers Mt Everest
/// (8849 m). Anything outside this range is almost certainly user
/// error or a flying-receiver scenario we'll address separately.
pub const ALT_MIN_M: f64 = -500.0;
pub const ALT_MAX_M: f64 = 9000.0;

/// Step / increment values for the `SpinRow`s. Lat/lon use 0.0001°
/// (~11 m at the equator) which is plenty of precision for pass
/// prediction; altitude is in whole metres.
const LATLON_STEP_DEG: f64 = 0.0001;
const ALT_STEP_M: f64 = 1.0;
const LATLON_DIGITS: u32 = 4;
const ALT_DIGITS: u32 = 0;

// ─── Pass enumeration tunables ─────────────────────────────────────────

/// How many upcoming passes the panel surfaces by default. Six fits
/// comfortably in the activity-bar panel without scrolling, and
/// covers ~12 hours of LEO activity at most stations (NOAA + Meteor
/// are 4-6 passes/day each visible above the horizon).
pub const DEFAULT_PASS_DISPLAY_COUNT: usize = 6;

/// Minimum peak elevation (degrees) for a pass to appear in the
/// list. Below ~5° the signal usually has too much horizon
/// attenuation to decode anything useful; we filter here so the
/// list reads as "things you can actually catch".
pub const MIN_PASS_ELEVATION_DEG: f64 = 5.0;

/// Forward window for pass enumeration. 24 h gives the user plenty
/// of advance planning info; cheap to compute (a few hundred
/// elevation evaluations per satellite per call).
pub const PASS_LOOKAHEAD_HOURS: i64 = 24;

/// Seconds per minute / hour, named so the countdown formatter in
/// [`format_pass_title`] reads as logic rather than as a wall of
/// `60`s and `3_600`s. Per project convention (CLAUDE.md "DSP
/// conventions"): name magic numbers.
const SECS_PER_MINUTE: i64 = 60;
const SECS_PER_HOUR: i64 = 60 * SECS_PER_MINUTE;

// ─── Config keys ───────────────────────────────────────────────────────

/// Persisted latitude (degrees, north-positive).
pub const KEY_STATION_LAT_DEG: &str = "sat_station_lat_deg";
/// Persisted longitude (degrees, east-positive).
pub const KEY_STATION_LON_DEG: &str = "sat_station_lon_deg";
/// Persisted altitude (metres above WGS84 ellipsoid).
pub const KEY_STATION_ALT_M: &str = "sat_station_alt_m";
/// Persisted RFC3339 timestamp of the last successful TLE refresh.
pub const KEY_TLE_LAST_REFRESH: &str = "sat_tle_last_refresh";
/// Persisted "auto-record APT passes" toggle.
pub const KEY_AUTO_RECORD_APT: &str = "sat_auto_record_apt";
/// Persisted "also save audio (.wav) on auto-record" toggle.
/// Pairs with [`KEY_AUTO_RECORD_APT`] — only meaningful when
/// auto-record is on. Default: `false` (opt-in). Per #533.
pub const KEY_AUTO_RECORD_AUDIO: &str = "sat_auto_record_audio";

// ─── Panel ─────────────────────────────────────────────────────────────

/// Widgets composing the Satellites scheduler panel. `Clone` is
/// derived so `connect_satellites_panel` can hand pieces to
/// closures without lifetime acrobatics — every field is a `GObject`
/// wrapper, so clone is a cheap refcount bump.
#[derive(Clone)]
pub struct SatellitesPanel {
    /// Root widget — `AdwPreferencesPage` packed into the
    /// "satellites" `GtkStack` child.
    pub widget: adw::PreferencesPage,

    // Ground station group --------------------------------------------------
    /// Latitude in decimal degrees (`LAT_MIN_DEG..=LAT_MAX_DEG`).
    pub lat_row: adw::SpinRow,
    /// Longitude in decimal degrees (`LON_MIN_DEG..=LON_MAX_DEG`).
    pub lon_row: adw::SpinRow,
    /// Altitude in metres above the WGS84 ellipsoid.
    pub alt_row: adw::SpinRow,
    /// US ZIP code entry. The built-in apply button (Enter / click)
    /// fires `apply` and `connect_satellites_panel` runs the lookup,
    /// updating `lat_row` and `lon_row`. Result text lands in
    /// [`zip_status_row`](Self::zip_status_row) — `AdwEntryRow` has
    /// no subtitle slot of its own. No custom spinner suffix on
    /// purpose — it conflicts with the built-in apply button.
    pub zip_row: adw::EntryRow,
    /// Status / feedback row for ZIP lookups. Always visible — the
    /// initial title is a hint ("Type a US ZIP code and press ↵")
    /// that gets rewritten by `connect_satellites_panel` on each
    /// lookup attempt: "Looking up…" while in flight, then
    /// "Resolved: Christiansburg, VA (647 m)" or the error text
    /// from `PostalLookupError::Display` on failure.
    pub zip_status_row: adw::ActionRow,

    // TLE status group ------------------------------------------------------
    /// Action row carrying the "last refreshed" timestamp in its
    /// subtitle. Updated by `connect_satellites_panel` when a
    /// refresh completes.
    pub last_refresh_row: adw::ActionRow,
    /// Refresh button packed as a suffix on `last_refresh_row`.
    /// Click handler in `connect_satellites_panel` spawns a
    /// blocking TLE fetch on a worker thread.
    pub refresh_button: gtk4::Button,
    /// Spinner shown next to the refresh button while a fetch is
    /// in flight. Sibling to the button rather than wrapping it so
    /// the button stays clickable visually after the fetch ends.
    pub refresh_spinner: gtk4::Spinner,

    // Recording group -------------------------------------------------------
    /// Auto-record toggle — drives #482's "open APT viewer +
    /// start decoding when a NOAA pass starts" wiring.
    pub auto_record_switch: adw::SwitchRow,
    /// "Also save audio (.wav)" toggle — when on AND
    /// `auto_record_switch` is on, the recorder fires
    /// `Action::StartAutoAudioRecord` at AOS and
    /// `Action::StopAutoAudioRecord` at LOS so the pass's
    /// demodulated audio lands in `~/sdr-recordings/audio-{slug}-
    /// {timestamp}.wav` paired with the PNG. Per #533.
    pub auto_record_audio_switch: adw::SwitchRow,

    // Next passes group -----------------------------------------------------
    /// The preferences group hosting the dynamically-built pass
    /// rows. `connect_satellites_panel` adds / removes
    /// `AdwActionRow`s from this group as the pass list is
    /// recomputed.
    pub passes_group: adw::PreferencesGroup,
    /// Empty-state row shown before any TLE refresh has succeeded
    /// or when no passes meet the elevation threshold in the
    /// lookahead window. Removed from the group when real pass
    /// rows are added; re-added when the list goes empty.
    pub passes_status_row: adw::ActionRow,
}

/// Weak counterpart of [`SatellitesPanel`] — every field is a
/// `glib::WeakRef`, so capturing one of these in a long-lived
/// closure (signal handler, `GLib` timer, async task) does NOT pin
/// the panel widgets alive.
///
/// **Why this exists:** the strong [`SatellitesPanel`] is `Clone`,
/// which made it tempting to capture clones inside closures stored
/// on the panel's own widgets — but doing so creates a refcount
/// cycle (widget → handler → closure → cloned panel → widget). The
/// cycle blocks teardown forever, including the 1 Hz countdown
/// timer's `WeakRef::upgrade` exit check (the upgrade keeps
/// returning `Some` because the panel can't drop). Using these
/// weak refs in every closure breaks the cycle and lets the panel
/// drop cleanly when the window closes.
///
/// `Clone` is derived so the same weak handle can be cheaply
/// cloned into multiple closures.
#[derive(Clone)]
pub struct SatellitesPanelWeak {
    /// Weak ref to [`SatellitesPanel::widget`].
    pub widget: glib::WeakRef<adw::PreferencesPage>,
    /// Weak ref to [`SatellitesPanel::lat_row`].
    pub lat_row: glib::WeakRef<adw::SpinRow>,
    /// Weak ref to [`SatellitesPanel::lon_row`].
    pub lon_row: glib::WeakRef<adw::SpinRow>,
    /// Weak ref to [`SatellitesPanel::alt_row`].
    pub alt_row: glib::WeakRef<adw::SpinRow>,
    /// Weak ref to [`SatellitesPanel::zip_row`].
    pub zip_row: glib::WeakRef<adw::EntryRow>,
    /// Weak ref to [`SatellitesPanel::zip_status_row`].
    pub zip_status_row: glib::WeakRef<adw::ActionRow>,
    /// Weak ref to [`SatellitesPanel::last_refresh_row`].
    pub last_refresh_row: glib::WeakRef<adw::ActionRow>,
    /// Weak ref to [`SatellitesPanel::refresh_button`].
    pub refresh_button: glib::WeakRef<gtk4::Button>,
    /// Weak ref to [`SatellitesPanel::refresh_spinner`].
    pub refresh_spinner: glib::WeakRef<gtk4::Spinner>,
    /// Weak ref to [`SatellitesPanel::auto_record_switch`].
    pub auto_record_switch: glib::WeakRef<adw::SwitchRow>,
    /// Weak ref to [`SatellitesPanel::auto_record_audio_switch`].
    pub auto_record_audio_switch: glib::WeakRef<adw::SwitchRow>,
    /// Weak ref to [`SatellitesPanel::passes_group`].
    pub passes_group: glib::WeakRef<adw::PreferencesGroup>,
    /// Weak ref to [`SatellitesPanel::passes_status_row`].
    pub passes_status_row: glib::WeakRef<adw::ActionRow>,
}

impl SatellitesPanel {
    /// Build a [`SatellitesPanelWeak`] suitable for capture in
    /// long-lived closures. See the type-level doc on
    /// `SatellitesPanelWeak` for why we use this everywhere instead
    /// of `Clone`.
    #[must_use]
    pub fn downgrade(&self) -> SatellitesPanelWeak {
        SatellitesPanelWeak {
            widget: self.widget.downgrade(),
            lat_row: self.lat_row.downgrade(),
            lon_row: self.lon_row.downgrade(),
            alt_row: self.alt_row.downgrade(),
            zip_row: self.zip_row.downgrade(),
            zip_status_row: self.zip_status_row.downgrade(),
            last_refresh_row: self.last_refresh_row.downgrade(),
            refresh_button: self.refresh_button.downgrade(),
            refresh_spinner: self.refresh_spinner.downgrade(),
            auto_record_switch: self.auto_record_switch.downgrade(),
            auto_record_audio_switch: self.auto_record_audio_switch.downgrade(),
            passes_group: self.passes_group.downgrade(),
            passes_status_row: self.passes_status_row.downgrade(),
        }
    }
}

impl SatellitesPanelWeak {
    /// Atomic upgrade — returns `Some(SatellitesPanel)` only if
    /// every widget is still alive. Returns `None` the moment
    /// any single field's underlying `GObject` has been dropped, so
    /// callers can short-circuit cleanly with a single `let-else`
    /// rather than threading partial-upgrade error handling
    /// through every closure body.
    #[must_use]
    pub fn upgrade(&self) -> Option<SatellitesPanel> {
        Some(SatellitesPanel {
            widget: self.widget.upgrade()?,
            lat_row: self.lat_row.upgrade()?,
            lon_row: self.lon_row.upgrade()?,
            alt_row: self.alt_row.upgrade()?,
            zip_row: self.zip_row.upgrade()?,
            zip_status_row: self.zip_status_row.upgrade()?,
            last_refresh_row: self.last_refresh_row.upgrade()?,
            refresh_button: self.refresh_button.upgrade()?,
            refresh_spinner: self.refresh_spinner.upgrade()?,
            auto_record_switch: self.auto_record_switch.upgrade()?,
            auto_record_audio_switch: self.auto_record_audio_switch.upgrade()?,
            passes_group: self.passes_group.upgrade()?,
            passes_status_row: self.passes_status_row.upgrade()?,
        })
    }
}

/// Build the Satellites scheduler panel widgets with first-run
/// defaults. Persisted values are restored later by
/// `window.rs::connect_satellites_panel` *before* it wires the
/// change-notify handlers, matching the scanner-panel pattern
/// (avoids spurious save-on-restore feedback during window
/// construction).
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "linear panel layout — ground-station group, TLE group, recording group (now with two switches per #533), and passes group are easier to follow as one block than artificially split into helpers"
)]
pub fn build_satellites_panel() -> SatellitesPanel {
    let page = adw::PreferencesPage::new();

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

    // ─── Recording ─────────────────────────────────────────────
    let recording_group = adw::PreferencesGroup::builder()
        .title("Recording")
        .description("Pick what happens when a pass becomes active.")
        .build();

    let auto_record_switch = adw::SwitchRow::builder()
        .title("Auto-record satellite passes")
        .subtitle("Tune to the satellite, start the decoder, save the imagery at LOS. Works for NOAA APT and Meteor-M LRPT.")
        .active(false)
        .build();
    recording_group.add(&auto_record_switch);

    // Pairs with auto-record. Only takes effect when both this
    // and `auto_record_switch` are on; sampled exclusively at
    // AOS so a mid-pass toggle can't leave a half-stopped writer.
    // Per #533. Also depends on #532 (pre-volume WAV writer) to
    // capture usable audio when speakers are muted.
    //
    // LRPT passes ignore this toggle even when it's on — the
    // demod is silent (the imagery is the artifact) so a 10-min
    // pass would write ~170 MB of stereo silence for no value.
    // The recorder enforces this in `tick_idle`. Per epic #469
    // task 7.4.
    let auto_record_audio_switch = adw::SwitchRow::builder()
        .title("Also save audio (.wav)")
        .subtitle("Capture demodulated audio alongside the image. Pairs with the PNG by filename. APT only — LRPT passes are silent.")
        .active(false)
        .build();
    recording_group.add(&auto_record_audio_switch);
    page.add(&recording_group);

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

    SatellitesPanel {
        widget: page,
        lat_row,
        lon_row,
        alt_row,
        zip_row,
        zip_status_row,
        last_refresh_row,
        refresh_button,
        refresh_spinner,
        auto_record_switch,
        auto_record_audio_switch,
        passes_group,
        passes_status_row,
    }
}

// ─── Config readers / writers used by the wiring layer ───────────────

/// Read the persisted ground station latitude, or fall back to
/// [`DEFAULT_STATION_LAT_DEG`].
#[must_use]
pub fn load_station_lat_deg(config: &Arc<ConfigManager>) -> f64 {
    read_f64_or(config, KEY_STATION_LAT_DEG, DEFAULT_STATION_LAT_DEG)
}

/// Read the persisted ground station longitude, or fall back to
/// [`DEFAULT_STATION_LON_DEG`].
#[must_use]
pub fn load_station_lon_deg(config: &Arc<ConfigManager>) -> f64 {
    read_f64_or(config, KEY_STATION_LON_DEG, DEFAULT_STATION_LON_DEG)
}

/// Read the persisted ground station altitude, or fall back to
/// [`DEFAULT_STATION_ALT_M`].
#[must_use]
pub fn load_station_alt_m(config: &Arc<ConfigManager>) -> f64 {
    read_f64_or(config, KEY_STATION_ALT_M, DEFAULT_STATION_ALT_M)
}

/// Read the persisted "auto-record APT passes" toggle state.
/// Defaults to `false` (opt-in).
#[must_use]
pub fn load_auto_record_apt(config: &Arc<ConfigManager>) -> bool {
    read_bool_or(config, KEY_AUTO_RECORD_APT, false)
}

/// Read the persisted "also save audio" toggle. Defaults to
/// `false` (opt-in). Per #533.
#[must_use]
pub fn load_auto_record_audio(config: &Arc<ConfigManager>) -> bool {
    read_bool_or(config, KEY_AUTO_RECORD_AUDIO, false)
}

/// Persist `value` under `key`. Single helper for the three
/// lat/lon/alt `SpinRow` change-notify handlers.
pub fn save_f64(config: &Arc<ConfigManager>, key: &str, value: f64) {
    config.write(|v| {
        v[key] = serde_json::json!(value);
    });
}

/// Persist `value` under [`KEY_AUTO_RECORD_APT`].
pub fn save_auto_record_apt(config: &Arc<ConfigManager>, value: bool) {
    config.write(|v| {
        v[KEY_AUTO_RECORD_APT] = serde_json::json!(value);
    });
}

/// Persist `value` under [`KEY_AUTO_RECORD_AUDIO`].
pub fn save_auto_record_audio(config: &Arc<ConfigManager>, value: bool) {
    config.write(|v| {
        v[KEY_AUTO_RECORD_AUDIO] = serde_json::json!(value);
    });
}

/// Persist a fresh "last refreshed" RFC3339 timestamp under
/// [`KEY_TLE_LAST_REFRESH`].
pub fn save_tle_last_refresh(config: &Arc<ConfigManager>, when: DateTime<Utc>) {
    config.write(|v| {
        v[KEY_TLE_LAST_REFRESH] = serde_json::json!(when.to_rfc3339());
    });
}

fn read_f64_or(config: &Arc<ConfigManager>, key: &str, default: f64) -> f64 {
    config.read(|v| {
        v.get(key)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(default)
    })
}

fn read_bool_or(config: &Arc<ConfigManager>, key: &str, default: bool) -> bool {
    config.read(|v| {
        v.get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(default)
    })
}

// ─── Helpers used by the wiring layer ─────────────────────────────────

/// Pass-quality boundaries (degrees of peak elevation). Tuned from
/// real-world receive experience:
///
/// * `>= 40°` → "winner" — clean image, clear land/cloud features.
/// * `>= 25°` → "good" — recognizable image with some noise at edges.
/// * `>= 15°` → "marginal" — main features survive but noisy.
/// * else (down to the [`MIN_PASS_ELEVATION_DEG`] floor) → "barely" —
///   mostly noise, only worth tuning if nothing better is in the
///   next few hours.
const QUALITY_WINNER_DEG: f64 = 40.0;
const QUALITY_GOOD_DEG: f64 = 25.0;
const QUALITY_MARGINAL_DEG: f64 = 15.0;

/// Hz → MHz conversion factor for the downlink formatter.
const HZ_PER_MHZ: f64 = 1_000_000.0;
/// Decimal-place ceiling for the formatted MHz string. Pinned to
/// 4 to preserve NOAA 18's 137.9125 MHz off-channel offset; any
/// catalog entry with finer-than-100-Hz precision would lose
/// digits past this.
const DOWNLINK_MAX_DECIMALS: usize = 4;
/// Decimal-place floor for the formatted MHz string. Padded out
/// to 3 even for round numbers so every panel row lines up
/// visually ("137.100" not "137.1").
const DOWNLINK_MIN_DECIMALS: usize = 3;

/// Map a pass's peak elevation to a one-word quality tag for the
/// pass row's subtitle. Helps the user spot which upcoming pass is
/// worth setting an alarm for vs. ones to skip past.
#[must_use]
pub fn pass_quality_label(peak_elev_deg: f64) -> &'static str {
    if peak_elev_deg >= QUALITY_WINNER_DEG {
        "winner"
    } else if peak_elev_deg >= QUALITY_GOOD_DEG {
        "good"
    } else if peak_elev_deg >= QUALITY_MARGINAL_DEG {
        "marginal"
    } else {
        "barely"
    }
}

/// Find the [`KnownSatellite`] entry whose display name matches the
/// pass's satellite. `None` for off-catalog satellites — shouldn't
/// happen in practice because the pass list is enumerated against
/// `KNOWN_SATELLITES`, but the name is the only key carried on
/// [`Pass`] so the lookup indirection is the natural shape. Shared
/// by every `*_for_pass` accessor in this module so the predicate
/// stays in exactly one place.
#[must_use]
fn known_satellite_for_pass(pass: &Pass) -> Option<&'static sdr_sat::KnownSatellite> {
    KNOWN_SATELLITES.iter().find(|s| s.name == pass.satellite)
}

/// Look up the downlink frequency for a satellite by its display
/// name. Returns `None` for satellites that aren't in
/// [`KNOWN_SATELLITES`].
#[must_use]
pub fn downlink_hz_for_pass(pass: &Pass) -> Option<u64> {
    known_satellite_for_pass(pass).map(|s| s.downlink_hz)
}

/// The full tuning quadruple — frequency, demod mode, channel
/// bandwidth, imaging protocol — for a given pass's satellite.
/// Returned as a tuple to keep the call site simple (the
/// play-button wiring layer destructures it directly into the
/// three `UiToDsp` setters; the recorder filters on the
/// fourth element).
///
/// The fourth element is `Option<ImagingProtocol>` so the play
/// button stays available on every catalog satellite (manual
/// tune is a user-initiated action). The recorder, in contrast,
/// only fires on satellites whose protocol is `Some(_)` — that
/// gate replaces the prior hardcoded `is_apt_capable` filter.
///
/// Returns `None` only when the pass's satellite isn't in
/// [`KNOWN_SATELLITES`] at all (off-catalog), same condition as
/// [`downlink_hz_for_pass`].
#[must_use]
pub fn tune_target_for_pass(
    pass: &Pass,
) -> Option<(
    u64,
    sdr_types::DemodMode,
    u32,
    Option<sdr_sat::ImagingProtocol>,
)> {
    known_satellite_for_pass(pass).map(|s| {
        (
            s.downlink_hz,
            s.demod_mode,
            s.bandwidth_hz,
            s.imaging_protocol,
        )
    })
}

/// Format a Hz frequency as a fixed-precision MHz string with
/// trailing zeros trimmed: `137_100_000` → `"137.100 MHz"`,
/// `137_912_500` → `"137.9125 MHz"`. Three decimals is enough to
/// disambiguate every NOAA / Meteor / ISS downlink we ship.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "u64 → f64 here only loses precision past ~2^53; our \
              downlink frequencies are in the 100s of MHz, far below \
              that ceiling"
)]
pub fn format_downlink_mhz(hz: u64) -> String {
    let mhz = hz as f64 / HZ_PER_MHZ;
    // Up to MAX decimals (4 → 137.9125), then trim trailing zeros
    // so 137.100 reads as "137.100" and 145.800 as "145.800" but
    // 137.9125 keeps its 4th digit.
    let raw = format!("{mhz:.DOWNLINK_MAX_DECIMALS$}");
    let trimmed = raw.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    // Always show at least MIN decimals so every entry lines up
    // visually in the panel ("137.100" not "137.1").
    let dot_idx = trimmed.find('.').unwrap_or(trimmed.len());
    let decimals = trimmed.len().saturating_sub(dot_idx + 1);
    let formatted = if decimals < DOWNLINK_MIN_DECIMALS {
        format!("{mhz:.DOWNLINK_MIN_DECIMALS$}")
    } else {
        trimmed.to_string()
    };
    format!("{formatted} MHz")
}

/// Description text shown on a pass row alongside its title-line
/// countdown. Format with downlink + quality tag:
/// `"winner · 137.100 MHz · max el 56° · AOS 245° → LOS 105°"`.
/// Falls back to the plain geometry-only form if the satellite
/// isn't in [`KNOWN_SATELLITES`] (no downlink to display).
#[must_use]
pub fn format_pass_subtitle(pass: &Pass) -> String {
    let quality = pass_quality_label(pass.max_elevation_deg);
    let geometry = format!(
        "max el {:.0}°  ·  AOS {:.0}° → LOS {:.0}°",
        pass.max_elevation_deg, pass.start_az_deg, pass.end_az_deg,
    );
    match downlink_hz_for_pass(pass) {
        Some(hz) => format!("{quality}  ·  {}  ·  {geometry}", format_downlink_mhz(hz)),
        None => format!("{quality}  ·  {geometry}"),
    }
}

/// Title-line countdown rendering. Examples:
///
/// * `"NOAA 19 — in 1h 12m"`
/// * `"NOAA 19 — in 4 min"`
/// * `"NOAA 19 — starting now"`
/// * `"NOAA 19 — in progress (3 min in)"`
/// * `"NOAA 19 — ended"` (only seen briefly between recomputes)
#[must_use]
pub fn format_pass_title(pass: &Pass, now: DateTime<Utc>) -> String {
    let to_start = pass.start - now;
    let to_end = pass.end - now;
    let secs = to_start.num_seconds();
    // Boundary conventions:
    // * `>= SECS_PER_HOUR` means a pass exactly 60 min away reads
    //   "in 1h 00m", not "in 60 min".
    // * `>= SECS_PER_MINUTE` means a pass exactly 60 s away reads
    //   "in 1 min", not "starting now".
    // * In the "in progress" branch, floor-div would render the
    //   first minute of an active pass as "0 min in"; clamp to 1
    //   so the user never sees a zero count for a running pass.
    let label = if secs >= SECS_PER_HOUR {
        let h = secs / SECS_PER_HOUR;
        let m = (secs % SECS_PER_HOUR) / SECS_PER_MINUTE;
        format!("in {h}h {m:02}m")
    } else if secs >= SECS_PER_MINUTE {
        format!("in {} min", secs / SECS_PER_MINUTE)
    } else if secs > 0 {
        "starting now".to_string()
    } else if to_end.num_seconds() > 0 {
        let mins_in = ((-secs) / SECS_PER_MINUTE).max(1);
        format!("in progress ({mins_in} min in)")
    } else {
        "ended".to_string()
    };
    format!("{} — {}", pass.satellite, label)
}

/// Enumerate the next [`DEFAULT_PASS_DISPLAY_COUNT`] visible passes
/// across every entry in [`KNOWN_SATELLITES`], sorted by start
/// time.
///
/// Errors looking up a TLE for a particular satellite (not yet
/// fetched, decommissioned, etc.) are logged and silently skipped
/// — the rest of the list still renders. Returns an empty `Vec`
/// when no satellites have usable TLEs (e.g. before the first
/// successful refresh).
#[must_use]
pub fn enumerate_upcoming_passes(
    cache: &TleCache,
    station: &GroundStation,
    from: DateTime<Utc>,
) -> Vec<Pass> {
    let to = from + ChronoDuration::hours(PASS_LOOKAHEAD_HOURS);
    let mut passes = Vec::new();
    for known in KNOWN_SATELLITES {
        // `cached_tle_for` (NOT `tle_for`) — this loop runs on the
        // GTK main thread on every lat/lon/alt edit, and `tle_for`
        // can trigger a synchronous HTTP fetch on cache miss /
        // staleness. A network call here would freeze the panel
        // mid-edit. The user explicitly refreshes via the button,
        // which uses `force_refresh` off-thread instead.
        match cache.cached_tle_for(known.norad_id) {
            Ok((line1, line2)) => match Satellite::from_tle(known.name, &line1, &line2) {
                Ok(sat) => {
                    let mut found =
                        upcoming_passes(station, &sat, from, to, MIN_PASS_ELEVATION_DEG);
                    passes.append(&mut found);
                }
                Err(e) => log_satellite_skip(known.name, &e.to_string()),
            },
            Err(TleCacheError::NotFound { .. }) => {
                // Common case before the first refresh — quiet log.
                tracing::debug!(
                    "no cached TLE for {} (NORAD {}); refresh to enable",
                    known.name,
                    known.norad_id,
                );
            }
            Err(e) => log_satellite_skip(known.name, &e.to_string()),
        }
    }
    passes.sort_by_key(|p| p.start);
    passes.truncate(DEFAULT_PASS_DISPLAY_COUNT);
    passes
}

fn log_satellite_skip(name: &str, why: &str) {
    tracing::warn!("skipping satellite {name} in pass list: {why}");
}

/// Format the "last refreshed" timestamp for the action row's
/// subtitle. Shows "Never" if no refresh has been recorded; shows
/// the local-timezone wall-clock time otherwise so it reads
/// naturally to the user.
#[must_use]
pub fn format_last_refresh(config: &Arc<ConfigManager>) -> String {
    let raw = config.read(|v| {
        v.get(KEY_TLE_LAST_REFRESH)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    match raw {
        None => "Never".to_string(),
        Some(rfc3339) => match chrono::DateTime::parse_from_rfc3339(&rfc3339) {
            Ok(dt) => dt
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string(),
            Err(_) => rfc3339, // Show the raw stamp on parse failure
        },
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn defaults_lie_inside_spinrow_bounds() {
        // Belt-and-braces: if someone tweaks DEFAULT_STATION_*
        // without updating the bounds, the SpinRow would clamp
        // the seeded value silently. Pin the invariant.
        assert!((LAT_MIN_DEG..=LAT_MAX_DEG).contains(&DEFAULT_STATION_LAT_DEG));
        assert!((LON_MIN_DEG..=LON_MAX_DEG).contains(&DEFAULT_STATION_LON_DEG));
        assert!((ALT_MIN_M..=ALT_MAX_M).contains(&DEFAULT_STATION_ALT_M));
    }

    #[test]
    fn read_f64_or_returns_default_for_missing_key() {
        let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({})));
        assert_eq!(read_f64_or(&cfg, "missing_key", 42.5), 42.5);
    }

    #[test]
    fn read_f64_or_returns_default_for_wrong_type() {
        let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            "wrong_type": "not a float",
        })));
        assert_eq!(read_f64_or(&cfg, "wrong_type", 42.5), 42.5);
    }

    #[test]
    fn read_f64_or_returns_persisted_value() {
        let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            "stored": 12.34,
        })));
        assert_eq!(read_f64_or(&cfg, "stored", 99.0), 12.34);
    }

    #[test]
    fn read_bool_or_returns_persisted_value() {
        let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            "auto": true,
        })));
        assert!(read_bool_or(&cfg, "auto", false));
    }

    #[test]
    fn format_last_refresh_says_never_when_unset() {
        let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({})));
        assert_eq!(format_last_refresh(&cfg), "Never");
    }

    #[test]
    fn format_last_refresh_renders_rfc3339_in_local_time() {
        // 2024-06-15T18:30:00Z is unambiguous; we don't pin the
        // local-formatted string (it depends on the test runner's
        // TZ). Pin two timezone-independent invariants instead:
        //
        //   1. The output isn't the literal "Never" placeholder.
        //   2. The output isn't the raw RFC3339 string — i.e. byte
        //      10 is the date-vs-time separator, and the formatter
        //      uses a space (`%Y-%m-%d %H:%M %Z`) where RFC3339
        //      uses `T`. Checking *that specific position* avoids
        //      false negatives from timezone abbreviations like
        //      `UTC` / `EDT` / `PDT` that happen to contain a `T`.
        let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            "sat_tle_last_refresh": "2024-06-15T18:30:00Z",
        })));
        let formatted = format_last_refresh(&cfg);
        assert_ne!(formatted, "Never");
        assert_eq!(
            formatted.chars().nth(10),
            Some(' '),
            "expected a space at position 10 (date/time separator); got: {formatted}",
        );
    }

    #[test]
    fn format_last_refresh_falls_back_to_raw_on_parse_error() {
        let cfg = Arc::new(ConfigManager::in_memory(&serde_json::json!({
            "sat_tle_last_refresh": "garbage timestamp",
        })));
        assert_eq!(format_last_refresh(&cfg), "garbage timestamp");
    }

    fn synthetic_pass(now: DateTime<Utc>, offset_min: i64) -> Pass {
        let start = now + ChronoDuration::minutes(offset_min);
        Pass {
            satellite: "NOAA 19".to_string(),
            start,
            end: start + ChronoDuration::minutes(12),
            max_elevation_deg: 56.0,
            max_el_time: start + ChronoDuration::minutes(6),
            start_az_deg: 245.0,
            end_az_deg: 105.0,
        }
    }

    #[test]
    fn format_pass_subtitle_shows_elevation_and_azimuths() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 30);
        let subtitle = format_pass_subtitle(&pass);
        assert!(subtitle.contains("max el 56"));
        assert!(subtitle.contains("AOS 245"));
        assert!(subtitle.contains("LOS 105"));
    }

    #[test]
    fn format_pass_subtitle_includes_quality_tag_and_downlink() {
        // NOAA 19 with 56° peak is a "winner" tier pass; downlink
        // is 137.100 MHz from the catalog.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 30);
        let subtitle = format_pass_subtitle(&pass);
        assert!(subtitle.contains("winner"), "subtitle: {subtitle}");
        assert!(subtitle.contains("137.100 MHz"), "subtitle: {subtitle}");
    }

    #[test]
    fn format_pass_subtitle_falls_back_when_satellite_not_in_catalog() {
        // A pass for a satellite the panel doesn't know about (user
        // has manually loaded a TLE, future) — subtitle still works,
        // just without the freq. The fallback is still
        // `quality · geometry`, NOT geometry-only — assert all three
        // invariants so a regression that drops the quality tag in
        // the off-catalog branch trips here rather than reaching the
        // user as an inconsistent row.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let mut pass = synthetic_pass(now, 30);
        pass.satellite = "FAKESAT-7".to_string();
        let subtitle = format_pass_subtitle(&pass);
        assert!(subtitle.contains("winner"), "subtitle: {subtitle}");
        assert!(subtitle.contains("max el 56"), "subtitle: {subtitle}");
        assert!(!subtitle.contains("MHz"), "subtitle: {subtitle}");
    }

    #[test]
    fn pass_quality_label_pins_boundary_values() {
        // Boundary table: the threshold value itself takes the
        // higher tier (`>=`), one tick below drops to the next.
        assert_eq!(pass_quality_label(60.0), "winner");
        assert_eq!(pass_quality_label(40.0), "winner");
        assert_eq!(pass_quality_label(39.9), "good");
        assert_eq!(pass_quality_label(25.0), "good");
        assert_eq!(pass_quality_label(24.9), "marginal");
        assert_eq!(pass_quality_label(15.0), "marginal");
        assert_eq!(pass_quality_label(14.9), "barely");
        assert_eq!(pass_quality_label(5.0), "barely");
    }

    #[test]
    fn format_downlink_mhz_renders_three_decimals_minimum() {
        // 137.100 MHz reads as "137.100", not "137.1" — the panel
        // wants every entry to line up visually.
        assert_eq!(format_downlink_mhz(137_100_000), "137.100 MHz");
        assert_eq!(format_downlink_mhz(145_800_000), "145.800 MHz");
    }

    #[test]
    fn format_downlink_mhz_preserves_extra_precision_when_needed() {
        // NOAA 18 is on 137.9125 MHz exactly — the formatter must
        // not round to 3 decimals and lose the off-channel offset.
        assert_eq!(format_downlink_mhz(137_912_500), "137.9125 MHz");
    }

    #[test]
    fn downlink_hz_for_pass_finds_catalog_entry() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 30); // satellite = "NOAA 19"
        assert_eq!(downlink_hz_for_pass(&pass), Some(137_100_000));
    }

    #[test]
    fn downlink_hz_for_pass_returns_none_for_unknown_satellite() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let mut pass = synthetic_pass(now, 30);
        pass.satellite = "MYSTERY-SAT".to_string();
        assert_eq!(downlink_hz_for_pass(&pass), None);
    }

    #[test]
    fn tune_target_for_pass_returns_full_tuning_quadruple() {
        // Pin the (downlink_hz, demod_mode, bandwidth_hz,
        // imaging_protocol) quadruple for a known catalog entry.
        // A future refactor that splits or reorders the tuple —
        // or that drifts the catalog values — fails here before
        // reaching the play-button wiring layer or the recorder.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 30); // satellite = "NOAA 19"
        let target = tune_target_for_pass(&pass).expect("NOAA 19 is in catalog");
        assert_eq!(target.0, 137_100_000);
        assert_eq!(target.1, sdr_types::DemodMode::Nfm);
        assert_eq!(target.2, 38_000);
        assert_eq!(target.3, Some(sdr_sat::ImagingProtocol::Apt));
    }

    #[test]
    fn tune_target_for_pass_returns_lrpt_protocol_for_meteor() {
        // Per epic #469 task 7, METEOR-M 2 / METEOR-M2 3 are now
        // flagged `Some(ImagingProtocol::Lrpt)` in the catalog so
        // the recorder enrolls them in the auto-record flow. The
        // play button still uses the same `tune_target_for_pass`
        // path; the protocol field is only consumed by the
        // recorder, so this test pins the catalog→LRPT routing.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let mut pass = synthetic_pass(now, 30);
        pass.satellite = "METEOR-M 2".to_string();
        let target = tune_target_for_pass(&pass).expect("METEOR-M 2 is in catalog");
        assert_eq!(
            target.3,
            Some(sdr_sat::ImagingProtocol::Lrpt),
            "Meteor protocol must be Lrpt after epic #469 task 7",
        );
    }

    #[test]
    fn tune_target_for_pass_returns_none_for_unknown_satellite() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let mut pass = synthetic_pass(now, 30);
        pass.satellite = "MYSTERY-SAT".to_string();
        assert!(tune_target_for_pass(&pass).is_none());
    }

    #[test]
    fn format_pass_title_uses_h_m_for_far_passes() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 75); // 1 h 15 min away
        let title = format_pass_title(&pass, now);
        assert_eq!(title, "NOAA 19 — in 1h 15m");
    }

    #[test]
    fn format_pass_title_uses_minutes_for_near_passes() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 12); // 12 min away
        assert_eq!(format_pass_title(&pass, now), "NOAA 19 — in 12 min");
    }

    #[test]
    fn format_pass_title_says_starting_now_inside_one_minute() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass starts in 30 seconds.
        let pass = Pass {
            satellite: "NOAA 19".to_string(),
            start: now + ChronoDuration::seconds(30),
            end: now + ChronoDuration::minutes(12),
            max_elevation_deg: 50.0,
            max_el_time: now + ChronoDuration::minutes(5),
            start_az_deg: 0.0,
            end_az_deg: 0.0,
        };
        assert_eq!(format_pass_title(&pass, now), "NOAA 19 — starting now");
    }

    #[test]
    fn format_pass_title_says_in_progress_for_active_passes() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Started 3 minutes ago, ends in 9.
        let pass = synthetic_pass(now, -3);
        let title = format_pass_title(&pass, now);
        assert!(
            title.contains("in progress"),
            "expected 'in progress', got {title:?}"
        );
        assert!(
            title.contains("3 min in"),
            "expected '3 min in', got {title:?}"
        );
    }

    #[test]
    fn format_pass_title_says_ended_after_los() {
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass ended 30 minutes ago.
        let pass = synthetic_pass(now, -42);
        assert_eq!(format_pass_title(&pass, now), "NOAA 19 — ended");
    }

    #[test]
    fn format_pass_title_at_exact_one_hour_uses_h_m_format() {
        // Boundary: a pass starting in exactly 60 min should read
        // "in 1h 00m", not "in 60 min". The strict `>` version of
        // this code surfaced the latter — fixed via `>=`.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 60);
        assert_eq!(format_pass_title(&pass, now), "NOAA 19 — in 1h 00m");
    }

    #[test]
    fn format_pass_title_at_exact_one_minute_says_one_min() {
        // Boundary: a pass starting in exactly 60 s should read
        // "in 1 min", not "starting now". `>=` fixes this.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        let pass = synthetic_pass(now, 1);
        assert_eq!(format_pass_title(&pass, now), "NOAA 19 — in 1 min");
    }

    #[test]
    fn format_pass_title_clamps_in_progress_min_to_at_least_one() {
        // First 60 seconds of an active pass: floor-div would say
        // "0 min in", which reads like the pass hasn't started.
        // Clamp to a minimum of 1 so the user always sees a real
        // count.
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 18, 0, 0).unwrap();
        // Pass started 30 seconds ago, ends in 12 minutes.
        let pass = Pass {
            satellite: "NOAA 19".to_string(),
            start: now - ChronoDuration::seconds(30),
            end: now + ChronoDuration::minutes(12),
            max_elevation_deg: 45.0,
            max_el_time: now + ChronoDuration::minutes(5),
            start_az_deg: 0.0,
            end_az_deg: 0.0,
        };
        assert_eq!(
            format_pass_title(&pass, now),
            "NOAA 19 — in progress (1 min in)"
        );
    }
}
