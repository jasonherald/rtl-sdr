//! Orbcomm-side `DspToUi` handlers: viewer log append, channel-strip
//! refresh, and enable-switch ack. Split out of `window/dsp_events.rs`
//! beside `acars_events.rs`, same module-per-topic pattern (issue
//! #865, Task 11). The viewer is the only consumer for now — a later
//! task adds a sidebar panel reading the same `AppState` fields.

use super::DspEventCtx;

/// `DspToUi::OrbcommEvent` arm of [`super::handle_dsp_message`]. No-op
/// when no viewer is open — the decode tap keeps running regardless
/// (mirrors `AptLine` / `SstvLineDecoded`: cheap enough to always run,
/// the user can open the viewer mid-session and start seeing events
/// from that moment on).
pub(super) fn on_orbcomm_event(ctx: &DspEventCtx, event: &sdr_orbcomm::OrbcommEvent) {
    let DspEventCtx { state, .. } = ctx;
    if let Some(handles) = state.orbcomm_viewer_handles.borrow().as_ref() {
        crate::orbcomm_viewer::append_log_entry(
            handles,
            &crate::orbcomm_viewer::format_packet_row(event),
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
}
