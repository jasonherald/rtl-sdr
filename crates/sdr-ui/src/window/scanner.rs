//! Scanner panel wiring and scanner axis-lock UI state.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{AppState, Rc, SidebarPanels, UiToDsp, adw, glib, sidebar, spectrum};

/// Toast display time (seconds) for scanner "force-disable" notices.
pub(super) const SCANNER_TOAST_TIMEOUT_SECS: u32 = 3;

/// Shared "kill the scanner on a manual tune" hook. Built once in
/// `build_window` and cloned into every manual-change handler
/// (frequency selector, demod dropdown, bandwidth row, bookmark
/// recall / preset selection). Calling [`Self::trigger`] is a
/// no-op when the scanner is already off, so wiring it into a
/// handler that fires during programmatic widget updates is
/// cheap and idempotent.
///
/// Holds `glib::WeakRef`s rather than owned widget clones —
/// each clone of this helper is captured by a signal handler
/// that lives on a widget in the window, so a strong ref chain
/// (handler → this helper → widget → handler) would keep the
/// window alive after teardown. Upgrade-or-early-return in
/// `trigger` handles the post-teardown case.
pub(super) struct ScannerForceDisable {
    pub(super) master_switch: glib::WeakRef<gtk4::Switch>,
    pub(super) toast_overlay: glib::WeakRef<adw::ToastOverlay>,
}

impl ScannerForceDisable {
    /// Force the scanner off and toast the user about why. No-op
    /// when the master switch has been dropped (post-teardown)
    /// or when the scanner is already off. Calls `set_active(false)`
    /// on the master switch — the switch's `connect_active_notify`
    /// handler dispatches `SetScannerEnabled(false)` to the
    /// engine, so no explicit DSP send is needed here.
    pub(super) fn trigger(&self, reason: &str) {
        let Some(master_switch) = self.master_switch.upgrade() else {
            return;
        };
        if !master_switch.is_active() {
            return;
        }
        master_switch.set_active(false);
        if let Some(overlay) = self.toast_overlay.upgrade() {
            let toast = adw::Toast::builder()
                .title(format!("Scanner stopped — {reason}"))
                .timeout(SCANNER_TOAST_TIMEOUT_SECS)
                .build();
            overlay.add_toast(toast);
        }
    }
}

/// Outcome of a [`refresh_scanner_axis_lock`] call. Lets the
/// bookmark-mutation caller (which has access to the scanner
/// sidebar widgets) keep `state.scanner_active_key`,
/// `active_channel_row`, and `lockout_row` in sync when a
/// refresh drops the previously-active channel from the
/// rotation — otherwise the sidebar would still show the old
/// channel name (and the lockout button stays visible) until
/// the next `ScannerActiveChannelChanged` event arrived. The
/// master-switch caller ignores the return because it engages
/// the lock from a clean slate (no prior active to drop). Per
/// `CodeRabbit` round 5 on PR #562.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScannerAxisRefreshOutcome {
    /// Lock state didn't change in a way the sidebar cares
    /// about: either the prior active channel still exists in
    /// the new set (highlight reinstated), or there was no
    /// prior active to begin with, or the lock was disengaged
    /// because the channel set went empty.
    Unchanged,
    /// The prior active channel is no longer in the refreshed
    /// scanner set (user disabled `scan_enabled` on it or
    /// deleted the bookmark mid-scan). Caller should clear the
    /// scanner-sidebar surfaces via
    /// `clear_scanner_active_channel_ui` so the displayed
    /// channel name + lockout-row visibility match the actual
    /// "scanner on, no active channel" state.
    ActiveChannelDropped,
}

/// Recompute the scanner X-axis envelope from the live
/// bookmark list and refresh the spectrum's lock + Display
/// panel status row. Called from both the scanner master-
/// switch handler (when the user flips it on) AND the bookmark
/// mutation callback (when the user toggles `scan_enabled` /
/// deletes / adds a bookmark mid-scan, which can shift the
/// envelope without the master switch moving). Without the
/// second call site, scan-list edits silently let the lock
/// stay pinned to a stale range until the user flips the
/// master switch off-and-on. Per #516 smoke feedback.
///
/// Returns [`ScannerAxisRefreshOutcome::ActiveChannelDropped`]
/// when the previously-active channel got removed from the new
/// scanner set (so the caller can clear its sidebar surfaces).
pub(super) fn refresh_scanner_axis_lock(
    bookmarks: &[sidebar::navigation_panel::Bookmark],
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    spectrum_handle: &spectrum::SpectrumHandle,
    status_row: &adw::ActionRow,
    active_key: Option<&sdr_scanner::ChannelKey>,
) -> ScannerAxisRefreshOutcome {
    let default_dwell_ms = sidebar::scanner_panel::load_default_dwell_ms(config);
    let default_hang_ms = sidebar::scanner_panel::load_default_hang_ms(config);
    let channels = sidebar::navigation_panel::project_scanner_channels(
        bookmarks,
        default_dwell_ms,
        default_hang_ms,
    );
    if let Some((min_hz, max_hz)) = sidebar::navigation_panel::scanner_channel_envelope(&channels) {
        // Snapshot the active-channel context BEFORE
        // `enter_scanner_mode` resets it to `None`. Without
        // this, a mid-scan bookmark mutation (the user toggles
        // a `scan_enabled` flag while a channel is being
        // sampled) would briefly clear the FFT highlight band
        // and waterfall projection until the next
        // `ScannerActiveChannelChanged` event arrived — a
        // visually jarring blink during live editing. Per
        // `CodeRabbit` round 2 on PR #562.
        spectrum_handle.enter_scanner_mode(min_hz, max_hz);
        // Reapply only if the previously-active channel is still in
        // the refreshed scanner set. Match by `ChannelKey` (name +
        // integer frequency) — the stable identity the lockout and
        // active-channel tracking already use — instead of the old
        // float `(frequency, bandwidth)` compare: a bookmark save
        // can legitimately change the bandwidth of the still-active
        // channel, and the refreshed value should be reapplied, not
        // treated as a drop. Return `ActiveChannelDropped` only
        // when the key is absent (user disabled or deleted the
        // active bookmark). Per CR round 1 on PR #844.
        let outcome = if let Some(key) = active_key {
            if let Some(ch) = channels.iter().find(|ch| ch.key == *key) {
                #[allow(clippy::cast_precision_loss)]
                spectrum_handle
                    .set_scanner_active_channel(ch.key.frequency_hz as f64, ch.bandwidth);
                ScannerAxisRefreshOutcome::Unchanged
            } else {
                // Leave `active_channel_*` cleared by
                // `enter_scanner_mode` above (matches the
                // "scanner on, no active channel" state) AND
                // tell the caller the active was dropped, so
                // the sidebar widgets get cleared in the same
                // tick instead of waiting for the next DSP
                // `ScannerActiveChannelChanged` event.
                ScannerAxisRefreshOutcome::ActiveChannelDropped
            }
        } else {
            ScannerAxisRefreshOutcome::Unchanged
        };
        update_scanner_axis_status_row(status_row, Some((min_hz, max_hz)));
        outcome
    } else {
        // No channels left in the scanner set. The lock
        // disengages, so the sidebar's active-channel surfaces
        // also belong cleared — but `clear_scanner_active_channel_ui`
        // already runs on the engine-side `ScannerEmptyRotation`
        // event that this code path implies. Reporting
        // `Unchanged` here keeps the bookmark-mutation
        // callback from double-clearing.
        spectrum_handle.exit_scanner_mode();
        update_scanner_axis_status_row(status_row, None);
        ScannerAxisRefreshOutcome::Unchanged
    }
}

/// Sync the Display panel's read-only "Scanner axis" status row
/// to match the current scanner-axis-lock state. Called from
/// every site that engages / disengages the lock — the master
/// switch handler in `connect_scanner_panel`, plus the DSP
/// scanner-stop fan-out (`ScannerEmptyRotation`,
/// `ScannerMutexStopped`) so the row tracks the actual lock
/// state instead of just the master switch position. Per issue
/// #516.
pub(super) fn update_scanner_axis_status_row(row: &adw::ActionRow, range_hz: Option<(f64, f64)>) {
    if let Some((min_hz, max_hz)) = range_hz {
        let subtitle = format!(
            "{} – {}",
            spectrum::frequency_axis::format_frequency(min_hz),
            spectrum::frequency_axis::format_frequency(max_hz),
        );
        row.set_subtitle(&subtitle);
        row.set_visible(true);
    } else {
        row.set_subtitle("");
        row.set_visible(false);
    }
}

/// Clear the scanner's active-channel UI surfaces back to the
/// idle look: empty cache, placeholder label, hidden lockout
/// button. Shared between the four events that mean "scanner
/// isn't parked on a channel anymore":
///   - `ScannerActiveChannelChanged { key: None }` (explicit
///     idle edge)
///   - `ScannerEmptyRotation` (rotation exhausted)
///   - `ScannerMutexStopped::ScannerStoppedFor{Recording,Transcription}`
///     (mutex fired)
///
/// Without the helper, those stop paths would depend on the
/// engine sending a separate `ActiveChannelChanged { key: None }`
/// event in the same tick — which it does today, but relying on
/// that ordering across four sites was brittle.
pub(super) fn clear_scanner_active_channel_ui(
    scanner_panel: &sidebar::scanner_panel::ScannerPanel,
    state: &AppState,
) {
    *state.scanner_active_key.borrow_mut() = None;
    // Drop any buffered channel-marker hop so the next transcript
    // text doesn't inherit a divider from a channel that's no
    // longer active. Reaches every stop path that funnels through
    // this helper (`ScannerActiveChannelChanged { key: None }`,
    // `ScannerEmptyRotation`, `ScannerMutexStopped`). Per
    // CodeRabbit round 1 on PR #558.
    *state.pending_channel_marker.borrow_mut() = None;
    scanner_panel
        .active_channel_row
        .set_subtitle(sidebar::scanner_panel::ACTIVE_CHANNEL_PLACEHOLDER);
    scanner_panel.lockout_row.set_visible(false);
}

/// Connect scanner panel controls to DSP commands.
///
/// Wiring:
/// - master switch → `UiToDsp::SetScannerEnabled`
/// - default dwell / hang sliders → persist to `ConfigManager`
///   and re-project the bookmark list into
///   `UiToDsp::UpdateScannerChannels` so a running scanner picks
///   up the new per-channel dwell/hang on its next tick.
pub(super) fn connect_scanner_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
) {
    let scanner = &panels.scanner;

    wire_scanner_master_switch(panels, state, config, spectrum_handle, scanner);
    wire_scanner_lockout_button(state, scanner);
    wire_scanner_timing_rows(panels, state, config, scanner);
}

/// Master switch -> `SetScannerEnabled` (notify-driven so F8 / force-disable / DSP syncs all fire it).
/// Split out per the 50-NLOC gate (#817).
fn wire_scanner_master_switch(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    scanner: &sidebar::scanner_panel::ScannerPanel,
) {
    // Master switch → SetScannerEnabled. Using `connect_active_notify`
    // (not `connect_state_set`) so programmatic toggles fire too:
    //   - F8 shortcut calls `set_active` which changes the active
    //     property and fires notify::active.
    //   - `ScannerForceDisable::trigger` calls `set_active(false)`
    //     on the same switch for manual-tune force-disable.
    //   - DSP-origin widget syncs (ScannerEmptyRotation,
    //     ScannerMutexStopped::ScannerStopped*) call
    //     `set_active(false)` so notify::active fires here.
    //     `set_state(false)` would NOT trigger this handler —
    //     per GtkSwitch semantics, `state` and `active` are
    //     separate properties; `set_state` fires only
    //     notify::state. The previous comment claimed
    //     otherwise; corrected per `CodeRabbit` round 3 on PR
    //     #562 once the post-stop scanner-axis-lock teardown
    //     started depending on this handler running.
    //     The resulting redundant `SetScannerEnabled(false)`
    //     dispatch is idempotent at the engine — it's cheaper
    //     to pay one extra message per event than to add a
    //     suppress flag for every DSP-origin sync site.
    // Master switch dispatches `SetScannerEnabled` AND drives
    // the spectrum's scanner-axis lock. On enable, compute the
    // (min, max) envelope of all scanner-flagged bookmarks and
    // push it to the spectrum so the X axis pins to that range
    // until the scanner stops. On disable, clear the lock so
    // the spectrum reverts to "current channel ± half BW".
    // The display panel's status row mirrors the lock state via
    // the `update_scanner_axis_status_row` helper. Per issue
    // #516.
    let state_switch = Rc::clone(state);
    let bookmarks_for_switch = Rc::clone(&panels.bookmarks);
    let config_for_switch = std::sync::Arc::clone(config);
    let spectrum_for_switch = Rc::clone(spectrum_handle);
    let display_axis_row = panels.display.scanner_axis_row.clone();
    scanner.master_switch.connect_active_notify(move |sw| {
        let enabled = sw.is_active();
        state_switch.send_dsp(UiToDsp::SetScannerEnabled(enabled));
        if enabled {
            // Compute envelope from the LIVE bookmark list so
            // mid-scan scan-flag toggles + adds/deletes pick up
            // on the next master-switch flip. The same helper
            // also fires from the bookmark mutation callback to
            // refresh while the scanner is already running.
            // Outcome is irrelevant here: this enable path
            // engages the lock from a clean slate (no prior
            // active to drop), so `ActiveChannelDropped` can't
            // fire. Per issue #516.
            let _ = refresh_scanner_axis_lock(
                &bookmarks_for_switch.bookmarks.borrow(),
                &config_for_switch,
                &spectrum_for_switch,
                &display_axis_row,
                None,
            );
        } else {
            spectrum_for_switch.exit_scanner_mode();
            update_scanner_axis_status_row(&display_axis_row, None);
        }
    });
}

/// Lockout button -> LockoutScannerChannel(active key).
/// Split out per the 50-NLOC gate (#817).
fn wire_scanner_lockout_button(
    state: &Rc<AppState>,
    scanner: &sidebar::scanner_panel::ScannerPanel,
) {
    // Lockout button → `LockoutScannerChannel(key)`. The active
    // channel key is updated on every `ScannerActiveChannelChanged`
    // in `handle_dsp_message` and stashed on `state.scanner_active_key`.
    // The button is hidden whenever that key is `None` (same
    // handler), so a click here is guaranteed to have a key —
    // but we check and early-return defensively in case a click
    // races a state change.
    let state_lockout = Rc::clone(state);
    scanner.lockout_button.connect_clicked(move |_| {
        let Some(key) = state_lockout.scanner_active_key.borrow().clone() else {
            tracing::debug!("lockout clicked with no active key — no-op");
            return;
        };
        state_lockout.send_dsp(UiToDsp::LockoutScannerChannel(key));
    });
}

// Restore persisted slider values BEFORE wiring the notify
// handlers (`wire_scanner_timing_rows` body order). `set_value` on a
// SpinRow fires `value-changed`, so if we wired first and restored
// after we'd trigger a spurious `save_default_*_ms` +
// `project_and_push_scanner_channels` during window construction —
// plus `build_window` re-seeds the scanner right after
// `connect_sidebar_panels` returns, which would pile on a second
// redundant dispatch per slider.
/// Dwell / hang rows with persisted seeds.
/// Split out per the 50-NLOC gate (#817).
fn wire_scanner_timing_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    scanner: &sidebar::scanner_panel::ScannerPanel,
) {
    let dwell_ms = sidebar::scanner_panel::load_default_dwell_ms(config);
    scanner.default_dwell_row.set_value(f64::from(dwell_ms));
    let hang_ms = sidebar::scanner_panel::load_default_hang_ms(config);
    scanner.default_hang_row.set_value(f64::from(hang_ms));

    // Default dwell slider: persist on every value change, then
    // re-project the bookmark list so `ScannerChannel::dwell_ms`
    // picks up the new default on channels without an override.
    let config_dwell = std::sync::Arc::clone(config);
    let bookmarks_dwell = Rc::clone(&panels.bookmarks);
    let state_dwell = Rc::clone(state);
    let config_dwell_project = std::sync::Arc::clone(config);
    scanner.default_dwell_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = row.value() as u32;
        sidebar::scanner_panel::save_default_dwell_ms(&config_dwell, ms);
        sidebar::navigation_panel::project_and_push_scanner_channels(
            &bookmarks_dwell.bookmarks.borrow(),
            &state_dwell,
            &config_dwell_project,
        );
    });

    // Default hang slider: same pattern as dwell.
    let config_hang = std::sync::Arc::clone(config);
    let bookmarks_hang = Rc::clone(&panels.bookmarks);
    let state_hang = Rc::clone(state);
    let config_hang_project = std::sync::Arc::clone(config);
    scanner.default_hang_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = row.value() as u32;
        sidebar::scanner_panel::save_default_hang_ms(&config_hang, ms);
        sidebar::navigation_panel::project_and_push_scanner_channels(
            &bookmarks_hang.bookmarks.borrow(),
            &state_hang,
            &config_hang_project,
        );
    });
}
