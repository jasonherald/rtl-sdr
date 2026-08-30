//! Orbcomm-side `DspToUi` handlers: viewer log append, channel-strip
//! refresh, enable-switch ack, and the "Heard via Orbcomm" panel
//! model (issue #865, Tasks 11 + 12). Split out of
//! `window/dsp_events.rs` beside `acars_events.rs`, same
//! module-per-topic pattern.

use std::rc::{Rc, Weak};
use std::time::Instant;

use super::DspEventCtx;
use crate::state::AppState;

/// `DspToUi::OrbcommEvent` arm of [`super::handle_dsp_message`]. Two
/// independent consumers, neither gated on the other being present:
///
/// * the viewer log — no-op when no viewer is open (mirrors `AptLine`
///   / `SstvLineDecoded`: cheap enough to always run, the user can
///   open the viewer mid-session and start seeing events from that
///   moment on);
/// * the "Heard via Orbcomm" panel model — always recorded, since
///   `AppState::orbcomm_heard` (unlike the viewer) has no open/closed
///   state to gate on.
pub(super) fn on_orbcomm_event(ctx: &DspEventCtx, event: &sdr_orbcomm::OrbcommEvent) {
    let DspEventCtx { state, .. } = ctx;
    if let Some(handles) = state.orbcomm_viewer_handles.borrow().as_ref() {
        crate::orbcomm_viewer::append_log_entry(
            handles,
            &crate::orbcomm_viewer::format_packet_row(event),
        );
    }
    record_heard_satellite(state, event);
}

/// Feed the "Heard via Orbcomm" panel model (Task 12) on `Sync` /
/// `Ephemeris` packets — the only two kinds that carry a `sat_id`.
/// `MessageComplete` events and other packet kinds are outside this
/// task's concern and are silently skipped.
fn record_heard_satellite(state: &Rc<AppState>, event: &sdr_orbcomm::OrbcommEvent) {
    use sdr_orbcomm::OrbcommEventKind;
    use sdr_orbcomm::packet::OrbcommPacket;

    let (sat_id, position) = match &event.kind {
        OrbcommEventKind::Packet {
            packet: OrbcommPacket::Sync { sat_id, .. },
            ..
        } => (*sat_id, None),
        OrbcommEventKind::Packet {
            packet: OrbcommPacket::Ephemeris(eph),
            ..
        } => (eph.sat_id, Some((eph.lat_deg, eph.lon_deg, eph.alt_m))),
        _ => return,
    };

    state
        .orbcomm_heard
        .borrow_mut()
        .record(sat_id, position, Instant::now());

    render_heard_group_if_wired(state);
}

/// Trigger an immediate "Heard via Orbcomm" row rebuild if the panel
/// has wired up (`window/satellites/heard.rs::wire_heard_group`) and
/// its render closure is still alive. Silent no-op otherwise — the
/// next 5 s tick (once the panel does wire up) will still pick up
/// everything recorded so far.
fn render_heard_group_if_wired(state: &Rc<AppState>) {
    if let Some(render) = state
        .orbcomm_heard_render
        .borrow()
        .as_ref()
        .and_then(Weak::upgrade)
    {
        render();
    } else {
        tracing::trace!(
            "Orbcomm heard-group render closure unavailable (panel not wired yet, or dropped); \
             falling back to the periodic tick"
        );
    }
}

/// `DspToUi::OrbcommChannelStats` arm of [`super::handle_dsp_message`].
/// Mirrors `on_acars_channel_stats`: stash into `AppState` (read by a
/// future sidebar panel) and, if a viewer is open, refresh its strip
/// immediately rather than waiting for the next tick.
pub(super) fn on_orbcomm_channel_stats(ctx: &DspEventCtx, stats: Box<[sdr_orbcomm::ChannelStats]>) {
    let DspEventCtx { state, .. } = ctx;
    let stats = stats.into_vec();
    if let Some(handles) = state.orbcomm_viewer_handles.borrow().as_ref() {
        crate::orbcomm_viewer::refresh_channel_strip(handles, &stats);
    }
    *state.orbcomm_channel_stats.borrow_mut() = stats;
}

/// `DspToUi::OrbcommEnabledChanged` arm of [`super::handle_dsp_message`].
/// Unlike ACARS, construction can't fail synchronously (no airband
/// lock to contend for), so this is a plain `bool` ack rather than a
/// `Result` — always mirror it into `AppState::orbcomm_enabled` and,
/// if open, the viewer's switch. Also fires on the DSP's own
/// force-disable ack at source-stop cleanup, which is exactly why the
/// switch tracks this ack instead of setting itself optimistically on
/// user toggle.
pub(super) fn on_orbcomm_enabled_changed(ctx: &DspEventCtx, enabled: bool) {
    let DspEventCtx { state, .. } = ctx;
    state.orbcomm_enabled.set(enabled);
    if let Some(handles) = state.orbcomm_viewer_handles.borrow().as_ref() {
        crate::orbcomm_viewer::apply_enabled_ack(handles, enabled);
    }
    // The "Heard via Orbcomm" group's visibility is gated on this
    // flag too (Task 12) — re-render so a disable hides it
    // immediately rather than waiting out the next 5 s tick.
    render_heard_group_if_wired(state);
}
