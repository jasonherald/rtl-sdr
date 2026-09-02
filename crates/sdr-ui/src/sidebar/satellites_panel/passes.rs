//! Pass-list helpers used by the Satellites wiring layer — quality
//! tags, catalog lookups (downlink / NORAD id / tune target),
//! title + subtitle formatters, and the pass enumerator. Split out
//! of `satellites_panel.rs` per the file-size pass (issue #819).

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sdr_sat::{
    GroundStation, KNOWN_SATELLITES, Pass, Satellite, TleCache, TleCacheError, TleFreshness,
    upcoming_passes,
};

use super::{
    DEFAULT_PASS_DISPLAY_COUNT, MIN_PASS_ELEVATION_DEG, PASS_LOOKAHEAD_HOURS, QUALITY_GOOD_DEG,
    QUALITY_MARGINAL_DEG, QUALITY_WINNER_DEG, SECS_PER_HOUR, SECS_PER_MINUTE,
};

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

/// Look up the NORAD catalog id for a pass's satellite. Returns
/// `None` for off-catalog passes — same condition as
/// [`downlink_hz_for_pass`]. Used by the notify scheduler (#510)
/// to key the watched-satellite set without leaking the
/// `KNOWN_SATELLITES` shape into [`super::satellites_notify`].
#[must_use]
pub fn norad_id_for_pass(pass: &Pass) -> Option<u32> {
    known_satellite_for_pass(pass).map(|s| s.norad_id)
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
    u32,
)> {
    known_satellite_for_pass(pass).map(|s| {
        (
            s.downlink_hz,
            s.demod_mode,
            s.bandwidth_hz,
            s.imaging_protocol,
            s.norad_id,
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
    let subtitle = match downlink_hz_for_pass(pass) {
        Some(hz) => format!("{quality}  ·  {}  ·  {geometry}", format_downlink_mhz(hz)),
        None => format!("{quality}  ·  {geometry}"),
    };
    // Stale elements still predict, but the user must see that the
    // AOS may be minutes off and a refresh is due (#718).
    if pass.tle_freshness() == TleFreshness::Fresh {
        subtitle
    } else {
        format!("{subtitle}  ·  TLE {} d old", pass.tle_age.num_days())
    }
}

/// Title-line countdown rendering. Examples:
///
/// * `"METEOR-M2 3 — in 1h 12m"`
/// * `"METEOR-M2 3 — in 4 min"`
/// * `"METEOR-M2 3 — starting now"`
/// * `"METEOR-M2 3 — in progress (3 min in)"`
/// * `"METEOR-M2 3 — ended"` (only seen briefly between recomputes)
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
    // `checked_add_signed`: `+` panics at the edge of chrono's range
    // (#720); a clamped window just yields no passes.
    let to = from
        .checked_add_signed(ChronoDuration::hours(PASS_LOOKAHEAD_HOURS))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
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
                    match upcoming_passes(station, &sat, from, to, MIN_PASS_ELEVATION_DEG) {
                        Ok(mut found) => passes.append(&mut found),
                        // Expired elements or a propagation failure:
                        // no list entry rather than a confident wrong
                        // one (#718, #719). The user sees it in the log
                        // and in the refresh status; a refresh fixes
                        // the common (stale) case.
                        Err(e) => log_satellite_skip(known.name, &e.to_string()),
                    }
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
