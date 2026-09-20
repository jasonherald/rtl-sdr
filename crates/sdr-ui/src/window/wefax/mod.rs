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

use crate::header::demod_selector;
use crate::sidebar::SidebarPanels;
use crate::sidebar::satellites_panel::{load_station_lat_deg, load_station_lon_deg};
use crate::sidebar::wefax_catcher::{Action, CatcherState, SavedTune, StationChoice, TickCtx};
use crate::sidebar::wefax_panel::WefaxPanelHandles;
use crate::state::AppState;

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
    toast_overlay: &adw::ToastOverlay,
) {
    let handles = Rc::clone(&panels.wefax.handles);
    *state.wefax_panel_handles.borrow_mut() = Some(Rc::clone(&handles));

    populate_station_combo(&handles.station_row, state);

    let deps = Rc::new(WefaxDeps {
        state: Rc::clone(state),
        parent_provider: build_parent_provider(panels),
        toast_overlay: toast_overlay.downgrade(),
    });

    let scanner_switch = panels.scanner.master_switch.clone();
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
/// means [`StationChoice::Auto`]; `idx - 1` indexes this SAME ranking
/// (recomputed identically in [`current_station_choice`]) — the
/// ranking is stable because the coordinates it's built from only
/// change via the Satellites panel, and re-ranking on every keypress
/// there is out of scope for v1.
fn populate_station_combo(station_row: &adw::ComboRow, state: &Rc<AppState>) {
    let lat = load_station_lat_deg(&state.config);
    let lon = load_station_lon_deg(&state.config);
    let ranked = sdr_sat::stations_by_distance(lat, lon);
    let mut labels: Vec<String> = vec!["Auto (nearest)".to_string()];
    labels.extend(
        ranked
            .iter()
            .map(|(s, km)| format!("{} ({km:.0} km)", s.name)),
    );
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    station_row.set_model(Some(&gtk4::StringList::new(&refs)));
    station_row.set_selected(0);
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
    *state.wefax_saved_tune.borrow_mut() = snapshot_saved_tune(state);
    state.wefax_enabled.set(true);
    spawn_wefax_tick(state, Rc::clone(deps));
}

/// Snapshot the user's current tune + demod mode as a [`SavedTune`],
/// encoding the mode via the header dropdown's own index table
/// (`demod_selector`) so `SavedTune::demod_mode`'s u8 stays in lock
/// -step with the one authoritative mode <-> index mapping in the
/// codebase rather than a second, hand-rolled one here.
fn snapshot_saved_tune(state: &Rc<AppState>) -> SavedTune {
    let mode = state.demod_mode.get();
    let idx = demod_selector::demod_mode_to_index(mode).unwrap_or(0);
    SavedTune {
        center_hz: state.center_frequency.get(),
        demod_mode: u8::try_from(idx).unwrap_or(0),
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
        choice: current_station_choice(state, lat, lon),
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
/// [`StationChoice::Auto`]; any other index pins the rotation to that
/// ranked station's name. Recomputes the same `stations_by_distance`
/// ranking [`populate_station_combo`] used to build the combo's
/// model, so the index lines up with the displayed row.
fn current_station_choice(state: &Rc<AppState>, lat: f64, lon: f64) -> StationChoice {
    let Some(handles) = state.wefax_panel_handles.borrow().clone() else {
        return StationChoice::Auto;
    };
    let idx = handles.station_row.selected();
    if idx == 0 {
        return StationChoice::Auto;
    }
    let ranked = sdr_sat::stations_by_distance(lat, lon);
    let pos = (idx - 1) as usize;
    ranked
        .get(pos)
        .map_or(StationChoice::Auto, |(s, _)| StationChoice::Pinned(s.name))
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
