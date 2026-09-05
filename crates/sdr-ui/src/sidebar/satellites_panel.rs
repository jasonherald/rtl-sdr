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

use gtk4::glib;
use gtk4::prelude::ObjectExt;
use libadwaita as adw;

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
/// Config key for the auto-record quality threshold (selected
/// `AutoRecordQuality` tier). Per #511.
pub const KEY_AUTO_RECORD_QUALITY: &str = "sat_auto_record_quality";
/// Persisted "save false-colour composites alongside per-APID
/// PNGs" toggle. Pairs with [`KEY_AUTO_RECORD_APT`] but only
/// has effect for LRPT passes (where multiple APIDs are
/// decoded and the composite recipes have something to combine);
/// NOAA APT passes are single-channel and ignore this. Default:
/// `false` (opt-in — composites add a few MB per pass and not
/// every user wants the extras). Per #547.
pub const KEY_AUTO_RECORD_COMPOSITES: &str = "sat_auto_record_composites";

/// Pass-quality tiers that gate the auto-record-on-pass feature.
/// User-selectable via the `AdwComboRow` next to the auto-record
/// switch — only passes whose peak elevation meets or exceeds the
/// chosen tier's `min_elev_deg` actually trigger an auto-record.
///
/// Tiers map to the existing pass-quality tags from #507 / PR #508
/// (winner / good / marginal / barely): "winners only" is the
/// strictest (≥ 40°), "all passes" is the floor (≥
/// `MIN_PASS_ELEVATION_DEG` = 5°). Default is `WinnersAndGood` —
/// the band where APT decode actually produces a recognizable
/// image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoRecordQuality {
    /// ≥ 40° peak (winner-tier passes only). Strictest.
    WinnersOnly,
    /// ≥ 25° peak (winner + good). Default — matches the previous
    /// hardcoded constant.
    WinnersAndGood,
    /// ≥ 15° peak (+ marginal).
    MarginalOrBetter,
    /// ≥ [`MIN_PASS_ELEVATION_DEG`] (5°) — every pass above the floor.
    AllPasses,
}

impl AutoRecordQuality {
    /// Order in the `AdwComboRow` (matching declaration order). New
    /// variants MUST be appended — existing index → variant
    /// mappings are persisted in the user's config and an insertion
    /// in the middle would silently shift the user's selection.
    pub const ALL: [Self; 4] = [
        Self::WinnersOnly,
        Self::WinnersAndGood,
        Self::MarginalOrBetter,
        Self::AllPasses,
    ];

    /// Default tier — `WinnersAndGood`. Matches the previously-
    /// hardcoded `AUTO_RECORD_MIN_ELEV_DEG = 25.0` so existing
    /// users get the same behavior on upgrade.
    pub const DEFAULT: Self = Self::WinnersAndGood;

    /// Minimum peak elevation (degrees) for this tier. The
    /// recorder's `tick_idle` gate compares
    /// `pass.max_elevation_deg >= self.min_elev_deg()`. Drives off
    /// the same `QUALITY_*_DEG` constants the per-pass quality tag
    /// (`pass_quality_label`) reads, so the user-facing tier name
    /// in the row subtitle (`winner` / `good` / `marginal`) stays
    /// numerically aligned with this combo's threshold. Per CR
    /// round 1 on PR #574.
    #[must_use]
    pub fn min_elev_deg(self) -> f64 {
        match self {
            Self::WinnersOnly => QUALITY_WINNER_DEG,
            Self::WinnersAndGood => QUALITY_GOOD_DEG,
            Self::MarginalOrBetter => QUALITY_MARGINAL_DEG,
            Self::AllPasses => MIN_PASS_ELEVATION_DEG,
        }
    }

    /// Human-readable label for the `AdwComboRow`'s `string_list`.
    #[must_use]
    pub fn display_label(self) -> &'static str {
        match self {
            Self::WinnersOnly => "Winners only (≥ 40°)",
            Self::WinnersAndGood => "Winners and good (≥ 25°)",
            Self::MarginalOrBetter => "Marginal or better (≥ 15°)",
            Self::AllPasses => "All passes (≥ 5°)",
        }
    }

    /// Map a u32 combo index back to the variant. Returns
    /// [`Self::DEFAULT`] for out-of-range indices so a corrupted
    /// or future-version config can't crash the panel hydration.
    #[must_use]
    pub fn from_index(idx: u32) -> Self {
        usize::try_from(idx)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
            .unwrap_or(Self::DEFAULT)
    }

    /// Map this variant back to its u32 combo index. Inverse of
    /// `from_index`.
    #[must_use]
    pub fn to_index(self) -> u32 {
        Self::ALL
            .iter()
            .position(|v| *v == self)
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0)
    }
}
/// Config key for the persisted Doppler-tracking master switch
/// (Satellites panel). Default `true` so first-launch users get
/// auto-corrected passes out of the box. Per issue #521.
pub const KEY_DOPPLER_TRACKING_ENABLED: &str = "sat_doppler_tracking_enabled";
/// Config key for the watched-satellites NORAD-id list (per #510).
/// Stored as a JSON array of `u32`. Empty / missing → no
/// notifications fire.
pub const KEY_WATCHED_SATELLITES: &str = "sat_watched_norad_ids";
/// Config key for the user-configurable pre-pass notify lead time,
/// in whole minutes. Per #510.
pub const KEY_NOTIFY_LEAD_MIN: &str = "sat_notify_lead_min";

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

    // Notifications group ---------------------------------------------------
    /// Pre-pass desktop-alert lead time, in whole minutes
    /// (`NOTIFY_LEAD_MIN_LOWER..=NOTIFY_LEAD_MIN_UPPER`). Drives
    /// the `NotifyScheduler` ticker — the user clicks 🔔 next to a
    /// pass to subscribe; this row controls *how early* the alert
    /// fires. Per #510.
    pub notify_lead_row: adw::SpinRow,

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
    /// "Save false-colour composites" toggle — LRPT only. When
    /// on AND `auto_record_switch` is on, the recorder's
    /// `SaveLrptPass` handler writes one extra PNG per recipe in
    /// `lrpt_viewer::COMPOSITE_CATALOG` into the per-pass
    /// directory, named `composite-{slug}.png`. NOAA APT passes
    /// ignore this — they're single-channel by nature. Per #547.
    pub auto_record_composites_switch: adw::SwitchRow,
    /// Combo row selecting which pass-quality tier triggers
    /// auto-record. Sensitive only when `auto_record_switch` is
    /// on. Per #511.
    pub auto_record_quality_row: adw::ComboRow,
    /// Master switch for Doppler-correction tracking during
    /// satellite passes. Default ON. When OFF, the
    /// `DopplerTracker` stays dormant regardless of frequency
    /// match or pass schedule. Per issue #521.
    pub doppler_switch: adw::SwitchRow,

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
    /// Weak ref to [`SatellitesPanel::notify_lead_row`].
    pub notify_lead_row: glib::WeakRef<adw::SpinRow>,
    /// Weak ref to [`SatellitesPanel::auto_record_switch`].
    pub auto_record_switch: glib::WeakRef<adw::SwitchRow>,
    /// Weak ref to [`SatellitesPanel::auto_record_audio_switch`].
    pub auto_record_audio_switch: glib::WeakRef<adw::SwitchRow>,
    /// Weak ref to [`SatellitesPanel::auto_record_composites_switch`].
    pub auto_record_composites_switch: glib::WeakRef<adw::SwitchRow>,
    /// Weak ref to [`SatellitesPanel::auto_record_quality_row`].
    pub auto_record_quality_row: glib::WeakRef<adw::ComboRow>,
    /// Weak ref to [`SatellitesPanel::doppler_switch`].
    pub doppler_switch: glib::WeakRef<adw::SwitchRow>,
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
            notify_lead_row: self.notify_lead_row.downgrade(),
            auto_record_switch: self.auto_record_switch.downgrade(),
            auto_record_audio_switch: self.auto_record_audio_switch.downgrade(),
            auto_record_composites_switch: self.auto_record_composites_switch.downgrade(),
            auto_record_quality_row: self.auto_record_quality_row.downgrade(),
            doppler_switch: self.doppler_switch.downgrade(),
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
            notify_lead_row: self.notify_lead_row.upgrade()?,
            auto_record_switch: self.auto_record_switch.upgrade()?,
            auto_record_audio_switch: self.auto_record_audio_switch.upgrade()?,
            auto_record_composites_switch: self.auto_record_composites_switch.upgrade()?,
            auto_record_quality_row: self.auto_record_quality_row.upgrade()?,
            doppler_switch: self.doppler_switch.upgrade()?,
            passes_group: self.passes_group.upgrade()?,
            passes_status_row: self.passes_status_row.upgrade()?,
        })
    }
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

mod build;
mod passes;
mod persistence;

pub use build::build_satellites_panel;
pub use passes::{
    downlink_hz_for_pass, enumerate_upcoming_passes, format_downlink_mhz, format_pass_subtitle,
    format_pass_title, norad_id_for_pass, pass_quality_label, tune_target_for_pass,
};
pub use persistence::{
    format_last_refresh, load_auto_record_apt, load_auto_record_audio, load_auto_record_composites,
    load_auto_record_quality, load_doppler_tracking_enabled, load_notify_lead_min,
    load_station_alt_m, load_station_lat_deg, load_station_lon_deg, load_watched_satellites,
    save_auto_record_apt, save_auto_record_audio, save_auto_record_composites,
    save_auto_record_quality, save_doppler_tracking_enabled, save_f64, save_notify_lead_min,
    save_tle_last_refresh, save_watched_satellites,
};

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests;
