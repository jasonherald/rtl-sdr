use super::*;
use chrono::{Duration as ChronoDuration, TimeZone};

/// Default `min_elev_deg` for `tick(...)` in this test module —
/// matches `AutoRecordQuality::DEFAULT.min_elev_deg()` (= 25°,
/// the "winners and good" tier). Centralized so a future bump to
/// the default tier doesn't sprinkle 56 literal updates across
/// the file. Per CR round 1 on PR #574.
const DEFAULT_MIN_ELEV_DEG: f64 = 25.0;

/// Build a synthetic NOAA 19 pass starting `aos_offset_secs`
/// from `now`, lasting `duration_secs`, with the given peak
/// elevation. Mirrors the synthetic-pass fixture pattern used
/// by the `satellites_panel` tests.
/// Slug `pass_satellite_slug` derives from the fixture's
/// "METEOR-M2 3" name — the prefix the path tests check against.
const FIXTURE_SLUG: &str = "METEOR-M2-3";

fn synthetic_meteor_m2_3(
    now: DateTime<Utc>,
    aos_offset_secs: i64,
    duration_secs: i64,
    peak_elev_deg: f64,
) -> Pass {
    // Synthesises a METEOR-M2 3 pass for state-machine tests.
    // Renamed from `synthetic_noaa19` per CR round 1: the helper
    // historically built NOAA-19 passes, but NOAA-19 was
    // decommissioned 2025-08-13 and is no longer in
    // `KNOWN_SATELLITES`. METEOR-M2 3 is an active LRPT entry,
    // so the helper now emits its slug. The recorder tests don't
    // care which imaging protocol the catalog dispatches (Apt
    // vs Lrpt) — they exercise state-machine transitions,
    // audio-toggle paths, and back-to-back pass arming, all of
    // which behave identically for any catalog satellite.
    let start = now + ChronoDuration::seconds(aos_offset_secs);
    Pass {
        satellite: "METEOR-M2 3".to_string(),
        start,
        end: start + ChronoDuration::seconds(duration_secs),
        max_elevation_deg: peak_elev_deg,
        max_el_time: start + ChronoDuration::seconds(duration_secs / 2),
        start_az_deg: 245.0,
        end_az_deg: 105.0,
        tle_age: chrono::Duration::zero(),
    }
}

fn default_tune() -> SavedTune {
    SavedTune {
        freq_hz: 100_000_000.0,
        vfo_offset_hz: 0.0,
        mode: DemodMode::Wfm,
        bandwidth_hz: 200_000,
        was_running: true,
        scanner_running: false,
        squelch_enabled: false,
        auto_squelch_enabled: false,
        squelch_db: -50.0,
        ctcss_mode: CtcssMode::Off,
        fm_if_nr_enabled: false,
        deemphasis_idx: 0,
        notch_enabled: false,
        doppler_enabled: false,
    }
}

/// One recorder tick at `at` seeing just `pass`, with the default tune
/// and minimum elevation; `record` / `audio` are the two user toggles.
fn tick(
    r: &mut AutoRecorder,
    at: DateTime<Utc>,
    pass: &Pass,
    record: bool,
    audio: bool,
) -> Vec<Action> {
    r.tick(
        at,
        std::slice::from_ref(pass),
        record,
        audio,
        DEFAULT_MIN_ELEV_DEG,
        default_tune(),
    )
}

/// Every field of the pre-AOS tune pinned to a non-default value (each
/// distinct from `default_tune()`), so the restore at LOS can be
/// checked field by field. Per CR rounds 1 + 3 on PR #557.
const SAVED_FREQ_HZ: f64 = 89_700_000.0;
const SAVED_VFO_OFFSET_HZ: f64 = 25_000.0;
const SAVED_BANDWIDTH_HZ: u32 = 180_000;
const SAVED_SQUELCH_DB: f32 = -42.5;
const SAVED_CTCSS_TONE_HZ: f32 = 100.0;

// ─── Per-pass output paths (epic #469 task 7.4) ──────────

/// Build a synthetic METEOR-M 2 pass. Used by the
/// supported-protocols-gate tests below — the catalog flags
/// METEOR-M 2 with `imaging_protocol: None` today, but
/// these tests need to simulate "what happens if a future
/// edit flips it to `Some(Lrpt)` before the wiring layer
/// supports it." The recorder gate is what saves us in
/// that scenario.
fn synthetic_meteor_m2(
    now: DateTime<Utc>,
    aos_offset_secs: i64,
    duration_secs: i64,
    peak_elev_deg: f64,
) -> Pass {
    let start = now + ChronoDuration::seconds(aos_offset_secs);
    Pass {
        satellite: "METEOR-M2 3".to_string(),
        start,
        end: start + ChronoDuration::seconds(duration_secs),
        max_elevation_deg: peak_elev_deg,
        max_el_time: start + ChronoDuration::seconds(duration_secs / 2),
        start_az_deg: 245.0,
        end_az_deg: 105.0,
        tle_age: chrono::Duration::zero(),
    }
}

/// LRPT recorder configured with both Apt + Lrpt support so
/// the Meteor pass actually arms. The default constructor
/// only supports `[Apt]` today (Task 7.5 flips it to
/// `[Apt, Lrpt]`); these tests need the wider set to
/// exercise the LRPT path.
fn lrpt_recorder() -> AutoRecorder {
    AutoRecorder::with_supported_protocols(&[
        sdr_sat::ImagingProtocol::Apt,
        sdr_sat::ImagingProtocol::Lrpt,
    ])
}

mod arming;
mod lifecycle;
mod lrpt;
mod paths_audio;
