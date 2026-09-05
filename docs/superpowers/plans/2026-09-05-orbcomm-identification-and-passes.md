# Orbcomm Identification + Pass Prediction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Identify Orbcomm spacecraft by matching each decoded ephemeris position against SGP4-propagated Orbcomm TLEs (turning `Sat 0xNN` into `ORBCOMM FM06`), add a "Next Orbcomm passes" panel section, and fold in three Orbcomm-panel polish items (#898).

**Architecture:** A pure ECEF position matcher in `sdr-sat` (leap-second corrected), an Orbcomm TLE *group* fetch added to the existing `TleCache`, a learned+persisted `sat_id → name` table on `AppState`, and UI wiring in the Orbcomm panel + its DSP-event handler. `sdr-orbcomm` is untouched; the matcher takes raw `f64`s.

**Tech Stack:** Rust, `chrono` (already in `sdr-sat`), the workspace SGP4 core (`sdr-sat::sgp4_core`), GTK4/libadwaita (`sdr-ui`), `serde_json` config (`sdr-config`).

**Spec:** `docs/superpowers/specs/2026-09-05-orbcomm-identification-and-passes-design.md`

## Global Constraints

- `sdr-orbcomm` is **NOT modified**. The matcher lives in `sdr-sat` and takes raw `f64`s (no dependency on `sdr-orbcomm::Ephemeris`).
- No `unwrap()`/`panic!()`/`println!()` in library code; use `tracing`. GTK callbacks fail closed (early-return), matching existing `orbcomm_panel.rs`/`acars_viewer.rs` style.
- **Codacy quality gates (enforced — design to them, don't hit them in review):**
  - **Function/method ≤ 50 NLOC** (`Lizard_nloc-medium`). Codacy counts ~4–7 lines stricter than local `uvx lizard`, so **target local ≤ ~44**. Carve into sub-helpers proactively.
  - **File ≤ 500 NLOC** (`Lizard_file-nloc-medium`). Keep new files focused; split before approaching it.
  - **Function parameters ≤ 8** (`Lizard_parameter-count-medium`).
  - **Diff coverage** stays green by unit-testing all new **pure** logic; keep GTK/DSP glue thin so the untested surface is minimal.
  - Docs: **markdownlint** — every fenced code block needs a language (MD040); MD024 runs `siblings_only:false` (no duplicate headings *anywhere* in a doc).
- **Every code task runs `uvx lizard <changed .rs files> -T nloc=50`** as a gate step (after clippy, before fmt) and must report no function over 50 / file over 500.
- CI clippy invocation to match before push: `cargo clippy --all-targets --workspace -- -D warnings` (no features).
- Run each cargo gate as its own bare, unpiped Bash command. `cargo fmt --all -- --check` is the **last** gate before any push.
- Per-package `sdr-ui` compilation needs a transcription feature (feature-mutex): use `--features whisper-cpu` for `sdr-ui` test/build gates. `sdr-sat` and `sdr-config` have no such mutex — build/test them without features.
- **Never** `git add -A`/`git add .` (repo carries untracked `build/` + `.vscode/`, plus unrelated `.codacy/*` drift). Stage explicit paths; verify `git status --short`.
- Branch `feature/orbcomm-identification-passes` (created; spec committed). One PR. After the first push, **wait for CodeRabbit to post its review for that exact SHA before the next push**. After push, check Codacy: `codacy pull-request gh jasonherald rtl-sdr <PR>` — resolve issues + verify coverage before merge.
- No `sdr-transcription` changes → triple-build rule does not apply.
- No new dependencies (so `Cargo.lock` is unchanged; a `cargo check --workspace --locked` still runs in the final gate).
- Author commits `Jason Herald <392+jasonherald@users.noreply.github.com>` (repo default; don't override). End each commit message with the two trailer lines shown in the task steps.
- **Live-calibration gate** (DSP rule): the matcher's threshold + leap constant are validated against a real Orbcomm pass (Task 9), the same bar we held for the LRPT differential flag.

---

### Task 1: Pure ECEF spacecraft matcher in `sdr-sat`

**Files:**
- Create: `crates/sdr-sat/src/identify.rs`
- Create: `crates/sdr-sat/src/identify/tests.rs`
- Modify: `crates/sdr-sat/src/lib.rs` (`pub mod identify;` + re-exports)

**Interfaces:**
- Consumes: `crate::sgp4_core::{Satellite, geodetic_to_ecef, eci_to_ecef}`, `chrono::{DateTime, Utc, Duration}`.
- Produces (in `crate::identify`, re-exported from `lib.rs`):
  - `pub struct SpacecraftMatch { pub name: String, pub distance_km: f64 }`
  - `pub const GPS_UTC_LEAP_SECONDS: i64 = 18;`
  - `pub const DEFAULT_MATCH_MAX_DIST_KM: f64 = 50.0;`
  - `pub const MATCH_AMBIGUITY_MARGIN: f64 = 2.0;`
  - `pub fn identify_spacecraft(lat_deg: f64, lon_deg: f64, alt_m: f64, when: DateTime<Utc>, candidates: &[(String, Satellite)], max_dist_km: f64) -> Option<SpacecraftMatch>`

- [ ] **Step 1: Write the failing tests**

Create `crates/sdr-sat/src/identify/tests.rs`:

```rust
use super::*;
use crate::sgp4_core::{Satellite, eci_to_ecef};
use chrono::{TimeZone, Utc};

// Real Orbcomm TLEs (Celestrak, epoch 2026-248) used as fixtures.
const FM06: (&str, &str, &str) = (
    "ORBCOMM FM06",
    "1 25118U 97084G   26248.14598269  .00000608  00000+0  20228-3 0  9991",
    "2 25118  45.0146  27.7326 0001119 213.3371 317.0700 14.47727064505974",
);
const FM04: (&str, &str, &str) = (
    "ORBCOMM FM04",
    "1 25159U 98007C   26248.16811710  .00000396  00000+0  18072-3 0  9991",
    "2 25159 107.9604 334.0349 0041099 188.9146 171.1268 14.35849962488071",
);

fn sat(t: (&str, &str, &str)) -> Satellite {
    Satellite::from_tle(t.0, t.1, t.2).expect("valid TLE fixture")
}

/// A satellite's own propagated position (at true UTC `t`) fed back as
/// the decoded ECEF, with `when = t + leap`, matches that satellite at
/// ~0 km — exercising the leap-second correction end to end.
#[test]
fn matches_self_with_leap_correction() {
    let t = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let fm06 = sat(FM06);
    let eci = fm06.propagate(t).unwrap();
    let target = eci_to_ecef(eci.position_km, t);
    let candidates = vec![("ORBCOMM FM06".into(), sat(FM06)), ("ORBCOMM FM04".into(), sat(FM04))];
    let when = t + chrono::Duration::seconds(GPS_UTC_LEAP_SECONDS);
    let m = identify_from_ecef(target, when, &candidates, DEFAULT_MATCH_MAX_DIST_KM)
        .expect("should identify FM06");
    assert_eq!(m.name, "ORBCOMM FM06");
    assert!(m.distance_km < 5.0, "distance {} km too large", m.distance_km);
}

/// A point far from every candidate → no match.
#[test]
fn no_match_when_far() {
    let t = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let candidates = vec![("ORBCOMM FM06".into(), sat(FM06))];
    // Origin-ish ECEF (deep inside Earth) is >6000 km from any orbit.
    assert!(identify_from_ecef([0.0, 0.0, 0.0], t, &candidates, DEFAULT_MATCH_MAX_DIST_KM).is_none());
}

/// Two candidates near-equidistant from the target → ambiguous → None.
#[test]
fn ambiguous_returns_none() {
    let t = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let fm06 = sat(FM06);
    let eci = fm06.propagate(t).unwrap();
    let target = eci_to_ecef(eci.position_km, t);
    // Same satellite twice under different names: both at distance ~0,
    // runner-up not MARGIN× farther → ambiguous.
    let candidates = vec![("A".into(), sat(FM06)), ("B".into(), sat(FM06))];
    let when = t + chrono::Duration::seconds(GPS_UTC_LEAP_SECONDS);
    assert!(identify_from_ecef(target, when, &candidates, 500.0).is_none());
}
```

- [ ] **Step 2: Run — expect FAIL (module missing)**

Run: `cargo test -p sdr-sat identify`
Expected: FAIL to compile (`identify` module / `identify_from_ecef` undefined).

- [ ] **Step 3: Implement the matcher**

Create `crates/sdr-sat/src/identify.rs`:

```rust
//! Positional Orbcomm spacecraft identification: match a decoded
//! ephemeris sub-satellite position against SGP4-propagated candidate
//! TLEs. Pure — no I/O. The decoded timestamp is GPS-derived and
//! uncorrected (see `GPS_UTC_LEAP_SECONDS`), so propagation is done at
//! `when − leap`.

use chrono::{DateTime, Duration, Utc};

use crate::sgp4_core::{Satellite, eci_to_ecef, geodetic_to_ecef};

/// GPS−UTC offset (leap seconds). GPS time has no leap seconds; the
/// Orbcomm ephemeris timestamp is GPS-derived and uncorrected, so it
/// runs this many seconds ahead of true UTC. 18 s as of 2026-01; update
/// when a new leap second is announced. A wrong value only loosens
/// matches — the distance threshold absorbs small errors.
pub const GPS_UTC_LEAP_SECONDS: i64 = 18;

/// Default max ECEF distance (km) for a confident match.
pub const DEFAULT_MATCH_MAX_DIST_KM: f64 = 50.0;

/// The nearest candidate must be at least this many times closer than
/// the runner-up to be accepted (guards ambiguous overhead cases).
pub const MATCH_AMBIGUITY_MARGIN: f64 = 2.0;

/// A positional identification result.
#[derive(Debug, Clone, PartialEq)]
pub struct SpacecraftMatch {
    /// Matched TLE name line, e.g. `"ORBCOMM FM06"`.
    pub name: String,
    /// ECEF distance (km) between decoded and propagated position.
    pub distance_km: f64,
}

/// Identify a spacecraft from a decoded sub-satellite geodetic position
/// + timestamp. Converts to ECEF and delegates to
/// [`identify_from_ecef`]. `when` is the raw ephemeris timestamp (the
/// leap-second correction is applied internally).
#[must_use]
pub fn identify_spacecraft(
    lat_deg: f64,
    lon_deg: f64,
    alt_m: f64,
    when: DateTime<Utc>,
    candidates: &[(String, Satellite)],
    max_dist_km: f64,
) -> Option<SpacecraftMatch> {
    let target = geodetic_to_ecef(lat_deg, lon_deg, alt_m);
    identify_from_ecef(target, when, candidates, max_dist_km)
}

/// Core matcher over an ECEF target (km). Propagates each candidate to
/// `when − GPS_UTC_LEAP_SECONDS`, keeps the nearest and runner-up, and
/// returns the nearest iff it is within `max_dist_km` AND at least
/// `MATCH_AMBIGUITY_MARGIN`× closer than the runner-up.
fn identify_from_ecef(
    target_km: [f64; 3],
    when: DateTime<Utc>,
    candidates: &[(String, Satellite)],
    max_dist_km: f64,
) -> Option<SpacecraftMatch> {
    let prop_time = when - Duration::seconds(GPS_UTC_LEAP_SECONDS);
    let mut best: Option<(usize, f64)> = None;
    let mut runner_up_km = f64::INFINITY;
    for (i, (_, sat)) in candidates.iter().enumerate() {
        let Ok(eci) = sat.propagate(prop_time) else { continue };
        let ecef = eci_to_ecef(eci.position_km, prop_time);
        let d = distance_km(ecef, target_km);
        match best {
            Some((_, bd)) if d >= bd => {
                if d < runner_up_km {
                    runner_up_km = d;
                }
            }
            _ => {
                if let Some((_, bd)) = best {
                    runner_up_km = bd;
                }
                best = Some((i, d));
            }
        }
    }
    let (idx, dist) = best?;
    if dist < max_dist_km && runner_up_km >= dist * MATCH_AMBIGUITY_MARGIN {
        Some(SpacecraftMatch { name: candidates[idx].0.clone(), distance_km: dist })
    } else {
        None
    }
}

fn distance_km(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[cfg(test)]
mod tests;
```

Add to `crates/sdr-sat/src/lib.rs`: `pub mod identify;` and re-export `pub use identify::{DEFAULT_MATCH_MAX_DIST_KM, GPS_UTC_LEAP_SECONDS, MATCH_AMBIGUITY_MARGIN, SpacecraftMatch, identify_spacecraft};` (alphabetized within the existing `pub use` block).

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p sdr-sat identify`
Expected: 3 tests PASS.

- [ ] **Step 5: Codacy + cargo gates**

Run: `cargo build -p sdr-sat`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `uvx lizard crates/sdr-sat/src/identify.rs -T nloc=50`  (expect: no function > 50 NLOC, file < 500)
Run: `cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-sat/src/identify.rs crates/sdr-sat/src/identify/tests.rs crates/sdr-sat/src/lib.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(sat): pure ECEF Orbcomm spacecraft matcher (leap-second corrected)

identify_spacecraft matches a decoded ephemeris sub-satellite position
against SGP4-propagated candidate TLEs, propagating at when−18s (GPS/UTC
leap offset) and requiring a nearest within a distance threshold that
also beats the runner-up by a margin. Pure, no I/O; takes raw f64s so
sdr-orbcomm stays untouched.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 2: Orbcomm TLE group fetch/cache in `sdr-sat`

**Files:**
- Modify: `crates/sdr-sat/src/tle_cache.rs` (group URL, `GroupFetcher`, parser, cache-read, force-refresh)
- Modify: `crates/sdr-sat/src/tle_cache/tests.rs` (add tests — if tests are inline, add a `#[cfg(test)] mod tests` block at file bottom following the existing style)
- Modify: `crates/sdr-sat/src/lib.rs` (export `celestrak_group_url`, `ORBCOMM_TLE_GROUP`)

**Interfaces:**
- Consumes: existing `TleCache` internals (`cache_path` dir, atomic `write_cache`, freshness check, `parse_tle_text` sliding-window).
- Produces (on `TleCache` / in `crate::tle_cache`, re-exported):
  - `pub const ORBCOMM_TLE_GROUP: &str = "ORBCOMM";`
  - `pub fn celestrak_group_url(slug: &str) -> String`
  - `pub type GroupFetcher = dyn Fn(&str) -> Result<String, TleCacheError> + Send + Sync;`
  - `TleCache::with_group_fetcher(self, Arc<GroupFetcher>) -> Self`
  - `pub fn parse_group_tles(text: &str) -> Vec<(String, String, String)>` (name, line1, line2)
  - `TleCache::cached_group_tles(&self, slug: &str) -> Result<Vec<(String, String, String)>, TleCacheError>`
  - `TleCache::force_refresh_group(&self, slug: &str) -> Result<Vec<(String, String, String)>, TleCacheError>`

- [ ] **Step 1: Write the failing tests**

Add to the `sdr-sat` `tle_cache` tests (mirror the existing temp-dir + injected-fetcher pattern already used there):

```rust
#[test]
fn parse_group_tles_extracts_all_named_entries() {
    let body = "\
ORBCOMM FM06            
1 25118U 97084G   26248.14598269  .00000608  00000+0  20228-3 0  9991
2 25118  45.0146  27.7326 0001119 213.3371 317.0700 14.47727064505974
ORBCOMM FM04            
1 25159U 98007C   26248.16811710  .00000396  00000+0  18072-3 0  9991
2 25159 107.9604 334.0349 0041099 188.9146 171.1268 14.35849962488071
";
    let got = parse_group_tles(body);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, "ORBCOMM FM06");
    assert!(got[0].1.starts_with("1 25118"));
    assert!(got[0].2.starts_with("2 25118"));
    assert_eq!(got[1].0, "ORBCOMM FM04");
}

#[test]
fn parse_group_tles_skips_malformed() {
    // A stray line with no following element lines is ignored.
    let body = "GARBAGE\nnot a tle line\nORBCOMM FM06\n1 25118U 97084G   26248.1 .0 0 0 0 9991\n2 25118  45.0 27.7 0001 213 317 14.47\n";
    let got = parse_group_tles(body);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, "ORBCOMM FM06");
}

#[test]
fn force_refresh_group_writes_and_reads_cache() {
    let dir = tempfile::tempdir().unwrap();
    let body = "ORBCOMM FM06\n1 25118U 97084G   26248.1  .0  0  0 0  9991\n2 25118  45.0146  27.7326 0001119 213.3371 317.0700 14.47727064505974\n".to_string();
    let cache = TleCache::with_dir(dir.path().to_path_buf())
        .with_group_fetcher(std::sync::Arc::new(move |_slug: &str| Ok(body.clone())));
    let fetched = cache.force_refresh_group("ORBCOMM").unwrap();
    assert_eq!(fetched.len(), 1);
    // Cache-only read now returns the same without a fetcher hit.
    let cached = cache.cached_group_tles("ORBCOMM").unwrap();
    assert_eq!(cached[0].0, "ORBCOMM FM06");
}
```

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p sdr-sat tle_cache`
Expected: FAIL to compile (new items undefined).

- [ ] **Step 3: Implement the group path**

In `crates/sdr-sat/src/tle_cache.rs`:

```rust
/// Celestrak group slug for the Orbcomm constellation.
pub const ORBCOMM_TLE_GROUP: &str = "ORBCOMM";

/// Celestrak GP URL for a whole named group (many TLEs in one body).
#[must_use]
pub fn celestrak_group_url(slug: &str) -> String {
    format!("https://celestrak.org/NORAD/elements/gp.php?GROUP={slug}&FORMAT=tle")
}

/// Injected group fetcher (test seam), mirroring [`Fetcher`] but keyed
/// by group slug and returning a multi-entry body.
pub type GroupFetcher = dyn Fn(&str) -> Result<String, TleCacheError> + Send + Sync;

/// Parse a multi-entry TLE body into `(name, line1, line2)` triples.
/// A name is the last non-blank line before a `1 …` line whose matching
/// `2 …` line immediately follows; malformed groups are skipped.
#[must_use]
pub fn parse_group_tles(text: &str) -> Vec<(String, String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < lines.len() {
        let l1 = lines[i].trim_end();
        let l2 = lines[i + 1].trim_end();
        if l1.starts_with("1 ") && l2.starts_with("2 ") {
            let name = (i > 0)
                .then(|| lines[i - 1].trim())
                .filter(|n| !n.is_empty())
                .unwrap_or("UNKNOWN")
                .to_string();
            out.push((name, l1.to_string(), l2.to_string()));
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}
```

Add a `group_fetcher: Option<Arc<GroupFetcher>>` field to `TleCache` (default `None`), a `with_group_fetcher` builder, a `group_cache_path(slug) -> PathBuf` (= `self.dir.join(format!("group-{slug}.tle"))`), and:

```rust
/// Cache-only read of a group's parsed TLEs. Never hits the network —
/// safe to call on the GTK thread. Errors if the group file is absent.
pub fn cached_group_tles(&self, slug: &str) -> Result<Vec<(String, String, String)>, TleCacheError> {
    let path = self.group_cache_path(slug);
    let text = std::fs::read_to_string(&path).map_err(|source| TleCacheError::Io { path, source })?;
    Ok(parse_group_tles(&text))
}

/// Forced network round trip: fetch the group body, write it atomically
/// to the group cache file, and return the parsed triples. Call
/// off-thread (blocking).
pub fn force_refresh_group(&self, slug: &str) -> Result<Vec<(String, String, String)>, TleCacheError> {
    let body = match &self.group_fetcher {
        Some(f) => f(slug)?,
        None => default_group_fetch(slug, self.fetch_timeout)?,
    };
    let path = self.group_cache_path(slug);
    self.write_cache_text(&path, &body)?; // reuse the atomic tempfile+rename writer
    Ok(parse_group_tles(&body))
}
```

Implement `default_group_fetch(slug, timeout)` as a blocking `reqwest` GET on `celestrak_group_url(slug)` returning `TleCacheError::Fetch(..)` on error (copy the body-fetch shape from the existing per-NORAD `fetch`). If `write_cache` is currently private and NORAD-specific, add a small `write_cache_text(&self, path: &Path, text: &str)` that does the same atomic tempfile+rename the existing writer uses (factor the existing writer's body into it if trivial; otherwise a parallel 6-line helper). Keep every new fn ≤ 50 NLOC.

Export from `lib.rs`: add `celestrak_group_url`, `ORBCOMM_TLE_GROUP`, `GroupFetcher`, `parse_group_tles` to the `tle_cache::{…}` re-export line.

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p sdr-sat tle_cache`
Expected: the 3 new tests PASS (plus existing).

- [ ] **Step 5: Codacy + cargo gates**

Run: `cargo build -p sdr-sat`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `uvx lizard crates/sdr-sat/src/tle_cache.rs -T nloc=50`  (no fn > 50; file < 500 — if the file is near 500 after additions, note it in the report; splitting `tle_cache.rs` is out of scope unless it crosses 500)
Run: `cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-sat/src/tle_cache.rs crates/sdr-sat/src/lib.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(sat): Orbcomm TLE group fetch/parse/cache

Add a group path to TleCache (GROUP=ORBCOMM&FORMAT=tle → one cached
group file, parsed to (name, line1, line2) triples) alongside the
per-NORAD fetcher: celestrak_group_url, GroupFetcher test seam,
parse_group_tles, cached_group_tles (GTK-thread-safe), force_refresh_group
(off-thread). Feeds spacecraft identification + the Orbcomm passes list.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 3: Pure telemetry-mapping helper (#898 #1) in `sdr-ui`

**Files:**
- Modify: `crates/sdr-ui/src/orbcomm_render.rs` (add `HeardFields` + `heard_fields`)
- Modify: `crates/sdr-ui/src/orbcomm_render/tests.rs` (add tests)

**Interfaces:**
- Consumes: `sdr_orbcomm::{OrbcommEvent, OrbcommEventKind}`, `sdr_orbcomm::packet::OrbcommPacket`.
- Produces (in `crate::orbcomm_render`):
  - `pub struct HeardFields { pub sat_id: u8, pub position: Option<(f64, f64, f64)>, pub vel_ms: Option<f64>, pub sat_time_unix: Option<i64> }`
  - `pub fn heard_fields(event: &sdr_orbcomm::OrbcommEvent) -> Option<HeardFields>`

- [ ] **Step 1: Write the failing tests**

Add to `crates/sdr-ui/src/orbcomm_render/tests.rs`:

```rust
#[test]
fn heard_fields_ephemeris_populates_all() {
    let ev = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet {
            packet: OrbcommPacket::Ephemeris(sample_ephemeris(51.2, 7.4)),
            repaired: false,
        },
    };
    let f = heard_fields(&ev).expect("ephemeris yields fields");
    assert_eq!(f.sat_id, 0x2C);
    assert_eq!(f.position, Some((51.2, 7.4, 715_000.0)));
    assert_eq!(f.vel_ms, Some(7_450.0));
    assert_eq!(f.sat_time_unix, Some(19 * 3600 + 42 * 60 + 11));
}

#[test]
fn heard_fields_sync_has_id_only() {
    let ev = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet {
            packet: OrbcommPacket::Sync { code: 0x656565, sat_id: 0x2C },
            repaired: false,
        },
    };
    let f = heard_fields(&ev).expect("sync yields id");
    assert_eq!(f.sat_id, 0x2C);
    assert_eq!(f.position, None);
    assert_eq!(f.vel_ms, None);
    assert_eq!(f.sat_time_unix, None);
}

#[test]
fn heard_fields_message_complete_is_none() {
    let ev = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::MessageComplete { bytes: vec![1, 2, 3], partial: false },
    };
    assert!(heard_fields(&ev).is_none());
}
```

(`sample_ephemeris` already exists in this test module from #897. If it lives in a `#[cfg(test)]` helper there, reuse it; otherwise define it as in `orbcomm_render/tests.rs`.)

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p sdr-ui --features whisper-cpu orbcomm_render`
Expected: FAIL to compile (`heard_fields`/`HeardFields` undefined).

- [ ] **Step 3: Implement**

Add to `crates/sdr-ui/src/orbcomm_render.rs`:

```rust
/// The per-spacecraft fields extracted from an identity-bearing event.
/// `Sync` gives only `sat_id`; `Ephemeris` fills position/velocity/time.
pub struct HeardFields {
    pub sat_id: u8,
    pub position: Option<(f64, f64, f64)>,
    pub vel_ms: Option<f64>,
    pub sat_time_unix: Option<i64>,
}

/// Map an event to its heard-spacecraft fields, or `None` for events
/// that carry no `sat_id` (`MessageComplete`). Pure — the single source
/// of the event→fields mapping used by the heard model and the matcher.
#[must_use]
pub fn heard_fields(event: &sdr_orbcomm::OrbcommEvent) -> Option<HeardFields> {
    use sdr_orbcomm::OrbcommEventKind;
    use sdr_orbcomm::packet::OrbcommPacket;
    match &event.kind {
        OrbcommEventKind::Packet { packet: OrbcommPacket::Sync { sat_id, .. }, .. } => {
            Some(HeardFields { sat_id: *sat_id, position: None, vel_ms: None, sat_time_unix: None })
        }
        OrbcommEventKind::Packet { packet: OrbcommPacket::Ephemeris(eph), .. } => Some(HeardFields {
            sat_id: eph.sat_id,
            position: Some((eph.lat_deg, eph.lon_deg, eph.alt_m)),
            vel_ms: Some(eph.vel_ms),
            sat_time_unix: Some(eph.sat_time_unix),
        }),
        _ => None,
    }
}
```

Ensure `orbcomm_render.rs`'s imports cover `OrbcommEvent` (already imported for `format_packet_row`).

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p sdr-ui --features whisper-cpu orbcomm_render`
Expected: PASS.

- [ ] **Step 5: Codacy + cargo gates**

Run: `cargo build -p sdr-ui --features whisper-cpu`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `uvx lizard crates/sdr-ui/src/orbcomm_render.rs -T nloc=50`
Run: `cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/orbcomm_render.rs crates/sdr-ui/src/orbcomm_render/tests.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): pure heard_fields telemetry mapping (#898)

Extract the event→(sat_id, position, velocity, time) mapping into a
pure, unit-tested helper in orbcomm_render — one source for the heard
model and the spacecraft matcher, and closes the coverage gap the DSP
handler previously carried.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 4: AppState fields + learned-table persistence + TLE loader

**Files:**
- Modify: `crates/sdr-ui/src/state.rs` (`orbcomm_sat_names`, `orbcomm_tles` fields + init)
- Create: `crates/sdr-ui/src/sidebar/orbcomm_persistence.rs` (load/save the learned table) + `crates/sdr-ui/src/sidebar/orbcomm_persistence/tests.rs`
- Modify: `crates/sdr-ui/src/sidebar/mod.rs` (`mod orbcomm_persistence;` + re-export)

**Interfaces:**
- Consumes: `sdr_config::ConfigManager` (`read`/`write` closures), `sdr_sat::Satellite`, `sdr_sat::TleCache`.
- Produces:
  - `AppState.orbcomm_sat_names: RefCell<std::collections::HashMap<u8, String>>`
  - `AppState.orbcomm_tles: RefCell<Vec<(String, sdr_sat::Satellite)>>`
  - `crate::sidebar::orbcomm_persistence::load_orbcomm_sat_names(&Arc<ConfigManager>) -> HashMap<u8, String>`
  - `crate::sidebar::orbcomm_persistence::save_orbcomm_sat_names(&Arc<ConfigManager>, &HashMap<u8, String>)`
  - `crate::sidebar::orbcomm_persistence::orbcomm_tles_from_cache(&TleCache) -> Vec<(String, sdr_sat::Satellite)>`

- [ ] **Step 1: Write the failing tests**

Create `crates/sdr-ui/src/sidebar/orbcomm_persistence/tests.rs`:

```rust
use super::*;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn sat_names_round_trip_through_config() {
    let cfg = Arc::new(sdr_config::ConfigManager::in_memory(&serde_json::json!({})));
    let mut table = HashMap::new();
    table.insert(0x2C_u8, "ORBCOMM FM06".to_string());
    table.insert(0x05_u8, "ORBCOMM FM04".to_string());
    save_orbcomm_sat_names(&cfg, &table);
    let loaded = load_orbcomm_sat_names(&cfg);
    assert_eq!(loaded, table);
}

#[test]
fn load_missing_key_is_empty() {
    let cfg = Arc::new(sdr_config::ConfigManager::in_memory(&serde_json::json!({})));
    assert!(load_orbcomm_sat_names(&cfg).is_empty());
}
```

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p sdr-ui --features whisper-cpu orbcomm_persistence`
Expected: FAIL (module undefined).

- [ ] **Step 3: Implement persistence + loader**

Create `crates/sdr-ui/src/sidebar/orbcomm_persistence.rs` (mirror `satellites_panel/persistence.rs` map pattern; keys stored as decimal strings since JSON object keys are strings):

```rust
//! Persistence for the learned Orbcomm `sat_id → name` table and the
//! parsed Orbcomm TLE candidate list. Keyed off a single JSON object
//! under `orbcomm_sat_names` (precedent: watched-satellites set).

use std::collections::HashMap;
use std::sync::Arc;

use sdr_config::ConfigManager;
use sdr_sat::{Satellite, TleCache};

const KEY_ORBCOMM_SAT_NAMES: &str = "orbcomm_sat_names";

/// Load the learned `sat_id → name` table (empty if absent/malformed).
#[must_use]
pub fn load_orbcomm_sat_names(config: &Arc<ConfigManager>) -> HashMap<u8, String> {
    config.read(|v| {
        v.get(KEY_ORBCOMM_SAT_NAMES)
            .and_then(serde_json::Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, val)| {
                        let id = k.parse::<u8>().ok()?;
                        let name = val.as_str()?.to_string();
                        Some((id, name))
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Persist the learned table as a JSON object keyed by decimal sat_id.
pub fn save_orbcomm_sat_names(config: &Arc<ConfigManager>, table: &HashMap<u8, String>) {
    let obj: serde_json::Map<String, serde_json::Value> = table
        .iter()
        .map(|(id, name)| (id.to_string(), serde_json::Value::String(name.clone())))
        .collect();
    config.write(|v| {
        v[KEY_ORBCOMM_SAT_NAMES] = serde_json::Value::Object(obj);
    });
}

/// Parse the cached Orbcomm group TLEs into propagatable `Satellite`s.
/// Skips entries whose elements fail to parse. Empty if the group cache
/// is absent (never fetches — callers refresh off-thread).
#[must_use]
pub fn orbcomm_tles_from_cache(cache: &TleCache) -> Vec<(String, Satellite)> {
    let Ok(triples) = cache.cached_group_tles(sdr_sat::ORBCOMM_TLE_GROUP) else {
        return Vec::new();
    };
    triples
        .into_iter()
        .filter_map(|(name, l1, l2)| Satellite::from_tle(&name, &l1, &l2).ok().map(|s| (name, s)))
        .collect()
}

#[cfg(test)]
mod tests;
```

Register the `{}` default: wherever the app's config defaults JSON is built (search for `KEY_WATCHED_SATELLITES` default registration or the defaults object passed to `ConfigManager::load`), add `"orbcomm_sat_names": {}`. If defaults are assembled per-key lazily (the watched-set uses `unwrap_or_default` on read, no explicit default), no defaults edit is needed — confirm by grepping for how `watched_satellites` registers its default and follow the same choice.

Add `orbcomm_sat_names: RefCell<HashMap<u8, String>>` and `orbcomm_tles: RefCell<Vec<(String, sdr_sat::Satellite)>>` to `AppState` (`state.rs`) with initializers `RefCell::new(HashMap::new())` / `RefCell::new(Vec::new())` (the real population happens in Task 5/7 wiring; a later step loads names from config at startup).

Add `mod orbcomm_persistence;` + `pub use orbcomm_persistence::{load_orbcomm_sat_names, orbcomm_tles_from_cache, save_orbcomm_sat_names};` to `crates/sdr-ui/src/sidebar/mod.rs`.

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p sdr-ui --features whisper-cpu orbcomm_persistence`
Expected: PASS.

- [ ] **Step 5: Codacy + cargo gates**

Run: `cargo build -p sdr-ui --features whisper-cpu`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `uvx lizard crates/sdr-ui/src/sidebar/orbcomm_persistence.rs crates/sdr-ui/src/state.rs -T nloc=50`
Run: `cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/state.rs crates/sdr-ui/src/sidebar/orbcomm_persistence.rs \
        crates/sdr-ui/src/sidebar/orbcomm_persistence/tests.rs crates/sdr-ui/src/sidebar/mod.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): persist learned Orbcomm sat_id→name table + TLE loader

AppState gains orbcomm_sat_names (learned, persisted under a JSON
object key like watched-satellites) and orbcomm_tles (parsed Satellite
candidates). orbcomm_persistence provides config load/save + a
cache→Satellite loader (cache-only, GTK-thread-safe).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 5: Wire identification + event-gating into the DSP handler

**Files:**
- Modify: `crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs`

**Interfaces:**
- Consumes: `crate::orbcomm_render::heard_fields` (Task 3), `sdr_sat::identify_spacecraft` + `DEFAULT_MATCH_MAX_DIST_KM` (Task 1), `AppState.orbcomm_sat_names`/`orbcomm_tles` (Task 4), the panel handles (`orbcomm_panel_handles`), `crate::sidebar::orbcomm_persistence::save_orbcomm_sat_names`.
- Produces: updated `on_orbcomm_event` (event-gated repaint/breakdown + identification hook). Adds `config: Arc<ConfigManager>` access — obtain it the way other handlers reach config (via `DspEventCtx` if present, else store an `Arc<ConfigManager>` on `AppState`; check how `save_*` is reached elsewhere in `dsp_events` and follow it — if config isn't reachable from `DspEventCtx`, add an `Arc<ConfigManager>` field to `AppState` set at construction and use `state.config`).

- [ ] **Step 1: Rewrite `on_orbcomm_event` (event-gated + identification)**

Replace the body so it uses `heard_fields`, only repaints heard on identity events, only refreshes the breakdown on `Packet` events, and attempts identification for unknown ephemeris `sat_id`s:

```rust
pub(super) fn on_orbcomm_event(ctx: &DspEventCtx, event: &sdr_orbcomm::OrbcommEvent) {
    let DspEventCtx { state, .. } = ctx;
    let is_packet = matches!(event.kind, sdr_orbcomm::OrbcommEventKind::Packet { .. });
    if is_packet {
        state.orbcomm_tally.borrow_mut().record(event);
    }

    let fields = crate::orbcomm_render::heard_fields(event);
    if let Some(f) = &fields {
        state.orbcomm_heard.borrow_mut().record(
            f.sat_id, f.position, f.vel_ms, f.sat_time_unix, std::time::Instant::now(),
        );
        maybe_identify(state, f);
    }

    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.append_log_entry(&crate::orbcomm_render::format_packet_row(event));
        if is_packet {
            refresh_breakdown(handles, state);
        }
        if fields.is_some() {
            crate::sidebar::orbcomm_panel::repaint_heard(handles, state);
        }
    }
}

/// Try to resolve an unknown ephemeris `sat_id` to a real name via
/// ephemeris↔TLE matching; on success, learn + persist it.
fn maybe_identify(state: &std::rc::Rc<AppState>, f: &crate::orbcomm_render::HeardFields) {
    let (Some((lat, lon, alt)), Some(t)) = (f.position, f.sat_time_unix) else { return };
    if state.orbcomm_sat_names.borrow().contains_key(&f.sat_id) {
        return;
    }
    let tles = state.orbcomm_tles.borrow();
    if tles.is_empty() {
        return;
    }
    let Some(when) = chrono::DateTime::from_timestamp(t, 0) else { return };
    if let Some(m) = sdr_sat::identify_spacecraft(
        lat, lon, alt, when, &tles, sdr_sat::DEFAULT_MATCH_MAX_DIST_KM,
    ) {
        drop(tles);
        state.orbcomm_sat_names.borrow_mut().insert(f.sat_id, m.name.clone());
        crate::sidebar::orbcomm_persistence::save_orbcomm_sat_names(
            &state.config, &state.orbcomm_sat_names.borrow(),
        );
        tracing::info!("Orbcomm: identified Sat {:#04X} as {} ({:.0} km)", f.sat_id, m.name, m.distance_km);
    }
}
```

(If `state.config` does not exist, add `pub config: Arc<sdr_config::ConfigManager>` to `AppState` and populate it at construction — the panel/persistence need it. Reach it however the satellites panel already does; do not introduce a second config source.)

Keep each fn ≤ 50 NLOC (split `maybe_identify` further only if it grows).

- [ ] **Step 2: Build + tests + Codacy gates**

Run: `cargo build -p sdr-ui --features whisper-cpu`
Run: `cargo test -p sdr-ui --features whisper-cpu orbcomm`  (existing orbcomm tests still pass)
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `uvx lizard crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs -T nloc=50`
Run: `cargo fmt --all -- --check`

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs crates/sdr-ui/src/state.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): identify Orbcomm spacecraft from ephemeris; gate panel refreshes

on_orbcomm_event now routes through the pure heard_fields helper, only
repaints the heard list on identity events and refreshes the breakdown
on Packet events (#898), and attempts ephemeris↔TLE identification for
unknown sat_ids — learning + persisting sat_id→name on a confident match.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 6: By-Spacecraft name resolution + incremental log buffer (#898 #3)

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/orbcomm_panel.rs`
- Modify: `crates/sdr-ui/src/sidebar/orbcomm_panel/tests.rs` (add the log-rotation test; create the test module if absent)

**Interfaces:**
- Consumes: `AppState.orbcomm_sat_names`, `HeardRow` (has `sat_id`? — see note), `MAX_LOG_ENTRIES`.
- Produces: name-resolving By-Spacecraft rows; `append_log_entry` using incremental buffer edits.

Note: `HeardRow` currently carries `label` (already `sat_label(sat_id)`) but not the raw `sat_id`. To resolve a learned name, `repaint_heard` needs the `sat_id`. Add `pub sat_id: u8` to `HeardRow` (and populate it in `HeardSatellites::rows`) so the panel can look it up in `orbcomm_sat_names`. This is a 2-line model change with an obvious test (extend an existing `satellites_heard` test to assert `rows()[0].sat_id`).

- [ ] **Step 1: Add `sat_id` to `HeardRow`; resolve names in `repaint_heard`**

In `satellites_heard.rs`: add `pub sat_id: u8` to `HeardRow`, populate from the map key in `rows`. Extend one existing heard test to assert `sat_id`.

In `orbcomm_panel.rs::repaint_heard` (and/or `rebuild_heard_list`): when building each `AdwActionRow`, resolve the title through the learned table:

```rust
let title = state
    .orbcomm_sat_names
    .borrow()
    .get(&row.sat_id)
    .cloned()
    .unwrap_or_else(|| row.label.clone());
// subtitle keeps the existing position/vel/time line; when a real name
// is shown, prepend the raw `row.label` (Sat 0xNN) to the subtitle.
```

Keep the pure `orbcomm_render` log formatters unchanged (they stay `Sat 0xNN`).

- [ ] **Step 2: Incremental log buffer with a rotation test**

Write the failing test first in `orbcomm_panel/tests.rs` for a pure rotation helper. Extract the ring-management from `append_log_entry` into a pure fn so it is testable without GTK:

```rust
#[test]
fn log_ring_caps_at_max_entries() {
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for i in 0..(MAX_LOG_ENTRIES + 10) {
        push_log_ring(&mut ring, format!("line {i}"));
    }
    assert_eq!(ring.len(), MAX_LOG_ENTRIES);
    assert_eq!(ring.front().unwrap(), &format!("line {}", 10)); // oldest 10 dropped
}
```

Implement `fn push_log_ring(ring: &mut VecDeque<String>, entry: String)` (push_back + pop_front while `len > MAX_LOG_ENTRIES`), and have `append_log_entry` call it, then apply the delta to the `GtkTextBuffer` incrementally: `insert` the new entry at the end iter and, when an entry was evicted, `delete` from the buffer start through the first newline. Preserve the existing auto-scroll-to-bottom behavior. Keep `append_log_entry` ≤ 50 NLOC (the ring logic now lives in `push_log_ring`).

- [ ] **Step 3: Run tests + gates**

Run: `cargo test -p sdr-ui --features whisper-cpu satellites_heard`  (sat_id assertion)
Run: `cargo test -p sdr-ui --features whisper-cpu orbcomm_panel`  (log ring)
Run: `cargo build -p sdr-ui --features whisper-cpu`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `uvx lizard crates/sdr-ui/src/sidebar/orbcomm_panel.rs crates/sdr-ui/src/sidebar/satellites_heard.rs -T nloc=50`
Run: `cargo fmt --all -- --check`

- [ ] **Step 4: Smoke test (user)** — names appear in By-Spacecraft after a pass; log scrolls without flicker; still bounded.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/sidebar/orbcomm_panel.rs crates/sdr-ui/src/sidebar/orbcomm_panel/tests.rs \
        crates/sdr-ui/src/sidebar/satellites_heard.rs crates/sdr-ui/src/sidebar/satellites_heard/tests.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): show learned spacecraft names in By-Spacecraft; incremental log

By-Spacecraft rows resolve their title through the learned sat_id→name
table (falling back to Sat 0xNN). The packet log now edits the
GtkTextBuffer incrementally with a pure, tested push_log_ring rotation
instead of rebuilding the whole buffer per entry (#898).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 7: "Next Orbcomm passes" panel section + background TLE refresh

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/orbcomm_panel.rs` (new passes `AdwPreferencesGroup` + handle + a pure `sort_passes` helper)
- Modify: `crates/sdr-ui/src/sidebar/orbcomm_panel/tests.rs` (pure pass-formatting/sort test)
- Modify: the Orbcomm panel connect / TLE-refresh wiring (`orbcomm_panel.rs::connect_orbcomm_panel` and, if reused, `window/satellites/passes.rs`)

**Interfaces:**
- Consumes: `sdr_sat::{upcoming_passes, GroundStation, Pass, Satellite, ORBCOMM_TLE_GROUP}`, `AppState.orbcomm_tles`, `crate::sidebar::orbcomm_persistence::orbcomm_tles_from_cache`, the config `sat_station_*` keys.
- Produces: a passes group in the panel, populated on decode-enable / TLE-refresh / a 60 s timer; a background `gio::spawn_blocking(force_refresh_group)` that fills `orbcomm_tles`.

- [ ] **Step 1: Pure pass-collection helper + test**

Write the failing test first: a helper `collect_orbcomm_passes(station, &[(String, Satellite)], from, hours, min_el) -> Vec<Pass>` that loops candidates through `upcoming_passes`, collects, sorts by `start`, truncates to N. Test it with the two TLE fixtures + a known station over an 8 h window → returns a non-empty, start-sorted `Vec<Pass>` (assert monotonic `start`). Mirror `enumerate_upcoming_passes` (`satellites_panel/passes.rs:215`) but over the passed candidate list rather than `KNOWN_SATELLITES`.

```rust
#[test]
fn collect_orbcomm_passes_sorted_by_start() {
    let station = sdr_sat::GroundStation::new(37.1353, -80.4188, 660.0);
    let cands = vec![
        ("ORBCOMM FM06".to_string(), sat(FM06)),
        ("ORBCOMM FM04".to_string(), sat(FM04)),
    ];
    let from = chrono::Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let passes = collect_orbcomm_passes(&station, &cands, from, 8, 10.0);
    for w in passes.windows(2) {
        assert!(w[0].start <= w[1].start);
    }
}
```

Implement `collect_orbcomm_passes` (≤ 50 NLOC); ignore per-satellite `Err(TleExpired)`/`Propagation` (skip that candidate). Cap the returned count with a named `const MAX_ORBCOMM_PASSES: usize = 12;`.

- [ ] **Step 2: Passes group in the panel**

Add a `passes_group: adw::PreferencesGroup` + `passes_rows: RefCell<Vec<adw::ActionRow>>` to `OrbcommPanelHandles`; build the group in a new `build_passes_group()` sub-builder (title "Next Orbcomm passes"), appended below the By-Spacecraft group in `build_orbcomm_panel` (keep `build_orbcomm_panel` ≤ 50 NLOC — it already delegates to sub-builders). Add a `refresh_passes(&self, passes: &[Pass], heard: &HashMap<u8,String>)` method rendering `name · HH:MM–HH:MM · NN°` rows (local time; reuse the `chrono::Local` formatting from the existing satellites passes rows). A bird whose name is in the learned table gets a "heard" marker (e.g. a `📡`/`•` prefix or a suffix subtitle).

- [ ] **Step 3: Wire refresh (GTK + off-thread fetch)**

In `connect_orbcomm_panel`: read the `GroundStation` from config (`sat_station_lat_deg/lon_deg/alt_m`); on connect and on a 60 s `glib::timeout_add_seconds_local`, rebuild the passes group from `state.orbcomm_tles` via `collect_orbcomm_passes`. On decode-enable (in `on_orbcomm_enabled_changed`, enabled path) and via the existing satellites TLE-refresh button, kick a background refresh:

```rust
// off the GTK thread; never blocks the UI
let cache = Arc::clone(&tle_cache);
gio::spawn_blocking(move || cache.force_refresh_group(sdr_sat::ORBCOMM_TLE_GROUP))
    .await // inside a glib::spawn_future_local
    // on Ok: parse via orbcomm_tles_from_cache, store into state.orbcomm_tles, refresh_passes
```

Reuse the exact `gio::spawn_blocking` + `glib::spawn_future_local` shape from `window/satellites/passes.rs:108`. If the panel doesn't already hold an `Arc<TleCache>`, thread it in from the same place the satellites panel gets it (`window/satellites.rs:254`) — do not construct a second cache. On startup, seed `state.orbcomm_tles` from `orbcomm_tles_from_cache` (cache-only, no network) and load `orbcomm_sat_names` from config.

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p sdr-ui --features whisper-cpu orbcomm_panel`
Run: `cargo build -p sdr-ui --features whisper-cpu`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `uvx lizard crates/sdr-ui/src/sidebar/orbcomm_panel.rs -T nloc=50`  (file may be the largest changed — confirm < 500; if it crosses, split the passes UI into `orbcomm_panel/passes.rs` as part of this task)
Run: `cargo fmt --all -- --check`

- [ ] **Step 5: Smoke test (user)** — passes section lists upcoming Orbcomm passes (compare against the known FM06/FM108 windows); heard birds marked; refresh button updates it.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/sidebar/orbcomm_panel.rs crates/sdr-ui/src/sidebar/orbcomm_panel/tests.rs \
        crates/sdr-ui/src/state.rs crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs
git status --short
git commit -m "$(cat <<'EOF'
feat(ui): Next Orbcomm passes panel section + background TLE group refresh

A "Next Orbcomm passes" group lists upcoming passes (collect_orbcomm_passes
over the cached Orbcomm TLEs via upcoming_passes, start-sorted), refreshed
on enable / TLE-refresh / a 60s timer, with heard birds marked. TLE group
refresh runs off the GTK thread; the UI only reads the cache.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 8: Docs

**Files:**
- Modify: `/data/source/rtl-sdr/CLAUDE.md` (Orbcomm bullet: names + passes)
- Modify: `crates/sdr-ui/src/sidebar/orbcomm_panel.rs` module doc if the layout gained a section

- [ ] **Step 1: Update the CLAUDE.md Orbcomm bullet** to note spacecraft identification (ephemeris↔TLE, `sdr-sat::identify`), the learned/persisted `sat_id→name` table, and the "Next Orbcomm passes" section. Keep it one bullet; no duplicate headings (MD024). Fenced blocks (if any) get a language (MD040).

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md crates/sdr-ui/src/sidebar/orbcomm_panel.rs
git status --short
git commit -m "$(cat <<'EOF'
docs: note Orbcomm spacecraft identification + passes in CLAUDE.md

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v
EOF
)"
```

---

### Task 9: Full gates, live calibration, push, PR, review

- [ ] **Step 1: Full workspace gates** (separate bare commands, fmt last)

Run: `cargo test --workspace`
Run: `cargo clippy --all-targets --workspace -- -D warnings`
Run: `cargo check --workspace --locked`
Run: `uvx lizard crates/sdr-sat/src/identify.rs crates/sdr-sat/src/tle_cache.rs crates/sdr-ui/src/orbcomm_render.rs crates/sdr-ui/src/sidebar/orbcomm_panel.rs crates/sdr-ui/src/sidebar/orbcomm_persistence.rs crates/sdr-ui/src/window/dsp_events/orbcomm_events.rs -T nloc=50` (no fn > 50, no file > 500)
Run: `make lint`
Run: `cargo fmt --all -- --check`

- [ ] **Step 2: Live-calibration gate (user + Claude)** — during a real Orbcomm pass (e.g. an FM06/FM108 overhead), confirm the By-Spacecraft list resolves the right bird(s) and the passes section matches reality. If matches are systematically ~100+ km off, re-check `GPS_UTC_LEAP_SECONDS`; tune `DEFAULT_MATCH_MAX_DIST_KM` from the observed distances. Record the calibration in the commit/PR.

- [ ] **Step 3: Push + PR**

```bash
git push -u origin feature/orbcomm-identification-passes
gh pr create --title "Orbcomm spacecraft identification + pass prediction (#866) + panel polish (#898)" --body "<summary: ECEF ephemeris↔TLE matcher (leap-corrected), Orbcomm TLE group fetch, learned+persisted sat_id→name names in By-Spacecraft, Next-Orbcomm-passes section, and the three #898 polish items. sdr-orbcomm untouched. Live-calibrated against a real pass. Design + plan doc paths. Closes #866, #898.>

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01J9GuV8KED73BuajBFznQ2v"
```

- [ ] **Step 4: CodeRabbit + Codacy** — wait for CodeRabbit's posted review for the pushed SHA; batch fixes, reply to each, re-run gates (fmt last), push once. Then `codacy pull-request gh jasonherald rtl-sdr <PR>` — confirm no new issues (function-length / file-length / params / coverage) and resolve any conversations. Repeat until both are clean.

---

## Self-Review

**Spec coverage:**
- §1 pure matcher (ECEF, leap seconds, threshold+margin) → Task 1 ✓
- §2 TLE group fetch/parse/cache → Task 2 ✓
- §3 learned table + persistence + stop-rematching → Task 4 (persist) + Task 5 (learn/stop) ✓
- §4 names in By-Spacecraft → Task 6 ✓; passes section → Task 7 ✓; matching hook → Task 5 ✓
- §4 #898: pure telemetry helper → Task 3; event-gating → Task 5; incremental log → Task 6 ✓
- Data-flow (off-thread refresh, cache-only reads) → Task 7 ✓
- Testing (matcher/helper/parser/round-trip/log-rotation) → Tasks 1,2,3,4,6 ✓; live gate → Task 9 ✓
- Non-goal `sdr-orbcomm` untouched → no task edits it ✓

**Placeholder scan:** No "TBD"/"handle edge cases"/"similar to Task N". Two deliberate "confirm how X is reached" notes (config access in Task 5; default registration in Task 4) are explicit branch-instructions with a named fallback, not placeholders.

**Type consistency:** `identify_spacecraft(lat,lon,alt,when,&[(String,Satellite)],max)` — Task 1 def, Task 5 call ✓. `heard_fields → Option<HeardFields{sat_id,position,vel_ms,sat_time_unix}>` — Task 3 def, Task 5 use ✓. `parse_group_tles/cached_group_tles/force_refresh_group → Vec<(String,String,String)>` — Task 2 def, Task 4 (`orbcomm_tles_from_cache`) use ✓. `HeardRow.sat_id` — Task 6 adds, Task 6 uses ✓. `push_log_ring`, `collect_orbcomm_passes`, `MAX_ORBCOMM_PASSES` — defined and used within Tasks 6/7 ✓. `save_orbcomm_sat_names(&Arc<ConfigManager>, &HashMap<u8,String>)` — Task 4 def, Task 5 call ✓.

**Codacy discipline:** every code task carries a `uvx lizard … -T nloc=50` step; new files are small and focused (`identify.rs`, `orbcomm_persistence.rs`); `orbcomm_panel.rs` is the file most at risk of the 500-NLOC limit — Task 7 Step 4 instructs splitting the passes UI into `orbcomm_panel/passes.rs` if it crosses. All new pure logic is unit-tested to hold diff coverage.
