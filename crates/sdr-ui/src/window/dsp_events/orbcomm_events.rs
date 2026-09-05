//! Orbcomm-side `DspToUi` handlers: drive the Orbcomm activity panel
//! (packet log, channel grid, By-Spacecraft list, packet-type
//! breakdown, enable-switch ack) plus the pure heard-spacecraft and
//! tally models on `AppState`.

use std::rc::Rc;
use std::time::Instant;

use super::DspEventCtx;
use crate::sidebar::orbcomm_panel::{OrbcommPanelHandles, repaint_heard};
use crate::state::AppState;

pub(super) fn on_orbcomm_event(ctx: &DspEventCtx, event: &sdr_orbcomm::OrbcommEvent) {
    let DspEventCtx { state, .. } = ctx;
    state.orbcomm_tally.borrow_mut().record(event);
    record_heard_satellite(state, event);
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.append_log_entry(&crate::orbcomm_render::format_packet_row(event));
        refresh_breakdown(handles, state);
        repaint_heard(handles, state);
    }
}

fn record_heard_satellite(state: &Rc<AppState>, event: &sdr_orbcomm::OrbcommEvent) {
    use sdr_orbcomm::OrbcommEventKind;
    use sdr_orbcomm::packet::OrbcommPacket;

    let (sat_id, position, vel, time) = match &event.kind {
        OrbcommEventKind::Packet {
            packet: OrbcommPacket::Sync { sat_id, .. },
            ..
        } => (*sat_id, None, None, None),
        OrbcommEventKind::Packet {
            packet: OrbcommPacket::Ephemeris(eph),
            ..
        } => (
            eph.sat_id,
            Some((eph.lat_deg, eph.lon_deg, eph.alt_m)),
            Some(eph.vel_ms),
            Some(eph.sat_time_unix),
        ),
        _ => return,
    };
    state
        .orbcomm_heard
        .borrow_mut()
        .record(sat_id, position, vel, time, Instant::now());
}

pub(super) fn on_orbcomm_channel_stats(ctx: &DspEventCtx, stats: Box<[sdr_orbcomm::ChannelStats]>) {
    let DspEventCtx { state, .. } = ctx;
    let stats = stats.into_vec();
    if let Some(handles) = state.orbcomm_panel_handles.borrow().as_ref() {
        handles.refresh_channel_grid(&stats);
        refresh_breakdown(handles, state); // checksum/repaired totals live here
    }
    *state.orbcomm_channel_stats.borrow_mut() = stats;
}

pub(super) fn on_orbcomm_enabled_changed(ctx: &DspEventCtx, enabled: bool) {
    let DspEventCtx { state, .. } = ctx;
    state.orbcomm_enabled.set(enabled);
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
