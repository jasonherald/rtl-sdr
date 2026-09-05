//! Orbcomm-side `DspToUi` handlers: drive the Orbcomm activity panel
//! (packet log, channel grid, By-Spacecraft list, packet-type
//! breakdown, enable-switch ack) plus the pure heard-spacecraft and
//! tally models on `AppState`.

use std::rc::Rc;

use super::DspEventCtx;
use crate::sidebar::orbcomm_panel::{OrbcommPanelHandles, repaint_heard};
use crate::state::AppState;

pub(super) fn on_orbcomm_event(ctx: &DspEventCtx, event: &sdr_orbcomm::OrbcommEvent) {
    let DspEventCtx { state, .. } = ctx;
    let is_packet = matches!(event.kind, sdr_orbcomm::OrbcommEventKind::Packet { .. });
    if is_packet {
        state.orbcomm_tally.borrow_mut().record(event);
    }

    let fields = crate::orbcomm_render::heard_fields(event);
    if let Some(f) = &fields {
        state.orbcomm_heard.borrow_mut().record(
            f.sat_id,
            f.position,
            f.vel_ms,
            f.sat_time_unix,
            std::time::Instant::now(),
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
///
/// Matches against the reception time (`Utc::now()`), not the decoded
/// ephemeris's own timestamp — that timestamp can be off by hours
/// (`sdr-orbcomm` #900), while a live-received ephemeris's position is
/// current, so "now" is the trustworthy reference for propagation.
fn maybe_identify(state: &Rc<AppState>, f: &crate::orbcomm_render::HeardFields) {
    let Some((lat, lon, alt)) = f.position else {
        return;
    };
    if state.orbcomm_sat_names.borrow().contains_key(&f.sat_id) {
        return;
    }
    let tles = state.orbcomm_tles.borrow();
    if tles.is_empty() {
        return;
    }
    let when = chrono::Utc::now();
    if let Some(m) = sdr_sat::identify_spacecraft(
        lat,
        lon,
        alt,
        when,
        &tles,
        sdr_sat::DEFAULT_MATCH_MAX_DIST_KM,
    ) {
        drop(tles);
        state
            .orbcomm_sat_names
            .borrow_mut()
            .insert(f.sat_id, m.name.clone());
        crate::sidebar::orbcomm_persistence::save_orbcomm_sat_names(
            &state.config,
            &state.orbcomm_sat_names.borrow(),
        );
        tracing::info!(
            "Orbcomm: identified Sat {:#04X} as {} ({:.0} km)",
            f.sat_id,
            m.name,
            m.distance_km
        );
    }
}

pub(super) fn on_orbcomm_channel_stats(ctx: &DspEventCtx, stats: Box<[sdr_orbcomm::ChannelStats]>) {
    let DspEventCtx { state, .. } = ctx;
    let stats = stats.into_vec();
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.refresh_channel_grid(&stats);
    }
    *state.orbcomm_channel_stats.borrow_mut() = stats;
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        refresh_breakdown(handles, state); // checksum/repaired totals live here
    }
}

pub(super) fn on_orbcomm_enabled_changed(ctx: &DspEventCtx, enabled: bool) {
    let DspEventCtx { state, .. } = ctx;
    state.orbcomm_enabled.set(enabled);
    if enabled {
        // Kick a background Orbcomm TLE group refresh so the
        // identification matcher and the passes section both have a
        // fresh candidate list for this session. Off the GTK thread;
        // a no-op if the platform never gave us a TLE cache.
        // Staleness-gated (force=false): the cached candidates already
        // seed identification, so skip the fetch if the group cache
        // is still fresh rather than re-fetching on every toggle.
        crate::sidebar::orbcomm_panel::refresh_orbcomm_tles(state, false);
    }
    if !enabled {
        state.orbcomm_tally.borrow_mut().reset();
        // Clear before any handles read it below — the borrow_mut here
        // must not overlap with refresh_breakdown's borrow() of the
        // same RefCell.
        *state.orbcomm_channel_stats.borrow_mut() = Vec::new();
    }
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.apply_enabled_ack(enabled);
        if !enabled {
            handles.refresh_channel_grid(&[]);
        }
        refresh_breakdown(handles, state);
        repaint_heard(handles, state);
    }
}

/// Sum checksum-fail + repaired across channels and repaint the
/// packet-type breakdown label.
fn refresh_breakdown(handles: &OrbcommPanelHandles, state: &Rc<AppState>) {
    let (fail, repaired) =
        state
            .orbcomm_channel_stats
            .borrow()
            .iter()
            .fold((0u64, 0u64), |(f, r), s| {
                (
                    f.saturating_add(s.checksum_fail),
                    r.saturating_add(s.repaired),
                )
            });
    let text = state
        .orbcomm_tally
        .borrow()
        .format_breakdown(fail, repaired);
    handles.set_breakdown(&text);
}
