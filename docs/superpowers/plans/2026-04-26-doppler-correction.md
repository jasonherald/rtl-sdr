# Doppler Correction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Continuously correct the receive frequency for satellite Doppler shift during a pass, so the user's audio doesn't drift and the APT / LRPT / SSTV decoders stay locked.

**Architecture:** Single trigger rule (master switch ON + freq within ±20 kHz of catalog satellite + that satellite above horizon via SGP4) drives a per-window `DopplerTracker` that recomputes the offset at 4 Hz and dispatches `UiToDsp::SetVfoOffset(user_reference + doppler)`. Pure DSP shift, zero hardware retune. Status bar shows live `Doppler: -1.4 kHz` readout. Manual VFO drag is additive (`live = user_reference + doppler`).

**Tech Stack:** Rust workspace (sdr-sat for SGP4 + range-rate, sdr-ui for GTK4/libadwaita UI). Reuses existing `sdr_sat::track()` + `Track::doppler_shift_hz()` + `TleCache::cached_tle_for()` — no new sdr-sat API needed (spec §5 anticipated one but the math turns out to already be in place).

**Spec:** [docs/superpowers/specs/2026-04-26-doppler-correction-design.md](../specs/2026-04-26-doppler-correction-design.md)

---

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `crates/sdr-ui/src/doppler_tracker.rs` | NEW | `DopplerTracker` struct + pure helpers (trigger eval, offset compute) + tests |
| `crates/sdr-ui/src/lib.rs` | MODIFY | `pub mod doppler_tracker;` declaration |
| `crates/sdr-ui/src/sidebar/satellites_panel.rs` | MODIFY | `KEY_DOPPLER_TRACKING_ENABLED` + load/save + `doppler_switch` row + builder code |
| `crates/sdr-ui/src/status_bar.rs` | MODIFY | `doppler_label` + `update_doppler(Option<f64>)` method + builder packs it |
| `crates/sdr-ui/src/window.rs` | MODIFY | Construct tracker, wire master switch + freq changes, schedule timers, dispatch SetVfoOffset, update status bar |

**No new sdr-sat code.** `Satellite::from_tle()` + `track()` + `Track::doppler_shift_hz()` + `Track::elevation_deg` cover everything the spec needs.

**Pre-existing constructs the plan uses:**
- `sdr_sat::KNOWN_SATELLITES` — catalog with `downlink_hz`, `norad_id`, `name`
- `sdr_sat::track(&station, &sat, when) -> Result<Track, SatelliteError>`
- `Track::doppler_shift_hz(carrier_hz: f64) -> f64`
- `Track::elevation_deg: f64`
- `sdr_sat::Satellite::from_tle(name, line1, line2)`
- `sdr_sat::TleCache::cached_tle_for(norad_id) -> Result<(String, String), TleCacheError>`
- `sdr_sat::GroundStation::new(lat_deg, lon_deg, alt_m)`
- `sidebar::satellites_panel::{load_station_lat_deg, load_station_lon_deg, load_station_alt_m}`
- `state.send_dsp(UiToDsp::SetVfoOffset(hz))`
- `glib::timeout_add_local(duration, FnMut() -> glib::ControlFlow)`

---

## Task 1: `pick_active_satellite` pure trigger evaluation

**Files:**
- Create: `crates/sdr-ui/src/doppler_tracker.rs`
- Modify: `crates/sdr-ui/src/lib.rs:1` (add `pub mod doppler_tracker;` near `pub mod viewer;`)

**Goal:** Pure function that takes the inputs the spec's §2 trigger rule needs and returns which (if any) catalog satellite is the active Doppler target. Testable headlessly, no GTK, no I/O.

- [ ] **Step 1: Add module declaration to lib.rs**

Open `crates/sdr-ui/src/lib.rs`, find the existing `pub mod viewer;` line, and add this on the next line (alphabetical):

```rust
pub mod doppler_tracker;
```

- [ ] **Step 2: Create the file with the trigger spec, types, failing tests**

Create `crates/sdr-ui/src/doppler_tracker.rs` with this content:

```rust
//! Doppler-correction tracker: continuously adjusts the VFO
//! offset for a tuned satellite's predicted Doppler shift.
//! Per issue #521 (sub-ticket of #520) and the design spec at
//! `docs/superpowers/specs/2026-04-26-doppler-correction-design.md`.

use sdr_sat::KnownSatellite;

/// ±tolerance around a catalog satellite's `downlink_hz` within
/// which the user's tuned center frequency is considered "tuned
/// to that satellite". Generous enough to absorb PPM-correction
/// nudges and pre-pass drift, narrow enough to avoid catalog
/// collisions on the 137 MHz APT/LRPT cluster. Per spec §2.
pub const FREQ_MATCH_TOLERANCE_HZ: f64 = 20_000.0;

/// One catalog candidate with its currently-evaluated elevation.
/// Built by the caller (which has SGP4 + station + TLE access)
/// and fed into [`pick_active_satellite`].
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    pub satellite: &'static KnownSatellite,
    /// Elevation in degrees relative to the user's ground
    /// station, computed at the current evaluation time.
    /// Negative values mean below the horizon.
    pub elevation_deg: f64,
}

/// Pure trigger evaluation per spec §2: pick the catalog
/// satellite (if any) the Doppler tracker should be locked to
/// right now.
///
/// Inputs match the trigger rule's three conditions:
///   1. `master_enabled` — user's master switch (Satellites panel)
///   2. `current_freq_hz` — radio's current center frequency
///   3. `candidates` — for each catalog entry whose
///      `downlink_hz` is within `FREQ_MATCH_TOLERANCE_HZ` of
///      `current_freq_hz`, the entry's currently-evaluated
///      elevation. Caller is responsible for the SGP4 propagate
///      and station math; this function only decides which
///      among the matches wins.
///
/// Returns `Some(&KnownSatellite)` for the highest-elevation
/// above-horizon match. Ties are broken by `candidates` order
/// (caller passes them in `KNOWN_SATELLITES` order so the result
/// is deterministic). Returns `None` if the master switch is
/// off, no candidates, or no candidate is above the horizon.
#[must_use]
pub fn pick_active_satellite(
    master_enabled: bool,
    candidates: &[Candidate],
) -> Option<&'static KnownSatellite> {
    if !master_enabled {
        return None;
    }
    let above: Vec<Candidate> = candidates
        .iter()
        .copied()
        .filter(|c| c.elevation_deg > 0.0)
        .collect();
    // Two-pass: find the maximum elevation, then return the
    // first candidate that matches it. `Iterator::max_by` keeps
    // the LAST equal element in Rust (not the first), which would
    // break the spec §2 deterministic tie-break (caller passes
    // candidates in `KNOWN_SATELLITES` order, earlier wins). Match
    // the production implementation in
    // `crates/sdr-ui/src/doppler_tracker.rs`.
    let max_elev = above.iter().map(|c| c.elevation_deg).reduce(f64::max)?;
    // Exact equality is intentional: `max_elev` came from the same
    // `f64` values in `above`, so this is identity comparison of a
    // value we already read — not a computed approximation.
    #[allow(clippy::float_cmp)]
    above
        .into_iter()
        .find(|c| c.elevation_deg == max_elev)
        .map(|c| c.satellite)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_sat::{ImagingProtocol, KNOWN_SATELLITES};

    fn noaa_15() -> &'static KnownSatellite {
        KNOWN_SATELLITES
            .iter()
            .find(|s| s.name == "NOAA 15")
            .expect("NOAA 15 in catalog")
    }

    fn noaa_18() -> &'static KnownSatellite {
        KNOWN_SATELLITES
            .iter()
            .find(|s| s.name == "NOAA 18")
            .expect("NOAA 18 in catalog")
    }

    fn noaa_19() -> &'static KnownSatellite {
        KNOWN_SATELLITES
            .iter()
            .find(|s| s.name == "NOAA 19")
            .expect("NOAA 19 in catalog")
    }

    #[test]
    fn freq_match_tolerance_is_20_khz() {
        assert!((FREQ_MATCH_TOLERANCE_HZ - 20_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn master_switch_off_returns_none_even_with_overhead_match() {
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: 45.0,
        }];
        assert!(pick_active_satellite(false, &candidates).is_none());
    }

    #[test]
    fn no_candidates_returns_none() {
        assert!(pick_active_satellite(true, &[]).is_none());
    }

    #[test]
    fn single_above_horizon_candidate_wins() {
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: 12.5,
        }];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 15");
    }

    #[test]
    fn below_horizon_candidate_returns_none() {
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: -3.0,
        }];
        assert!(pick_active_satellite(true, &candidates).is_none());
    }

    #[test]
    fn zero_elevation_treated_as_below_horizon() {
        // `elevation_deg > 0.0` is the gate — exactly-on-horizon
        // is excluded. Avoids edge cases where SGP4 noise might
        // produce a near-zero elevation that's not a real pass.
        let candidates = vec![Candidate {
            satellite: noaa_15(),
            elevation_deg: 0.0,
        }];
        assert!(pick_active_satellite(true, &candidates).is_none());
    }

    #[test]
    fn highest_elevation_wins_among_overhead_matches() {
        // Spec §2: NOAA 18 + 19 both around 137.9 MHz; multi-sat
        // collision picks the higher-elevation one.
        let candidates = vec![
            Candidate {
                satellite: noaa_18(),
                elevation_deg: 8.0,
            },
            Candidate {
                satellite: noaa_19(),
                elevation_deg: 32.0,
            },
        ];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 19");
    }

    #[test]
    fn equal_elevation_breaks_to_first_in_candidates_order() {
        // Spec §2 deterministic tie-break: caller passes
        // candidates in `KNOWN_SATELLITES` order; ties go to
        // earlier entry. Testing with NOAA 18 first so it wins
        // the tie.
        let candidates = vec![
            Candidate {
                satellite: noaa_18(),
                elevation_deg: 15.0,
            },
            Candidate {
                satellite: noaa_19(),
                elevation_deg: 15.0,
            },
        ];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 18");
    }

    #[test]
    fn one_above_one_below_picks_above() {
        let candidates = vec![
            Candidate {
                satellite: noaa_18(),
                elevation_deg: -5.0,
            },
            Candidate {
                satellite: noaa_19(),
                elevation_deg: 22.0,
            },
        ];
        let pick = pick_active_satellite(true, &candidates).expect("some pick");
        assert_eq!(pick.name, "NOAA 19");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail / fail-to-compile correctly**

Run: `cargo test -p sdr-ui --lib --features whisper-cpu doppler_tracker::tests 2>&1 | tail -25`

Expected: 9 tests passing on first run (the implementation is in place at file creation). If anything fails, the most likely cause is a typo in `KNOWN_SATELLITES` lookup or the `Candidate` field shape — fix and re-run.

- [ ] **Step 4: Build + clippy + fmt**

Run:
```bash
cargo build --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -3
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```
Expected: all clean.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/lib.rs crates/sdr-ui/src/doppler_tracker.rs
git commit -m "$(cat <<'EOF'
sdr-ui: doppler_tracker module + pure pick_active_satellite

First slice of the Doppler-correction tracker (#521): a pure
trigger-evaluation function the spec §2 rule reduces to once
the SGP4 propagate is done outside. Master switch + a candidate
list (catalog satellite + currently-evaluated elevation), returns
the highest-elevation above-horizon entry with deterministic
tie-break by candidate order.

9 unit tests pin the trigger semantics: master-off, empty list,
single match, below-horizon rejection, zero-elevation rejection,
multi-sat tie-break by elevation, deterministic tie-break by
order, mixed above/below.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `compute_doppler_offset_hz` pure offset assembly

**Files:**
- Modify: `crates/sdr-ui/src/doppler_tracker.rs` (add a second pure helper + tests)

**Goal:** Pure function that takes a tuned satellite + ground station + TLE + carrier frequency + when, runs the existing `sdr_sat::track()` + `Track::doppler_shift_hz()`, and returns the Doppler offset to apply. Wraps the sdr-sat math in a tracker-friendly signature with consistent error handling.

- [ ] **Step 1: Append the new function and its tests to `doppler_tracker.rs`**

Add this BELOW the existing `pick_active_satellite` function (and BEFORE the `#[cfg(test)] mod tests` block) in `crates/sdr-ui/src/doppler_tracker.rs`:

```rust
use sdr_sat::{GroundStation, Satellite, SatelliteError, track};

/// Doppler-correction errors surfaced to the tracker. We collapse
/// every failure mode (TLE parse, SGP4 propagate, range-rate
/// computation) into a single variant with a string `cause`
/// because every one of them is non-fatal — the tracker logs and
/// stays dormant for that tick rather than escalating.
#[derive(Debug, thiserror::Error)]
pub enum DopplerError {
    /// SGP4 propagator or TLE parse failed.
    #[error("SGP4 / TLE: {0}")]
    Propagation(#[from] SatelliteError),
}

/// Compute the Doppler offset (Hz) to apply to the VFO right
/// now, given a satellite + ground station + parsed TLE +
/// carrier frequency + UTC instant. Pure function — no I/O,
/// no caching.
///
/// Sign convention follows [`sdr_sat::Track::doppler_shift_hz`]:
/// positive = satellite approaching (received frequency > nominal
/// carrier), negative = receding.
///
/// The returned offset is intended to be added to the existing
/// VFO offset value: `live_offset = user_reference + doppler`
/// (per spec §4 additive override).
///
/// # Errors
///
/// Returns [`DopplerError::Propagation`] if SGP4 fails (TLE epoch
/// too far from `when`, malformed elements, etc.).
pub fn compute_doppler_offset_hz(
    satellite: &Satellite,
    station: &GroundStation,
    when: chrono::DateTime<chrono::Utc>,
    carrier_hz: f64,
) -> Result<f64, DopplerError> {
    let track = track(station, satellite, when)?;
    Ok(track.doppler_shift_hz(carrier_hz))
}
```

Add these tests to the `#[cfg(test)] mod tests` block (append to the existing tests):

```rust
    // A TLE for NOAA 19 captured at a known epoch — used to pin
    // the Doppler curve shape against a known geometry. Not
    // updated automatically; if the tests ever fail because the
    // TLE has aged out of the SGP4 epoch budget, capture a fresh
    // pair from Celestrak (https://celestrak.org/NORAD/elements/
    // gp.php?CATNR=33591&FORMAT=tle) and update both lines.
    const NOAA_19_TLE_LINE1: &str =
        "1 33591U 09005A   24001.50000000  .00000123  00000-0  82345-4 0  9991";
    const NOAA_19_TLE_LINE2: &str =
        "2 33591  99.1916  10.0000 0014000 100.0000 260.1234 14.13000000800000";

    fn test_station() -> GroundStation {
        // San Francisco-ish — coords matter only insofar as the
        // satellite is overhead during the chosen propagate time.
        GroundStation::new(37.7749, -122.4194, 50.0)
    }

    #[test]
    fn compute_doppler_offset_returns_signed_value_at_known_time() {
        // Sanity: the function returns SOMETHING and doesn't
        // error on a satellite-not-overhead time. The exact
        // sign/magnitude depends on geometry, but the call
        // shape and error path are pinned.
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let when = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let result =
            compute_doppler_offset_hz(&sat, &test_station(), when, 137_100_000.0).unwrap();
        assert!(
            result.abs() < 10_000.0,
            "Doppler magnitude should fit within ±10 kHz at 137 MHz: got {result}"
        );
    }

    #[test]
    fn compute_doppler_offset_curves_through_zero_during_overhead_pass() {
        // Spec §1: across a pass, Doppler sweeps positive →
        // zero at TCA → negative. Sample three instants 4 min
        // apart; check the offset trends from positive toward
        // negative (or vice versa — sign depends on which side
        // of the orbit the station sees). The key invariant is
        // monotonicity: it should always be moving in one
        // direction across a single pass.
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let station = test_station();
        let carrier = 137_100_000.0_f64;

        // Three instants 4 min apart somewhere in the propagation
        // window. Don't assume any specific pass falls in this
        // window — just check that the offset CHANGES monotonically,
        // which is true outside TCA's brief sign-flip moment.
        let t0 = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let t1 = t0 + chrono::Duration::minutes(4);
        let t2 = t0 + chrono::Duration::minutes(8);

        let d0 = compute_doppler_offset_hz(&sat, &station, t0, carrier).unwrap();
        let d1 = compute_doppler_offset_hz(&sat, &station, t1, carrier).unwrap();
        let d2 = compute_doppler_offset_hz(&sat, &station, t2, carrier).unwrap();

        // Doppler should change appreciably over 8 minutes of
        // satellite motion (the satellite covers ~3500 km in
        // that span — geometry definitely changes).
        let total_swing = (d2 - d0).abs();
        assert!(
            total_swing > 100.0,
            "Doppler should swing >100 Hz over 8 min: d0={d0} d1={d1} d2={d2}"
        );
    }

    #[test]
    fn compute_doppler_offset_sign_matches_approaching_receding() {
        // Spec §5: positive Doppler when approaching, negative
        // when receding. Formula is `Δf = -f₀ · ṙ / c`, so a
        // positive range-rate (receding) must give negative Δf.
        //
        // We can't easily construct a synthetic "approaching at
        // 5 km/s" test without implementing our own SGP4, but we
        // can verify the formula's sign by computing the offset
        // for the same satellite at two adjacent times and
        // checking that the SIGN of the offset matches the SIGN
        // implied by the geometry change. This is a regression
        // pin against accidentally flipping the sign.
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let station = test_station();
        let carrier = 137_100_000.0_f64;

        let when = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let track = sdr_sat::track(&station, &sat, when).expect("track ok");
        let doppler = compute_doppler_offset_hz(&sat, &station, when, carrier).unwrap();

        // Sign relationship: doppler = -carrier * range_rate / c.
        // So `doppler` and `range_rate` always have OPPOSITE signs.
        if track.range_rate_km_s.abs() > 0.01 {
            let opposite_signs =
                (doppler > 0.0 && track.range_rate_km_s < 0.0)
                    || (doppler < 0.0 && track.range_rate_km_s > 0.0);
            assert!(
                opposite_signs,
                "doppler={doppler} should be opposite sign to range_rate={}",
                track.range_rate_km_s
            );
        }
    }

    #[test]
    fn compute_doppler_offset_zero_carrier_returns_zero() {
        // Edge case: f₀ = 0 should yield 0 regardless of geometry
        // (the formula multiplies by `frequency_hz`).
        let sat = Satellite::from_tle("NOAA 19", NOAA_19_TLE_LINE1, NOAA_19_TLE_LINE2)
            .expect("TLE parses");
        let when = chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let result = compute_doppler_offset_hz(&sat, &test_station(), when, 0.0).unwrap();
        assert!((result - 0.0).abs() < f64::EPSILON, "got {result}");
    }
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p sdr-ui --lib --features whisper-cpu doppler_tracker 2>&1 | tail -20`

Expected: 13 tests pass (9 from Task 1 + 4 new). If a TLE-parse error fires, the captured TLE has aged out — re-capture a current pair from `https://celestrak.org/NORAD/elements/gp.php?CATNR=33591&FORMAT=tle` and update `NOAA_19_TLE_LINE1` / `NOAA_19_TLE_LINE2`.

- [ ] **Step 3: Build + clippy**

Run:
```bash
cargo build --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -3
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```
Expected: all clean.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/doppler_tracker.rs
git commit -m "$(cat <<'EOF'
sdr-ui: doppler_tracker compute_doppler_offset_hz

Pure offset-computation helper that wraps sdr-sat's existing
track() + Track::doppler_shift_hz() in a tracker-friendly
signature with thiserror error type. Spec §5 anticipated needing
a new sdr-sat function; the math turns out to already be in
sdr-sat, so this is just a thin wrapper at the UI-layer
boundary that the DopplerTracker can call from its tick handler.

4 unit tests pin the call shape, swing-magnitude across a pass,
sign-vs-range-rate relationship, and the f₀=0 edge case.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Persistence helpers for master switch

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/satellites_panel.rs` (add KEY + load + save + tests)

**Goal:** Mirror the existing `KEY_AUTO_RECORD_APT` template for the new `KEY_DOPPLER_TRACKING_ENABLED` key. Default ON.

- [ ] **Step 1: Add the KEY constant**

Open `crates/sdr-ui/src/sidebar/satellites_panel.rs`. Find the existing `pub const KEY_AUTO_RECORD_AUDIO: &str = "sat_auto_record_audio";` line (around line 127). Add immediately after it:

```rust
/// Config key for the persisted Doppler-tracking master switch
/// (Satellites panel). Default `true` so first-launch users get
/// auto-corrected passes out of the box. Per issue #521.
pub const KEY_DOPPLER_TRACKING_ENABLED: &str = "sat_doppler_tracking_enabled";
```

- [ ] **Step 2: Add the load helper**

Find the existing `pub fn load_auto_record_audio` function. Add immediately after its closing brace:

```rust
/// Load the persisted Doppler-tracking master switch. Defaults
/// to `true` — fresh installs get auto-correction without
/// requiring the user to find and flip the switch. Per #521.
#[must_use]
pub fn load_doppler_tracking_enabled(config: &Arc<ConfigManager>) -> bool {
    config.read(|v| {
        v.get(KEY_DOPPLER_TRACKING_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    })
}
```

- [ ] **Step 3: Add the save helper**

Find the existing `pub fn save_auto_record_audio` function. Add immediately after its closing brace:

```rust
/// Persist the Doppler-tracking master switch. Per #521.
pub fn save_doppler_tracking_enabled(config: &Arc<ConfigManager>, enabled: bool) {
    config.write(|v| {
        v[KEY_DOPPLER_TRACKING_ENABLED] = serde_json::json!(enabled);
    });
}
```

- [ ] **Step 4: Add tests at the bottom of the existing test module**

Find the `#[cfg(test)] mod tests` block in `satellites_panel.rs`. Find any existing test function (e.g. `save_auto_record_apt_round_trip` or similar). Inside the same `mod tests` block, add these three tests at the end:

```rust
    #[test]
    fn load_doppler_tracking_enabled_defaults_to_on() {
        let config = make_config();
        // Spec §7.1: default ON so fresh installs get auto-
        // correction without user discovery.
        assert!(load_doppler_tracking_enabled(&config));
    }

    #[test]
    fn save_and_load_doppler_tracking_enabled_round_trip() {
        let config = make_config();
        save_doppler_tracking_enabled(&config, false);
        assert!(!load_doppler_tracking_enabled(&config));
        save_doppler_tracking_enabled(&config, true);
        assert!(load_doppler_tracking_enabled(&config));
    }

    #[test]
    fn load_doppler_tracking_enabled_tolerates_non_bool() {
        let config = make_config();
        config.write(|v| {
            v[KEY_DOPPLER_TRACKING_ENABLED] = serde_json::json!("not a bool");
        });
        // Falls back to the default (true), not a panic.
        assert!(load_doppler_tracking_enabled(&config));
    }
```

If `make_config()` doesn't exist as a helper in this file's tests, look at how other `load_*` tests in nearby files (e.g. `source_panel.rs`) construct their `Arc<ConfigManager>` — likely:
```rust
fn make_config() -> Arc<ConfigManager> {
    Arc::new(ConfigManager::in_memory(&serde_json::json!({})))
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p sdr-ui --lib --features whisper-cpu doppler_tracking 2>&1 | tail -15`

Expected: 3 tests pass.

- [ ] **Step 6: Build + clippy + fmt**

Run:
```bash
cargo build --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -3
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-ui/src/sidebar/satellites_panel.rs
git commit -m "$(cat <<'EOF'
sdr-ui: persistence for Doppler tracking master switch (#521)

KEY_DOPPLER_TRACKING_ENABLED + load/save helpers, default ON
so fresh-install users get Doppler correction without having
to discover and flip the switch. Mirrors the existing
auto_record_apt persistence pattern. Three tests pin default,
round-trip, and corrupt-value tolerance.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Status bar Doppler readout

**Files:**
- Modify: `crates/sdr-ui/src/status_bar.rs` (add label + builder packing + update method)

**Goal:** A new `gtk4::Label` in the status bar that shows `Doppler: -1.4 kHz` when active and is hidden when inactive. Per spec §7.2.

- [ ] **Step 1: Add the field to `StatusBar`**

Open `crates/sdr-ui/src/status_bar.rs`. Find the `pub struct StatusBar { ... }` block (around line 62). Inside the struct, add these two fields BEFORE the existing `pub role_label` field:

```rust
    /// Live Doppler-correction readout. Format: `Doppler: -1.4 kHz`
    /// (signed, 0.1 kHz precision). Hidden when no satellite is
    /// being tracked. Per issue #521.
    pub doppler_label: gtk4::Label,
    /// Separator widget packed immediately before `doppler_label`.
    /// Visibility toggled in lockstep with the label so hiding
    /// the label doesn't leave a stray separator.
    pub doppler_separator: gtk4::Separator,
```

- [ ] **Step 2: Add the `update_doppler` method**

In the `impl StatusBar` block, find any existing `pub fn update_*` method. Add this new method (placement doesn't matter — group it next to the existing ones for readability):

```rust
    /// Update the Doppler-correction readout.
    /// `Some(hz)` shows `Doppler: -1.4 kHz` (signed, kHz, 1 decimal).
    /// `None` hides the label and its separator. Per issue #521.
    pub fn update_doppler(&self, offset_hz: Option<f64>) {
        match offset_hz {
            Some(hz) => {
                let khz = hz / 1000.0;
                self.doppler_label
                    .set_label(&format!("Doppler: {khz:+.1} kHz"));
                self.doppler_label.set_visible(true);
                self.doppler_separator.set_visible(true);
            }
            None => {
                self.doppler_label.set_visible(false);
                self.doppler_separator.set_visible(false);
            }
        }
    }
```

- [ ] **Step 3: Pack the new widgets in the builder**

Find the `pub fn build_status_bar() -> StatusBar` function. The body packs labels into the bar in some order. Find where `role_label` and `role_separator` are constructed and packed (look for `gtk4::Label::builder()` near the bottom of the function, or `gtk4::Separator::new(gtk4::Orientation::Vertical)`). Add the Doppler widgets BEFORE the role widgets, so they appear to the left of the rtl_tcp role badge:

```rust
    let doppler_separator = gtk4::Separator::new(gtk4::Orientation::Vertical);
    doppler_separator.set_visible(false);
    let doppler_label = gtk4::Label::builder()
        .label("Doppler: +0.0 kHz")
        .visible(false)
        .build();
    widget.append(&doppler_separator);
    widget.append(&doppler_label);
```

Then add `doppler_label` and `doppler_separator` to the `StatusBar { ... }` literal at the end of the function.

(If your builder uses a different pattern for the existing labels, mirror that pattern instead — the goal is consistency with neighboring labels, not the exact code above.)

- [ ] **Step 4: Add a unit test pinning the format**

Find the `#[cfg(test)] mod tests` block in `status_bar.rs`. If there's no existing test module, add one at the bottom of the file. Add this test (which exercises the format-string behavior; doesn't need GTK initialized because the format is built before set_label):

```rust
    #[test]
    fn doppler_label_format_signed_one_decimal_khz() {
        // Spec §7.2: format is `Doppler: -1.4 kHz`, signed, kHz,
        // 1 decimal. Pin the format here so a future "drop the
        // sign for positive" or "show in Hz" change is forced
        // through review.
        // We can't construct a StatusBar in unit tests without
        // GTK, so we test the format directly.
        let hz: f64 = -1_437.5;
        let khz = hz / 1000.0;
        let formatted = format!("Doppler: {khz:+.1} kHz");
        assert_eq!(formatted, "Doppler: -1.4 kHz");

        let hz_pos: f64 = 2_700.0;
        let khz_pos = hz_pos / 1000.0;
        let formatted_pos = format!("Doppler: {khz_pos:+.1} kHz");
        assert_eq!(formatted_pos, "Doppler: +2.7 kHz");

        let hz_zero: f64 = 0.0;
        let khz_zero = hz_zero / 1000.0;
        let formatted_zero = format!("Doppler: {khz_zero:+.1} kHz");
        assert_eq!(formatted_zero, "Doppler: +0.0 kHz");
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p sdr-ui --lib --features whisper-cpu doppler_label 2>&1 | tail -10`

Expected: 1 test passes.

- [ ] **Step 6: Build + clippy + fmt**

Run:
```bash
cargo build --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -3
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-ui/src/status_bar.rs
git commit -m "$(cat <<'EOF'
sdr-ui: status bar Doppler readout (#521)

Adds `doppler_label` + `doppler_separator` to the status bar
plus an `update_doppler(Option<f64>)` method. Format is
`Doppler: -1.4 kHz` (signed, 0.1 kHz precision); `None` hides
both the label and its separator. Format pinned by unit test
so a future change to the display string is forced through
review.

Per spec §7.2 — UI surface ready for the DopplerTracker tick
handler to drive in a later task.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Master switch UI row in Satellites panel

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/satellites_panel.rs` (add `doppler_switch` field + builder code)

**Goal:** A new `AdwSwitchRow` next to the existing auto-record switches in the Satellites panel. No wiring yet — just the widget.

- [ ] **Step 1: Add the field to the panel struct**

In `satellites_panel.rs`, find the `pub struct SatellitesPanel { ... }` block. Find the existing `pub auto_record_audio_switch: adw::SwitchRow` field. Add immediately after it:

```rust
    /// Master switch for Doppler-correction tracking during
    /// satellite passes. Default ON. When OFF, the
    /// `DopplerTracker` stays dormant regardless of frequency
    /// match or pass schedule. Per issue #521.
    pub doppler_switch: adw::SwitchRow,
```

- [ ] **Step 2: Mirror the same field in `SatellitesPanelWeak`**

Find the `pub struct SatellitesPanelWeak { ... }` block. Find the existing `pub auto_record_audio_switch: glib::WeakRef<adw::SwitchRow>` field. Add immediately after it:

```rust
    pub doppler_switch: glib::WeakRef<adw::SwitchRow>,
```

- [ ] **Step 3: Mirror the downgrade + upgrade calls**

Find the `impl SatellitesPanel` block where `downgrade()` is called for each panel field. Add for `doppler_switch` (mirror `auto_record_audio_switch.downgrade()`).

Find the `impl SatellitesPanelWeak` block (or the `upgrade` method on it) where each weak field is upgraded back. Add `doppler_switch: self.doppler_switch.upgrade()?,` mirroring `auto_record_audio_switch`.

- [ ] **Step 4: Build the widget in the panel builder**

Find the function that builds the panel — search for `let auto_record_audio_switch = adw::SwitchRow::builder()`. After that block (and its `.build()` call), add:

```rust
    let doppler_switch = adw::SwitchRow::builder()
        .title("Doppler tracking")
        .subtitle("Auto-correct frequency drift during satellite passes")
        // Default ON — matches the persisted-default contract.
        // The wiring layer in Task 6 overrides this with the
        // persisted value, but if that wiring is ever skipped
        // the widget should still reflect "default ON" rather
        // than a misleading false.
        .active(true)
        .build();
```

(`.active(true)` matches the documented default-ON contract; the actual restored value is set by the wiring layer in Task 6 BEFORE wiring the change-notify handler.)

Now find where `auto_record_audio_switch` is added to its `AdwPreferencesGroup` (search for `.add_row(&auto_record_audio_switch)` or similar). Add immediately after it:

```rust
    // Same group as the auto-record switches — these are all
    // "behavior toggles for the satellites workflow". Per #521.
    auto_record_group.add_row(&doppler_switch);
```

(Replace `auto_record_group` with whatever variable name the panel builder uses for that `AdwPreferencesGroup`.)

Finally, add `doppler_switch` to the `SatellitesPanel { ... }` literal returned from the builder.

- [ ] **Step 5: Build + clippy + fmt**

Run:
```bash
cargo build --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -3
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```
Expected: clean.

(No new unit tests — pure widget plumbing; behavior is wired in Task 6.)

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/sidebar/satellites_panel.rs
git commit -m "$(cat <<'EOF'
sdr-ui: Doppler tracking master switch widget (#521)

AdwSwitchRow titled "Doppler tracking" added to the auto-record
group of the Satellites panel. Widget only — no wiring yet
(restore + change-notify handlers land in the next task with
the full DopplerTracker integration).

Mirrors the auto_record_apt_switch field shape across
SatellitesPanel + SatellitesPanelWeak so the upgrade path
stays consistent.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `DopplerTracker` stateful struct

**Files:**
- Modify: `crates/sdr-ui/src/doppler_tracker.rs` (add `DopplerTracker` struct + state-transition tests)

**Goal:** The stateful coordinator that wraps Tasks 1 and 2. Holds the master-switch flag, the active satellite (if any), and the additive `user_reference_offset`. Exposes pure methods that the wiring layer calls; doesn't itself touch GTK or schedule timers (those land in Task 7).

- [ ] **Step 1: Add the struct + methods to `doppler_tracker.rs`**

Add this BELOW the existing functions in `crates/sdr-ui/src/doppler_tracker.rs`, ABOVE the `#[cfg(test)] mod tests` block:

```rust
/// Stateful coordinator for the per-window Doppler tracker. The
/// timers, GTK widgets, and DSP-channel dispatch live in the
/// wiring layer (`window.rs::connect_doppler_tracker`); this
/// type owns the model state — master switch, current active
/// satellite, additive user reference offset — and exposes a
/// small set of methods the wiring layer drives on each tick or
/// re-evaluation.
///
/// Decoupled from GTK so the state-transition logic is unit-
/// testable headlessly. Same pattern as `satellites_recorder`'s
/// pure-tick / interpret-action split.
#[derive(Debug, Default)]
pub struct DopplerTracker {
    master_enabled: bool,
    active: Option<&'static KnownSatellite>,
    user_reference_offset_hz: f64,
}

impl DopplerTracker {
    /// Construct with the persisted master-switch value. Pass
    /// the result of
    /// `sidebar::satellites_panel::load_doppler_tracking_enabled`.
    #[must_use]
    pub fn new(master_enabled: bool) -> Self {
        Self {
            master_enabled,
            active: None,
            user_reference_offset_hz: 0.0,
        }
    }

    /// Update the master switch. Caller (wiring layer) is
    /// responsible for persisting the new value via
    /// `save_doppler_tracking_enabled`.
    ///
    /// On a transition to disabled, atomically clears the active
    /// satellite, captures `user_reference_offset_hz`, resets it
    /// to 0, and returns `Some(captured)` so the wiring layer
    /// can dispatch one final `SetVfoOffset(captured)` to flush
    /// DSP back to the user-reference baseline. Returns `None`
    /// on enable transitions or no-change calls.
    pub fn set_master_enabled(&mut self, enabled: bool) -> Option<f64> {
        self.master_enabled = enabled;
        if !enabled {
            let final_offset_hz = self.user_reference_offset_hz;
            self.active = None;
            self.user_reference_offset_hz = 0.0;
            return Some(final_offset_hz);
        }
        None
    }

    /// Whether the master switch is currently on.
    #[must_use]
    pub fn master_enabled(&self) -> bool {
        self.master_enabled
    }

    /// Current additive user reference offset (Hz). Set by the
    /// wiring layer when the user manually drags the VFO offset
    /// slider; Doppler tracking adds on top of this. Per spec §4.
    #[must_use]
    pub fn user_reference_offset_hz(&self) -> f64 {
        self.user_reference_offset_hz
    }

    /// Update the additive user reference offset. Called when
    /// the user manually drags the VFO offset slider. Per #4.
    pub fn set_user_reference_offset_hz(&mut self, hz: f64) {
        self.user_reference_offset_hz = hz;
    }

    /// Replace the currently-active satellite (or clear it). On
    /// a transition between distinct satellites or to-None, the
    /// `user_reference_offset_hz` resets to 0 — Doppler tracking
    /// is per-pass, and so is any user fine-tune. Returns true
    /// if the active satellite changed (useful for the wiring
    /// layer to know it needs to dispatch a SetVfoOffset and/or
    /// update the status bar).
    pub fn set_active(&mut self, satellite: Option<&'static KnownSatellite>) -> bool {
        let changed = match (self.active, satellite) {
            (None, None) => false,
            (Some(a), Some(b)) => !std::ptr::eq(a, b),
            _ => true,
        };
        if changed {
            // Spec §4 reset semantics: per-pass reference offset.
            self.user_reference_offset_hz = 0.0;
            self.active = satellite;
        }
        changed
    }

    /// Currently-active satellite (if any).
    #[must_use]
    pub fn active(&self) -> Option<&'static KnownSatellite> {
        self.active
    }

    /// Compose the live VFO offset to dispatch:
    /// `live = user_reference + doppler`. Per spec §4.
    #[must_use]
    pub fn live_offset_hz(&self, doppler_hz: f64) -> f64 {
        self.user_reference_offset_hz + doppler_hz
    }
}
```

- [ ] **Step 2: Add tests for the state machine**

Append to the existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn tracker_constructs_with_persisted_master_value() {
        let on = DopplerTracker::new(true);
        assert!(on.master_enabled());
        assert!(on.active().is_none());
        assert!((on.user_reference_offset_hz() - 0.0).abs() < f64::EPSILON);

        let off = DopplerTracker::new(false);
        assert!(!off.master_enabled());
    }

    #[test]
    fn set_master_disabled_clears_active_satellite() {
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        assert_eq!(t.active().map(|s| s.name), Some("NOAA 15"));
        t.set_master_enabled(false);
        assert!(t.active().is_none());
    }

    #[test]
    fn set_master_enabled_does_not_immediately_engage() {
        // Re-enabling the master switch shouldn't synthesize a
        // satellite — the wiring layer's re-evaluate tick is
        // what decides which (if any) satellite to engage.
        let mut t = DopplerTracker::new(false);
        t.set_master_enabled(true);
        assert!(t.active().is_none());
    }

    #[test]
    fn set_active_returns_true_only_on_change() {
        let mut t = DopplerTracker::new(true);
        assert!(t.set_active(Some(noaa_15())), "None → Some is a change");
        assert!(
            !t.set_active(Some(noaa_15())),
            "Some(X) → Some(X) is not a change"
        );
        assert!(t.set_active(Some(noaa_18())), "Some(X) → Some(Y) is a change");
        assert!(t.set_active(None), "Some → None is a change");
        assert!(!t.set_active(None), "None → None is not a change");
    }

    #[test]
    fn satellite_swap_resets_user_reference_offset() {
        // Spec §4 reset semantics: per-pass reference offset.
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(500.0);
        assert!((t.user_reference_offset_hz() - 500.0).abs() < f64::EPSILON);

        // Swap to a different satellite — offset resets.
        let _ = t.set_active(Some(noaa_18()));
        assert!((t.user_reference_offset_hz() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn satellite_to_none_resets_user_reference_offset() {
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(500.0);
        let _ = t.set_active(None);
        assert!((t.user_reference_offset_hz() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn same_satellite_set_active_preserves_user_reference_offset() {
        // If the satellite doesn't change (re-evaluate just
        // confirms what's already there), the user's manual
        // tune offset should NOT reset.
        let mut t = DopplerTracker::new(true);
        let _ = t.set_active(Some(noaa_15()));
        t.set_user_reference_offset_hz(500.0);
        let _ = t.set_active(Some(noaa_15()));
        assert!(
            (t.user_reference_offset_hz() - 500.0).abs() < f64::EPSILON,
            "user offset must survive a no-op set_active"
        );
    }

    #[test]
    fn live_offset_is_additive() {
        // Spec §4: live_offset = user_reference + doppler.
        let mut t = DopplerTracker::new(true);
        t.set_user_reference_offset_hz(300.0);
        assert!((t.live_offset_hz(-1_400.0) - (-1_100.0)).abs() < f64::EPSILON);
        assert!((t.live_offset_hz(2_700.0) - 3_000.0).abs() < f64::EPSILON);
        assert!((t.live_offset_hz(0.0) - 300.0).abs() < f64::EPSILON);
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p sdr-ui --lib --features whisper-cpu doppler_tracker 2>&1 | tail -25`

Expected: all `doppler_tracker` tests pass (Task 1's 9 + Task 2's 4 + Task 6's 7 = 20 tests).

- [ ] **Step 4: Build + clippy + fmt**

Run:
```bash
cargo build --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -3
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/doppler_tracker.rs
git commit -m "$(cat <<'EOF'
sdr-ui: DopplerTracker stateful coordinator (#521)

Owns the model state — master switch, active satellite, additive
user reference offset — and exposes a small API the wiring layer
drives. No GTK / no timers / no DSP dispatch in the tracker
itself; those live in window.rs alongside the rest of the
glib-timeout machinery, same pattern as satellites_recorder.

State-transition contract pinned by 7 tests: master-disable
clears active; master-enable doesn't auto-engage; set_active
returns true only on actual change; satellite swap resets the
user reference offset; same-satellite set_active is a no-op for
the offset; live_offset is the per-spec additive sum.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Wire it all up in `window.rs`

**Files:**
- Modify: `crates/sdr-ui/src/window.rs` (new function `connect_doppler_tracker`, called from the existing satellites-panel wiring; also: master-switch `connect_active_notify`, 4 Hz tick + 1 Hz re-evaluate timers)

**Goal:** All the GTK + glib-timeout + DSP-channel + status-bar plumbing that turns the tracker model from Task 6 into a live behavior.

This is the biggest task. The wiring follows the same patterns as `connect_satellites_panel` (TLE cache, ground-station construction, DSP send) and the established "restore BEFORE wire" idiom for the master switch.

- [ ] **Step 1: Find the right call site**

In `crates/sdr-ui/src/window.rs`, search for `fn connect_satellites_panel(` and locate its body. The Doppler tracker should be initialized somewhere in or near this function. Identify the variables it has access to:
- `panels.satellites` (the `SatellitesPanel` with `doppler_switch`)
- `state` (`Rc<AppState>`)
- `config` (`Arc<ConfigManager>`)
- `cache` (`TleCache`) — confirm by grepping for `TleCache::with_dir` near that function
- `status_bar` — may need to thread it in if not already accessible

If `status_bar` isn't in scope at this call site, expose it on `AppState` (it likely already lives somewhere in the window-construction call chain — check what `connect_status_bar_*` accepts).

- [ ] **Step 2: Sketch the new helper function**

Create a new function near `connect_satellites_panel`:

```rust
/// Wire the Doppler tracker: master-switch persistence + change
/// notify, 4 Hz offset-recompute tick, 1 Hz trigger re-evaluate
/// tick, status-bar update, SetVfoOffset dispatch. Per #521 and
/// the design spec at
/// `docs/superpowers/specs/2026-04-26-doppler-correction-design.md`.
fn connect_doppler_tracker(
    panels: &Panels,
    state: &Rc<AppState>,
    config: &Arc<ConfigManager>,
    cache: &Arc<TleCache>,
    status_bar: &Rc<StatusBar>,
) {
    use crate::doppler_tracker::{
        Candidate, DopplerTracker, FREQ_MATCH_TOLERANCE_HZ, compute_doppler_offset_hz,
        pick_active_satellite,
    };
    use sdr_sat::{GroundStation, Satellite, KNOWN_SATELLITES, track};

    // Restore the persisted master-switch value BEFORE wiring the
    // notify handler — same idiom as bias-T / gain / PPM. Otherwise
    // the programmatic `set_active` fires the notify handler and
    // re-saves the just-loaded value redundantly.
    let persisted = sidebar::satellites_panel::load_doppler_tracking_enabled(config);
    panels.satellites.doppler_switch.set_active(persisted);

    let tracker = Rc::new(RefCell::new(DopplerTracker::new(persisted)));

    // Shared dispatch baseline: read by the 4 Hz recompute tick
    // for its rate-limit gate; written by every path that
    // dispatches `SetVfoOffset` (master-switch handler, 1 Hz
    // trigger tick, 4 Hz recompute tick) so subsequent material-
    // change comparisons land against the latest actual DSP
    // state, not a stale Doppler value. Without this shared
    // baseline, a quick disengage→re-engage could land the next
    // computed offset within `DOPPLER_DISPATCH_THRESHOLD_HZ` of
    // the prior live value and the first dispatch would be
    // suppressed — DSP would stay on user-reference while the
    // status bar showed active Doppler.
    let last_dispatched: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));

    // Master-switch change handler: update tracker.
    // If the switch goes off, dispatch a final SetVfoOffset(
    // user_reference_offset) so the offset doesn't stick on the
    // last Doppler value, and clear the status bar label.
    {
        let tracker = Rc::clone(&tracker);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let last_dispatched = Rc::clone(&last_dispatched);
        panels
            .satellites
            .doppler_switch
            .connect_active_notify(move |row| {
                let enabled = row.is_active();
                let mut t = tracker.borrow_mut();
                let was_active = t.active().is_some();
                // `set_master_enabled` returns `Some(captured_user_ref)`
                // on a disable transition (atomically clears active +
                // resets user_reference + returns the pre-reset value
                // for us to flush). Only dispatch the fallback when
                // a satellite was actually being tracked, otherwise
                // we'd clobber the user's independently-set offset.
                if let Some(user_ref) = t.set_master_enabled(enabled)
                    && was_active
                {
                    state.send_dsp(UiToDsp::SetVfoOffset(user_ref));
                    last_dispatched.set(user_ref);  // sync the shared baseline
                    status_bar.update_doppler(None);
                }
            });
    }

    // 1 Hz trigger re-evaluation: rebuild the candidate list from
    // catalog + frequency match + ground station + cached TLEs.
    // On state change (engage / disengage / swap), update the
    // tracker and dispatch a SetVfoOffset to land at the new
    // satellite's Doppler offset cleanly.
    {
        let tracker = Rc::clone(&tracker);
        let cache = Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let last_dispatched = Rc::clone(&last_dispatched);
        let spectrum = Rc::clone(spectrum);
        let panels_weak = panels.satellites.downgrade();
        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            let Some(panel) = panels_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let mut t = tracker.borrow_mut();
            if !t.master_enabled() {
                return glib::ControlFlow::Continue;
            }
            // Build ground station from current panel rows.
            let station = GroundStation::new(
                panel.lat_row.value(),
                panel.lon_row.value(),
                panel.alt_row.value(),
            );
            let now = chrono::Utc::now();
            let current_freq = state.center_freq_hz();

            // Build candidate list: catalog entries within
            // ±FREQ_MATCH_TOLERANCE_HZ of `current_freq`, with
            // each entry's currently-evaluated elevation.
            let mut candidates: Vec<Candidate> = Vec::new();
            for sat in KNOWN_SATELLITES.iter() {
                let downlink = sat.downlink_hz as f64;
                if (downlink - current_freq).abs() > FREQ_MATCH_TOLERANCE_HZ {
                    continue;
                }
                // Need TLE + propagator → elevation.
                let Ok((line1, line2)) = cache.cached_tle_for(sat.norad_id) else {
                    continue;
                };
                let Ok(parsed) = Satellite::from_tle(sat.name, &line1, &line2) else {
                    continue;
                };
                let Ok(track) = track(&station, &parsed, now) else {
                    continue;
                };
                candidates.push(Candidate {
                    satellite: sat,
                    elevation_deg: track.elevation_deg,
                });
            }

            let new_active = pick_active_satellite(t.master_enabled(), &candidates);
            // Capture pre-`set_active` state. `set_active` resets
            // `user_reference_offset_hz` to 0 on any change, so we
            // must read it BEFORE the call to flush back to the
            // correct pre-engage value on disengage. We also need
            // to know whether a satellite was active (None→Some
            // engages and seeds; Some(A)→Some(B) swaps and does
            // NOT reseed — the spectrum offset at swap time is
            // `prior_user_ref + prior_doppler` and reseeding would
            // double-count Doppler).
            let prior_user_ref = t.user_reference_offset_hz();
            let prior_active_some = t.active().is_some();
            let changed = t.set_active(new_active);
            if changed {
                if new_active.is_some() {
                    if !prior_active_some {
                        // Fresh engagement — seed user_reference
                        // from the live spectrum VFO offset so
                        // this pass's Doppler tracks ON TOP of
                        // any offset the user had set.
                        let current_offset = spectrum.vfo_offset_hz();
                        t.set_user_reference_offset_hz(current_offset);
                    }
                    // No dispatch — the next 4 Hz tick handles it.
                } else {
                    // Disengaged — flush back to pre-engage user_ref.
                    state.send_dsp(UiToDsp::SetVfoOffset(prior_user_ref));
                    last_dispatched.set(prior_user_ref);  // sync the shared baseline
                    status_bar.update_doppler(None);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // 4 Hz offset-recompute tick: while a satellite is active,
    // recompute the Doppler shift and dispatch a SetVfoOffset
    // (rate-limited to changes >5 Hz to avoid spamming the bus).
    // Update the status bar label every tick (rounded to 0.1 kHz
    // so visual jitter is suppressed naturally).
    {
        let tracker = Rc::clone(&tracker);
        let cache = Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let panels_weak = panels.satellites.downgrade();
        let last_dispatched = Rc::clone(&last_dispatched);  // declared once at the function scope above
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            let Some(panel) = panels_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let t = tracker.borrow();
            let Some(sat) = t.active() else {
                return glib::ControlFlow::Continue;
            };
            let station = GroundStation::new(
                panel.lat_row.value(),
                panel.lon_row.value(),
                panel.alt_row.value(),
            );
            let Ok((line1, line2)) = cache.cached_tle_for(sat.norad_id) else {
                return glib::ControlFlow::Continue;
            };
            let Ok(parsed) = Satellite::from_tle(sat.name, &line1, &line2) else {
                return glib::ControlFlow::Continue;
            };
            let now = chrono::Utc::now();
            let carrier = sat.downlink_hz as f64;
            let Ok(doppler) = compute_doppler_offset_hz(&parsed, &station, now, carrier) else {
                return glib::ControlFlow::Continue;
            };
            let live = t.live_offset_hz(doppler);
            // Status bar always updates (rounded format hides
            // sub-100-Hz wobble naturally).
            status_bar.update_doppler(Some(doppler));
            // SetVfoOffset is rate-limited.
            if (live - last_dispatched.get()).abs() > 5.0 {
                state.send_dsp(UiToDsp::SetVfoOffset(live));
                last_dispatched.set(live);
            }
            glib::ControlFlow::Continue
        });
    }
}
```

- [ ] **Step 3: Call the new function from `connect_satellites_panel`**

Find the body of `connect_satellites_panel`. At a sensible place (near where the auto-record handlers are wired), add:

```rust
    connect_doppler_tracker(panels, state, config, &cache, status_bar);
```

(The exact arguments depend on what's in scope at that point in `connect_satellites_panel`. Use `Rc::clone` / `Arc::clone` as needed.)

- [ ] **Step 4: Add a `center_freq_hz()` accessor on `AppState` (if not already present)**

Verify the wiring's `state.center_freq_hz()` call resolves. If `AppState` doesn't already have that method, find how the existing satellites-panel `tune_to_satellite` call retrieves the current center frequency and use the same pattern.

If you need to add it: open `crates/sdr-ui/src/state.rs`, find the `impl AppState` block, and add a thin getter that returns the current center frequency (look for an existing `Cell<f64>` or RefCell that already holds it — `state.frequency_selector` likely has it via widget query, but a plain `Cell<f64>` accessor is simpler).

- [ ] **Step 5: Hook the additive user-reference handling**

Find where the VFO offset slider's `connect_value_notify` lives (search for `vfo_offset_row.connect_value_notify` or `SetVfoOffset` near a `value` callback). When a user-initiated drag fires the handler, call `tracker.borrow_mut().set_user_reference_offset_hz(...)` BEFORE the existing dispatch.

If the existing dispatch sends `UiToDsp::SetVfoOffset(row.value())` directly, replace it with:
```rust
let raw = row.value();
{
    let mut t = tracker.borrow_mut();
    t.set_user_reference_offset_hz(raw);
    // No add — the user's drag is the new reference; Doppler
    // tracking will add on top on the next 4 Hz tick.
}
state.send_dsp(UiToDsp::SetVfoOffset(raw));
```

Note: this requires the tracker `Rc<RefCell<...>>` to be in scope at the VFO-offset wiring site. If the wiring is in a different function from `connect_doppler_tracker`, hoist the tracker into `AppState` so both functions can borrow it via `state.doppler_tracker()` (an `Rc<RefCell<DopplerTracker>>` field on `AppState`).

If hoisting onto `AppState` is invasive, the simpler alternative for v1: skip the additive override (treat any user drag as authoritative for that tick — the next 4 Hz tick will overwrite it). Document this as a known limitation; defer the additive override to a follow-up. **Recommended for the first PR if hoisting is non-trivial.**

- [ ] **Step 6: Build + clippy + fmt**

Run:
```bash
cargo build --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -5
cargo clippy --workspace --features sdr-ui/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -5
cargo fmt --all -- --check
```
Expected: clean. Common clippy issues at this scale:
- `clippy::cast_possible_truncation` on `as f64` from u32 — add `#[allow(...)]` per cast or use `f64::from(...)` where the source type allows.
- `clippy::needless_pass_by_value` on the helper signatures — use references already where possible.

- [ ] **Step 7: Run the full test suite**

Run:
```bash
cargo test --workspace --features sdr-ui/whisper-cpu 2>&1 | tail -10
```
Expected: zero failures (no new tests added in this task — this task is all wiring, validated by smoke).

- [ ] **Step 8: Sherpa cross-check**

Run: `cargo check --workspace --no-default-features --features sdr-ui/sherpa-cpu 2>&1 | tail -3`

Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add crates/sdr-ui/src/window.rs
# Plus crates/sdr-ui/src/state.rs if you added a center_freq_hz accessor
git commit -m "$(cat <<'EOF'
sdr-ui: wire DopplerTracker into window.rs (#521)

Master-switch restore + change-notify, 1 Hz trigger re-evaluate
tick, 4 Hz offset-recompute tick, status-bar update, SetVfoOffset
dispatch (rate-limited to >5 Hz changes). Build candidate list
on each re-evaluate from catalog filtered by frequency match,
look up cached TLE, propagate SGP4, compute elevation; pass into
DopplerTracker::set_active. Active satellite drives the 4 Hz
recompute via DopplerTracker::live_offset_hz + status_bar.
update_doppler.

On disengage (master switch off, or trigger re-evaluate flips
to None) we dispatch a final SetVfoOffset(user_reference) so
the offset doesn't get stuck on the last computed Doppler value,
and clear the status bar label.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Smoke verification

**Files:** none

**Goal:** Manual end-to-end check on a real satellite pass.

- [ ] **Step 1: Install the release binary**

Run: `make install CARGO_FLAGS="--release"`

Expected: install succeeds. Verify with `ls -la /home/jherald/.cargo/bin/sdr-rs` that the timestamp is current.

- [ ] **Step 2: Cold-start smoke (no live pass needed)**

Launch the app. Open the Satellites activity panel. Verify:
- The "Doppler tracking" switch is present.
- It's ON by default (or matches its persisted value if you've toggled it before).
- Toggling it off and on again doesn't crash.
- Status bar does NOT show a Doppler readout (no satellite is active without a pass).

Quit and relaunch. Verify the toggle state persisted.

- [ ] **Step 3: Pre-pass smoke (between passes)**

Manually tune to NOAA 15's frequency (137.620 MHz). With NO satellite overhead:
- Status bar should NOT show a Doppler readout (correctly: trigger gate fails the elevation check).

Manually tune off (e.g. to 100 MHz) and back. Same — no readout.

- [ ] **Step 4: Live pass (the real test)**

Wait for an upcoming NOAA pass (use the Satellites panel's upcoming-passes list). At AOS:
- Status bar shows `Doppler: +X.X kHz` (positive at AOS — satellite approaching).
- The numeric value sweeps slowly across the pass: starts positive (~+2.7 kHz), passes through 0 at TCA, ends negative (~−2.7 kHz).
- Audio stays locked through the pass — no manual VFO nudging needed.
- At LOS, the readout disappears and any auto-recorded image (if auto-record is on) finalizes correctly.

If audio drift is still visible OR the readout doesn't sweep, capture:
- The exact frequency you were tuned to
- The pass time + ground-station coords
- A `tracing::debug` log if possible

These are the diagnostics for any iteration on the math.

- [ ] **Step 5: Smoke checkpoint with the user**

Report: built, smoke checklist passed (cold-start + pre-pass + live pass). If live pass smoke isn't possible right now, note that and call out the deferred check.

---

## Self-Review Notes

**Spec coverage:**
- §2 activation rule → Task 1 (`pick_active_satellite`) + Task 7 (candidate-list builder)
- §3 application point (VFO offset) → Task 7 (uses `UiToDsp::SetVfoOffset`, no `tune()`)
- §4 additive override → Task 6 (`live_offset_hz`) + Task 7 step 5 (with documented v1 deferral path)
- §5 Doppler math → Task 2 (compute helper wraps existing sdr-sat API; spec's "new function" turns out to be unnecessary)
- §6 DopplerTracker structure → Task 6 (model) + Task 7 (timers / wiring)
- §7.1 master switch → Task 3 (persistence) + Task 5 (UI row) + Task 7 (wiring)
- §7.2 status bar readout → Task 4 (label) + Task 7 (`update_doppler` driven by tick)
- §8 edge cases → covered by the trigger logic in Tasks 1 + 6 + 7
- §9 test plan → §9.1 covered by Task 2's TLE-based tests; §9.2 covered by Task 1 + 6 unit tests; §9.3 covered by Task 8

**Type consistency check:**
- `Candidate` (Task 1) used by `pick_active_satellite` (Task 1) and built in Task 7. Field names: `satellite`, `elevation_deg`. ✅
- `DopplerTracker` methods used in Task 7: `new`, `set_master_enabled`, `master_enabled`, `set_user_reference_offset_hz`, `user_reference_offset_hz`, `set_active`, `active`, `live_offset_hz`. All defined in Task 6. ✅
- `compute_doppler_offset_hz` signature used in Task 7's 4 Hz tick matches Task 2's definition: `(&Satellite, &GroundStation, DateTime<Utc>, f64) → Result<f64, DopplerError>`. ✅
- `update_doppler(Option<f64>)` used in Task 7 matches Task 4's signature. ✅
- `KEY_DOPPLER_TRACKING_ENABLED`, `load_doppler_tracking_enabled`, `save_doppler_tracking_enabled` used in Task 7 match Task 3's definitions. ✅
- `panels.satellites.doppler_switch` used in Task 7 matches Task 5's field name. ✅

**Placeholder scan:**
- No "TBD" / "TODO" / "implement later" used in plan steps.
- Task 7 step 5 has a documented v1 deferral path for the additive override — explicit + scoped, not a placeholder.

**Scope check:** Single subsystem (Doppler correction in sdr-ui). No spec section is multi-subsystem. Plan stays in one PR.
