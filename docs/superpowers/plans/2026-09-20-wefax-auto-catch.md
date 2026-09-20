# WEFAX Auto-Catch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a WEFAX "auto-catch" activity that scans receivable HF fax channels, detects a real fax signal, and decodes a full chart unattended — no hand-tuning, no static images.

**Architecture:** A pure `WefaxCatcher` state machine (mirroring `satellites_recorder.rs`) drives tune/mode via `Action`s interpreted in the UI layer. A new `sdr-dsp` fax-presence detector (Goertzel subcarrier-band energy ratio) tells the catcher when a channel actually carries fax; a built-in geo/schedule-ranked station catalog in `sdr-sat` tells it what to scan. The existing gated `WefaxDecoder` is unchanged — it already refuses to draw until a real chart's phasing lock.

**Tech Stack:** Rust workspace, GTK4/libadwaita (sdr-ui), `sdr-dsp` (Goertzel), `sdr-sat` (catalog/geo), `sdr-core` (controller/messages), `chrono` (UTC schedule math).

**Spec:** `docs/superpowers/specs/2026-09-20-wefax-auto-catch-design.md`

## Global Constraints

- **Depends on #911 (merged).** Parent enhancement #913. Deferred items live in **#914** — keep them strictly OUT of scope (no per-product schedules, no FM-sweep detector, no custom stations, no partial-chart toggle, no day/night hint, no panel thumbnail/recent-list/scan-now button).
- **Codacy quality gates (enforced — build to these, not to review):** every function/method ≤ **50 NLOC** (target local ≤ **~44**, because Codacy counts ~4–7 stricter than local `uvx lizard`); every file ≤ **500 NLOC**; function parameter count ≤ **8**. Carve helpers proactively rather than hitting the limit in review.
- **Diff coverage stays green:** every new PURE function/branch is unit-tested. The pure pieces (detector, catalog/geo/schedule, catcher state machine) are the bulk of the logic and are TDD'd with no GTK.
- **Docs pass markdownlint:** fenced code blocks declare a language; no duplicate headings anywhere (MD024 `siblings_only:false`).
- **No `unwrap()`/`panic!()`/`println!()` in library crates;** `thiserror` for errors (never `anyhow` outside `src/main.rs`); `tracing` macros for logging; prefer `&str` params; named constants for magic numbers; tests inline at file bottom in `#[cfg(test)] mod tests`.
- **Never `git add -A`/`git add .`** — the repo carries untracked `build/` + `.vscode/`; stage explicit paths only, and verify `git status --short` still shows those as `??` before committing.
- **Gate order before every push (run as SEPARATE bare Bash commands, not chained):**
  1. `cargo test -p <changed crate>` (sdr-ui needs `--features whisper-cpu` due to the transcription feature mutex)
  2. `cargo clippy --all-targets --workspace -- -D warnings` (NO extra features — matches CI exactly)
  3. `uvx lizard <changed .rs files> -T nloc=50` (must report "No thresholds exceeded")
  4. `cargo check --locked` **only if** any `Cargo.toml` changed
  5. `cargo fmt --all -- --check` — **LAST**, immediately before `git push`; any post-check edit re-runs it
- **GTK panel wiring is smoke-tested by the USER** (`make install CARGO_FLAGS="--release --no-default-features --features sherpa-cuda"` + a checklist). Claude never launches the binary.
- **After each push, WAIT for CodeRabbit**; address every CR AND Codacy inline thread (reply + resolve), then re-review before the next push.

## File Structure

New files:
- `crates/sdr-dsp/src/wefax/presence.rs` — `WefaxPresenceDetector` (Goertzel band-energy ratio + hysteresis). Pure.
- `crates/sdr-sat/src/wefax_stations.rs` — `WefaxStation`, `DailyWindow`, `KNOWN_WEFAX_STATIONS`, geo/schedule pure helpers.
- `crates/sdr-ui/src/sidebar/wefax_catcher.rs` — `WefaxCatcher` pure state machine (`State`, `Action`, `tick`).
- `crates/sdr-ui/src/sidebar/wefax_panel.rs` — the activity panel widget + `connect_wefax_panel`.
- `crates/sdr-ui/src/window/wefax/mod.rs` + `crates/sdr-ui/src/window/wefax/catcher.rs` — `interpret_wefax_action` + glib-timer driver.

Modified files:
- `crates/sdr-dsp/src/wefax.rs` — `mod presence; pub use presence::WefaxPresenceDetector;`
- `crates/sdr-sat/src/lib.rs` — `mod wefax_stations; pub use wefax_stations::{...};`
- `crates/sdr-core/src/messages.rs` — add `DspToUi::WefaxPresence(bool)`.
- `crates/sdr-core/src/controller/wefax.rs` — instantiate the detector, emit `WefaxPresence` edge-triggered.
- `crates/sdr-ui/src/sidebar/activity_bar.rs` — new `LEFT_ACTIVITIES` entry + persistence keys.
- `crates/sdr-ui/src/sidebar/mod.rs` — `WefaxPanel` field + `build_panels`.
- `crates/sdr-ui/src/window/layout.rs` — `left_stack.add_named(&panels.wefax.widget, Some("wefax"))`.
- `crates/sdr-ui/src/window.rs` — call `connect_wefax_panel`.
- `crates/sdr-ui/src/state.rs` — `AppState` fields for catcher + panel handles.

---

### Task 1: Fax-presence detector (`sdr-dsp`)

**Files:**
- Create: `crates/sdr-dsp/src/wefax/presence.rs`
- Modify: `crates/sdr-dsp/src/wefax.rs` (add `mod presence;` + re-export)
- Test: inline `#[cfg(test)] mod tests` in `presence.rs`

**Interfaces:**
- Consumes: nothing (leaf).
- Produces: `WefaxPresenceDetector::new(sample_rate_hz: f64) -> Self`; `update(&mut self, samples: &[f32]) -> bool` (returns current hysteresis-gated presence after consuming the block); `is_present(&self) -> bool`; `reset(&mut self)`. Band constants reuse `super::{SUBCARRIER_BLACK_HZ, SUBCARRIER_WHITE_HZ}`.

Design: a small internal `Goertzel` accumulator (mirror the math in `wefax/tones.rs:43-66`) run for **in-band** bins (1500/1700/1900/2100/2300 Hz) and **out-of-band** bins (700/3200 Hz) over fixed `BLOCK_LEN` blocks. Per block: `ratio = in_power / (in_power + out_power)`. Hysteresis: `present` flips true after `ON_BLOCKS` consecutive blocks with `ratio ≥ PRESENT_RATIO`, flips false after `OFF_BLOCKS` consecutive blocks below it.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    const SR: f64 = 12_000.0;

    // Push `secs` of a signal produced by `f(sample_index) -> f32`.
    fn drive(det: &mut WefaxPresenceDetector, secs: f64, mut f: impl FnMut(usize) -> f32) -> bool {
        let n = (secs * SR) as usize;
        let mut present = false;
        let chunk: Vec<f32> = (0..n).map(|i| f(i)).collect();
        for block in chunk.chunks(256) {
            present = det.update(block);
        }
        present
    }

    #[test]
    fn fax_subcarrier_sweep_is_present() {
        // FM sweep between black(1500) and white(2300), the fax subcarrier.
        let mut det = WefaxPresenceDetector::new(SR);
        let present = drive(&mut det, 3.0, |i| {
            let t = i as f32 / SR as f32;
            let inst = 1900.0 + 400.0 * (2.0 * PI * 2.0 * t).sin(); // sweeps 1500..2300
            (2.0 * PI * inst * t).sin()
        });
        assert!(present, "a fax-band FM subcarrier must read as present");
    }

    #[test]
    fn white_noise_is_absent() {
        let mut det = WefaxPresenceDetector::new(SR);
        let mut seed = 0x1234_5678u32;
        let present = drive(&mut det, 3.0, |_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        });
        assert!(!present, "broadband noise must not read as present");
    }

    #[test]
    fn off_band_tone_is_absent() {
        let mut det = WefaxPresenceDetector::new(SR);
        let present = drive(&mut det, 3.0, |i| {
            let t = i as f32 / SR as f32;
            (2.0 * PI * 3_500.0 * t).sin() // above the fax band
        });
        assert!(!present, "an out-of-band tone must not read as present");
    }

    #[test]
    fn brief_blip_does_not_trigger_but_sustained_does() {
        let mut det = WefaxPresenceDetector::new(SR);
        // 0.1 s of fax then silence should NOT latch present (hysteresis).
        let blip = drive(&mut det, 0.1, |i| {
            let t = i as f32 / SR as f32;
            (2.0 * PI * 1_900.0 * t).sin()
        });
        assert!(!blip, "a brief blip must not latch present");
    }
}
```

- [ ] **Step 2: Run tests, confirm they fail** — `cargo test -p sdr-dsp --lib wefax::presence` → FAIL ("cannot find type `WefaxPresenceDetector`").

- [ ] **Step 3: Implement `presence.rs`**

```rust
//! Fax-subcarrier presence detector: is a WEFAX signal on this channel?
//!
//! A Goertzel band-energy ratio over the 1500-2300 Hz subcarrier band vs
//! out-of-band bins, with hysteresis. Used to pick a channel (auto-catch)
//! and to keep static from ever starting an image. Pure; no I/O.

use super::{SUBCARRIER_BLACK_HZ, SUBCARRIER_CENTER_HZ, SUBCARRIER_WHITE_HZ};

/// Samples per Goertzel block (matches `tones.rs`).
const BLOCK_LEN: usize = 1024;
/// In-band/total power fraction above which a block counts as "fax".
const PRESENT_RATIO: f64 = 0.55;
/// Consecutive on-blocks to latch present (~1-2 s at 12 kHz).
const ON_BLOCKS: u32 = 12;
/// Consecutive off-blocks to drop present.
const OFF_BLOCKS: u32 = 16;
/// In-band probe frequencies spanning the subcarrier.
fn in_band_hz() -> [f64; 5] {
    [
        SUBCARRIER_BLACK_HZ,
        (SUBCARRIER_BLACK_HZ + SUBCARRIER_CENTER_HZ) / 2.0,
        SUBCARRIER_CENTER_HZ,
        (SUBCARRIER_CENTER_HZ + SUBCARRIER_WHITE_HZ) / 2.0,
        SUBCARRIER_WHITE_HZ,
    ]
}
/// Out-of-band probes: below and above the fax band.
const OUT_BAND_HZ: [f64; 2] = [700.0, 3_200.0];

/// One Goertzel single-bin accumulator over a `BLOCK_LEN` block.
struct Goertzel {
    coeff: f64,
    q1: f64,
    q2: f64,
}

impl Goertzel {
    fn new(sample_rate_hz: f64, target_hz: f64) -> Self {
        let k = (target_hz / sample_rate_hz * BLOCK_LEN as f64).round();
        let w = 2.0 * std::f64::consts::PI * k / BLOCK_LEN as f64;
        Self { coeff: 2.0 * w.cos(), q1: 0.0, q2: 0.0 }
    }
    fn push(&mut self, s: f64) {
        let q0 = self.coeff * self.q1 - self.q2 + s;
        self.q2 = self.q1;
        self.q1 = q0;
    }
    /// Bin power, then reset for the next block.
    fn take_power(&mut self) -> f64 {
        let p = self.q1 * self.q1 + self.q2 * self.q2 - self.coeff * self.q1 * self.q2;
        self.q1 = 0.0;
        self.q2 = 0.0;
        p.max(0.0)
    }
}

/// Hysteresis-gated fax-subcarrier presence detector.
pub struct WefaxPresenceDetector {
    in_band: Vec<Goertzel>,
    out_band: Vec<Goertzel>,
    n: usize,
    on_streak: u32,
    off_streak: u32,
    present: bool,
}

impl WefaxPresenceDetector {
    /// Build a detector for the given audio sample rate.
    #[must_use]
    pub fn new(sample_rate_hz: f64) -> Self {
        Self {
            in_band: in_band_hz().iter().map(|&f| Goertzel::new(sample_rate_hz, f)).collect(),
            out_band: OUT_BAND_HZ.iter().map(|&f| Goertzel::new(sample_rate_hz, f)).collect(),
            n: 0,
            on_streak: 0,
            off_streak: 0,
            present: false,
        }
    }

    /// Feed audio samples; returns the current hysteresis-gated presence.
    pub fn update(&mut self, samples: &[f32]) -> bool {
        for &s in samples {
            let s = f64::from(s);
            for g in &mut self.in_band {
                g.push(s);
            }
            for g in &mut self.out_band {
                g.push(s);
            }
            self.n += 1;
            if self.n >= BLOCK_LEN {
                self.evaluate_block();
                self.n = 0;
            }
        }
        self.present
    }

    /// Current gated presence without feeding samples.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.present
    }

    /// Clear all state (call when retuning to a new channel).
    pub fn reset(&mut self) {
        for g in self.in_band.iter_mut().chain(self.out_band.iter_mut()) {
            let _ = g.take_power();
        }
        self.n = 0;
        self.on_streak = 0;
        self.off_streak = 0;
        self.present = false;
    }

    fn evaluate_block(&mut self) {
        let inb: f64 = self.in_band.iter_mut().map(Goertzel::take_power).sum();
        let outb: f64 = self.out_band.iter_mut().map(Goertzel::take_power).sum();
        let total = inb + outb;
        let ratio = if total > 1e-12 { inb / total } else { 0.0 };
        if ratio >= PRESENT_RATIO {
            self.on_streak += 1;
            self.off_streak = 0;
            if self.on_streak >= ON_BLOCKS {
                self.present = true;
            }
        } else {
            self.off_streak += 1;
            self.on_streak = 0;
            if self.off_streak >= OFF_BLOCKS {
                self.present = false;
            }
        }
    }
}
```

- [ ] **Step 4: Wire the module** — in `crates/sdr-dsp/src/wefax.rs` add `mod presence;` and `pub use presence::WefaxPresenceDetector;` next to the existing submodule declarations. Confirm `SUBCARRIER_BLACK_HZ/CENTER_HZ/WHITE_HZ` are in scope for `presence.rs` (they're defined in `wefax.rs`; `use super::...`).

- [ ] **Step 5: Run tests, confirm pass** — `cargo test -p sdr-dsp --lib wefax::presence` → all 4 pass. If `white_noise_is_absent` flakes, raise `PRESENT_RATIO`; if `fax_subcarrier_sweep_is_present` fails, lower `ON_BLOCKS`. Tune constants, keep tests green.

- [ ] **Step 6: Real-data gate (decision + fixture)**

Decision to make and record in the commit message: **commit a short real fixture or not.** Recommended: yes — a ~12 s mono f32 WAV of the recovered NMF fax audio (present) and a ~12 s static clip (absent), both at 12 kHz, under `crates/sdr-dsp/tests/data/` (public-domain US-Gov NMF signal, note provenance in `tests/data/README.md`, kept out of `cargo package` via the existing `exclude`). Add an integration test `crates/sdr-dsp/tests/wefax_presence.rs`:

```rust
//! Real-data gate for the fax-presence detector (public-domain NMF audio).
use sdr_dsp::wefax::WefaxPresenceDetector;

fn read_mono_f32(path: &str) -> (f64, Vec<f32>) {
    let mut r = hound::WavReader::open(path).expect("open");
    let sr = f64::from(r.spec().sample_rate);
    let s = r.samples::<f32>().map(|x| x.expect("s")).collect();
    (sr, s)
}

#[test]
fn nmf_fax_audio_reads_present() {
    let (sr, s) = read_mono_f32("tests/data/wefax_nmf_present_12k.wav");
    let mut det = WefaxPresenceDetector::new(sr);
    for b in s.chunks(1024) {
        det.update(b);
    }
    assert!(det.is_present(), "real NMF fax audio must read present");
}

#[test]
fn static_reads_absent() {
    let (sr, s) = read_mono_f32("tests/data/wefax_static_12k.wav");
    let mut det = WefaxPresenceDetector::new(sr);
    for b in s.chunks(1024) {
        det.update(b);
    }
    assert!(!det.is_present(), "static must read absent");
}
```

To produce the fixtures: trim the recovered `faxFIX.wav` (24 kHz) to ~12 s and resample to 12 kHz mono (`sox faxFIX.wav -r 12000 -c 1 tests/data/wefax_nmf_present_12k.wav trim 30 12`); make the static clip from a dead-channel segment of the original IQ demod, or a noise clip. If committing fixtures is undesirable, mark both tests `#[ignore]` with a comment pointing at the scratchpad audio — but prefer committing, per the real-data-gate rule.

- [ ] **Step 7: Gates** — run each separately: `cargo test -p sdr-dsp`; `cargo clippy --all-targets --workspace -- -D warnings`; `uvx lizard crates/sdr-dsp/src/wefax/presence.rs -T nloc=50`; then `cargo fmt --all -- --check`.

- [ ] **Step 8: Commit**

```bash
git add crates/sdr-dsp/src/wefax/presence.rs crates/sdr-dsp/src/wefax.rs crates/sdr-dsp/tests/wefax_presence.rs crates/sdr-dsp/tests/data/wefax_nmf_present_12k.wav crates/sdr-dsp/tests/data/wefax_static_12k.wav crates/sdr-dsp/tests/data/README.md
git commit -m "feat(dsp): WEFAX fax-subcarrier presence detector (#913)"
```

---

### Task 2: WEFAX station catalog + geo/schedule (`sdr-sat`)

**Files:**
- Create: `crates/sdr-sat/src/wefax_stations.rs`
- Modify: `crates/sdr-sat/src/lib.rs` (`mod wefax_stations;` + re-export)
- Test: inline `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing (leaf; `chrono` for UTC).
- Produces:
  - `pub struct WefaxStation { pub name: &'static str, pub channels_hz: &'static [u64], pub lat_deg: f64, pub lon_deg: f64, pub schedule: &'static [DailyWindow] }`
  - `pub struct DailyWindow { pub start_min_utc: u16, pub end_min_utc: u16 }`
  - `pub static KNOWN_WEFAX_STATIONS: &[WefaxStation]`
  - `pub fn great_circle_km(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64`
  - `pub fn stations_by_distance(user_lat: f64, user_lon: f64) -> Vec<(&'static WefaxStation, f64)>` (nearest first; km)
  - `pub fn is_active(station: &WefaxStation, now: chrono::DateTime<chrono::Utc>) -> bool`
  - `pub fn next_window_start(station: &WefaxStation, now: chrono::DateTime<chrono::Utc>) -> Option<chrono::DateTime<chrono::Utc>>`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn great_circle_new_orleans_to_boston_is_about_1900km() {
        // NMG ~ (30.0, -90.0), Boston ~ (42.4, -71.0)
        let d = great_circle_km(30.0, -90.0, 42.4, -71.0);
        assert!((1_800.0..2_100.0).contains(&d), "got {d} km");
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
            schedule: &[DailyWindow { start_min_utc: 0, end_min_utc: 360 }],
        };
        assert!(is_active(&s, Utc.with_ymd_and_hms(2026, 9, 20, 3, 0, 0).unwrap()));
        assert!(!is_active(&s, Utc.with_ymd_and_hms(2026, 9, 20, 7, 0, 0).unwrap()));
    }

    #[test]
    fn next_window_start_wraps_to_next_day() {
        let s = WefaxStation {
            name: "TEST",
            channels_hz: &[4_000_000],
            lat_deg: 0.0,
            lon_deg: 0.0,
            schedule: &[DailyWindow { start_min_utc: 0, end_min_utc: 360 }],
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
}
```

- [ ] **Step 2: Run, confirm fail** — `cargo test -p sdr-sat --lib wefax_stations` → FAIL.

- [ ] **Step 3: Implement `wefax_stations.rs`**

```rust
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
const CONTINUOUS: &[DailyWindow] = &[DailyWindow { start_min_utc: 0, end_min_utc: 1_440 }];

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

/// Minutes past 0000Z for a UTC instant.
fn minute_of_day(now: DateTime<Utc>) -> u16 {
    (now.hour() * 60 + now.minute()) as u16
}

/// Is the station within a broadcast window at `now`?
#[must_use]
pub fn is_active(station: &WefaxStation, now: DateTime<Utc>) -> bool {
    let m = minute_of_day(now);
    station.schedule.iter().any(|w| m >= w.start_min_utc && m < w.end_min_utc)
}

/// The next window START at or after `now` (wrapping to tomorrow).
#[must_use]
pub fn next_window_start(station: &WefaxStation, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let m = minute_of_day(now);
    let midnight = Utc
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single()?;
    let today = station.schedule.iter().map(|w| w.start_min_utc).filter(|&s| s > m).min();
    if let Some(s) = today {
        return Some(midnight + Duration::minutes(i64::from(s)));
    }
    let first = station.schedule.iter().map(|w| w.start_min_utc).min()?;
    Some(midnight + Duration::days(1) + Duration::minutes(i64::from(first)))
}
```

- [ ] **Step 4: Wire module** — `crates/sdr-sat/src/lib.rs`: add `mod wefax_stations;` and re-export `pub use wefax_stations::{DailyWindow, WefaxStation, KNOWN_WEFAX_STATIONS, great_circle_km, stations_by_distance, is_active, next_window_start};`. Confirm `chrono` is a dep of `sdr-sat` (it is — SGP4/passes use it).

- [ ] **Step 5: Run tests, confirm pass** — `cargo test -p sdr-sat --lib wefax_stations` → all 5 pass. Verify catalog coords/freqs against the published NWS RFAX schedule before finalizing (the values above are the transcription target; correct any that drift).

- [ ] **Step 6: Gates** — `cargo test -p sdr-sat`; `cargo clippy --all-targets --workspace -- -D warnings`; `uvx lizard crates/sdr-sat/src/wefax_stations.rs -T nloc=50`; `cargo fmt --all -- --check`.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-sat/src/wefax_stations.rs crates/sdr-sat/src/lib.rs
git commit -m "feat(sat): WEFAX station catalog + geo/schedule helpers (#913)"
```

---

### Task 3: `WefaxPresence` message + controller emit (`sdr-core`)

**Files:**
- Modify: `crates/sdr-core/src/messages.rs` (add `DspToUi::WefaxPresence(bool)`)
- Modify: `crates/sdr-core/src/controller/wefax.rs` (instantiate detector; emit edge-triggered)
- Test: inline test for the edge-trigger helper in `controller/wefax.rs`

**Interfaces:**
- Consumes: `sdr_dsp::wefax::WefaxPresenceDetector` (Task 1).
- Produces: `DspToUi::WefaxPresence(bool)`; a pure helper `presence_edge(last: &mut Option<bool>, current: bool) -> Option<bool>` (returns `Some(current)` only when it changed).

- [ ] **Step 1: Failing test for the edge helper** (inline in `controller/wefax.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::presence_edge;

    #[test]
    fn presence_edge_only_fires_on_change() {
        let mut last = None;
        assert_eq!(presence_edge(&mut last, false), Some(false)); // first observation emits
        assert_eq!(presence_edge(&mut last, false), None);        // unchanged
        assert_eq!(presence_edge(&mut last, true), Some(true));   // rising edge
        assert_eq!(presence_edge(&mut last, true), None);
        assert_eq!(presence_edge(&mut last, false), Some(false)); // falling edge
    }
}
```

- [ ] **Step 2: Run, confirm fail** — `cargo test -p sdr-core --lib controller::wefax` → FAIL.

- [ ] **Step 3: Add the message** — in `crates/sdr-core/src/messages.rs`, add to the `DspToUi` enum next to `WefaxState`:

```rust
/// Edge-triggered fax-subcarrier presence on the tuned WEFAX channel.
WefaxPresence(bool),
```

- [ ] **Step 4: Implement in `controller/wefax.rs`** — add the pure helper and wire the detector into the existing `wefax_decode_tap`. Add a `WefaxPresenceDetector` + `Option<bool>` last-presence to the wefax decoder state (next to `wefax_decoder`), init it at the same `audio_sample_rate()` the decoder uses, `update()` it with the same pre-gate mono buffer, and emit on edge:

```rust
/// Emit only when presence changed. Pure; unit-tested.
fn presence_edge(last: &mut Option<bool>, current: bool) -> Option<bool> {
    if *last == Some(current) {
        None
    } else {
        *last = Some(current);
        Some(current)
    }
}
```

In the tap, after computing the mono buffer and running the decoder:

```rust
let present = state.wefax_presence.update(&mono[..count]);
if let Some(edge) = presence_edge(&mut state.wefax_presence_last, present) {
    let _ = dsp_tx.send(DspToUi::WefaxPresence(edge));
}
```

Reset the detector + last-presence wherever the decoder is reset/re-inited (mode change, `ResetImagingDecoders`), so a stale presence doesn't carry across channels. Keep the tap function under 50 NLOC — extract the presence emit into a small `emit_presence_if_changed(state, dsp_tx, mono)` helper if the tap grows.

- [ ] **Step 5: Run tests + build** — `cargo test -p sdr-core --lib controller::wefax` (edge test passes); `cargo build -p sdr-core` (tap compiles).

- [ ] **Step 6: Gates** — `cargo test -p sdr-core`; `cargo clippy --all-targets --workspace -- -D warnings`; `uvx lizard crates/sdr-core/src/controller/wefax.rs crates/sdr-core/src/messages.rs -T nloc=50`; `cargo fmt --all -- --check`.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-core/src/messages.rs crates/sdr-core/src/controller/wefax.rs
git commit -m "feat(core): emit edge-triggered WEFAX presence from the decode tap (#913)"
```

---

### Task 4: `WefaxCatcher` state machine (`sdr-ui`)

**Files:**
- Create: `crates/sdr-ui/src/sidebar/wefax_catcher.rs`
- Modify: `crates/sdr-ui/src/sidebar/mod.rs` (`mod wefax_catcher;`)
- Test: inline `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `sdr_sat::{WefaxStation, stations_by_distance, is_active}`; `sdr_core::messages::WefaxState`.
- Produces: `WefaxCatcher::new() -> Self`; `tick(&mut self, ctx: TickCtx) -> Vec<Action>`; `state(&self) -> &CatcherState`. `Action`, `CatcherState`, `TickCtx`, `Candidate`, `SavedTune` (reuse the satellites `SavedTune` shape or a local minimal one), and a `StationChoice { Auto, Pinned(&'static str) }`.

This is the heart of the feature and is fully unit-tested. Model it on `satellites_recorder.rs` (`State` at :92, `Action` at :269, `tick` at :450). Keep each `tick_*` sub-function ≤ ~44 NLOC.

- [ ] **Step 1: Write the failing tests** (representative set — expand to cover every transition)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use sdr_core::messages::WefaxState;

    fn ctx(enabled: bool, present: bool, wefax: WefaxState) -> TickCtx {
        TickCtx {
            enabled,
            choice: StationChoice::Auto,
            user_lat: 30.0,
            user_lon: -90.0,
            now_min: 0,                 // ticks are sample-free; monotone counter
            wefax_state: wefax,
            fax_present: present,
            saved_tune: SavedTune::default(),
        }
    }

    #[test]
    fn enable_snapshots_tune_and_starts_scanning() {
        let mut c = WefaxCatcher::new();
        let acts = c.tick(ctx(true, false, WefaxState::Idle));
        assert!(matches!(c.state(), CatcherState::Scanning { .. }));
        // first candidate tuned + mode set + decoder reset
        assert!(acts.iter().any(|a| matches!(a, Action::Tune(_))));
        assert!(acts.iter().any(|a| matches!(a, Action::SetDemodMode)));
        assert!(acts.iter().any(|a| matches!(a, Action::ResetDecoder)));
    }

    #[test]
    fn presence_on_a_channel_locks_it() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle)); // -> Scanning
        // let the dwell elapse with no signal would advance; now signal appears
        let _ = c.tick(ctx(true, true, WefaxState::Idle));
        assert!(matches!(c.state(), CatcherState::Locked { .. }));
    }

    #[test]
    fn imaging_then_complete_saves_and_stays() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        c.tick(ctx(true, true, WefaxState::Idle));          // Locked
        c.tick(ctx(true, true, WefaxState::Imaging));       // Imaging (viewer opens)
        let acts = c.tick(ctx(true, true, WefaxState::Stopped)); // complete
        assert!(acts.iter().any(|a| matches!(a, Action::SavePng(_))));
        // stays on the working channel (not back to a fresh scan rotation)
        assert!(matches!(c.state(), CatcherState::Locked { .. } | CatcherState::Imaging { .. }));
    }

    #[test]
    fn disable_restores_tune_and_idles() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        let acts = c.tick(ctx(false, false, WefaxState::Idle));
        assert!(acts.iter().any(|a| matches!(a, Action::RestoreTune(_))));
        assert!(matches!(c.state(), CatcherState::Idle));
    }

    #[test]
    fn false_lock_times_out_back_to_scanning() {
        let mut c = WefaxCatcher::new();
        c.tick(ctx(true, false, WefaxState::Idle));
        c.tick(ctx(true, true, WefaxState::Idle)); // Locked
        // presence drops and stays down past the lock timeout, no imaging
        for _ in 0..LOCK_TIMEOUT_TICKS + 1 {
            c.tick(ctx(true, false, WefaxState::Idle));
        }
        assert!(matches!(c.state(), CatcherState::Scanning { .. }));
    }
}
```

- [ ] **Step 2: Run, confirm fail** — `cargo test -p sdr-ui --features whisper-cpu --lib sidebar::wefax_catcher` → FAIL.

- [ ] **Step 3: Implement `wefax_catcher.rs`** — define the types and `tick` dispatching to per-state helpers. Full skeleton (fill the per-state bodies to satisfy the tests; keep each helper small):

```rust
//! Pure WEFAX auto-catch state machine. `tick()` takes the observable
//! world in and returns Actions the UI layer interprets. No GTK, no I/O —
//! mirrors `satellites_recorder::AutoRecorder`.

use sdr_core::messages::WefaxState;
use sdr_sat::{is_active, stations_by_distance, WefaxStation};
use std::path::PathBuf;

/// Dwell (in ticks) per candidate channel while scanning.
pub const SCAN_DWELL_TICKS: u32 = 3;
/// Ticks a Locked channel may go without imaging before it's a false lock.
pub const LOCK_TIMEOUT_TICKS: u32 = 10;

/// Which station(s) to scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StationChoice {
    #[default]
    Auto,
    Pinned(&'static str),
}

/// A snapshot of the user's tune, restored when the catcher stops.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedTune {
    pub center_hz: f64,
    pub demod_mode: u8, // encode the mode; the interpret layer maps it
    pub was_wefax: bool,
}

/// One scan candidate = a station's single channel.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub station: &'static str,
    pub freq_hz: u64,
}

/// Inputs to one tick.
pub struct TickCtx {
    pub enabled: bool,
    pub choice: StationChoice,
    pub user_lat: f64,
    pub user_lon: f64,
    pub now_min: u16,      // minute-of-day UTC for schedule checks
    pub wefax_state: WefaxState,
    pub fax_present: bool,
    pub saved_tune: SavedTune,
}

/// Actions the UI layer interprets.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Tune(u64),
    SetDemodMode,          // DemodMode::Wefax
    ResetDecoder,
    OpenViewer,
    SavePng(PathBuf),
    RestoreTune(SavedTune),
    Toast(String),
}

#[derive(Clone, Debug)]
pub enum CatcherState {
    Idle,
    Scanning { candidates: Vec<Candidate>, idx: usize, dwell: u32, saved: SavedTune },
    Locked { cand: Candidate, waited: u32, saved: SavedTune },
    Imaging { cand: Candidate, saved: SavedTune },
}

pub struct WefaxCatcher {
    state: CatcherState,
}

impl WefaxCatcher {
    #[must_use]
    pub fn new() -> Self {
        Self { state: CatcherState::Idle }
    }

    #[must_use]
    pub fn state(&self) -> &CatcherState {
        &self.state
    }

    pub fn tick(&mut self, ctx: TickCtx) -> Vec<Action> {
        // Disable from any active state -> restore + Idle.
        if !ctx.enabled {
            return self.go_idle(&ctx);
        }
        match std::mem::replace(&mut self.state, CatcherState::Idle) {
            CatcherState::Idle => self.on_idle(&ctx),
            CatcherState::Scanning { candidates, idx, dwell, saved } => {
                self.on_scanning(&ctx, candidates, idx, dwell, saved)
            }
            CatcherState::Locked { cand, waited, saved } => self.on_locked(&ctx, cand, waited, saved),
            CatcherState::Imaging { cand, saved } => self.on_imaging(&ctx, cand, saved),
        }
    }

    // --- per-state helpers (each ≤ ~44 NLOC) ---

    fn go_idle(&mut self, ctx: &TickCtx) -> Vec<Action> {
        let was_active = !matches!(self.state, CatcherState::Idle);
        self.state = CatcherState::Idle;
        if was_active {
            vec![Action::RestoreTune(ctx.saved_tune.clone())]
        } else {
            Vec::new()
        }
    }

    fn build_candidates(ctx: &TickCtx) -> Vec<Candidate> {
        let ranked = stations_by_distance(ctx.user_lat, ctx.user_lon);
        ranked
            .into_iter()
            .filter(|(s, _)| match ctx.choice {
                StationChoice::Auto => true,
                StationChoice::Pinned(name) => s.name == name,
            })
            // prioritize schedule-active stations, but keep all as fallback
            .flat_map(|(s, _)| Self::station_candidates(s, ctx.now_min))
            .collect()
    }

    fn station_candidates(s: &'static WefaxStation, _now_min: u16) -> Vec<Candidate> {
        s.channels_hz
            .iter()
            .map(|&f| Candidate { station: s.name, freq_hz: f })
            .collect()
    }

    fn on_idle(&mut self, ctx: &TickCtx) -> Vec<Action> {
        let candidates = Self::build_candidates(ctx);
        if candidates.is_empty() {
            self.state = CatcherState::Idle;
            return vec![Action::Toast("No receivable WEFAX stations".into())];
        }
        let first = candidates[0].clone();
        self.state = CatcherState::Scanning {
            candidates,
            idx: 0,
            dwell: 0,
            saved: ctx.saved_tune.clone(),
        };
        Self::tune_actions(&first)
    }

    fn tune_actions(c: &Candidate) -> Vec<Action> {
        vec![Action::ResetDecoder, Action::Tune(c.freq_hz), Action::SetDemodMode]
    }

    fn on_scanning(
        &mut self,
        ctx: &TickCtx,
        candidates: Vec<Candidate>,
        idx: usize,
        dwell: u32,
        saved: SavedTune,
    ) -> Vec<Action> {
        if ctx.fax_present {
            let cand = candidates[idx].clone();
            self.state = CatcherState::Locked { cand, waited: 0, saved };
            return Vec::new();
        }
        if dwell + 1 >= SCAN_DWELL_TICKS {
            let next = (idx + 1) % candidates.len();
            let cand = candidates[next].clone();
            self.state = CatcherState::Scanning { candidates, idx: next, dwell: 0, saved };
            return Self::tune_actions(&cand);
        }
        self.state = CatcherState::Scanning { candidates, idx, dwell: dwell + 1, saved };
        Vec::new()
    }

    fn on_locked(&mut self, ctx: &TickCtx, cand: Candidate, waited: u32, saved: SavedTune) -> Vec<Action> {
        if matches!(ctx.wefax_state, WefaxState::Imaging) {
            self.state = CatcherState::Imaging { cand, saved };
            return vec![Action::OpenViewer];
        }
        if !ctx.fax_present && waited + 1 >= LOCK_TIMEOUT_TICKS {
            // false lock — resume scanning from a fresh rotation
            return self.on_idle(ctx);
        }
        self.state = CatcherState::Locked { cand, waited: waited + 1, saved };
        Vec::new()
    }

    fn on_imaging(&mut self, ctx: &TickCtx, cand: Candidate, saved: SavedTune) -> Vec<Action> {
        if matches!(ctx.wefax_state, WefaxState::Stopped) {
            // Chart complete: save; STAY on this working channel (-> Locked, waiting for next chart).
            self.state = CatcherState::Locked { cand, waited: 0, saved };
            return vec![Action::SavePng(PathBuf::new())]; // interpret layer fills the real path
        }
        if !ctx.fax_present {
            // sustained signal loss mid-image -> back to scanning
            return self.on_idle(ctx);
        }
        self.state = CatcherState::Imaging { cand, saved };
        Vec::new()
    }
}

impl Default for WefaxCatcher {
    fn default() -> Self {
        Self::new()
    }
}
```

Note the `SavePng(PathBuf::new())` placeholder path is intentional — the pure machine doesn't know the recordings dir; the interpret layer (Task 6) substitutes the real path via the existing WEFAX save helper. Adjust the tests if you change that contract.

- [ ] **Step 4: Run tests, confirm pass** — `cargo test -p sdr-ui --features whisper-cpu --lib sidebar::wefax_catcher` → all pass. Add tests for the remaining paths you implement (schedule prioritization, pinned station, empty rotation) until every `tick_*` branch is covered.

- [ ] **Step 5: Gates** — `cargo test -p sdr-ui --features whisper-cpu`; `cargo clippy --all-targets --workspace -- -D warnings`; `uvx lizard crates/sdr-ui/src/sidebar/wefax_catcher.rs -T nloc=50`; `cargo fmt --all -- --check`.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/sidebar/wefax_catcher.rs crates/sdr-ui/src/sidebar/mod.rs
git commit -m "feat(ui): pure WEFAX auto-catch state machine (#913)"
```

---

### Task 5: WEFAX activity panel + registration (`sdr-ui`, GTK — user-smoke-tested)

**Files:**
- Create: `crates/sdr-ui/src/sidebar/wefax_panel.rs`
- Modify: `crates/sdr-ui/src/sidebar/activity_bar.rs` (new `LEFT_ACTIVITIES` entry + persistence keys)
- Modify: `crates/sdr-ui/src/sidebar/mod.rs` (`WefaxPanel` field + `build_panels`)
- Modify: `crates/sdr-ui/src/window/layout.rs` (stack child `"wefax"`)

**Interfaces:**
- Produces: `WefaxPanel { pub widget: adw::PreferencesPage, pub handles: Rc<WefaxPanelHandles> }`; `build_wefax_panel() -> WefaxPanel`; `WefaxPanelHandles { enable_switch: gtk4::Switch, station_row: adw::ComboRow, status_label: gtk4::Label, suppress_switch_notify: Cell<bool> }`.

Follow the standard `AdwPreferencesPage`/`AdwPreferencesGroup` convention (read `scanner_panel.rs` or `radio_panel.rs` for the idiom; do NOT copy Orbcomm's `ScrolledWindow` layout). The enable-switch group mirrors `orbcomm_panel::build_enable_group` (`orbcomm_panel.rs:115`).

- [ ] **Step 1: Add the activity entry** — in `activity_bar.rs`, append to `LEFT_ACTIVITIES` (after the orbcomm entry at `:180`), and add the three persistence keys next to the others (`:220-248`). Pick the next free accelerator — verify Ctrl+9 is the current max and use `Ctrl+0` (confirm no conflict in the shortcut table):

```rust
ActivityBarEntry {
    name: "wefax",
    icon_name: "image-x-generic-symbolic",
    display_name: "WEFAX",
    shortcut_label: "Ctrl+0",
    accelerator: "<Ctrl>0",
},
```

- [ ] **Step 2: Build the panel** — create `wefax_panel.rs`:

```rust
//! WEFAX auto-catch activity panel. Turn Decode on; the catcher scans and
//! decodes a chart unattended. Standard AdwPreferencesPage layout.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::glib;

pub struct WefaxPanelHandles {
    pub enable_switch: gtk4::Switch,
    pub station_row: adw::ComboRow,
    pub status_label: gtk4::Label,
    pub suppress_switch_notify: Cell<bool>,
}

pub struct WefaxPanel {
    pub widget: adw::PreferencesPage,
    pub handles: Rc<WefaxPanelHandles>,
}

fn build_enable_group() -> (adw::PreferencesGroup, gtk4::Switch) {
    let group = adw::PreferencesGroup::builder().title("Decode").build();
    let row = adw::ActionRow::builder()
        .title("Auto-catch WEFAX")
        .subtitle("Scan receivable stations and decode a chart automatically")
        .build();
    let sw = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    row.add_suffix(&sw);
    row.set_activatable_widget(Some(&sw));
    group.add(&row);
    (group, sw)
}

#[must_use]
pub fn build_wefax_panel() -> WefaxPanel {
    let page = adw::PreferencesPage::new();
    let (enable_group, enable_switch) = build_enable_group();
    page.add(&enable_group);

    let station_group = adw::PreferencesGroup::builder().title("Station").build();
    let station_row = adw::ComboRow::builder().title("Station").build();
    // Model populated in connect_wefax_panel from stations_by_distance (Auto + names).
    station_group.add(&station_row);
    page.add(&station_group);

    let status_group = adw::PreferencesGroup::builder().title("Status").build();
    let status_label = gtk4::Label::builder().label("Idle").xalign(0.0).build();
    let status_row = adw::ActionRow::builder().title("State").build();
    status_row.add_suffix(&status_label);
    status_group.add(&status_row);
    page.add(&status_group);

    WefaxPanel {
        widget: page,
        handles: Rc::new(WefaxPanelHandles {
            enable_switch,
            station_row,
            status_label,
            suppress_switch_notify: Cell::new(false),
        }),
    }
}
```

- [ ] **Step 3: Register in the panels struct** — `sidebar/mod.rs`: add `pub wefax: WefaxPanel` to `SidebarPanels` (`:48`) and construct it in `build_panels` (`:102`) with `wefax: wefax_panel::build_wefax_panel()`. Add `pub mod wefax_panel;`.

- [ ] **Step 4: Add the stack child** — `window/layout.rs`, next to the orbcomm child (`:617`): `left_stack.add_named(&panels.wefax.widget, Some("wefax"));`. The `"wefax"` string MUST match the `LEFT_ACTIVITIES` entry `name`.

- [ ] **Step 5: Build check** — `cargo build -p sdr-ui --features whisper-cpu` compiles; the activity icon appears (verified in Task 6's smoke). No unit tests here (pure-GTK construction).

- [ ] **Step 6: Gates** — `cargo clippy --all-targets --workspace -- -D warnings`; `uvx lizard crates/sdr-ui/src/sidebar/wefax_panel.rs -T nloc=50`; `cargo fmt --all -- --check`.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-ui/src/sidebar/wefax_panel.rs crates/sdr-ui/src/sidebar/activity_bar.rs crates/sdr-ui/src/sidebar/mod.rs crates/sdr-ui/src/window/layout.rs
git commit -m "feat(ui): WEFAX activity panel + sidebar registration (#913)"
```

---

### Task 6: Wire it together — connect + interpret + driver (`sdr-ui`, GTK — user-smoke-tested)

**Files:**
- Create: `crates/sdr-ui/src/window/wefax/mod.rs`, `crates/sdr-ui/src/window/wefax/catcher.rs`
- Modify: `crates/sdr-ui/src/sidebar/wefax_panel.rs` (add `connect_wefax_panel`)
- Modify: `crates/sdr-ui/src/window.rs` (call `connect_wefax_panel`; register the accelerator/help auto-derive)
- Modify: `crates/sdr-ui/src/state.rs` (`AppState` fields)
- Modify: `crates/sdr-ui/src/window/dsp_events.rs` (feed `WefaxState`/`WefaxPresence` into the catcher inputs; update the panel status)

**Interfaces:**
- Consumes: `WefaxCatcher` (Task 4), `WefaxPanel` handles (Task 5), the `DspToUi::WefaxPresence`/`WefaxState` messages (Task 3), `sdr_sat::stations_by_distance` for the combo model.
- Produces: `interpret_wefax_action(deps: &WefaxDeps, action: Action)`; a glib-timer driver `spawn_wefax_tick(state)`.

Mirror `window/satellites/recorder.rs` (`build_recorder_interpreter` :20, `interpret_recorder_action` :93, `RecorderDeps` :75).

- [ ] **Step 1: AppState fields** — in `state.rs`, add (near the existing `wefax_viewer`/`wefax_image` fields):

```rust
pub wefax_catcher: RefCell<crate::sidebar::wefax_catcher::WefaxCatcher>,
pub wefax_enabled: Cell<bool>,
pub wefax_last_state: Cell<sdr_core::messages::WefaxState>,
pub wefax_present: Cell<bool>,
pub wefax_saved_tune: RefCell<crate::sidebar::wefax_catcher::SavedTune>,
```

Initialize them in `AppState`'s constructor.

- [ ] **Step 2: `interpret_wefax_action`** — `window/wefax/catcher.rs`:

```rust
//! Turns WefaxCatcher Actions into UiToDsp messages / viewer calls / toasts.
use crate::sidebar::wefax_catcher::{Action, SavedTune};
// WefaxDeps captures: state (Rc<AppState>), parent-window resolver, toast overlay.

pub fn interpret_wefax_action(deps: &WefaxDeps, action: Action) {
    match action {
        Action::Tune(freq_hz) => deps.state.send_dsp(UiToDsp::Tune(freq_hz as f64)),
        Action::SetDemodMode => deps.state.send_dsp(UiToDsp::SetDemodMode(DemodMode::Wefax)),
        Action::ResetDecoder => deps.state.send_dsp(UiToDsp::ClearWefaxImage), // + decoder reset msg
        Action::OpenViewer => crate::wefax_viewer::window::open_wefax_viewer_if_needed(&deps.parent, &deps.state),
        Action::SavePng(_) => { /* the existing WefaxImageComplete auto-save path already writes PNGs; no-op or force-flush */ }
        Action::RestoreTune(t) => restore_tune(deps, &t),
        Action::Toast(msg) => deps.toast.add_toast(adw::Toast::new(&msg)),
    }
}
```

Note: PNG auto-save already happens on `DspToUi::WefaxImageComplete` (`dsp_events.rs:783`), so `Action::SavePng` may be a no-op — confirm during implementation and simplify the `Action` set if so (update Task 4's tests to match). `RestoreTune` maps `SavedTune` back to `UiToDsp::Tune` + `SetDemodMode`.

- [ ] **Step 3: The tick driver** — `spawn_wefax_tick(state)` installs a `glib::timeout_add_local` (~500 ms) that, while `state.wefax_enabled`, builds a `TickCtx` from the cached `wefax_last_state`/`wefax_present`/ground-station coords/`saved_tune`, calls `state.wefax_catcher.borrow_mut().tick(ctx)`, and runs each returned `Action` through `interpret_wefax_action`. Read the ground-station lat/lon from config the same way `satellites_panel.rs` does.

- [ ] **Step 4: `connect_wefax_panel`** — in `wefax_panel.rs`: populate the `station_row` model from `stations_by_distance(user_lat, user_lon)` (an "Auto (nearest)" first row + station names with km); on `enable_switch.connect_active_notify` (guarded by `suppress_switch_notify`): refuse if `state.scanner.is_enabled()` (toast + revert the switch, mirroring Orbcomm's scanner mutual-exclusion), else snapshot the tune into `wefax_saved_tune`, set `wefax_enabled`, and `spawn_wefax_tick(state)`. On disable, set `wefax_enabled=false` (the next tick restores the tune) . Call `connect_wefax_panel(panels, state)` from `window.rs::connect_sidebar_panels` next to the orbcomm connect (`window.rs:1653`).

- [ ] **Step 5: Feed DSP events** — in `dsp_events.rs`, extend `on_wefax_state` (`:860`) to also cache `state.wefax_last_state` and update the panel `status_label`; add an `on_wefax_presence(state, present)` arm for the new `DspToUi::WefaxPresence` that caches `state.wefax_present`. Wire the new message in the `DspToUi` match.

- [ ] **Step 6: Build + gates** — `cargo build -p sdr-ui --features whisper-cpu`; `cargo test -p sdr-ui --features whisper-cpu`; `cargo clippy --all-targets --workspace -- -D warnings`; `uvx lizard <all changed .rs> -T nloc=50`; `cargo fmt --all -- --check`.

- [ ] **Step 7: USER SMOKE TEST** — Claude runs `make install CARGO_FLAGS="--release --no-default-features --features sherpa-cuda"` and provides this checklist (never launches the binary):
  - WEFAX activity appears in the left bar (Ctrl+0) with the standard panel layout.
  - Toggling Decode on with the scanner running is refused with a toast; off otherwise.
  - With an Airspy/SpyVerter on HF, Decode on → status cycles Scanning → (on a live fax) Signal found → Imaging; the viewer auto-opens and a chart draws; PNG auto-saves.
  - Decode off restores the previous tune/mode.
  - No static image is ever drawn on a dead channel.

- [ ] **Step 8: Commit** (after smoke passes)

```bash
git add crates/sdr-ui/src/window/wefax/mod.rs crates/sdr-ui/src/window/wefax/catcher.rs crates/sdr-ui/src/sidebar/wefax_panel.rs crates/sdr-ui/src/window.rs crates/sdr-ui/src/state.rs crates/sdr-ui/src/window/dsp_events.rs
git commit -m "feat(ui): wire the WEFAX auto-catch activity end to end (#913)"
```

---

## Self-Review

**Spec coverage:** activity tab (Task 5/6) ✓; auto-scan across channels (Task 4 candidates + Task 2 catalog) ✓; geo-ranking from ground station (Task 2 + Task 6 combo) ✓; block schedule (Task 2 `is_active`/`next_window_start`; prioritization is a v1-light filter in `build_candidates` — expand if scanning dead windows proves wasteful) ✓; fax-presence detector, no-static gate (Task 1 + Task 3) ✓; dedicated state machine mirroring auto-recorder (Task 4) ✓; catcher owns VFO + scanner mutual-exclusion (Task 6 connect) ✓; stay-on-working-channel (Task 4 `on_imaging` → Locked) ✓; reset between channels (Task 4 `ResetDecoder`) ✓; viewer auto-open on imaging (Task 4 `OpenViewer` + Task 6) ✓; testing strategy (pure TDD Tasks 1/2/4 + real-data gate Task 1 + user smoke Task 6) ✓; deferred items kept out (#914) ✓.

**Open decision flagged for the implementer:** whether `Action::SavePng` is needed at all given the existing `WefaxImageComplete` auto-save — resolve in Task 6 Step 2 and reconcile Task 4's test. Whether to commit real audio fixtures — resolve in Task 1 Step 6 (recommended: yes).

**Type consistency:** `WefaxPresenceDetector::{new,update,is_present,reset}` used consistently (Tasks 1,3); `WefaxStation`/`stations_by_distance`/`is_active` (Tasks 2,4,6); `WefaxCatcher::{new,tick,state}` + `Action`/`CatcherState`/`TickCtx`/`SavedTune`/`StationChoice` (Tasks 4,6); `DspToUi::WefaxPresence(bool)` (Tasks 3,6). Consistent.
