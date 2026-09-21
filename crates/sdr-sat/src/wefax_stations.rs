//! Built-in WEFAX (HF radiofax) station catalog + geo/schedule helpers.
//! Mirrors the `KNOWN_SATELLITES` pattern: static domain data + pure
//! functions. Frequencies/schedules transcribed from the published
//! NWS/RFAX schedules. Ground-station catalog (not user-editable in v1).

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};

/// A daily recurring UTC broadcast window, in minutes past 0000Z.
#[derive(Clone, Copy, Debug)]
pub struct DailyWindow {
    pub start_min_utc: u16,
    pub end_min_utc: u16,
}

/// A WEFAX transmitting station.
#[derive(Clone, Copy, Debug)]
pub struct WefaxStation {
    pub name: &'static str,
    /// Published fax carrier frequencies (real RF, Hz).
    pub channels_hz: &'static [u64],
    pub lat_deg: f64,
    pub lon_deg: f64,
    /// Recurring daily windows when this station broadcasts charts.
    pub schedule: &'static [DailyWindow],
}

/// Full daily coverage placeholder for near-continuous broadcasters.
const CONTINUOUS: &[DailyWindow] = &[DailyWindow {
    start_min_utc: 0,
    end_min_utc: 1_440,
}];

/// The built-in catalog. Geo-filtering hides far ones per user location.
pub static KNOWN_WEFAX_STATIONS: &[WefaxStation] = &[
    WefaxStation {
        name: "NMG New Orleans",
        channels_hz: &[4_317_900, 8_503_900, 12_789_900],
        lat_deg: 29.88,
        lon_deg: -89.94,
        schedule: CONTINUOUS,
    },
    WefaxStation {
        name: "NMF Boston",
        channels_hz: &[4_235_000, 6_340_500, 9_110_000, 12_750_000],
        lat_deg: 41.70,
        lon_deg: -70.52,
        schedule: CONTINUOUS,
    },
    WefaxStation {
        name: "NMC Point Reyes",
        channels_hz: &[4_346_000, 8_682_000, 12_786_000, 17_151_200],
        lat_deg: 38.10,
        lon_deg: -122.87,
        schedule: CONTINUOUS,
    },
    WefaxStation {
        name: "NOJ Kodiak",
        channels_hz: &[4_298_000, 8_459_000, 12_412_500],
        lat_deg: 57.65,
        lon_deg: -152.63,
        schedule: CONTINUOUS,
    },
];

/// Great-circle distance between two lat/lon points (km), haversine.
#[must_use]
pub fn great_circle_km(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    const R_KM: f64 = 6_371.0;
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dlat = (b_lat - a_lat).to_radians();
    let dlon = (b_lon - a_lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R_KM * h.sqrt().asin()
}

/// Catalog ranked nearest-first from the user's location (with km).
#[must_use]
pub fn stations_by_distance(user_lat: f64, user_lon: f64) -> Vec<(&'static WefaxStation, f64)> {
    let mut v: Vec<_> = KNOWN_WEFAX_STATIONS
        .iter()
        .map(|s| (s, great_circle_km(user_lat, user_lon, s.lat_deg, s.lon_deg)))
        .collect();
    v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// Every catalog station+channel flattened into one row per channel,
/// ordered by station distance (nearest first via [`stations_by_distance`])
/// then by channel in catalog order within each station. Feeds the
/// "Fax stations" picker (issue #919): one activatable row per
/// station+channel, so a user can tune-and-camp on a specific frequency
/// rather than auto-scanning.
#[must_use]
pub fn channels_by_distance(user_lat: f64, user_lon: f64) -> Vec<(&'static str, u64, f64)> {
    stations_by_distance(user_lat, user_lon)
        .into_iter()
        .flat_map(|(station, km)| {
            station
                .channels_hz
                .iter()
                .map(move |&freq_hz| (station.name, freq_hz, km))
        })
        .collect()
}

/// Rough HF-propagation-vs-time-of-day guide for a fax channel's
/// frequency. Lower HF bands (< 6 MHz) propagate best at night; the
/// highest bands (>= 10 MHz) favor daylight; the band in between is
/// "transitional" (dawn/dusk, or either depending on solar conditions).
/// A coarse heuristic only — real propagation depends on season, solar
/// activity, and path — but enough to hint "try this one after dark."
#[must_use]
pub fn band_hint(freq_hz: u64) -> &'static str {
    match freq_hz {
        0..=5_999_999 => "night",
        6_000_000..=9_999_999 => "transitional",
        _ => "day",
    }
}

/// Format a channel frequency for display, e.g. `4235 kHz` or
/// `6340.5 kHz` (trailing `.0` trimmed). All catalog frequencies are
/// round to the nearest 100 Hz, so one decimal digit is always enough.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "freq_hz is a WEFAX HF channel (<=30 MHz), well below f64's 2^53 mantissa ceiling"
)]
pub fn format_channel_khz(freq_hz: u64) -> String {
    let khz = freq_hz as f64 / 1_000.0;
    let formatted = format!("{khz:.1}");
    let trimmed = formatted.strip_suffix(".0").unwrap_or(&formatted);
    format!("{trimmed} kHz")
}

/// Minutes past 0000Z for a UTC instant.
fn minute_of_day(now: DateTime<Utc>) -> u16 {
    u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(u16::MAX)
}

/// Is the station within a broadcast window at `now`?
#[must_use]
pub fn is_active(station: &WefaxStation, now: DateTime<Utc>) -> bool {
    let m = minute_of_day(now);
    station
        .schedule
        .iter()
        .any(|w| m >= w.start_min_utc && m < w.end_min_utc)
}

/// The next window START at or after `now` (wrapping to tomorrow). The
/// comparison is against the full `now` instant, not just its minute: a
/// window whose start is at or after `now` to the second is returned; one
/// whose minute has already begun this second (e.g. a 0600 start when
/// `now` is 06:00:01) is treated as past and skipped.
#[must_use]
pub fn next_window_start(station: &WefaxStation, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let midnight = Utc
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single()?;
    let today = station
        .schedule
        .iter()
        .filter_map(|w| midnight.checked_add_signed(Duration::minutes(i64::from(w.start_min_utc))))
        .filter(|&start| start >= now)
        .min();
    if let Some(start) = today {
        return Some(start);
    }
    let first = station.schedule.iter().map(|w| w.start_min_utc).min()?;
    // `checked_*` (not `+`): `DateTime + Duration` panics on range overflow,
    // and this is a public fn over arbitrary `now`/`DailyWindow` inputs — a
    // near-range-limit date must yield `None`, not panic. A candidate whose
    // start overflows is likewise dropped by the `filter_map` above.
    midnight.checked_add_signed(Duration::days(1) + Duration::minutes(i64::from(first)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn great_circle_new_orleans_to_boston_is_about_2200km() {
        // NMG ~ (30.0, -90.0), Boston ~ (42.4, -71.0). Verified by hand via
        // both haversine and the spherical law of cosines: true great-circle
        // distance between these rounded coordinates is ~2,184 km (the
        // brief's original ~1,900 km / 1,800..2,100 estimate was incorrect).
        let d = great_circle_km(30.0, -90.0, 42.4, -71.0);
        assert!((2_100.0..2_300.0).contains(&d), "got {d} km");
    }

    #[test]
    fn stations_ranked_nearest_first_from_new_orleans() {
        // From NMG's location, NMG is nearest.
        let ranked = stations_by_distance(30.0, -90.0);
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].0.name, "NMG New Orleans");
        // strictly non-decreasing distance
        assert!(ranked.windows(2).all(|w| w[0].1 <= w[1].1));
    }

    #[test]
    fn schedule_active_window_is_detected() {
        // A station with a 0000-0600Z window is active at 0300Z, not at 0700Z.
        let s = WefaxStation {
            name: "TEST",
            channels_hz: &[4_000_000],
            lat_deg: 0.0,
            lon_deg: 0.0,
            schedule: &[DailyWindow {
                start_min_utc: 0,
                end_min_utc: 360,
            }],
        };
        assert!(is_active(
            &s,
            Utc.with_ymd_and_hms(2026, 9, 20, 3, 0, 0).unwrap()
        ));
        assert!(!is_active(
            &s,
            Utc.with_ymd_and_hms(2026, 9, 20, 7, 0, 0).unwrap()
        ));
    }

    #[test]
    fn next_window_start_includes_a_window_starting_exactly_now() {
        // A window that starts at exactly the current minute must be
        // returned as the next start (boundary: `s == m`), not skipped to
        // tomorrow's first window.
        let s = WefaxStation {
            name: "TEST",
            channels_hz: &[4_000_000],
            lat_deg: 0.0,
            lon_deg: 0.0,
            schedule: &[DailyWindow {
                start_min_utc: 360, // 0600Z
                end_min_utc: 720,
            }],
        };
        // now == 0600Z, exactly the window start.
        let now = Utc.with_ymd_and_hms(2026, 9, 20, 6, 0, 0).unwrap();
        let next = next_window_start(&s, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 9, 20, 6, 0, 0).unwrap());
    }

    #[test]
    fn next_window_start_skips_a_start_already_past_this_minute() {
        // One second past a 0600Z start: the minute has begun, so that
        // window is past and the next start is tomorrow's 0600Z — proves
        // the comparison is second-accurate, not minute-granular.
        let s = WefaxStation {
            name: "TEST",
            channels_hz: &[4_000_000],
            lat_deg: 0.0,
            lon_deg: 0.0,
            schedule: &[DailyWindow {
                start_min_utc: 360, // 0600Z
                end_min_utc: 720,
            }],
        };
        let now = Utc.with_ymd_and_hms(2026, 9, 20, 6, 0, 1).unwrap();
        let next = next_window_start(&s, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 9, 21, 6, 0, 0).unwrap());
    }

    #[test]
    fn next_window_start_returns_none_at_range_limit_instead_of_panicking() {
        // Public fn over arbitrary inputs: at chrono's last representable
        // date the next-day wrap overflows. It must return None, not panic.
        use chrono::NaiveDate;
        let s = WefaxStation {
            name: "TEST",
            channels_hz: &[4_000_000],
            lat_deg: 0.0,
            lon_deg: 0.0,
            schedule: &[DailyWindow {
                start_min_utc: 0, // 0000Z — already past at noon, forcing the wrap
                end_min_utc: 1,
            }],
        };
        let now = NaiveDate::MAX.and_hms_opt(12, 0, 0).unwrap().and_utc();
        assert!(
            next_window_start(&s, now).is_none(),
            "a range-limit wrap must return None rather than panic"
        );
    }

    #[test]
    fn next_window_start_wraps_to_next_day() {
        let s = WefaxStation {
            name: "TEST",
            channels_hz: &[4_000_000],
            lat_deg: 0.0,
            lon_deg: 0.0,
            schedule: &[DailyWindow {
                start_min_utc: 0,
                end_min_utc: 360,
            }],
        };
        // At 0700Z, next start is 0000Z tomorrow.
        let now = Utc.with_ymd_and_hms(2026, 9, 20, 7, 0, 0).unwrap();
        let next = next_window_start(&s, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 9, 21, 0, 0, 0).unwrap());
    }

    #[test]
    fn catalog_channels_are_plausible_hf() {
        for s in KNOWN_WEFAX_STATIONS {
            assert!(!s.channels_hz.is_empty(), "{} has no channels", s.name);
            for &f in s.channels_hz {
                assert!((2_000_000..25_000_000).contains(&f), "{}: {f} Hz", s.name);
            }
        }
    }

    #[test]
    fn band_hint_boundaries() {
        assert_eq!(band_hint(5_900_000), "night");
        assert_eq!(band_hint(6_000_000), "transitional");
        assert_eq!(band_hint(9_900_000), "transitional");
        assert_eq!(band_hint(10_000_000), "day");
    }

    #[test]
    fn channels_by_distance_is_nonempty_and_station_ordered() {
        let rows = channels_by_distance(30.0, -90.0);
        let total_channels: usize = KNOWN_WEFAX_STATIONS
            .iter()
            .map(|s| s.channels_hz.len())
            .sum();
        assert_eq!(rows.len(), total_channels);
        assert!(!rows.is_empty());
        // First row must belong to the nearest station (NMG from New
        // Orleans), matching `stations_by_distance`'s own ranking test.
        assert_eq!(rows[0].0, "NMG New Orleans");

        // Station distance is non-decreasing as rows progress (channels
        // within a station share its distance, so this holds across the
        // whole flattened list, not just at station boundaries).
        assert!(rows.windows(2).all(|w| w[0].2 <= w[1].2));
    }

    #[test]
    fn format_channel_khz_trims_trailing_zero() {
        assert_eq!(format_channel_khz(4_235_000), "4235 kHz");
        assert_eq!(format_channel_khz(6_340_500), "6340.5 kHz");
    }
}
