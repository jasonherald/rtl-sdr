//! Doppler-correction tracker (#521): master-switch persistence, the
//! 1 Hz overhead-pass trigger, and the SGP4 recompute loop that
//! dispatches VFO offsets above the dispatch threshold. Split out of
//! `window/satellites.rs` per the Codacy 500-NLOC file gate on
//! PR #844.

use super::super::{AppState, Duration, Rc, RefCell, SidebarPanels, StatusBar, glib, sidebar};

/// Doppler-correction tracker wiring (#521). `restore_doppler_switch`
/// runs ALWAYS — the persisted master-switch value survives a launch
/// even when the TLE cache is unavailable (CR round 1 on PR #554);
/// `connect_doppler_tracker` runs only with a cache, since without
/// TLEs there is no SGP4 state for the behavior to evaluate.
pub(super) fn wire_doppler(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    cache: Option<&std::sync::Arc<sdr_sat::TleCache>>,
    status_bar: &Rc<StatusBar>,
) {
    restore_doppler_switch(panels, config);
    if let Some(cache_doppler) = cache {
        connect_doppler_tracker(panels, state, cache_doppler, status_bar);
    }
}

/// Cadence of the Doppler tracker's trigger re-evaluation —
/// 1 Hz. Spec §2's overhead-and-frequency-match test only
/// needs to flip on horizon crossing / dial change, which is
/// always slower than 1 s. Cheap: one SGP4 propagate per
/// catalog entry within the ±20 kHz window — typically zero
/// or one sat at a time.
pub(super) const DOPPLER_TRIGGER_TICK: Duration = Duration::from_secs(1);

/// Cadence of the Doppler tracker's offset recompute — 4 Hz
/// (250 ms). Per spec §3, fast enough that the residual
/// frequency error between updates stays inside the channel
/// filter, slow enough that the bus + status-bar updates
/// don't hammer GTK.
pub(super) const DOPPLER_RECOMPUTE_TICK: Duration = Duration::from_millis(250);

/// Minimum |Δoffset| (Hz) before re-dispatching `SetVfoOffset`
/// from the 4 Hz recompute tick. Sub-5-Hz changes are below
/// the channel filter's pass-band granularity for any LEO
/// imaging downlink we care about, so suppressing them is
/// pure bus-traffic relief.
pub(super) const DOPPLER_DISPATCH_THRESHOLD_HZ: f64 = 5.0;

/// Restore the persisted Doppler master-switch state to the
/// widget and wire change-notify to save back. Always called,
/// regardless of TLE-cache availability — the user's preference
/// must survive a launch where the cache happened to be
/// unavailable. The behavioral wiring (timers + tracker) lives
/// in [`connect_doppler_tracker`] and is gated separately.
/// Per CR round 1 on PR #554.
pub(super) fn restore_doppler_switch(
    panels: &SidebarPanels,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let persisted = sidebar::satellites_panel::load_doppler_tracking_enabled(config);
    panels.satellites.doppler_switch.set_active(persisted);

    let config = std::sync::Arc::clone(config);
    panels
        .satellites
        .doppler_switch
        .connect_active_notify(move |row| {
            sidebar::satellites_panel::save_doppler_tracking_enabled(&config, row.is_active());
        });
}

/// Wire the [`DopplerTracker`](crate::doppler_tracker::DopplerTracker):
/// 1 Hz trigger re-evaluation tick, 4 Hz offset-recompute
/// tick, status-bar update, [`UiToDsp::SetVfoOffset`] dispatch
/// (rate-limited to changes >`DOPPLER_DISPATCH_THRESHOLD_HZ`).
/// Per #521 and the design spec at
/// `docs/superpowers/specs/2026-04-26-doppler-correction-design.md`.
///
/// Master-switch persistence + initial restore happens in
/// [`restore_doppler_switch`], which is called unconditionally
/// from [`connect_satellites_panel`]. This function adds a
/// **second** change-notify handler on the same widget that
/// drives the tracker model — multiple GTK signal handlers on
/// one widget fire independently, no conflict. Wired only when
/// the TLE cache is available; without TLEs the trigger
/// re-evaluate has no candidate to engage.
#[allow(
    clippy::too_many_lines,
    reason = "three chained closures (master-switch handler + two timers) all \
              live in one function so they share the `tracker` and \
              `last_dispatched` Rcs by direct clone; splitting would mean \
              hoisting those onto AppState, which the design spec §4 \
              already explicitly defers"
)]
pub(super) fn connect_doppler_tracker(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    status_bar: &Rc<StatusBar>,
) {
    use crate::doppler_tracker::DopplerTracker;

    // Read the widget's current state — it was already restored
    // (and a persistence handler wired) by `restore_doppler_switch`,
    // which runs unconditionally before we enter this cache-gated
    // path. Per CR round 1 on PR #554.
    let initial = panels.satellites.doppler_switch.is_active();

    let tracker: Rc<RefCell<DopplerTracker>> = Rc::new(RefCell::new(DopplerTracker::new(initial)));

    // The dispatch baseline lives on `AppState` as
    // `last_dispatched_vfo_offset_hz` — written by the
    // `connect_vfo_offset_changed` callback, which fires from
    // BOTH the DSP echo (`DspToUi::VfoOffsetChanged`) and direct
    // user-drag dispatches. The tracker reads from there for
    // its rate-limit gate, so external writes (auto-record AOS
    // reset, spectrum drag) keep the baseline in sync — no
    // stale local value to worry about. Per CR round 7 on PR
    // #554. The fallback paths below also write the baseline
    // directly when they dispatch a `SetVfoOffset(user_ref)`
    // flush, so re-engagement within `DOPPLER_DISPATCH_THRESHOLD_HZ`
    // of the prior live value isn't suppressed.

    // Master-switch handler that drives the tracker. (A separate
    // change-notify handler in `restore_doppler_switch` already
    // persists the value — multiple GTK signal handlers fire
    // independently, no conflict.) On disable, `set_master_enabled`
    // atomically clears `active`, captures and resets
    // `user_reference_offset_hz`, and returns the captured value
    // for us to flush to DSP.
    wire_doppler_master_switch(panels, &tracker, state, status_bar);

    // 1 Hz trigger re-evaluation tick: rebuild the candidate
    // list from catalog × frequency match × ground station ×
    // cached TLEs, run `pick_active_satellite`, and call
    // `set_active` on the tracker. On a transition to None
    // (e.g. user retunes off the satellite, or the satellite
    // sets), dispatch a final SetVfoOffset(user_reference) and
    // clear the status bar — same teardown the master-switch
    // handler does for the off-while-active case.
    {
        let tracker = Rc::clone(&tracker);
        let cache = std::sync::Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let panel_weak = panels.satellites.downgrade();
        let _ = glib::timeout_add_local(DOPPLER_TRIGGER_TICK, move || {
            let Some(panel) = panel_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            doppler_trigger_tick(&panel, &tracker, &cache, &state, &status_bar)
        });
    }

    // 4 Hz offset-recompute tick: while a satellite is active,
    // recompute the Doppler shift and dispatch a SetVfoOffset
    // (rate-limited to changes >DOPPLER_DISPATCH_THRESHOLD_HZ
    // to avoid spamming the bus). Update the status-bar label
    // every tick — the kHz/0.1 rounded format already hides
    // sub-100-Hz wobble, no further suppression needed.
    {
        let tracker = Rc::clone(&tracker);
        let cache = std::sync::Arc::clone(cache);
        let state = Rc::clone(state);
        let status_bar = Rc::clone(status_bar);
        let panel_weak = panels.satellites.downgrade();
        let _ = glib::timeout_add_local(DOPPLER_RECOMPUTE_TICK, move || {
            let Some(panel) = panel_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            doppler_recompute_tick(&panel, &tracker, &cache, &state, &status_bar)
        });
    }
}

/// Master-switch handler for the Doppler tracker: flips the tracker's
/// enabled state and, when a satellite was actively tracked, restores
/// the user's reference offset + clears the status-bar badge. Split
/// out per the 50-NLOC gate (#817).
fn wire_doppler_master_switch(
    panels: &SidebarPanels,
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) {
    let tracker = Rc::clone(tracker);
    let state = Rc::clone(state);
    let status_bar = Rc::clone(status_bar);
    panels
        .satellites
        .doppler_switch
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            let mut t = tracker.borrow_mut();
            let was_active = t.active().is_some();
            let final_offset = t.set_master_enabled(enabled);
            drop(t);
            // Only dispatch the fallback `SetVfoOffset` when
            // a satellite was actually being tracked. Without
            // this guard, toggling Doppler off while no
            // satellite is engaged would still send
            // `SetVfoOffset(0.0)` and clobber any non-zero
            // VFO offset the user had set independently. Per
            // CR round 3 on PR #554.
            if was_active && let Some(offset) = final_offset {
                state.dispatch_vfo_offset(offset);
                status_bar.update_doppler(None);
            }
        });
}

/// One recompute-tick of the Doppler tracker: re-propagate the active
/// satellite's position and retune the VFO offset when the predicted
/// Doppler shift moved past the dispatch threshold. Split out per the
/// 50-NLOC gate (#817).
fn doppler_recompute_tick(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) -> glib::ControlFlow {
    use crate::doppler_tracker::{FREQ_MATCH_TOLERANCE_HZ, should_tick};

    // Lifecycle gate: master + running. The status-bar
    // badge clears on the first not-running tick so the
    // user gets immediate "Doppler is idle" feedback when
    // they press Stop — `update_doppler(None)` is
    // idempotent (set_visible(false) on an already-hidden
    // label is a no-op). Per #567.
    if !should_tick(tracker.borrow().master_enabled(), state.is_running.get()) {
        status_bar.update_doppler(None);
        return glib::ControlFlow::Continue;
    }
    let active_sat = tracker.borrow().active();
    let Some(sat) = active_sat else {
        return glib::ControlFlow::Continue;
    };
    // Has the user retuned away from the active satellite?
    // If so, disengage NOW rather than wait up to 1 s for
    // the trigger tick — otherwise stale Doppler keeps
    // dispatching against the new center frequency for up
    // to a full second. Per CR round 5 on PR #554.
    #[allow(
        clippy::cast_precision_loss,
        reason = "catalog downlinks sit in the 100s of MHz, well \
                  below f64's 2^53 mantissa ceiling"
    )]
    let downlink = sat.downlink_hz as f64;
    let current_freq = state.center_frequency.get();
    if (downlink - current_freq).abs() > FREQ_MATCH_TOLERANCE_HZ {
        abandon_doppler_tracking(tracker, state, status_bar);
        return glib::ControlFlow::Continue;
    }
    let Some(doppler) = predicted_doppler_hz(panel, cache, sat) else {
        return glib::ControlFlow::Continue;
    };
    let live = tracker.borrow().live_offset_hz(doppler);
    // Status bar updates every tick — the kHz/0.1
    // format hides sub-100-Hz jitter naturally.
    status_bar.update_doppler(Some(doppler));
    // SetVfoOffset is rate-limited to material changes.
    // Baseline lives on `AppState` and is kept in sync by
    // the `connect_vfo_offset_changed` callback (fires on
    // both DSP echo and direct user-drag dispatches). Per
    // CR round 7 on PR #554. We also write it eagerly at
    // dispatch so a fast back-to-back tick before the
    // echo round-trip doesn't over-dispatch — the echo
    // arrives later with the same value, harmless.
    let baseline = state.last_dispatched_vfo_offset_hz.get();
    if (live - baseline).abs() > DOPPLER_DISPATCH_THRESHOLD_HZ {
        state.dispatch_vfo_offset(live);
    }
    glib::ControlFlow::Continue
}

/// Drop the active satellite and restore the user's pre-tracking VFO
/// offset + a cleared status-bar badge (retune-away and off-while-
/// active both land here).
fn abandon_doppler_tracking(
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) {
    let mut t = tracker.borrow_mut();
    let prior_user_ref = t.user_reference_offset_hz();
    let _ = t.set_active(None);
    drop(t);
    state.dispatch_vfo_offset(prior_user_ref);
    status_bar.update_doppler(None);
}

/// Propagate the active satellite and return its predicted Doppler
/// shift, or `None` when the TLE was evicted between trigger
/// evaluation and recompute (the next 1 Hz trigger tick drops the
/// active sat since `cached_tle_for` fails there too) or SGP4
/// propagation fails.
fn predicted_doppler_hz(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    sat: &sdr_sat::KnownSatellite,
) -> Option<f64> {
    use crate::doppler_tracker::compute_doppler_offset_hz;
    use sdr_sat::{GroundStation, Satellite};

    let station = GroundStation::new(
        panel.lat_row.value(),
        panel.lon_row.value(),
        panel.alt_row.value(),
    );
    let (line1, line2) = cache.cached_tle_for(sat.norad_id).ok()?;
    let parsed = Satellite::from_tle(sat.name, &line1, &line2).ok()?;
    let now = chrono::Utc::now();
    #[allow(
        clippy::cast_precision_loss,
        reason = "catalog downlinks sit in the 100s of MHz, well \
                  below f64's 2^53 mantissa ceiling"
    )]
    let carrier = sat.downlink_hz as f64;
    compute_doppler_offset_hz(&parsed, &station, now, carrier)
        .map_err(|_| {
            tracing::debug!(
                satellite = sat.name,
                "Doppler recompute: SGP4 propagate failed; skipping tick"
            );
        })
        .ok()
}

/// One 1 Hz trigger-tick of the Doppler tracker: rebuild the
/// overhead-candidate list from fresh TLE propagation, elect the
/// active satellite, and apply the activation / deactivation edge to
/// the VFO + status bar. Split out per the 50-NLOC gate (#817).
fn doppler_trigger_tick(
    panel: &sidebar::satellites_panel::SatellitesPanel,
    tracker: &Rc<RefCell<crate::doppler_tracker::DopplerTracker>>,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
    state: &Rc<AppState>,
    status_bar: &Rc<StatusBar>,
) -> glib::ControlFlow {
    use crate::doppler_tracker::{pick_active_satellite, should_tick};
    use sdr_sat::GroundStation;

    let mut t = tracker.borrow_mut();
    // Lifecycle gate: master + running. While stopped,
    // no candidate rebuild + no `set_active` transition,
    // so a satellite setting below the horizon mid-stop
    // doesn't fire a spurious disengage dispatch into a
    // stopped DSP. On resume, this tick re-evaluates and
    // engages / disengages naturally against the live
    // geometry. Per #567.
    if !should_tick(t.master_enabled(), state.is_running.get()) {
        return glib::ControlFlow::Continue;
    }
    // Build the ground station from the live panel
    // values — the user can edit lat/lon/alt mid-pass
    // and the tracker should follow.
    let station = GroundStation::new(
        panel.lat_row.value(),
        panel.lon_row.value(),
        panel.alt_row.value(),
    );
    let now = chrono::Utc::now();
    let current_freq = state.center_frequency.get();

    // Build the candidate list: every catalog entry
    // whose downlink is within ±FREQ_MATCH_TOLERANCE_HZ
    // of the radio's current centre frequency, paired
    // with its currently-evaluated elevation. Iterate
    // in `KNOWN_SATELLITES` order so the spec §2
    // tie-break (earlier entry wins) is deterministic.
    let candidates = overhead_candidates(&station, now, current_freq, cache);

    let new_active = pick_active_satellite(t.master_enabled(), &candidates);
    // Capture pre-`set_active` state so we can:
    //   1. Flush back to the prior user reference on a
    //      Some → None disengage (`set_active` resets
    //      `user_reference_offset_hz` to 0 on any change,
    //      so reading it AFTER would always give 0).
    //   2. Decide whether this is a fresh engagement
    //      (None → Some) vs. a satellite swap
    //      (Some(A) → Some(B)) — only the former should
    //      seed `user_reference_offset_hz` from the live
    //      spectrum offset. On a swap, the live offset
    //      is `prior_user_ref + prior_doppler`; reseeding
    //      with that would copy the previous pass's
    //      Doppler into the new pass's baseline (a
    //      double-count). Per CR round 4 on PR #554.
    let prior_user_ref = t.user_reference_offset_hz();
    let prior_active_some = t.active().is_some();
    let changed = t.set_active(new_active);
    if changed {
        if new_active.is_some() {
            if prior_active_some {
                // Some(A) → Some(B) satellite swap.
                // Restore the pre-swap user_reference
                // (which `set_active` just reset to 0)
                // so it survives the satellite change.
                // Per CR round 5 on PR #554.
                t.set_user_reference_offset_hz(prior_user_ref);
            } else {
                // None → Some fresh engagement. Seed
                // `user_reference_offset_hz` from the
                // synchronously-tracked DSP baseline on
                // `AppState` so this pass's Doppler tracks
                // ON TOP of any offset the user had set
                // before AOS — and so disengage at LOS
                // restores that exact value via the
                // Some → None flush path.
                //
                // Round 6 deferred this seed because the
                // only available source was `spectrum.vfo_offset_hz()`,
                // which lags DSP echoes — auto-record's
                // AOS-side `SetVfoOffset(0.0)` would not yet
                // be reflected when the trigger tick fired,
                // so we'd capture the stale pre-AOS value.
                // Round 7 added `state.last_dispatched_vfo_offset_hz`,
                // which the `connect_vfo_offset_changed`
                // callback updates on every DSP echo (and
                // every direct user-drag dispatch). That
                // gives us the synchronously-tracked source
                // of truth the deferral was waiting for.
                // Per CR round 9 on PR #554.
                let baseline = state.last_dispatched_vfo_offset_hz.get();
                t.set_user_reference_offset_hz(baseline);
            }
            // No dispatch here — the next 4 Hz tick will
            // dispatch `live = user_reference + doppler`.
        } else {
            // Disengaged — flush the live offset back to
            // the pre-engage user reference (captured
            // before `set_active` reset it) and clear
            // the status badge.
            //
            // We don't need to explicitly clear the
            // tracker's `user_reference_offset_hz` here
            // — `set_active(None)` already did it on
            // line 216 of `doppler_tracker.rs` (the
            // `if changed { self.user_reference_offset_hz = 0.0; }`
            // branch), and the
            // `satellite_to_none_resets_user_reference_offset`
            // unit test pins that invariant. The
            // `prior_user_ref` we dispatch is the value
            // captured pre-`set_active`, so DSP gets the
            // user's pre-engage baseline; the tracker's
            // own field is already 0 for the next
            // engagement. Per CR round 8 on PR #554.
            drop(t);
            state.dispatch_vfo_offset(prior_user_ref);
            status_bar.update_doppler(None);
        }
    }
    glib::ControlFlow::Continue
}

/// Build the Doppler candidate list: every catalog entry whose
/// downlink is within ±`FREQ_MATCH_TOLERANCE_HZ` of the radio's
/// current centre frequency, paired with its currently-evaluated
/// elevation. Iterates in `KNOWN_SATELLITES` order so the spec §2
/// tie-break (earlier entry wins) is deterministic.
fn overhead_candidates(
    station: &sdr_sat::GroundStation,
    now: chrono::DateTime<chrono::Utc>,
    current_freq: f64,
    cache: &std::sync::Arc<sdr_sat::TleCache>,
) -> Vec<crate::doppler_tracker::Candidate> {
    use crate::doppler_tracker::{Candidate, FREQ_MATCH_TOLERANCE_HZ};
    use sdr_sat::{KNOWN_SATELLITES, Satellite, track};

    let mut candidates: Vec<Candidate> = Vec::new();
    for sat in KNOWN_SATELLITES {
        #[allow(
            clippy::cast_precision_loss,
            reason = "catalog downlinks sit in the 100s of MHz, well \
                      below f64's 2^53 mantissa ceiling"
        )]
        let downlink = sat.downlink_hz as f64;
        if (downlink - current_freq).abs() > FREQ_MATCH_TOLERANCE_HZ {
            continue;
        }
        let Ok((line1, line2)) = cache.cached_tle_for(sat.norad_id) else {
            continue;
        };
        let Ok(parsed) = Satellite::from_tle(sat.name, &line1, &line2) else {
            continue;
        };
        let Ok(track) = track(station, &parsed, now) else {
            continue;
        };
        candidates.push(Candidate {
            satellite: sat,
            elevation_deg: track.elevation_deg,
        });
    }
    candidates
}
