//! "Heard via Orbcomm" panel-group wiring (issue #865, Task 12).
//!
//! Rebuilds [`SatellitesPanel::heard_group`]'s rows from the pure
//! [`HeardSatellites`](sidebar::satellites_heard::HeardSatellites)
//! model on a 5 s tick, and immediately after every recorded event.
//! The model itself lives on `AppState::orbcomm_heard` (mirrors
//! `orbcomm_channel_stats`) so `window/dsp_events/orbcomm_events.rs`
//! can record new packets without depending on this module's GTK
//! types; this module owns only the render side, reached from that
//! handler through a `Weak<dyn Fn()>` stashed on
//! `AppState::orbcomm_heard_render` (mirrors
//! `recorder_action_interpreter`).
//!
//! Row rebuild mirrors `passes::rebuild_pass_rows`'s "drop every row,
//! rebuild from scratch" pattern: cheap for a session-scoped
//! satellite count (`sat_id` is a `u8`, so at most 256 rows, and in
//! practice a handful).

use std::rc::Rc;
use std::time::Instant;

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{AppState, RefCell, adw, glib, sidebar};

/// Tick cadence for the row rebuild. 5 s is responsive enough for a
/// decode whose events only ever arrive a few seconds apart, without
/// adding a second high-frequency `GLib` source alongside the 1 Hz
/// pass-countdown ticker (which drives a much more time-sensitive
/// display).
const HEARD_TICK_SECS: u32 = 5;

/// Format one heard-satellite row's subtitle: `"42 s ago"`, or once
/// an Ephemeris packet has decoded for it, `"42 s ago  ·  51.2°N
/// 7.4°E  ·  715 km"`. Reuses the orbcomm viewer's coordinate
/// formatters so the two renderings can't drift apart.
fn format_heard_subtitle(row: &sidebar::satellites_heard::HeardRow) -> String {
    let age = format!("{} s ago", row.age_secs);
    match row.position {
        Some((lat_deg, lon_deg, alt_m)) => format!(
            "{age}  ·  {} {}  ·  {:.0} km",
            crate::orbcomm_viewer::format_lat(lat_deg),
            crate::orbcomm_viewer::format_lon(lon_deg),
            alt_m / 1000.0,
        ),
        None => age,
    }
}

/// Drop the currently-displayed rows, re-run [`HeardSatellites::rows`]
/// against `now`, and rebuild. Toggles `heard_group`'s own visibility:
/// hidden when the Orbcomm decoder is disabled or nothing has been
/// heard within the expiry window.
fn render_heard_rows(
    panel_weak: &sidebar::satellites_panel::SatellitesPanelWeak,
    state: &Rc<AppState>,
    displayed_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
) {
    let Some(panel) = panel_weak.upgrade() else {
        return;
    };

    for row in displayed_rows.borrow_mut().drain(..) {
        panel.heard_group.remove(&row);
    }

    let rows = state.orbcomm_heard.borrow().rows(Instant::now());
    let visible = state.orbcomm_enabled.get() && !rows.is_empty();
    panel.heard_group.set_visible(visible);
    if !visible {
        return;
    }

    let mut widgets = Vec::with_capacity(rows.len());
    for row in rows {
        let action_row = adw::ActionRow::builder()
            .title(&row.label)
            .subtitle(format_heard_subtitle(&row))
            .build();
        panel.heard_group.add(&action_row);
        widgets.push(action_row);
    }
    *displayed_rows.borrow_mut() = widgets;
}

/// Wire the "Heard via Orbcomm" group: build the render closure, stash
/// a weak handle on `AppState` for the `dsp_events` handler to trigger
/// an immediate rebuild, paint once, then arm the 5 s tick (the tick's
/// closure is the render closure's strong owner — same lifecycle
/// pattern as the pass-countdown ticker, `panel_weak.upgrade()`
/// failing Breaks the source once the panel is dropped).
pub(super) fn wire_heard_group(
    panel_weak: &sidebar::satellites_panel::SatellitesPanelWeak,
    state: &Rc<AppState>,
) {
    // Widget handles for the rows currently attached to
    // `heard_group`, so a rebuild can remove exactly what it added
    // last time. Lives in the wiring layer (not on `SatellitesPanel`
    // itself), matching `passes::DisplayedPass`'s `displayed` list.
    let displayed_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let render: Rc<dyn Fn()> = {
        let panel_weak = panel_weak.clone();
        let state = Rc::clone(state);
        let displayed_rows = Rc::clone(&displayed_rows);
        Rc::new(move || {
            render_heard_rows(&panel_weak, &state, &displayed_rows);
        })
    };

    // Weak handle for `window/dsp_events/orbcomm_events.rs` — lets a
    // recorded event trigger an immediate rebuild without this
    // module depending on dsp_events or vice versa. The tick's
    // closure below is the strong owner.
    *state.orbcomm_heard_render.borrow_mut() = Some(Rc::downgrade(&render));

    // Initial paint. Nothing will be heard yet in the normal
    // start-up order (the panel builds before the DSP thread is
    // running), so this reliably no-ops to "hidden" — cheap, and
    // keeps the group's visibility in sync with the model from the
    // first frame rather than waiting out the first 5 s tick.
    render();

    let render_tick = Rc::clone(&render);
    let panel_weak_tick = panel_weak.clone();
    let _ = glib::timeout_add_seconds_local(HEARD_TICK_SECS, move || {
        if panel_weak_tick.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        render_tick();
        glib::ControlFlow::Continue
    });
}
