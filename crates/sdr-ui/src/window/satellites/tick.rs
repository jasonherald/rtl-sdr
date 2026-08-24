//! The 1 Hz countdown / auto-record tick: [`TickDeps`] capture
//! bundle, the per-tick `SavedTune` snapshot, the ACARS-gated action
//! dispatch, the pre-pass notify scheduler (#510), and the recorder
//! wiring entry point. Split out of `window/satellites.rs` per the
//! Codacy 500-NLOC file gate on PR #844.

use libadwaita::prelude::*;

use super::super::{
    AppState, Rc, RefCell, SidebarPanels, TuneCtx, TuneFn, UiToDsp, adw, glib, sidebar, spectrum,
};
use super::recorder::build_recorder_interpreter;
use super::{DisplayedPass, SATELLITES_COUNTDOWN_TICK};

/// Owned captures for the 1 Hz countdown / auto-record tick. Widget
/// handles are strong clones — when the panel is dropped the tick's
/// `panel_weak.upgrade()` returns `None` and the source Breaks,
/// dropping the whole chain. The radio/scanner/doppler rows feed the
/// per-tick [`SavedTune`] snapshot so pre-AOS state can be restored at
/// LOS (#555/#556); `state` and `interpret` drive the recorder state
/// machine; `watched`/`notify_scheduler`/`config` drive the pre-pass
/// desktop alerts (#510).
struct TickDeps {
    panel_weak: sidebar::satellites_panel::SatellitesPanelWeak,
    state: Rc<AppState>,
    displayed: Rc<RefCell<Vec<DisplayedPass>>>,
    recompute: Rc<dyn Fn()>,
    interpret: Rc<dyn Fn(sidebar::satellites_recorder::Action)>,
    spectrum: Rc<spectrum::SpectrumHandle>,
    config: std::sync::Arc<sdr_config::ConfigManager>,
    watched: Rc<RefCell<std::collections::HashSet<u32>>>,
    bandwidth_row: adw::SpinRow,
    scanner_switch: gtk4::Switch,
    squelch_enabled_row: adw::SwitchRow,
    auto_squelch_row: adw::SwitchRow,
    squelch_level_row: adw::SpinRow,
    ctcss_row: adw::ComboRow,
    fm_if_nr_row: adw::SwitchRow,
    deemphasis_row: adw::ComboRow,
    notch_enabled_row: adw::SwitchRow,
    doppler_switch: adw::SwitchRow,
}

/// Per-tick [`SavedTune`] snapshot: centre frequency + user-dragged
/// VFO offset, demod mode, and the radio/scanner/doppler widget states
/// that AOS force-disables and LOS restores (#555/#556). f64 `SpinRow`
/// values are rounded at this boundary so the snapshot carries clean
/// integers — no per-restore rounding.
fn snapshot_saved_tune(deps: &TickDeps) -> sidebar::satellites_recorder::SavedTune {
    use sidebar::satellites_recorder::SavedTune;
    // Round f64 SpinRow value to u32 at the snapshot
    // boundary so SavedTune carries a clean integer for
    // the eventual restore — no per-restore rounding.
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "user-set bandwidth is non-negative and \
                  fits in u32 for any realistic SDR channel \
                  width; the SpinRow's own min is positive"
    )]
    let bandwidth_hz_u32 = deps.bandwidth_row.value().round() as u32;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "squelch SpinRow value is in dBFS, bounded by the row's \
                  configured min/max (well within f32 range)"
    )]
    let squelch_db_f32 = deps.squelch_level_row.value() as f32;
    SavedTune {
        freq_hz: deps.state.center_frequency.get(),
        vfo_offset_hz: deps.spectrum.vfo_offset_hz(),
        mode: deps.state.demod_mode.get(),
        bandwidth_hz: bandwidth_hz_u32,
        was_running: deps.state.is_running.get(),
        scanner_running: deps.scanner_switch.is_active(),
        squelch_enabled: deps.squelch_enabled_row.is_active(),
        auto_squelch_enabled: deps.auto_squelch_row.is_active(),
        squelch_db: squelch_db_f32,
        ctcss_mode: sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(
            deps.ctcss_row.selected(),
        ),
        fm_if_nr_enabled: deps.fm_if_nr_row.is_active(),
        deemphasis_idx: deps.deemphasis_row.selected(),
        notch_enabled: deps.notch_enabled_row.is_active(),
        doppler_enabled: deps.doppler_switch.is_active(),
    }
}

/// Apply one tick's recorder actions, honouring the ACARS-disengage
/// gate (issue #589): when a `StartAutoRecord` lands while ACARS is
/// engaged, the whole batch is stashed and replayed by the
/// `AcarsEnabledChanged(Ok(false))` arm in `handle_dsp_message` once
/// the controller acks the disengage.
fn dispatch_recorder_actions(deps: &TickDeps, actions: Vec<sidebar::satellites_recorder::Action>) {
    use sidebar::satellites_recorder::Action as RecorderAction;
    // ACARS-disengage gate (issue #589): if any action
    // in this tick is `StartAutoRecord` AND ACARS is
    // currently engaged, stash the **whole batch** and
    // dispatch `SetAcarsEnabled(false)`. The
    // `AcarsEnabledChanged(Ok(false))` arm in
    // `handle_dsp_message` will drain the batch and
    // replay every action through `(deps.interpret)` once
    // the controller acks the disengage.
    //
    // Stashing the whole batch (not just
    // `StartAutoRecord`) makes the disengage ack a real
    // gate: same-tick siblings like
    // `StartAutoAudioRecord` and `ResetImagingDecoders`
    // would otherwise execute while the source was
    // still on airband geometry, capturing audio from
    // the wrong frequency until the disengage lands.
    // CR round 1 on PR #591.
    let needs_acars_gate = deps.state.acars_enabled.get()
        && actions
            .iter()
            .any(|a| matches!(a, RecorderAction::StartAutoRecord { .. }));
    if needs_acars_gate {
        tracing::info!(
            "auto-record AOS: gating {} action(s) on ACARS disengage ack",
            actions.len()
        );
        deps.state.acars_was_engaged_pre_pass.set(true);
        *deps.state.pending_aos_actions.borrow_mut() = Some(actions);
        deps.state.send_dsp(UiToDsp::SetAcarsEnabled(false));
    } else {
        for action in actions {
            (deps.interpret)(action);
        }
    }
}

/// #510 — pre-pass desktop alerts. Walk the displayed pass list, map
/// each to `(norad_id, &Pass)`, feed the pure scheduler, then perform
/// the notification I/O its actions request. The lead time is read
/// fresh from config on every tick so a user edit takes effect
/// immediately without restarting the timer.
fn run_notify_tick(
    deps: &TickDeps,
    notify_scheduler: &RefCell<sidebar::satellites_notify::NotifyScheduler>,
    now: chrono::DateTime<chrono::Utc>,
) {
    use sdr_sat::Pass;
    use sidebar::satellites_notify::Action as NotifyAction;
    use sidebar::satellites_panel::{load_notify_lead_min, norad_id_for_pass};
    // #510 — pre-pass desktop alerts. Walk the displayed
    // pass list, map each to (norad_id, &Pass), feed the
    // scheduler. Pure function in / pure actions out;
    // notification I/O happens in the action loop below.
    let lead_min = load_notify_lead_min(&deps.config);
    let lead = chrono::Duration::minutes(i64::from(lead_min));
    let watched_snapshot = deps.watched.borrow().clone();
    let notify_actions = {
        let displayed_borrow = deps.displayed.borrow();
        let pairs: Vec<(u32, &Pass)> = displayed_borrow
            .iter()
            .filter_map(|e| norad_id_for_pass(&e.pass).map(|id| (id, &e.pass)))
            .collect();
        notify_scheduler
            .borrow_mut()
            .tick(now, lead, lead_min, pairs, |id| {
                watched_snapshot.contains(&id)
            })
    };
    for action in notify_actions {
        match action {
            NotifyAction::Fire {
                norad_id,
                pass,
                lead_min,
            } => {
                crate::notify::send_pass_alert(&pass, norad_id, lead_min);
            }
        }
    }
}

/// Schedule the 1 Hz countdown ticker + auto-record state machine
/// (#482b). One `GLib` source drives both: pass-row countdown titles and
/// the pure `AutoRecorder::tick`, whose actions the `interpret`
/// closure applies against the live UI / DSP / filesystem. Captures
/// the panel weakly so the source returns `ControlFlow::Break` once
/// the panel is dropped.
fn arm_recorder_tick(deps: TickDeps) {
    use sdr_sat::Pass;
    use sidebar::satellites_panel::{AutoRecordQuality, format_pass_title};

    let recorder = RefCell::new(sidebar::satellites_recorder::AutoRecorder::new());
    let notify_scheduler = RefCell::new(sidebar::satellites_notify::NotifyScheduler::new());

    let _ = glib::timeout_add_local(SATELLITES_COUNTDOWN_TICK, move || {
        let Some(panel) = deps.panel_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let now = chrono::Utc::now();
        let mut needs_recompute = false;
        for entry in deps.displayed.borrow().iter() {
            if entry.pass.end <= now {
                needs_recompute = true;
                continue;
            }
            entry.row.set_title(&format_pass_title(&entry.pass, now));
        }
        // Drive the auto-record state machine. Snapshot the
        // pass list (cloned out of the displayed vec to keep
        // the borrow short) and the current tune so the
        // recorder gets a consistent view. Capture the VFO
        // offset alongside centre frequency — a user-dragged
        // carrier position needs to survive the AOS→LOS round
        // trip.
        let passes_snapshot: Vec<Pass> = deps
            .displayed
            .borrow()
            .iter()
            .map(|e| e.pass.clone())
            .collect();
        let auto_record_on = panel.auto_record_switch.is_active();
        // Per #533: the "also save audio" toggle is sampled
        // exclusively at AOS by the state machine; flipping
        // it mid-pass does NOT retroactively start or stop
        // recording (matches `auto_record_on`'s
        // "in-flight pass keeps running" semantics).
        let audio_record_on = panel.auto_record_audio_switch.is_active();
        let now_tune = snapshot_saved_tune(&deps);
        // Read the user's selected quality tier on every
        // tick — cheap (just a ComboRow.selected() call), and
        // means a mid-pass change applies immediately to the
        // next eligible pass without a restart. Per #511.
        let min_elev_deg =
            AutoRecordQuality::from_index(panel.auto_record_quality_row.selected()).min_elev_deg();
        let actions = recorder.borrow_mut().tick(
            now,
            &passes_snapshot,
            auto_record_on,
            audio_record_on,
            min_elev_deg,
            now_tune,
        );
        dispatch_recorder_actions(&deps, actions);

        run_notify_tick(&deps, &notify_scheduler, now);

        if needs_recompute {
            (deps.recompute)();
        }
        glib::ControlFlow::Continue
    });
}

/// Shared wiring state built once by [`connect_satellites_panel`]
/// and threaded into the recorder/tick layer: the weak panel handle,
/// the displayed-pass list, the pass-list recompute closure, the
/// watched-satellite set (#510), and the optional TLE cache.
pub(super) struct SatWiring {
    pub(super) panel_weak: sidebar::satellites_panel::SatellitesPanelWeak,
    pub(super) displayed: Rc<RefCell<Vec<DisplayedPass>>>,
    pub(super) recompute: Rc<dyn Fn()>,
    pub(super) watched: Rc<RefCell<std::collections::HashSet<u32>>>,
    pub(super) cache: Option<std::sync::Arc<sdr_sat::TleCache>>,
}

/// Wire the auto-record machinery: the pure [`AutoRecorder`] state
/// machine, its action interpreter (stashed weakly on `AppState` so
/// the `AcarsEnabledChanged(Ok(false))` arm in `handle_dsp_message`
/// can replay deferred AOS actions — issue #589 / CR round 1 on
/// PR #591; the strong owner is the tick source), and the 1 Hz tick
/// that drives it. The tick is only armed when a TLE cache exists —
/// without one `displayed` stays empty forever and the timer would
/// tick uselessly.
pub(super) fn wire_recorder(
    panels: &SidebarPanels,
    tune_ctx: &TuneCtx,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    toast_overlay: &adw::ToastOverlay,
    tune_to_satellite: &Rc<TuneFn>,
    set_playing: &Rc<dyn Fn(bool)>,
    wiring: &SatWiring,
) {
    let state = &tune_ctx.state;
    let cache = wiring.cache.as_ref();
    // 1 Hz countdown ticker. Only scheduled when the cache is
    // available — without it `displayed` stays empty forever and
    // the timer would tick uselessly. Captures the panel weakly
    // so the source returns `ControlFlow::Break` once any panel
    // widget has been dropped (otherwise GLib runs it forever,
    // holding a strong chain into the `displayed` vec and its
    // widgets).
    // Auto-record-on-pass state machine (#482b). Driven from the
    // same 1 Hz tick that updates pass-row countdowns — no second
    // GLib source. The recorder itself is pure (returns
    // `Vec<RecorderAction>`); the closure below interprets each
    // action against the live UI / DSP / filesystem.

    let interpret_action = build_recorder_interpreter(
        panels,
        state,
        cache,
        toast_overlay,
        tune_to_satellite,
        set_playing,
    );

    // Stash a `Weak` handle to the interpreter on AppState so
    // the `AcarsEnabledChanged(Ok(false))` arm in
    // `handle_dsp_message` can replay deferred AOS actions
    // without needing the closure plumbed through its parameter
    // list. Stored weakly to avoid an `AppState` ↔ closure
    // retain cycle (the closure captures `Rc<AppState>`
    // transitively); the strong owner is the recorder tick
    // `glib::timeout_add_local`. Issue #589 / CR round 1 on
    // PR #591.
    *state.recorder_action_interpreter.borrow_mut() = Some(Rc::downgrade(&interpret_action));

    if cache.is_some() {
        arm_recorder_tick(TickDeps {
            panel_weak: wiring.panel_weak.clone(),
            state: Rc::clone(state),
            displayed: Rc::clone(&wiring.displayed),
            recompute: Rc::clone(&wiring.recompute),
            interpret: Rc::clone(&interpret_action),
            spectrum: Rc::clone(&tune_ctx.spectrum_handle),
            config: std::sync::Arc::clone(config),
            watched: Rc::clone(&wiring.watched),
            bandwidth_row: panels.radio.bandwidth_row.clone(),
            scanner_switch: panels.scanner.master_switch.clone(),
            squelch_enabled_row: panels.radio.squelch_enabled_row.clone(),
            auto_squelch_row: panels.radio.auto_squelch_row.clone(),
            squelch_level_row: panels.radio.squelch_level_row.clone(),
            ctcss_row: panels.radio.ctcss_row.clone(),
            fm_if_nr_row: panels.radio.fm_if_nr_row.clone(),
            deemphasis_row: panels.radio.deemphasis_row.clone(),
            notch_enabled_row: panels.radio.notch_enabled_row.clone(),
            doppler_switch: panels.satellites.doppler_switch.clone(),
        });
    }
}
