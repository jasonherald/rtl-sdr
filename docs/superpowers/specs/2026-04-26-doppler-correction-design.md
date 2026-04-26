# Doppler Correction — Design

**Issue:** [#521](https://github.com/jasonherald/rtl-sdr/issues/521) (sub-ticket of [#520](https://github.com/jasonherald/rtl-sdr/issues/520) LRPT post-MVP enhancements)

**Goal:** Continuously correct the receive frequency for satellite Doppler shift during a pass, so the user's audio doesn't drift and the APT / LRPT / SSTV decoders stay locked.

**Cross-protocol:** applies to NOAA APT (epic [#468](https://github.com/jasonherald/rtl-sdr/issues/468), shipped), Meteor-M LRPT (epic [#469](https://github.com/jasonherald/rtl-sdr/issues/469), shipped), and ISS SSTV (epic [#472](https://github.com/jasonherald/rtl-sdr/issues/472), pending). Not just LRPT despite living under the LRPT post-MVP epic.

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
| Auto-record AOS tunes to NOAA 15 | Freq matches, satellite is by definition overhead → engages |
| User manually tunes to NOAA 15 mid-pass | Freq matches, satellite overhead → engages |
| User manually tunes to NOAA 15 between passes | Freq matches, but below horizon → does NOT engage (correctly!) |
| User tunes off-satellite | Freq doesn't match → disengages |
| User has no ground station coords set | "Above horizon" can't be evaluated → tracker stays dormant, status bar hidden |

No special "is auto-record running?" branch is needed; auto-record sets the frequency to the satellite, AOS makes the satellite overhead, and the auto-detect rule fires naturally.

## 3. Where the correction lands in the signal path

**`UiToDsp::SetVfoOffset(hz)` only.** Not an SDR `tune()` call.

Pure DSP shift, zero hardware churn, zero glitches. Limited to ±half the frontend bandwidth (typically 2.4 Msps with VFO bandwidth of ~50–250 kHz), so a ±3 kHz Doppler shift fits comfortably in the middle 5% of the window.

For everything we ship today (NOAA APT, Meteor LRPT, ISS SSTV), max Doppler is ±5 kHz over a full pass — VFO offset alone handles 100% of current passes without ever approaching the bandwidth limit.

**Out of scope for v1:** SDR center-frequency retune. Architectured-around (a future need for narrow-bandwidth modes or HEO/deep-space could add a coarse-retune branch in the same `DopplerTracker` without touching the activation rule), but not implemented today.

## 4. Manual-override behavior — additive

The user dragging the VFO offset slider while Doppler is active changes a per-session `user_reference_offset`. Doppler tracks **on top** of that:

```
live_offset = user_reference_offset + doppler_correction
```

So:

- User can fine-tune for personal taste (offset by +500 Hz to bias toward USB sideband, etc.) and Doppler still tracks correctly relative to that.
- A manual drag does NOT disable Doppler ("respectful of agency" but creates the "wait, why is Doppler off?" surprise — rejected).
- A manual drag is NOT overwritten on the next tick ("Doppler wins" — feels paternalistic, fights the user — rejected).

**Reset semantics:** `user_reference_offset` resets to 0 when the active satellite changes (new pass, different satellite, or trigger conditions go false). It does NOT persist across passes — Doppler tracking is per-pass, and so is any user fine-tune on top of it.

## 5. Range-rate / Doppler calculation — new `sdr-sat` API

```rust
// crates/sdr-sat/src/lib.rs (new public function)

/// Compute the predicted Doppler shift in Hz for the given
/// satellite at the given UTC instant, observed from the
/// given ground station. Sign convention: positive when
/// the satellite is approaching (receive frequency > nominal
/// carrier), negative when receding.
///
/// Pure function over SGP4 propagation — no caching, no I/O.
/// Caller (the UI's `DopplerTracker`) is responsible for
/// rate-limiting and threading.
pub fn doppler_offset_hz(
    satellite: &KnownSatellite,
    station: &GroundStation,
    when: chrono::DateTime<chrono::Utc>,
    tle: &Tle,
) -> Result<f64, DopplerError>;
```

**Math:**

1. SGP4 propagate the TLE to `when` → ECI position + velocity (km, km/s).
2. Convert to topocentric ENU (east-north-up) coordinates relative to `station`.
3. Range vector r = sat_pos − station_pos (m).
4. Range-rate ṙ = (r · v) / |r| (m/s, signed: positive = receding).
5. Doppler Δf = −ṙ · f_carrier / c (Hz). The negation makes "approaching" produce positive Δf, matching the table in §1.

**Carrier frequency:** read from `satellite.downlink_hz`. Not from the user's current center frequency — the catalog value is the truth and is what the SGP4-propagated geometry was implicitly computed for.

**Step 4 detail:** range-rate from ECI velocities can be computed without explicitly constructing the topocentric frame, since the dot product `r · v` is frame-invariant once both are expressed in the same Cartesian frame. We can just compute everything in ECI, which avoids a frame-conversion step. This is the standard "satellite Doppler from TLE" approach — see e.g. the [Vallado MATLAB code](https://www.celestrak.org/software/vallado-sw.php) or [predict4java](https://github.com/davidmoten/predict4java)'s `passDetails`.

**Reuse:** the existing TLE cache (`sdr-sat::tle_cache`, 24 h cached against Celestrak `gp.php?CATNR=…`) is the source of `Tle`. No new I/O. The existing `GroundStation` config (lat/lon/alt) is the source of `station`.

## 6. The `DopplerTracker` (sdr-ui)

New module: `crates/sdr-ui/src/doppler_tracker.rs`. One per `AppWindow`.

```rust
pub struct DopplerTracker {
    config: Arc<ConfigManager>,
    state: Rc<AppState>,
    enabled: Cell<bool>,                              // master switch
    active: RefCell<Option<ActiveSatellite>>,         // current trigger result
    user_reference_offset: Cell<f64>,                 // additive manual offset
    last_dispatched_offset: Cell<f64>,                // for change-detection
    tick_id: RefCell<Option<glib::SourceId>>,         // 4 Hz timer
    eval_id: RefCell<Option<glib::SourceId>>,         // 1 Hz re-evaluate trigger
}

struct ActiveSatellite {
    catalog: &'static KnownSatellite,
    tle: Tle,                                         // cached at engage time
    engaged_at: chrono::DateTime<chrono::Utc>,
}
```

**Two timers:**

1. **4 Hz tick** — recompute Doppler offset for the active satellite. Update the status bar label every tick (rounded to 0.1 kHz, so visual jitter is suppressed naturally). Dispatch `UiToDsp::SetVfoOffset(user_reference + doppler)` only when the new offset differs from `last_dispatched_offset` by more than 5 Hz — this rate-limits the controller bus on the sub-Hz wobble between SGP4 ticks without affecting the visible label or the actual tracking smoothness (5 Hz < 1/8 of a typical NFM bin width, well below audible).
2. **1 Hz re-evaluate** — re-check the trigger conditions (frequency match + above horizon + master switch). On state change (engage / disengage / satellite swap), tear down or reconfigure the 4 Hz tick.

The two timers are decoupled because trigger evaluation is cheap-and-rare while offset computation is cheap-and-frequent. Same module, same `Rc`, no cross-thread anything — both fire on the GTK main loop.

**SGP4 cost:** one propagation per 250 ms tick, plus one per 1 s re-evaluate. SGP4 is microseconds — total CPU cost is negligible.

## 7. UI surface

### 7.1 Master switch — Satellites panel

Add an `AdwSwitchRow` to the existing `AdwPreferencesGroup` in the Satellites activity panel (alongside auto-record APT / auto-record audio toggles):

```
🛰  Doppler tracking
    Auto-correct frequency drift during satellite passes.
```

Default **ON**. Persisted via new helper:

```rust
// crates/sdr-ui/src/sidebar/satellites_panel.rs
pub const KEY_DOPPLER_TRACKING_ENABLED: &str = "satellites_doppler_tracking";
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
| User retunes to a *different* satellite mid-pass | Re-evaluate fires, sees new active satellite, swaps. `user_reference_offset` resets to 0 (per §4 reset semantics). |
| Satellite drops below horizon while still tuned to its frequency | Re-evaluate fires, trigger goes false (elevation ≤ 0), dispatch final `SetVfoOffset(user_reference_offset)`, tracker dormant. |

## 9. Test plan

### 9.1 Pure-function tests in `sdr-sat`

- `doppler_offset_hz` known-pass test: feed a captured TLE for NOAA 15 plus a station and a sequence of UTC instants spanning AOS → TCA → LOS, assert Doppler curve is monotone-decreasing through the pass and crosses zero within ±15 s of TCA.
- Sign-convention test: synthetic "satellite directly approaching at 5 km/s" vs "directly receding at 5 km/s" — assert positive Doppler for approach, negative for recession.
- No-pass test: satellite below horizon → returns Doppler value anyway (the function is pure SGP4; the activation gate handles "not in pass"). Verify it doesn't error.

### 9.2 `DopplerTracker` unit tests in `sdr-ui`

`DopplerTracker` should be testable headless — its state machine is pure logic over inputs (current freq, master switch, ground station, TLE cache, time). Mirror the `satellites_recorder.rs` "pure tick → Vec<Action>" pattern: factor the trigger evaluation into a `fn evaluate(...) -> TriggerState` that takes inputs and returns a value, then unit-test `evaluate` against a matrix of (freq match × elevation × master switch × ground station present) cases.

### 9.3 Smoke (live)

Real NOAA pass — observe the status bar showing the parabolic offset curve. Audio should stay locked through the entire pass instead of drifting.

## 10. Out of scope (explicitly)

- **SDR center-freq retune** — VFO-only is sufficient for everything we ship today. Future-proofing left as an extension point in `DopplerTracker`'s tick handler, not pre-implemented.
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
