//! WEFAX auto-catch wiring (epic #913, Task 6): connects the docked
//! panel's Decode switch + station combo to the pure `WefaxCatcher`
//! state machine, and drives it with a ~500 ms `glib` tick loop.
//! Mirrors `window::satellites::recorder` + `window::satellites::tick`'s
//! split between a pure state machine, an action interpreter, and a
//! GTK-timer driver.

use std::rc::Rc;
use std::time::Duration;

use chrono::Timelike;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

mod catcher;
use catcher::{WefaxDeps, interpret_wefax_action};

use crate::sidebar::SidebarPanels;
use crate::sidebar::satellites_panel::{load_station_lat_deg, load_station_lon_deg};
use crate::sidebar::wefax_catcher::{Action, CatcherState, SavedTune, StationChoice, TickCtx};
use crate::sidebar::wefax_panel::WefaxPanelHandles;
use crate::state::AppState;
use sdr_types::DemodMode;

use super::{TuneCtx, tune_to_target};

/// Channel bandwidth to mirror when tuning to a station picked from the
/// "Fax stations" list (#919). WEFAX's passband is locked
/// (`WefaxDemodulator`'s `bandwidth_locked` config in `sdr-radio`), so
/// this only matters for the UI mirror (bandwidth row + status bar) —
/// the DSP clamps any `SetBandwidth` to the demod's fixed min==max==
/// default range regardless. Matches `sdr-radio`'s private
/// `WEFAX_DEFAULT_BANDWIDTH`.
const WEFAX_TUNE_BANDWIDTH_HZ: f64 = 2_400.0;

/// Tick interval for the auto-catch driver (ms). Fast enough that a
/// user watching the status row sees the Scanning -> Signal found ->
/// Imaging cycle move promptly; cheap enough (a handful of float ops
/// plus a `Vec` build over a 4-station catalog) to run indefinitely
/// while enabled.
const WEFAX_TICK_INTERVAL_MS: u64 = 500;

/// Wire the WEFAX activity panel: stash its handles on `AppState`,
/// populate the station combo from the ground-station catalog, and
/// wire the Decode enable switch (scanner mutual-exclusion, tune
/// -snapshot, tick-driver spawn).
pub(super) fn connect_wefax_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    tune_ctx: &TuneCtx,
    toast_overlay: &adw::ToastOverlay,
) {
    let handles = Rc::clone(&panels.wefax.handles);
    *state.wefax_panel_handles.borrow_mut() = Some(Rc::clone(&handles));

    populate_station_combo(&handles, state);

    let deps = Rc::new(WefaxDeps {
        state: Rc::clone(state),
        parent_provider: build_parent_provider(panels),
        toast_overlay: toast_overlay.downgrade(),
    });

    populate_stations_group(&handles, state, tune_ctx, &deps);

    let scanner_switch = panels.scanner.master_switch.clone();
    let bandwidth_row = panels.radio.bandwidth_row.clone();
    let state_for_switch = Rc::clone(state);
    let handles_for_switch = Rc::clone(&handles);
    handles
        .enable_switch
        .clone()
        .connect_active_notify(move |sw| {
            if handles_for_switch.suppress_switch_notify.get() {
                return;
            }
            on_enable_toggled(
                sw,
                &state_for_switch,
                &handles_for_switch,
                &scanner_switch,
                &bandwidth_row,
                &deps,
            );
        });
}

/// Parent-window resolver for [`Action::OpenViewer`]. Walks up the
/// widget tree from the WEFAX page; falls back to `None` if the
/// widget has been detached. Weak ref so this closure — captured by
/// the enable switch's signal handler for the app's lifetime — can't
/// keep the panel widget (and transitively the window) alive past
/// teardown. Mirrors `window::satellites::recorder`'s
/// `parent_provider_for_recorder`.
fn build_parent_provider(panels: &SidebarPanels) -> Rc<dyn Fn() -> Option<gtk4::Window>> {
    let widget_weak = panels.wefax.widget.downgrade();
    Rc::new(move || {
        widget_weak
            .upgrade()
            .and_then(|w| w.root())
            .and_then(|r| r.downcast::<gtk4::Window>().ok())
    })
}

/// Populate the station combo: "Auto (nearest)" first, then every
/// catalog station nearest-first with its distance, ranked from the
/// persisted ground-station coordinates at connect time. `idx == 0`
/// means [`StationChoice::Auto`]; `idx - 1` indexes `station_names`, the
/// ordered list captured HERE (not re-derived), so
/// [`current_station_choice`] can never desync from the displayed rows
/// even if the ground-station coordinates later change.
fn populate_station_combo(handles: &WefaxPanelHandles, state: &Rc<AppState>) {
    let lat = load_station_lat_deg(&state.config);
    let lon = load_station_lon_deg(&state.config);
    let ranked = sdr_sat::stations_by_distance(lat, lon);
    let mut labels: Vec<String> = vec!["Auto (nearest)".to_string()];
    labels.extend(
        ranked
            .iter()
            .map(|(s, km)| format!("{} ({km:.0} km)", s.name)),
    );
    // Capture the ordered station identities behind the rows (after the
    // row-0 "Auto" entry) so the selection resolves to a stable name.
    *handles.station_names.borrow_mut() = ranked.iter().map(|(s, _)| s.name).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    handles
        .station_row
        .set_model(Some(&gtk4::StringList::new(&refs)));
    handles.station_row.set_selected(0);
}

/// Populate the "Fax stations" group: one activatable row per
/// station+channel from `sdr_sat::channels_by_distance`, nearest-first,
/// ranked from the persisted ground-station coordinates at connect
/// time (mirrors `populate_station_combo` — not re-derived on later
/// coordinate changes). Each row's `(station, freq_hz)` identity is
/// baked directly into its own `connect_activated` closure, so the
/// click handler never needs to re-rank or look the row up by index.
fn populate_stations_group(
    handles: &Rc<WefaxPanelHandles>,
    state: &Rc<AppState>,
    tune_ctx: &TuneCtx,
    deps: &Rc<WefaxDeps>,
) {
    let lat = load_station_lat_deg(&state.config);
    let lon = load_station_lon_deg(&state.config);
    for (station, freq_hz, distance_km) in sdr_sat::channels_by_distance(lat, lon) {
        let row = build_station_row(station, freq_hz, distance_km);
        handles.stations_group.add(&row);
        let state = Rc::clone(state);
        let handles = Rc::clone(handles);
        let deps = Rc::clone(deps);
        let tune_ctx = tune_ctx.clone();
        row.connect_activated(move |_| {
            tune_and_camp(&state, &handles, &deps, &tune_ctx, station, freq_hz);
        });
    }
}

/// Build one "Fax stations" row: title = station + channel frequency,
/// subtitle = distance + rough propagation-vs-time band hint.
fn build_station_row(station: &str, freq_hz: u64, distance_km: f64) -> adw::ActionRow {
    let title = format!("{station} — {}", sdr_sat::format_channel_khz(freq_hz));
    let subtitle = format!("{distance_km:.0} km · {} band", sdr_sat::band_hint(freq_hz));
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build()
}

/// A "Fax stations" row was clicked: turn auto-catch off first (a
/// manual pick and the scan rotation are mutually exclusive), then
/// tune-and-camp on the exact channel shown, through the same full
/// tune path (`tune_to_target`) bookmark recall and satellite play
/// use — header, spectrum, demod dropdown, bandwidth, and status bar
/// all mirror the pick, unlike auto-catch's raw `UiToDsp::Tune`.
fn tune_and_camp(
    state: &Rc<AppState>,
    handles: &Rc<WefaxPanelHandles>,
    deps: &Rc<WefaxDeps>,
    tune_ctx: &TuneCtx,
    station: &'static str,
    freq_hz: u64,
) {
    stop_auto_catch_before_manual_tune(state, handles, deps);
    tracing::info!(
        target: "wefax_station_picker",
        station,
        freq_hz,
        "STATION_ROW_TUNE"
    );
    tune_to_target(
        tune_ctx,
        freq_hz,
        DemodMode::Wefax,
        WEFAX_TUNE_BANDWIDTH_HZ,
        "WEFAX station picker",
    );
}

/// Turn auto-catch off (if on) and force any pending restore-tune to
/// land NOW rather than on the next scheduled ~500 ms tick. Without
/// this, the tick loop's `Action::RestoreTune` (queued for whenever it
/// next fires) would race the manual tune `tune_and_camp` is about to
/// send and could clobber it — the two paths must never fight over the
/// receiver.
fn stop_auto_catch_before_manual_tune(
    state: &Rc<AppState>,
    handles: &Rc<WefaxPanelHandles>,
    deps: &Rc<WefaxDeps>,
) {
    if state.wefax_enabled.get() {
        handles.suppress_switch_notify.set(true);
        handles.enable_switch.set_active(false);
        handles.suppress_switch_notify.set(false);
        state.wefax_enabled.set(false);
    }
    // Always drive the catcher's disable tick synchronously — including
    // when the user already switched auto-catch off and the timer hasn't
    // ticked since — so a still-pending `RestoreTune` is interpreted (and
    // sent to the DSP) before the manual tune. A disabled tick on an idle
    // catcher emits nothing, so this is a no-op when nothing is pending.
    tick_once(state, deps);
}

/// Enable-switch handler: refuse (toast + revert) while the scanner
/// is running, otherwise snapshot the tune, arm `wefax_enabled`, and
/// spawn the tick driver. On disable, just clear `wefax_enabled` —
/// the next tick's `go_idle` restores the snapshotted tune.
fn on_enable_toggled(
    sw: &gtk4::Switch,
    state: &Rc<AppState>,
    handles: &Rc<WefaxPanelHandles>,
    scanner_switch: &gtk4::Switch,
    bandwidth_row: &adw::SpinRow,
    deps: &Rc<WefaxDeps>,
) {
    if !sw.is_active() {
        state.wefax_enabled.set(false);
        return;
    }
    if scanner_switch.is_active() {
        handles.suppress_switch_notify.set(true);
        sw.set_active(false);
        handles.suppress_switch_notify.set(false);
        interpret_wefax_action(
            deps,
            Action::Toast("WEFAX auto-catch is unavailable while the scanner is running".into()),
        );
        return;
    }
    *state.wefax_saved_tune.borrow_mut() = snapshot_saved_tune(state, bandwidth_row);
    state.wefax_enabled.set(true);
    spawn_wefax_tick(state, Rc::clone(deps));
}

/// Snapshot the user's current tune + demod mode + channel bandwidth as
/// a [`SavedTune`]. The mode is stored as the [`sdr_types::DemodMode`]
/// enum directly (the pure state machine needn't know the header
/// dropdown's index presentation); the bandwidth is read from the radio
/// panel's spin row so it can be restored after `SetDemodMode` resets it
/// to the mode default.
fn snapshot_saved_tune(state: &Rc<AppState>, bandwidth_row: &adw::SpinRow) -> SavedTune {
    let mode = state.demod_mode.get();
    SavedTune {
        center_hz: state.center_frequency.get(),
        demod_mode: mode,
        bandwidth_hz: bandwidth_row.value(),
        was_wefax: mode == sdr_types::DemodMode::Wefax,
    }
}

/// Install the ~500 ms tick loop. No-op (returns immediately) if a
/// loop is already armed — guards the enable switch being flipped
/// off and back on before the prior loop's final (restore) tick has
/// had a chance to run and disarm it.
fn spawn_wefax_tick(state: &Rc<AppState>, deps: Rc<WefaxDeps>) {
    if state.wefax_tick_armed.replace(true) {
        return;
    }
    let state = Rc::clone(state);
    glib::timeout_add_local(Duration::from_millis(WEFAX_TICK_INTERVAL_MS), move || {
        tick_once(&state, &deps);
        let idle = matches!(*state.wefax_catcher.borrow().state(), CatcherState::Idle);
        if !state.wefax_enabled.get() && idle {
            state.wefax_tick_armed.set(false);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// One tick: build a [`TickCtx`] from cached DSP-event state + the
/// ground-station coordinates + the station combo's current
/// selection, advance the catcher, and interpret every action it
/// returns.
fn tick_once(state: &Rc<AppState>, deps: &Rc<WefaxDeps>) {
    let lat = load_station_lat_deg(&state.config);
    let lon = load_station_lon_deg(&state.config);
    let ctx = TickCtx {
        enabled: state.wefax_enabled.get(),
        choice: current_station_choice(state),
        user_lat: lat,
        user_lon: lon,
        now_min: now_min_utc(),
        wefax_state: state.wefax_last_state.get(),
        fax_present: state.wefax_present.get(),
        saved_tune: state.wefax_saved_tune.borrow().clone(),
    };
    let actions = state.wefax_catcher.borrow_mut().tick(ctx);
    for action in actions {
        interpret_wefax_action(deps, action);
    }
    refresh_status_label(state);
}

/// Read the station combo's current selection: index 0 is
/// [`StationChoice::Auto`]; any other index pins the rotation to the
/// station identity captured in `station_names` when the model was
/// built. Resolving against the stored list — rather than a live
/// `stations_by_distance` recompute — keeps the selection aligned with
/// the displayed rows even if the ground-station coordinates change.
fn current_station_choice(state: &Rc<AppState>) -> StationChoice {
    let Some(handles) = state.wefax_panel_handles.borrow().clone() else {
        return StationChoice::Auto;
    };
    let idx = handles.station_row.selected();
    if idx == 0 {
        return StationChoice::Auto;
    }
    let pos = (idx - 1) as usize;
    handles
        .station_names
        .borrow()
        .get(pos)
        .map_or(StationChoice::Auto, |&name| StationChoice::Pinned(name))
}

/// Minutes past 0000Z for the current instant, saturating rather than
/// panicking on the (never-hit-in-practice) overflow path — mirrors
/// `sdr_sat::wefax_stations`'s private `minute_of_day` helper, which
/// isn't exported.
fn now_min_utc() -> u16 {
    let now = chrono::Utc::now();
    u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(u16::MAX)
}

/// Push a status string reflecting the catcher's current phase (and,
/// while `Imaging`, the decoder's own phase) into the panel's status
/// row. Called after every tick; also called from
/// `dsp_events::on_wefax_state` so a decoder-phase change between
/// ticks (e.g. `Imaging -> Stopped`) shows up immediately rather than
/// waiting up to [`WEFAX_TICK_INTERVAL_MS`].
fn refresh_status_label(state: &Rc<AppState>) {
    let Some(handles) = state.wefax_panel_handles.borrow().clone() else {
        return;
    };
    let text = wefax_status_text(
        state.wefax_catcher.borrow().state(),
        state.wefax_last_state.get(),
    );
    handles.status_label.set_label(&text);
}

/// Render the catcher's phase (+ decoder phase while `Imaging`) as
/// the short status string the panel's "State" row shows.
pub(super) fn wefax_status_text(
    catcher: &CatcherState,
    decoder: sdr_core::messages::WefaxState,
) -> String {
    match catcher {
        CatcherState::Idle => "Idle".to_string(),
        CatcherState::Scanning {
            candidates, idx, ..
        } => candidates.get(*idx).map_or_else(
            || "Scanning".to_string(),
            |c| format!("Scanning — {} ({} Hz)", c.station, c.freq_hz),
        ),
        CatcherState::Locked { cand, .. } => {
            format!("Signal found — {} ({} Hz)", cand.station, cand.freq_hz)
        }
        CatcherState::Imaging { cand, .. } => {
            format!(
                "Imaging — {} ({} Hz) [{decoder:?}]",
                cand.station, cand.freq_hz
            )
        }
    }
}

/// Called from `dsp_events::on_wefax_state` to refresh the status
/// label the moment the decoder phase changes, without waiting for
/// the next tick. No-op if the panel handles aren't stashed yet or
/// auto-catch was never enabled this session (both benign — the
/// label already shows "Idle" from `build_wefax_panel`).
pub(super) fn on_decoder_state_changed(state: &Rc<AppState>) {
    refresh_status_label(state);
}
