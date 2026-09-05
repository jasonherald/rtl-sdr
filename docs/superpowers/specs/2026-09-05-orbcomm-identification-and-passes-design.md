# Orbcomm Spacecraft Identification + Pass Prediction (+ panel polish) — Design

Date: 2026-09-05
Status: approved (design discussion in-session)
Issues: #866 (identification + pass prediction), #898 (panel polish). Builds on #897 (Orbcomm activity panel, merged). Part of epic #867.

## Goal

Turn the Orbcomm panel's anonymous `Sat 0xNN` spacecraft into **real names** (`ORBCOMM FM06`) by matching each decoded ephemeris position against SGP4-propagated Orbcomm TLEs, add a **"Next Orbcomm passes"** list to the panel, and fold in the three #898 polish items (they share the same files and one of them feeds the matcher).

## Scope

- **#866 identification** — ECEF position matcher, Orbcomm TLE group fetch/cache, a learned+persisted `sat_id → name` table, names surfaced in the By-Spacecraft list.
- **#866 pass prediction** — a "Next Orbcomm passes" panel section over the Orbcomm TLE group, reusing the existing `upcoming_passes`.
- **#898 polish** — (1) pure telemetry-mapping helper, (2) event-gated `repaint_heard`/`refresh_breakdown`, (3) incremental `GtkTextBuffer` log.

## Non-goals

- **`sdr-orbcomm` is not modified** — it stays pure/no-I/O; the matcher lives in `sdr-sat` and takes raw `f64`s (no dependency on `sdr-orbcomm::Ephemeris`).
- No `KNOWN_SATELLITES` catalog entries for Orbcomm — names come from the TLE name line.
- No NORAD-id ↔ Orbcomm-`sat_id` table (none exists publicly; identification is purely positional).
- No decode/DSP changes; the DSP↔UI message set is unchanged.

## Decode/timing facts (captured so implementation needs no re-research)

- Decoded `Ephemeris { sat_id: u8, sat_time_unix: i64, lat_deg, lon_deg, alt_m, vel_ms }` (`crates/sdr-orbcomm/src/packet.rs:231-246`). `sat_time_unix` is GPS week+TOW with **no leap-second correction** (`packet.rs:236-237`) — i.e. ~**+18 s ahead of true UTC** in 2026.
- At LEO speed (~7.5 km/s) 18 s ≈ **~135 km** of along-track error — larger than the ~18 km ephemeris accuracy validated in #865. So the matcher **must** apply a leap-second correction: propagate to `when − GPS_UTC_LEAP_SECONDS`.
- Ephemeris decode is already range-gated (altitude 400–1000 km, |lat| ≤ 90) in `sdr-orbcomm`, so positions fed to the matcher are plausible.

## Architecture

### 1. Pure matcher in `sdr-sat` — new module `crates/sdr-sat/src/identify.rs`

```rust
/// Result of a positional spacecraft identification.
pub struct SpacecraftMatch {
    pub name: String,        // the matched TLE's name line, e.g. "ORBCOMM FM06"
    pub distance_km: f64,    // ECEF distance between decoded and propagated position
}

/// GPS−UTC offset (leap seconds). GPS time has no leap seconds; the
/// Orbcomm ephemeris timestamp is GPS-derived and uncorrected, so it
/// runs this many seconds ahead of true UTC. 18 s as of 2026-01; update
/// when a new leap second is announced. A wrong value only loosens
/// matches (the threshold absorbs small errors).
pub const GPS_UTC_LEAP_SECONDS: i64 = 18;

/// Default max ECEF distance (km) for a confident match.
pub const DEFAULT_MATCH_MAX_DIST_KM: f64 = 50.0;
/// The nearest candidate must beat the runner-up by at least this
/// factor to be unambiguous (guards overhead cases with two nearby birds).
pub const MATCH_AMBIGUITY_MARGIN: f64 = 2.0;

/// Identify a spacecraft from a decoded sub-satellite position + time.
/// Converts (lat,lon,alt) to ECEF, propagates each candidate TLE to
/// `when − GPS_UTC_LEAP_SECONDS` → ECI → ECEF, and returns the nearest
/// candidate whose distance is < `max_dist_km` AND at least
/// `MATCH_AMBIGUITY_MARGIN`× closer than the runner-up. `None` otherwise.
pub fn identify_spacecraft(
    lat_deg: f64,
    lon_deg: f64,
    alt_m: f64,
    when: chrono::DateTime<chrono::Utc>,
    candidates: &[(String, Satellite)],
    max_dist_km: f64,
) -> Option<SpacecraftMatch>;
```

Uses existing `sdr-sat` internals: `GroundStation`-free. Position→ECEF via `sgp4_core::geodetic_to_ecef` (`sgp4_core.rs:337`); TLE→ECEF via `Satellite::propagate(when)` (`sgp4_core.rs:211`) then `sgp4_core::eci_to_ecef` (`sgp4_core.rs:323`). Pure; no I/O; unit-tested. Export `identify_spacecraft`, `SpacecraftMatch`, and the constants from `lib.rs`.

Note: `eci_to_ecef` / `geodetic_to_ecef` are `pub` in `sgp4_core` but not re-exported at the crate root today — the matcher lives inside `sdr-sat` so it uses them directly (`crate::sgp4_core::…`); no new root re-export needed for them.

### 2. Orbcomm TLE group fetch — extend `crates/sdr-sat/src/tle_cache.rs`

The cache is per-NORAD only today (`Fetcher = dyn Fn(u32) -> …`, `tle_cache.rs:105`); groups were removed historically. Re-add a **group path** alongside the per-NORAD one:

- `pub fn celestrak_group_url(slug: &str) -> String` → `https://celestrak.org/NORAD/elements/gp.php?GROUP={slug}&FORMAT=tle`.
- `pub type GroupFetcher = dyn Fn(&str) -> Result<String, TleCacheError> + Send + Sync;` + `with_group_fetcher(...)` (test seam mirroring `with_fetcher`). Default impl = blocking `reqwest` GET (same timeout).
- Group cache file: `~/.cache/sdr-rs/tle/group-{slug}.tle` (distinct from the `{norad}.tle` per-sat files); atomic write, 24 h TTL — same machinery as per-NORAD.
- `pub fn parse_group_tles(text: &str) -> Vec<(String, String, String)>` — scan the body for `name / line1 / line2` triples (line1 starts `1 `, line2 starts `2 `; the preceding non-blank line is the name). Reuses the existing sliding-window validation for the two element lines. The per-NORAD `fetch_validated` single-id gate is **not** applied to groups.
- `pub fn cached_group_tles(&self, slug: &str) -> Result<Vec<(String, String, String)>, TleCacheError>` — cache-only, GTK-thread-safe (never network).
- `pub fn force_refresh_group(&self, slug: &str) -> Result<Vec<(String, String, String)>, TleCacheError>` — forced network round trip, called off-thread.

Group slug constant: `pub const ORBCOMM_TLE_GROUP: &str = "ORBCOMM";` (in `sdr-sat`).

### 3. Learned `sat_id → name` table + persistence

- On `AppState`: `orbcomm_sat_names: RefCell<HashMap<u8, String>>` (the learned table) and `orbcomm_tles: RefCell<Vec<(String, sdr_sat::Satellite)>>` (parsed Orbcomm candidates ready to propagate).
- **Load at startup** from config key `orbcomm_sat_names` (JSON object; precedent `watched_satellites` in `sidebar/satellites_panel/persistence.rs:143-174`). Register a `{}` default so `merge_defaults` preserves it.
- **Populate on confident match**, then persist via `config.write(|v| v["orbcomm_sat_names"] = json!(map))`.
- Once a `sat_id` is in the table, **stop re-matching it** (reuse the name). This bounds matching cost and directly serves #898's "don't rematch/repaint every packet" concern.
- The table is inherently the **"actually transmitting" set** (only decoded birds appear) — used to mark live passes (§4).

### 4. UI surfacing (`crates/sdr-ui/`)

**Names in By-Spacecraft** (`sidebar/orbcomm_panel.rs`): resolve each row's title through the learned table — matched → `ORBCOMM FM06` as the `AdwActionRow` title with `Sat 0xNN` demoted into the subtitle; unmatched → `Sat 0xNN` title as today. The pure `orbcomm_render` log lines stay `Sat 0xNN` (no state access; the log is the raw-packet view).

**"Next Orbcomm passes" section** (`sidebar/orbcomm_panel.rs`): a new `AdwPreferencesGroup` (placed below By-Spacecraft), rows of `name · HH:MM–HH:MM · max-el°`, most-imminent first. Built by looping the Orbcomm TLEs (`orbcomm_tles`) through `sdr_sat::upcoming_passes(&station, &sat, from, to, min_el)` — the same shape as `enumerate_upcoming_passes` (`sidebar/satellites_panel/passes.rs:215`). `GroundStation` comes from config (same keys the satellites panel reads: `sat_station_{lat_deg,lon_deg,alt_m}`). Refreshed on: panel decode-enable, a TLE refresh, and a low-frequency timer (e.g. 60 s). Birds present in the learned table get a "heard" marker.

**TLE refresh wiring**: on Orbcomm decode-enable (and reusing the satellites-panel refresh button path), if the group cache is stale, kick a background `gio::spawn_blocking(force_refresh_group("ORBCOMM"))`; on completion, parse to `Vec<(String, Satellite)>`, store in `orbcomm_tles`, and repaint the passes section. The GTK thread otherwise reads only `cached_group_tles`.

**Matching hook** (`window/dsp_events/orbcomm_events.rs::record_heard_satellite`): after recording the heard entry, if the event carried an `Ephemeris` **and** `sat_id` is not yet in the learned table **and** `orbcomm_tles` is non-empty, call `identify_spacecraft(lat, lon, alt, utc_from(sat_time_unix), &orbcomm_tles, DEFAULT_MATCH_MAX_DIST_KM)`; on `Some`, insert into the table, persist, and refresh the By-Spacecraft rows.

**#898 polish, folded in:**
1. **Pure telemetry-mapping helper** — extract the event→fields mapping (`Sync` → `(sat_id, None, None, None)`, `Ephemeris` → `(sat_id, Some(pos), Some(vel), Some(time))`) into a pure, unit-tested fn (in `orbcomm_render` or a small module). It feeds both `record_heard_satellite` and the matcher, and closes the coverage gap #898 flagged.
2. **Event-gated refresh** — `on_orbcomm_event` only calls `repaint_heard` when the mapping recorded an identity event, and only `refresh_breakdown` for `Packet` events (`MessageComplete` changes neither).
3. **Incremental log buffer** — `append_log_entry` uses `GtkTextBuffer` `insert` (at end) + `delete` (oldest) instead of full `set_text`, with a unit test asserting the `MAX_LOG_ENTRIES` rotation.

## Data flow

```text
DSP: OrbcommEvent(Ephemeris) ──mpsc──> UI on_orbcomm_event
  └─ telemetry_fields(event)  [pure #898 helper]
       ├─ HeardSatellites.record(...)                     ──► By-Spacecraft rows
       └─ if Ephemeris && sat_id unknown && tles loaded:
            identify_spacecraft(lat,lon,alt, utc_from(sat_time_unix), &orbcomm_tles)
              # matcher applies −GPS_UTC_LEAP_SECONDS internally before propagating
              └─ Some(name) ─► learned table (persist) ─► By-Spacecraft title

Background (gio::spawn_blocking, off GTK thread):
  force_refresh_group("ORBCOMM") ─► parse ─► orbcomm_tles ─► repaint passes

Panel passes section (GTK thread, timer/enable/refresh):
  for (name, sat) in orbcomm_tles: upcoming_passes(&station, &sat, now, now+8h, min_el)
    ─► sort by start ─► rows (mark heard birds)
```

## Error handling / threading

- **Matching + rendering on the GTK thread** using cached TLEs (~14 propagations — cheap). **Network only off-thread** via `spawn_blocking`; `cached_group_tles` never hits the network.
- **No TLEs / stale cache** → no match, keep `Sat 0xNN`; passes section shows an empty/"refresh TLEs" state. Never blocks decode.
- **Expired TLEs** → `upcoming_passes` returns `SatelliteError::TleExpired`; surfaced as the empty/refresh state, not an error dialog.
- **Wrong leap constant / ephemeris noise** → looser matches; the distance threshold + ambiguity margin absorb it; a bad match is bounded by the threshold.
- **Config**: `orbcomm_sat_names` written through `ConfigManager::write` (atomic + auto-save); `{}` default registered.

## Testing

Unit (TDD, pure pieces):
- **Matcher** (`identify.rs`): a synthetic TLE propagated to `T`, its own sub-point fed back → matches with ~0 km; a point 500 km away → `None`; two near-equidistant candidates → `None` (ambiguity margin); leap-second offset applied (position at `T` vs `T−18` differs as expected).
- **Telemetry helper**: `Sync` → `(id, None, None, None)`; `Ephemeris` → populated; `MessageComplete`/`Other` → `None`.
- **Group parser**: a multi-entry Orbcomm TLE body → all `(name, l1, l2)` triples, names correct; malformed lines skipped.
- **Learned table**: config round-trip (save `{0x2C:"ORBCOMM FM06"}` → load equals); unknown `sat_id` → fallback label.
- **Log rotation**: incremental buffer holds ≤ `MAX_LOG_ENTRIES`, oldest dropped.

Real-data gate (DSP rule): a live/replayed Orbcomm pass identifies the right bird(s) and the predicted passes line up with reality — the same live-calibration bar we held for the LRPT differential flag. Calibrate `GPS_UTC_LEAP_SECONDS` / `DEFAULT_MATCH_MAX_DIST_KM` against that pass.

GTK panel wiring (names in rows, passes section, timer) is smoke-tested by the user per the standard workflow (`make install` + checklist); Claude does not launch the binary.

## Files touched

New:
- `crates/sdr-sat/src/identify.rs` — pure matcher + constants + tests.

Modified:
- `crates/sdr-sat/src/tle_cache.rs` — group fetch/parse/cache (`celestrak_group_url`, `GroupFetcher`, `parse_group_tles`, `cached_group_tles`, `force_refresh_group`).
- `crates/sdr-sat/src/lib.rs` — export `identify` items + `ORBCOMM_TLE_GROUP`.
- `crates/sdr-ui/src/state.rs` — `orbcomm_sat_names`, `orbcomm_tles` fields + init.
- `crates/sdr-ui/src/orbcomm_render.rs` (or a small new module) — pure telemetry-mapping helper (#898 #1).
- `crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs` — matching hook + event-gated refresh (#898 #2) + use the telemetry helper.
- `crates/sdr-ui/src/sidebar/orbcomm_panel.rs` — By-Spacecraft name resolution, "Next Orbcomm passes" section, incremental log (#898 #3).
- `crates/sdr-ui/src/sidebar/satellites_panel/persistence.rs` (or a sibling) — load/save `orbcomm_sat_names`.
- TLE-refresh wiring (`crates/sdr-ui/src/window/satellites/passes.rs` or the Orbcomm panel connect) — trigger the group refresh off-thread.

## Sequencing note

Larger than a typical single plan but cohesive (all Orbcomm-panel, shared files). If the plan proves too big during writing-plans, the natural seam to split on is **identification (§1–§3 + names in §4)** vs **the passes section (§4 passes)** — the passes list is independent of matching. Kept together here per the user's "everything in one plan" decision.

## Calibration correction (2026-09-05)

A live FM108 pass (88° overhead) showed that the decoded ephemeris's own timestamp (`sat_time_unix`) is roughly 10 hours off — a separate `sdr-orbcomm` decode bug tracked as issue #900. The ephemeris's decoded position is correct; only its embedded clock is wrong.

The matcher now propagates candidate TLEs to the reception time (`Utc::now()`) instead of `sat_time_unix`, since a live-received ephemeris reflects the satellite's current position regardless of what its own clock says. The 18 s leap-second correction is removed as no longer meaningful once the reference time is reception time, not a GPS-derived timestamp. The match threshold is raised from 50 km to 100 km to absorb ordinary propagation/reception-time slop.

Validated against the real FM108 pass: propagating to reception time identifies ORBCOMM FM108 at 47 km (runner-up 3190 km, unambiguous); propagating to the buggy `sat_time_unix` instead matches garbage.
