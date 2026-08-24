//! ACARS-side `DspToUi` handlers: message append/collapse into the
//! viewer, engage/disengage mirroring of the airband lock, deferred
//! AOS replay, and output-error toasts. Split out of
//! `window/dsp_events.rs` per the Codacy 500-NLOC file gate on
//! PR #844.

use gtk4::prelude::*;

use super::super::{UiToDsp, glib};
use super::super::{plain_toast, try_collapse_into_existing};
use super::DspEventCtx;

/// Pixel tolerance for the ACARS viewer "scrolled to top" test.
/// `GtkAdjustment` values are fractional, so an exact compare
/// against `lower()` would miss sub-pixel rests.
const SCROLL_TOP_TOLERANCE_PX: f64 = 1.0;

/// `DspToUi::AcarsMessage` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
pub(super) fn on_acars_message(ctx: &DspEventCtx, msg: &sdr_acars::AcarsMessage) {
    let DspEventCtx { state, .. } = ctx;
    // Bounded ring: pop oldest if at cap.
    let cap = crate::acars_config::default_recent_keep() as usize;
    let mut ring = state.acars_recent.borrow_mut();
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back((*msg).clone());
    drop(ring);
    state
        .acars_total_count
        .set(state.acars_total_count.get().saturating_add(1));

    // Mirror to the viewer store if a viewer is open and
    // not paused. Pause semantic per
    // `acars_viewer.rs::build_acars_viewer_window`:
    // toggle active = skip append; the bounded ring keeps
    // growing regardless.
    //
    // Bounded retention: cap the visible store at the same
    // ceiling as `acars_recent` so multi-hour sessions
    // don't grow UI memory + filter cost without bound.
    // Splice from the front (oldest first) before append
    // so the new row lands at the bottom.
    //
    // Collapse-duplicates (#586): when the viewer's
    // collapse toggle is active, walk the most recent
    // rows for a `(aircraft, mode, label, text)` key
    // match within `ACARS_COLLAPSE_WINDOW`. On hit, bump
    // the existing wrapper's count + last_seen and emit
    // an `items_changed` so the row re-binds with the
    // new `(×N)` prefix instead of appending a duplicate.
    //
    // Auto-scroll-to-top: if the viewer is scrolled to
    // the top, scroll back to position 0 after the
    // append/mutate so new rows flow into view. If the
    // user has scrolled down to read older rows, freeze
    // until they scroll back up.
    if let Some(handles) = state.acars_viewer_handles.borrow().as_ref()
        && !handles.pause_button.is_active()
    {
        // Capture scroll state BEFORE the append. With the
        // GtkStack wrap (issue #579), GTK shifts the visible
        // area to preserve content when a new row lands at
        // position 0 under the descending-time sort. Checking
        // adj.value() AFTER the append would see the shifted
        // value and skip the snap-to-top.
        let adj = handles.scrolled_window.vadjustment();
        let was_at_top = (adj.value() - adj.lower()).abs() < SCROLL_TOP_TOLERANCE_PX;

        append_acars_viewer_row(handles, msg, &adj, was_at_top);
        update_acars_aircraft_index(handles, msg);
    }

    tracing::trace!(
        "ACARS msg {} ({}, label {:?})",
        state.acars_total_count.get(),
        msg.aircraft.as_str(),
        msg.label
    );
}

/// `DspToUi::AcarsEnabledChanged` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
pub(super) fn on_acars_enabled_changed(
    ctx: &DspEventCtx,
    result: Result<bool, sdr_core::acars_airband_lock::AcarsEnableError>,
) {
    match result {
        Ok(true) => on_acars_engaged(ctx),
        Ok(false) => on_acars_disengaged(ctx),
        Err(err) => on_acars_enable_error(ctx, &err),
    }
}

/// Engage ack (`Ok(true)`) of [`on_acars_enabled_changed`]: mirror the
/// DSP's silent retune to airband center and lock the geometry rows.
fn on_acars_engaged(ctx: &DspEventCtx) {
    let DspEventCtx {
        spectrum_handle,
        state,
        status_bar,
        freq_selector,
        demod_dropdown,
        sample_rate_row,
        decimation_row,
        volume_button,
        ..
    } = ctx;
    // Engage-edge guard: the controller re-acknowledges
    // `SetAcarsEnabled(true)` when ACARS is already engaged. A
    // repeated `Ok(true)` must not overwrite `acars_saved_tune` /
    // `acars_saved_volume` with the (already airband-locked)
    // current values — disengage would then "restore" the ACARS
    // center frequency and volume 0.0. Per CR round 1 on PR #844.
    if state.acars_enabled.get() {
        state.acars_pending.set(false);
        tracing::debug!("acars engage ack repeated while already engaged; ignoring");
        return;
    }
    state.acars_enabled.set(true);
    state.acars_pending.set(false);
    state.acars_total_count.set(0);
    state.acars_recent.borrow_mut().clear();
    // Mirror the DSP's silent retune to airband
    // center on the header freq selector + status
    // bar + spectrum, and disable user input
    // since DSP rejects geometry commands while
    // engaged (round 14 on PR #584). Stash the
    // pre-engage `(center, vfo_offset)` tuple
    // so disengage can restore both — the
    // controller's restore path reapplies the
    // snapshot offset (CR round 13 on PR #584)
    // and `state.center_frequency` would
    // otherwise drift from the DSP snapshot.
    state.acars_saved_tune.set(Some((
        state.center_frequency.get(),
        spectrum_handle.vfo_offset_hz(),
    )));
    let center_hz = sdr_core::acars_airband_lock::ACARS_CENTER_HZ;
    state.center_frequency.set(center_hz);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    freq_selector.set_frequency(center_hz as u64);
    spectrum_handle.set_center_frequency(center_hz);
    freq_selector.widget.set_sensitive(false);
    // Mirror the DSP's airband lock on the other
    // geometry-mutating widgets (rounds 14-15 on
    // PR #584): SetDemodMode, SetSampleRate, and
    // SetDecimation are all rejected while engaged.
    demod_dropdown.set_sensitive(false);
    sample_rate_row.set_sensitive(false);
    decimation_row.set_sensitive(false);
    status_bar.update_frequency(center_hz);
    // Auto-mute the speaker (issue #588). With ACARS
    // engaged the demod is parked on the user's
    // single pre-engage VFO position, which is
    // unrelated to the 6 ACARS channels being
    // decoded silently in parallel — so whatever
    // comes out of the speaker is at best an
    // unrelated airband channel and at worst static.
    // Capture pre-engage volume + flip to 0; the
    // suppress flag prevents the value-changed
    // handler from persisting 0.0 to config or
    // double-dispatching SetVolume. We send
    // SetVolume(0.0) explicitly here.
    #[allow(clippy::cast_possible_truncation)]
    let pre_engage_volume = volume_button.value() as f32;
    state.acars_saved_volume.set(Some(pre_engage_volume));
    state.suppress_volume_notify.set(true);
    volume_button.set_value(0.0);
    state.suppress_volume_notify.set(false);
    state.send_dsp(UiToDsp::SetVolume(0.0));
    tracing::info!("ACARS engaged");
}

/// Disengage ack (`Ok(false)`) of [`on_acars_enabled_changed`]: restore
/// the pre-engage tune + row sensitivities.
fn on_acars_disengaged(ctx: &DspEventCtx) {
    let DspEventCtx {
        spectrum_handle,
        state,
        status_bar,
        freq_selector,
        demod_dropdown,
        sample_rate_row,
        decimation_row,
        ..
    } = ctx;
    state.acars_enabled.set(false);
    state.acars_pending.set(false);
    state.acars_recent.borrow_mut().clear();
    state.acars_total_count.set(0);
    state.acars_channel_stats.borrow_mut().clear();
    // Restore the pre-engage tune snapshot. DSP
    // retunes silently and reapplies its own
    // snapshot offset, but doesn't emit Tune /
    // VfoOffsetChanged echoes — so restore the
    // UI mirrors here. Order matches what a
    // user-driven `Tune` would do:
    // `state.center_frequency`, spectrum center,
    // then offset (which the freq selector +
    // status bar derive from `center + offset`).
    if let Some((center_hz, offset_hz)) = state.acars_saved_tune.take() {
        state.center_frequency.set(center_hz);
        spectrum_handle.set_center_frequency(center_hz);
        spectrum_handle.set_vfo_offset(offset_hz);
        let tuned_hz = center_hz + offset_hz;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let tuned_u64 = tuned_hz.max(0.0) as u64;
        freq_selector.set_frequency(tuned_u64);
        status_bar.update_frequency(tuned_hz);
    }
    freq_selector.widget.set_sensitive(true);
    demod_dropdown.set_sensitive(true);
    sample_rate_row.set_sensitive(true);
    decimation_row.set_sensitive(true);
    restore_acars_saved_volume(ctx);
    drain_deferred_aos_actions(ctx);
    tracing::info!("ACARS disengaged");
}

/// Auto-restore the pre-engage volume on ACARS disengage (issue
/// #588) — skipped when the user manually moved the slider during
/// the session. Split out of [`on_acars_disengaged`] per the 50-NLOC
/// gate (#817).
fn restore_acars_saved_volume(ctx: &DspEventCtx) {
    let DspEventCtx {
        state,
        volume_button,
        ..
    } = ctx;
    // Auto-restore volume (issue #588) — but only
    // if the user didn't manually move it during
    // the session. We muted to 0.0 on engage; if
    // current value is still ≈ 0, no override
    // happened, restore the saved value. If the
    // user moved it (current > tolerance), respect
    // their explicit choice and skip restore.
    // Tolerance 0.01 (1%) is well above ScaleButton
    // popover step granularity. Don't suppress on
    // restore: the value-changed handler's
    // dispatch + persist of the restored value is
    // exactly what we want.
    if let Some(saved) = state.acars_saved_volume.take() {
        const VOLUME_OVERRIDE_TOLERANCE: f64 = 0.01;
        let current = volume_button.value();
        if current.abs() < VOLUME_OVERRIDE_TOLERANCE {
            volume_button.set_value(f64::from(saved));
        } else {
            tracing::debug!(current, "ACARS disengage: keeping user-overridden volume");
        }
    }
}

/// Replay a deferred AOS batch after the disengage ack (issue #589).
/// Split out of [`on_acars_disengaged`] per the 50-NLOC gate (#817).
fn drain_deferred_aos_actions(ctx: &DspEventCtx) {
    let DspEventCtx { state, .. } = ctx;
    // Drain a deferred AOS batch (issue #589). When
    // a satellite auto-record tick fired during an
    // engaged session, the recorder tick site
    // stashed the entire `Vec<RecorderAction>`
    // and dispatched SetAcarsEnabled(false) — now
    // that the controller has acked the disengage
    // we replay every action through the same
    // recorder interpreter, in the original order.
    // Defer to next idle so we're outside the
    // dispatch borrow.
    let pending = state.pending_aos_actions.borrow_mut().take();
    if let Some(actions) = pending {
        let interp = state
            .recorder_action_interpreter
            .borrow()
            .clone()
            .and_then(|weak| weak.upgrade());
        let Some(interp) = interp else {
            // Interpreter gone (window tearing down, or wiring not
            // yet stashed) — the deferred satellite batch cannot
            // run. Log the drop instead of vanishing silently.
            // Per CR round 1 on PR #844.
            tracing::warn!(
                "AOS replay: dropping {} deferred action(s) — recorder interpreter unavailable",
                actions.len()
            );
            return;
        };
        tracing::info!(
            "AOS replay: ACARS disengaged, executing {} deferred action(s)",
            actions.len()
        );
        glib::idle_add_local_once(move || {
            for action in actions {
                interp(action);
            }
        });
    }
}

/// Engage/disengage failure of [`on_acars_enabled_changed`].
fn on_acars_enable_error(ctx: &DspEventCtx, err: &sdr_core::acars_airband_lock::AcarsEnableError) {
    let DspEventCtx {
        state,
        toast_overlay_weak,
        ..
    } = ctx;
    tracing::warn!("ACARS enable failed: {err}");
    // Clear the in-flight flag so the panel
    // refresh tick stops suppressing the
    // switch-state mirror. State.acars_enabled
    // is intentionally NOT mutated here per CR
    // round 1 on PR #584 — Err doesn't
    // disambiguate engage-vs-disengage failure.
    // The next refresh tick will resync the
    // switch to the unchanged
    // `state.acars_enabled` value, undoing the
    // user's failed toggle.
    state.acars_pending.set(false);
    // `acars_saved_volume` (and `acars_saved_tune`)
    // are intentionally NOT cleared here. Err
    // doesn't disambiguate engage-vs-disengage
    // failure: a failed disengage on an already-
    // engaged session needs the saved snapshots
    // preserved so the eventual successful
    // disengage can restore them; a failed engage
    // simply never set them.
    //
    // Abort any deferred AOS batch (issue #589).
    // The disengage couldn't complete, so the
    // satellite tune would still be rejected by
    // the airband lock. Drop the stashed batch +
    // clear the round-trip flag so LOS doesn't
    // try to re-engage onto an unstable state,
    // and surface a dedicated toast naming the
    // affected satellite (looked up from the
    // batch's `StartAutoRecord` entry).
    let aborted = state.pending_aos_actions.borrow_mut().take();
    if let Some(actions) = aborted {
        let satellite = actions.iter().find_map(|a| match a {
            crate::sidebar::satellites_recorder::Action::StartAutoRecord { satellite, .. } => {
                Some(satellite.clone())
            }
            _ => None,
        });
        state.acars_was_engaged_pre_pass.set(false);
        if let Some(satellite) = satellite {
            tracing::warn!(
                satellite = %satellite,
                error = %err,
                "AOS aborted: ACARS disengage failed",
            );
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(plain_toast(&format!(
                    "Pass {satellite} aborted: ACARS disengage failed"
                )));
            }
        }
    }
    // Surface the original engage/disengage
    // failure as a toast too so the user sees
    // the actionable error (e.g. "scanner is
    // running" or "RTL-SDR required").
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        overlay.add_toast(plain_toast(&format!("ACARS: {err}")));
    }
}

/// Row-append half of the ACARS viewer mirror: collapse-into-existing
/// when enabled, cap-bounded append, and the snap-to-top restore.
/// Split out of [`on_acars_message`] per the 50-NLOC gate (#817).
fn append_acars_viewer_row(
    handles: &crate::acars_viewer::ViewerHandles,
    msg: &sdr_acars::AcarsMessage,
    adj: &gtk4::Adjustment,
    was_at_top: bool,
) {
    let collapse_active = handles.collapse_button.is_active();
    let mut collapsed_into: Option<u32> = None;
    if collapse_active {
        collapsed_into = try_collapse_into_existing(&handles.store, msg);
    }

    if let Some(idx) = collapsed_into {
        handles.store.items_changed(idx, 1, 1);
    } else {
        let cap = crate::acars_config::default_recent_keep();
        let n = handles.store.n_items();
        if n >= cap {
            let excess = n - cap + 1;
            handles
                .store
                .splice(0, excess, &[] as &[gtk4::glib::Object]);
        }
        handles
            .store
            .append(&crate::acars_viewer::AcarsMessageObject::new(msg.clone()));
    }

    // Auto-scroll-to-top: snap back if the user was at
    // the top before the append. Direct adjustment
    // manipulation rather than `ColumnView::scroll_to`:
    // that API is gated behind gtk4 `v4_12` and the
    // workspace pins `v4_10`.
    if was_at_top {
        adj.set_value(adj.lower());
    }
}

/// Aircraft-index half of the ACARS viewer mirror (issue #579).
/// Split out of [`on_acars_message`] per the 50-NLOC gate (#817).
fn update_acars_aircraft_index(
    handles: &crate::acars_viewer::ViewerHandles,
    msg: &sdr_acars::AcarsMessage,
) {
    // Aircraft-index update (issue #579). Find or
    // insert the AircraftEntryObject for this tail.
    // New tails initialize with msg_count=1 (already
    // counting this message) so the column view's bind
    // reads the correct value on first paint. Existing
    // tails bump in place via record_message, then we
    // nudge the filter/sort models via items_changed
    // since GListStore doesn't fire that signal on
    // field mutation of an already-stored object.
    {
        let mut idx = handles.aircraft_index.borrow_mut();
        if let Some(obj) = idx.get(&msg.aircraft) {
            obj.record_message(msg);
            // O(n) over ~50 aircraft is fine; Clear
            // invalidates positions otherwise so we
            // re-find each time rather than tracking
            // a position field on the object.
            if let Some(pos) = handles.aircraft_store.find(obj) {
                handles.aircraft_store.items_changed(pos, 1, 1);
            }
        } else {
            let entry = crate::acars_viewer::AircraftEntry {
                tail: msg.aircraft,
                last_seen: msg.timestamp,
                msg_count: 1,
                last_label: msg.label,
            };
            let obj = crate::acars_viewer::AircraftEntryObject::new(entry);
            handles.aircraft_store.append(&obj);
            idx.insert(msg.aircraft, obj);
        }
    }
}

/// `DspToUi::AcarsChannelStats` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
pub(super) fn on_acars_channel_stats(ctx: &DspEventCtx, ch_stats: Box<[sdr_acars::ChannelStats]>) {
    let DspEventCtx { state, .. } = ctx;
    *state.acars_channel_stats.borrow_mut() = ch_stats.into_vec();
}

/// `DspToUi::AcarsOutputError` arm of [`handle_dsp_message`], split out per
/// the 50-NLOC gate (#817).
pub(super) fn on_acars_output_error(ctx: &DspEventCtx, kind: &'static str, message: &str) {
    let DspEventCtx {
        toast_overlay_weak, ..
    } = ctx;
    tracing::warn!(kind, message, "ACARS output error");
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        overlay.add_toast(plain_toast(&format!(
            "ACARS {kind} output error: {message}"
        )));
    }
}
