# Doppler Correction — Design

**Issue:** [#521](https://github.com/jasonherald/rtl-sdr/issues/521) (sub-ticket of [#520](https://github.com/jasonherald/rtl-sdr/issues/520) LRPT post-MVP enhancements)

**Goal:** Continuously correct the receive frequency for satellite Doppler shift during a pass, so the user's audio doesn't drift and the APT / LRPT / SSTV decoders stay locked.

**Cross-protocol** (status as of April 2026): applies to NOAA APT (epic [#468](https://github.com/jasonherald/rtl-sdr/issues/468), shipped), Meteor-M LRPT (epic [#469](https://github.com/jasonherald/rtl-sdr/issues/469), shipped), and ISS SSTV (epic [#472](https://github.com/jasonherald/rtl-sdr/issues/472), pending). Not just LRPT despite living under the LRPT post-MVP epic.

---

## 1. The bug we're solving

A polar-orbit satellite at ~800 km altitude moves at ~7.5 km/s relative to a fixed ground station. Over a typical 12-minute horizon-to-horizon pass, the radial component of that velocity changes from +7 km/s (approaching) to −7 km/s (receding), passing through zero at TCA (time of closest approach). At a 137 MHz APT carrier, that's roughly:

| Phase | Range-rate | Doppler shift |
|---|---|---|
| AOS (approaching, low elevation) | ≈ +6 km/s | ≈ +2.7 kHz |
| TCA (overhead) | ≈ 0 | ≈ 0 Hz |
| LOS (receding, low elevation) | ≈ −6 km/s | ≈ −2.7 kHz |

So the carrier sweeps roughly ±3 kHz across the pass, hitting +2.7 → 0 → −2.7. For NFM with 40 kHz channel bandwidth this is "audible drift but you stay locked." For LRPT QPSK the demod tolerates it but loses headroom. For ISS SSTV (long single-image transmission) it's enough to walk the user out of tune mid-image.

Today: the user fights this manually (or accepts the drift). With Doppler correction: the VFO offset auto-tracks the predicted Doppler curve so the demodulator sees a constant, on-frequency carrier.

## 2. Activation rule

**Single trigger:** Doppler engages when, at the current tick, **all three** are true:

1. The user-facing master switch (Satellites panel) is **on** (default ON, persisted).
2. The current center frequency is within **±20 kHz** of a `KNOWN_SATELLITES` entry's `downlink_hz` (covers PPM-correction nudges + pre-pass drift).
3. That satellite is **above the horizon** at the user's ground station (SGP4 elevation > 0°).

When multiple catalog entries match the frequency window (e.g. NOAA 18 and NOAA 19 both around 137.9 MHz), pick the one with the highest current elevation. Deterministic tie-break: order in `KNOWN_SATELLITES` (earliest wins).

When the trigger conditions go from true → false (pass ends, user re-tunes off the satellite, master switch flipped off), dispatch one final `UiToDsp::SetVfoOffset(user_reference_offset)` so the offset doesn't get stuck on the last computed Doppler value.

**Why this rule covers all the cases:**

| Case | Outcome |
|---|---|
| Auto-record AOS tunes to NOAA 15 | Freq matches, satellite is by definition above the horizon at AOS → engages |
| User manually tunes to NOAA 15 mid-pass | Freq matches, satellite above the horizon → engages |
| User manually tunes to NOAA 15 between passes | Freq matches, but below horizon → does NOT engage (correctly!) |
| User tunes off-satellite | Freq doesn't match → disengages |
| User has no ground station coords set | "Above horizon" can't be evaluated → tracker stays dormant, status bar hidden |

No special "is auto-record running?" branch is needed; auto-record sets the frequency to the satellite, AOS puts the satellite above the horizon, and the auto-detect rule fires naturally.

## 3. Where the correction lands in the signal path

**`UiToDsp::SetVfoOffset(hz)` only.** Not an SDR `tune()` call.

Pure DSP shift, zero hardware churn, zero glitches. Limited to ±half the frontend bandwidth (typically 2.4 Msps with VFO bandwidth of ~50–250 kHz), so a ±3 kHz Doppler shift fits comfortably in the middle 5% of the window.

For everything we ship as of April 2026 (NOAA APT, Meteor LRPT, ISS SSTV), max Doppler is ±5 kHz over a full pass — VFO offset alone handles 100% of these passes without ever approaching the bandwidth limit.

**Out of scope for v1:** SDR center-frequency retune. Architectured-around (a future need for narrow-bandwidth modes or HEO/deep-space could add a coarse-retune branch in the same `DopplerTracker` without touching the activation rule), but not implemented as of April 2026.

## 4. Manual-override behavior — additive (DEFERRED in v1)

> **v1 status:** the `user_reference_offset` field exists on `DopplerTracker` and the `live_offset_hz()` method computes the additive sum, but the **wiring** that updates `user_reference_offset` from a user VFO-slider drag is deferred. Reason: the spectrum widget's drag/click-to-tune path (`crates/sdr-ui/src/spectrum/mod.rs`) dispatches `UiToDsp::SetVfoOffset` directly via its own `dsp_tx` clone, completely bypassing `AppState::send_dsp` and the Doppler wiring layer. Hooking it requires either hoisting `Rc<RefCell<DopplerTracker>>` onto `AppState` (touches every spectrum-construction call site) or re-routing the spectrum drag through the wiring layer. v1 lands the model + the call site comment; the wiring is filed as a follow-up.
>
> **v1 effective behavior:** a user drag wins for ≤250 ms (until the next 4 Hz Doppler recompute), then Doppler reasserts. Acceptable for a v1 — see PR #554 and its CR thread.

The intended (post-deferral) design: the user dragging the VFO offset slider while Doppler is active changes a per-session `user_reference_offset`. Doppler tracks **on top** of that:

```text
live_offset = user_reference_offset + doppler_correction
```

So (once wired):

- User can fine-tune for personal taste (offset by +500 Hz to bias toward USB sideband, etc.) and Doppler still tracks correctly relative to that.
- A manual drag does NOT disable Doppler ("respectful of agency" but creates the "wait, why is Doppler off?" surprise — rejected).
- A manual drag is NOT overwritten on the next tick ("Doppler wins" — feels paternalistic, fights the user — rejected).

**Reset semantics:**

- **Fresh engagement** (None → Some at AOS): `user_reference_offset` is **seeded** from the live spectrum VFO offset so this pass's Doppler tracks ON TOP of whatever offset the user had set before AOS.
- **Satellite swap** (Some(A) → Some(B), e.g. NOAA 19 dropping below NOAA 18 in elevation mid-pass): `user_reference_offset` is **preserved** across the swap. The user's fine-tune offset belongs to the user, not to a specific satellite — losing it on a swap would be hostile UX. (Per CR round 5 on PR #554 — the original spec text said "resets to 0 on satellite change" but that was wrong.)
- **Disengagement** (Some → None at LOS, master off, or retune away): the pre-disengage `user_reference_offset` is captured and dispatched as one final `SetVfoOffset(captured)` so the VFO returns to the user's pre-tracking baseline. After the dispatch, `user_reference_offset` returns to 0.

Note: `user_reference_offset` itself is per-tracking-session, NOT persisted to disk. A fresh app launch starts at 0 and is seeded on the next AOS.

## 5. Range-rate / Doppler calculation — wraps existing `sdr-sat` API

> **v1 reality:** this section originally proposed a new `sdr_sat::doppler_offset_hz(...)` function. **It turned out the math was already shipped in `sdr-sat`** — `sdr_sat::track(station, satellite, when)` returns a `Track` whose `Track::doppler_shift_hz(carrier_hz)` method computes Δf using the same formula. The crate boundary is therefore unchanged: the new code lives entirely in `sdr-ui`.

`crates/sdr-ui/src/doppler_tracker.rs::compute_doppler_offset_hz` is a thin wrapper over the existing API:

```rust
// crates/sdr-ui/src/doppler_tracker.rs
pub fn compute_doppler_offset_hz(
    satellite: &Satellite,
    station: &GroundStation,
    when: chrono::DateTime<chrono::Utc>,
    carrier_hz: f64,
) -> Result<f64, DopplerError> {
    let track = sdr_sat::track(station, satellite, when)?;
    Ok(track.doppler_shift_hz(carrier_hz))
}
```

**Math (implemented inside `sdr-sat::track` + `Track::doppler_shift_hz`):**

1. SGP4 propagate the TLE to `when` → ECI position + velocity (km, km/s).
2. Compute range vector and range-rate in ECI (no topocentric conversion needed — the dot product `r · v` is frame-invariant).
3. Range-rate ṙ in km/s, signed: positive = receding.
4. Doppler Δf = −ṙ · f_carrier / c (Hz). The negation makes "approaching" produce positive Δf, matching the table in §1.

See `crates/sdr-sat/src/passes.rs::track` for the full implementation, including how station velocity (ω × r_station_eci) is added to satellite velocity for the relative range-rate.

**Carrier frequency:** read from `satellite.downlink_hz` (catalog truth). Not from the user's current center frequency.

**Reuse:** the existing TLE cache (`sdr-sat::TleCache`, 24 h cached against Celestrak `gp.php?CATNR=…`) is the source of TLE strings. The existing `GroundStation` config (lat/lon/alt from the Satellites panel rows) is the source of `station`.

## 6. The `DopplerTracker` (sdr-ui)

New module: `crates/sdr-ui/src/doppler_tracker.rs`. The shipped design is a small **pure state model** + a thin compute helper; all timers, persistence, GTK widgets, status-bar updates, and dispatch state live in the window-level wiring layer. (Spec originally proposed a fat `DopplerTracker` owning everything; the shipped split is cleaner and unit-testable headlessly.)

```rust
// crates/sdr-ui/src/doppler_tracker.rs

#[derive(Debug, Default)]
pub struct DopplerTracker {
    master_enabled: bool,
    active: Option<&'static KnownSatellite>,
    user_reference_offset_hz: f64,
}

impl DopplerTracker {
    pub fn new(master_enabled: bool) -> Self;
    pub fn set_master_enabled(&mut self, enabled: bool) -> Option<f64>;
    pub fn master_enabled(&self) -> bool;
    pub fn set_user_reference_offset_hz(&mut self, hz: f64);
    pub fn user_reference_offset_hz(&self) -> f64;
    pub fn set_active(&mut self, sat: Option<&'static KnownSatellite>) -> bool;
    pub fn active(&self) -> Option<&'static KnownSatellite>;
    pub fn live_offset_hz(&self, doppler_hz: f64) -> f64;
}

pub fn compute_doppler_offset_hz(
    sat: &Satellite,
    station: &GroundStation,
    when: DateTime<Utc>,
    carrier_hz: f64,
) -> Result<f64, DopplerError>;

pub fn pick_active_satellite(
    master_enabled: bool,
    candidates: &[Candidate],
) -> Option<&'static KnownSatellite>;
```

The wiring lives in `crates/sdr-ui/src/window.rs` as **two functions**:

- **`restore_doppler_switch(panels, config)`** — runs **unconditionally** at window construction. Restores the persisted master-switch value to the `AdwSwitchRow` and wires `connect_active_notify` to `save_doppler_tracking_enabled`. Independent of TLE-cache availability so the user's preference always persists.
- **`connect_doppler_tracker(panels, state, cache, status_bar)`** — runs only when the TLE cache is available. Owns the `Rc<RefCell<DopplerTracker>>`, the `Rc<Cell<f64>>` for the dispatch baseline, and **two `glib::timeout_add_local` timers**:
  1. **4 Hz tick** (`DOPPLER_RECOMPUTE_TICK = 250ms`) — checks the active satellite's freq-match (immediately disengages on retune-away), recomputes Doppler via `compute_doppler_offset_hz`, dispatches `UiToDsp::SetVfoOffset(live)` rate-limited to >`DOPPLER_DISPATCH_THRESHOLD_HZ = 5 Hz` changes, updates the status-bar label.
  2. **1 Hz re-evaluate** (`DOPPLER_TRIGGER_TICK = 1s`) — rebuilds the candidate list from catalog × frequency match × ground station × cached TLEs, calls `pick_active_satellite`, then `set_active`. On disengage transitions, dispatches the captured pre-engage `user_reference_offset_hz`.

  **Dispatch baseline for the rate-limit gate:**

  - **Storage & sync:** `state.last_dispatched_vfo_offset_hz: Cell<f64>` on `AppState`, kept in sync by the `connect_vfo_offset_changed` callback that fires from BOTH the DSP echo (`DspToUi::VfoOffsetChanged`) and direct user-drag dispatches.
  - **Sync coverage:** every VFO-mutating path (auto-record AOS reset, spectrum drag, click-to-tune, our own Doppler ticks) flows through that callback, so the 4 Hz tick's threshold comparison always lands against the actual current DSP state — never a stale local value.
  - **Fallback invariant:** every fallback dispatch path (master-switch off, trigger-tick disengage, freq-match-guard disengage in the 4 Hz tick) explicitly calls `state.last_dispatched_vfo_offset_hz.set(user_reference_offset_hz)` alongside its `SetVfoOffset(user_reference_offset_hz)` send, so the next engagement isn't suppressed by the threshold gate landing within `DOPPLER_DISPATCH_THRESHOLD_HZ` of a stale Doppler value.

  Plus a third change-notify handler on `panels.satellites.doppler_switch` that drives the tracker model when the master switch toggles (separate from the persistence handler in `restore_doppler_switch` — multiple GTK signal handlers fire independently).

**Why split:** decoupling state from wiring keeps the model unit-testable headlessly (20+ tests in `doppler_tracker.rs`), keeps the wiring layer's GTK-specific concerns out of the model, and makes the persistence-vs-behavior gating clean (persistence always; behavior only when TLE cache available).

**SGP4 cost:** one propagation per 250 ms tick, plus one per 1 s re-evaluate per catalog candidate. SGP4 is microseconds — total CPU cost is negligible.

## 7. UI surface

### 7.1 Master switch — Satellites panel

Add an `AdwSwitchRow` to the existing `AdwPreferencesGroup` in the Satellites activity panel (alongside auto-record APT / auto-record audio toggles):

```text
🛰  Doppler tracking
    Auto-correct frequency drift during satellite passes.
```

Default **ON**. Persisted via new helper:

```rust
// crates/sdr-ui/src/sidebar/satellites_panel.rs
pub const KEY_DOPPLER_TRACKING_ENABLED: &str = "sat_doppler_tracking_enabled";
pub fn load_doppler_tracking_enabled(config: &Arc<ConfigManager>) -> bool;  // default true
pub fn save_doppler_tracking_enabled(config: &Arc<ConfigManager>, enabled: bool);
```

### 7.2 Status bar readout

Add a label to the status bar (next to existing SNR / sample rate labels). Format:

- Active: `Doppler: -1.4 kHz` (signed; rounds to 0.1 kHz)
- Inactive: hidden (label `set_visible(false)`)

The label updates on every 4 Hz tick along with the offset dispatch, so user sees real-time tracking — the parabolic shape of a pass becomes visible: starts at +2.7 kHz, sweeps through 0 at TCA, ends at −2.7 kHz.

## 8. Edge cases

| Case | Behavior |
|---|---|
| User has no ground-station coords set | Trigger evaluation can't compute elevation → tracker dormant, status bar hidden. Same way auto-record gracefully no-ops. |
| TLE for active satellite is stale (>24 h) and Celestrak fetch fails | Tracker falls back to dormant; trace-warn the failure but don't toast (a toast every pass would be too noisy). User can manually refresh TLEs from the satellites panel. |
| User changes ground-station coords mid-pass | 1 Hz re-evaluate picks up the new station next tick; Doppler offset jumps once, then resumes smooth tracking. |
| User changes `KNOWN_SATELLITES` order at runtime | Can't happen — catalog is `&'static`. |
| User toggles master switch off mid-pass | Re-evaluate fires, trigger goes false, dispatch `SetVfoOffset(user_reference_offset)`, tracker dormant. |
| User toggles master switch on mid-pass while tuned to a satellite | Re-evaluate fires, trigger goes true, engage. |
| User retunes to a *different* satellite mid-pass | Re-evaluate fires, sees new active satellite, swaps. `user_reference_offset` is preserved across the swap (per §4 reset semantics) so the user's manual fine-tune survives. |
| Satellite drops below horizon while still tuned to its frequency | Re-evaluate fires, trigger goes false (elevation ≤ 0), dispatch final `SetVfoOffset(user_reference_offset)`, tracker dormant. |

## 9. Test plan

### 9.1 Pure-function tests for the Doppler math path

The math itself ships in `sdr-sat::track().doppler_shift_hz()` (already covered by `sdr-sat`'s own test suite). The new `compute_doppler_offset_hz` wrapper in `crates/sdr-ui/src/doppler_tracker.rs` is what gets tested here:

- **Known-pass shape test:** feed a captured TLE for NOAA 19 plus a station, find a real upcoming pass via `sdr_sat::upcoming_passes(...)`, sample `compute_doppler_offset_hz(...)` at AOS / TCA / LOS, and assert: AOS Doppler positive (approaching), LOS Doppler negative (receding), monotonic decrease across the pass, |TCA| < |AOS| AND |TCA| < |LOS| (radial velocity smallest at TCA).
- **Sign-convention regression pin:** at AOS, assert `range_rate_km_s < 0` (approaching) AND `compute_doppler_offset_hz(...) > 0` — opposite signs by the formula `Δf = -f₀ · ṙ / c`. Guards against an accidental sign flip in either layer.
- **Edge-case test:** `compute_doppler_offset_hz(..., carrier_hz = 0.0)` returns 0 regardless of geometry (formula multiplies by `frequency_hz`).

### 9.2 `DopplerTracker` unit tests in `sdr-ui`

`DopplerTracker` is testable headless — the state model holds no GTK / Rc / SourceId, so methods can be exercised directly. The trigger evaluation factors out as `pick_active_satellite(master_enabled, candidates) -> Option<&KnownSatellite>` (the caller does the SGP4 propagate to build candidates, the function picks the winner). Tests in `crates/sdr-ui/src/doppler_tracker.rs::tests` cover:

- `pick_active_satellite` matrix: master-off, no candidates, single match, below-horizon rejection, zero-elevation rejection, multi-sat tie-break by elevation, deterministic tie-break by candidate order, mixed above/below.
- `DopplerTracker` state transitions: master enable/disable (return-value contract, no-op short-circuit), `set_active` swap/disengage semantics (model-layer reset vs. wiring-layer compensation), additive `live_offset_hz`.

The wiring layer (`window.rs::connect_doppler_tracker`) is GTK + glib-timer territory; not headlessly testable. Verified via §9.3 smoke (real pass).

### 9.3 Smoke (live)

Real NOAA pass — observe the status bar showing the parabolic offset curve. Audio should stay locked through the entire pass instead of drifting.

## 10. Out of scope (explicitly)

- **SDR center-freq retune** — VFO-only is sufficient for everything we ship as of April 2026. Future-proofing left as an extension point in `DopplerTracker`'s tick handler, not pre-implemented.
- **Sparkline / Doppler graph** — the live numeric readout is enough for v1; a graph is a "fun visualization" rather than functional improvement.
- **Per-satellite tuning curves** — some users might want to bias their Doppler curve (e.g. for older satellites with known oscillator drift). Out of scope; the additive `user_reference_offset` covers ad-hoc cases without per-satellite persistence.
- **Geostationary satellites** — zero Doppler by definition; tracker would no-op, no special handling needed.
- **Future RX from MEO/HEO/deep-space probes** — Doppler can exceed VFO bandwidth; would need the SDR-retune branch we deferred. Reopen this design when there's a concrete user request.

## 11. Risk assessment

**Low overall.**

- **Hardware impact:** None — VFO offset is pure DSP. No `source.tune()` calls means no risk of dongle glitches, no risk of hitting tuner range limits.
- **Performance:** SGP4 propagation @ 4 Hz is microseconds of CPU per pass. Status-bar repaint @ 4 Hz is GTK's natural cadence.
- **Correctness:** The math is well-established (SGP4 + range-rate + Doppler equation are textbook). Test coverage in §9.1 pins the sign convention and the parabolic curve shape against a real captured TLE.
- **UX confusion:** The additive manual override (§4) is the one decision that could surprise users. Mitigated by the live status-bar readout — if Doppler is doing something the user didn't expect, it's visible, not silent.

The only thing this could break is "the user got used to manually riding the offset slider during a pass." For those users, flipping the master switch off restores prior behavior exactly. So even the regression path is a single toggle away.
