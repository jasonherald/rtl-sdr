//! "Next Orbcomm passes" section: pure pass collection over the
//! cached Orbcomm TLE candidates, row rendering, and the off-GTK-
//! thread TLE group refresh. Split out of `orbcomm_panel.rs` per the
//! Codacy 500-NLOC file gate (Task 7).

use std::collections::HashMap;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use gtk4::gio;
use gtk4::glib;
use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_sat::{GroundStation, Pass, Satellite, upcoming_passes};

use crate::sidebar::orbcomm_persistence::{load_orbcomm_sat_names, orbcomm_tles_from_cache};
use crate::sidebar::satellites_panel::{
    MIN_PASS_ELEVATION_DEG, PASS_LOOKAHEAD_HOURS, load_station_alt_m, load_station_lat_deg,
    load_station_lon_deg,
};
use crate::state::AppState;

use super::OrbcommPanelHandles;

/// Cap on the number of rendered "Next Orbcomm passes" rows.
pub(crate) const MAX_ORBCOMM_PASSES: usize = 12;

/// Recompute cadence (seconds) for the passes section. Cheap SGP4
/// sweep over a handful of candidates, no I/O — the countdown/rank
/// stays fresh even without a fresh TLE fetch.
const PASSES_REFRESH_TICK_SECS: u32 = 60;

/// Loop `candidates` through [`upcoming_passes`], skipping any
/// candidate whose elements are expired or fail to propagate over the
/// window, merge the results, sort by AOS (`start`), and cap at
/// [`MAX_ORBCOMM_PASSES`]. Mirrors
/// `satellites_panel::passes::enumerate_upcoming_passes` but over an
/// explicit candidate list (the Orbcomm TLE group) rather than
/// `KNOWN_SATELLITES`.
pub(crate) fn collect_orbcomm_passes(
    station: &GroundStation,
    candidates: &[(String, Satellite)],
    from: DateTime<Utc>,
    hours: i64,
    min_el_deg: f64,
) -> Vec<Pass> {
    let to = from
        .checked_add_signed(chrono::Duration::hours(hours))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    let mut passes = Vec::new();
    for (name, satellite) in candidates {
        match upcoming_passes(station, satellite, from, to, min_el_deg) {
            Ok(mut found) => passes.append(&mut found),
            // Expired elements or a propagation failure: skip this
            // candidate rather than surface a confident wrong pass —
            // same convention as `enumerate_upcoming_passes` (#718/#719).
            Err(e) => tracing::debug!("skipping Orbcomm pass candidate {name}: {e}"),
        }
    }
    passes.sort_by_key(|p| p.start);
    passes.truncate(MAX_ORBCOMM_PASSES);
    passes
}

/// Build the (initially empty) "Next Orbcomm passes" group.
pub(crate) fn build_passes_group() -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder()
        .title("Next Orbcomm passes")
        .description("Upcoming overhead windows for the cached ORBCOMM TLE group.")
        .build()
}

/// `true` if `name` matches a value in the learned `sat_id → name`
/// table — i.e. this candidate has been positively identified from a
/// decoded ephemeris this session.
fn is_heard(name: &str, heard: &HashMap<u8, String>) -> bool {
    heard.values().any(|n| n == name)
}

/// Format one pass row's title (name, heard-marked) and subtitle
/// (local AOS-LOS window + peak elevation).
fn format_orbcomm_pass_row(pass: &Pass, heard: &HashMap<u8, String>) -> (String, String) {
    let marker = if is_heard(&pass.satellite, heard) {
        "📡 "
    } else {
        ""
    };
    let title = format!("{marker}{}", pass.satellite);
    let start_local = pass.start.with_timezone(&chrono::Local);
    let end_local = pass.end.with_timezone(&chrono::Local);
    let subtitle = format!(
        "{}–{} (local) · {:.0}°",
        start_local.format("%H:%M"),
        end_local.format("%H:%M"),
        pass.max_elevation_deg,
    );
    (title, subtitle)
}

impl OrbcommPanelHandles {
    /// Rebuild the "Next Orbcomm passes" rows from a freshly computed
    /// pass list. Drops and rebuilds rather than diffing — the list is
    /// capped at [`MAX_ORBCOMM_PASSES`], so this is cheap.
    pub(crate) fn refresh_passes(&self, passes: &[Pass], heard: &HashMap<u8, String>) {
        let mut rows = self.passes_rows.borrow_mut();
        for row in rows.drain(..) {
            self.passes_group.remove(&row);
        }
        for pass in passes {
            let (title, subtitle) = format_orbcomm_pass_row(pass, heard);
            let row = adw::ActionRow::builder()
                .title(&title)
                .subtitle(&subtitle)
                .build();
            self.passes_group.add(&row);
            rows.push(row);
        }
    }
}

/// Re-run pass collection from `state.orbcomm_tles` against the
/// configured ground station and repaint the passes group. Cache-only
/// — never touches the network.
fn rebuild_passes(handles: &OrbcommPanelHandles, state: &Rc<AppState>) {
    let station = GroundStation::new(
        load_station_lat_deg(&state.config),
        load_station_lon_deg(&state.config),
        load_station_alt_m(&state.config),
    );
    let candidates = state.orbcomm_tles.borrow();
    let passes = collect_orbcomm_passes(
        &station,
        candidates.as_slice(),
        Utc::now(),
        PASS_LOOKAHEAD_HOURS,
        MIN_PASS_ELEVATION_DEG,
    );
    drop(candidates);
    handles.refresh_passes(&passes, &state.orbcomm_sat_names.borrow());
}

/// Cache-only seed: parse whatever Orbcomm group TLEs are already on
/// disk (no network) and load the learned `sat_id → name` table, so
/// the very first paint can show passes and mark heard birds without
/// waiting on a background fetch.
fn seed_orbcomm_state(state: &Rc<AppState>) {
    if let Some(cache) = state.orbcomm_tle_cache.borrow().as_ref() {
        *state.orbcomm_tles.borrow_mut() = orbcomm_tles_from_cache(cache);
    }
    *state.orbcomm_sat_names.borrow_mut() = load_orbcomm_sat_names(&state.config);
}

/// Wire the "Next Orbcomm passes" section: seed from the cache-only
/// candidate list + learned names, paint immediately, and rebuild
/// every [`PASSES_REFRESH_TICK_SECS`]. The panel lives for the app
/// lifetime (same convention as the By-Spacecraft aging tick in
/// `connect_orbcomm_panel`), so the tick holds a strong `Rc` and never
/// needs to stop.
pub(crate) fn wire_orbcomm_passes(handles: &Rc<OrbcommPanelHandles>, state: &Rc<AppState>) {
    seed_orbcomm_state(state);
    rebuild_passes(handles, state);

    let handles = Rc::clone(handles);
    let state = Rc::clone(state);
    glib::timeout_add_seconds_local(PASSES_REFRESH_TICK_SECS, move || {
        rebuild_passes(&handles, &state);
        glib::ControlFlow::Continue
    });
}

/// Kick a background Orbcomm TLE group refresh: `force_refresh_group`
/// runs on a `gio::spawn_blocking` worker thread — never blocks the
/// GTK thread on network I/O. On success, the returned triples are
/// parsed into `state.orbcomm_tles` and the passes section repaints;
/// on failure the error is logged and the previous candidate list is
/// left untouched. A no-op if the platform never gave us a TLE cache
/// directory (`state.orbcomm_tle_cache` is `None`).
///
/// Armed from two call sites: `on_orbcomm_enabled_changed`'s enabled
/// path, and the satellites-panel TLE-refresh button
/// (`window/satellites/passes.rs::finish_tle_refresh`) — both reuse
/// this single implementation rather than duplicating the
/// spawn/parse/repaint sequence.
pub(crate) fn refresh_orbcomm_tles(state: &Rc<AppState>) {
    let Some(cache) = state.orbcomm_tle_cache.borrow().clone() else {
        return;
    };
    let state = Rc::clone(state);
    glib::spawn_future_local(async move {
        let result =
            gio::spawn_blocking(move || cache.force_refresh_group(sdr_sat::ORBCOMM_TLE_GROUP))
                .await;
        apply_group_refresh_result(&state, result);
    });
}

/// Completion side of [`refresh_orbcomm_tles`]: parse a successful
/// fetch into `state.orbcomm_tles` and repaint; log and leave state
/// untouched on failure or a panicked worker task.
#[allow(clippy::type_complexity)]
fn apply_group_refresh_result(
    state: &Rc<AppState>,
    result: Result<
        Result<Vec<(String, String, String)>, sdr_sat::TleCacheError>,
        Box<dyn std::any::Any + Send>,
    >,
) {
    match result {
        Ok(Ok(triples)) => {
            let parsed: Vec<(String, Satellite)> = triples
                .into_iter()
                .filter_map(|(name, l1, l2)| {
                    Satellite::from_tle(&name, &l1, &l2).ok().map(|s| (name, s))
                })
                .collect();
            *state.orbcomm_tles.borrow_mut() = parsed;
            if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
                rebuild_passes(handles, state);
            }
        }
        Ok(Err(e)) => tracing::warn!("Orbcomm TLE group refresh failed: {e}"),
        Err(_) => tracing::warn!("Orbcomm TLE group refresh task panicked"),
    }
}
