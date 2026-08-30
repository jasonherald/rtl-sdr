//! Share activity wiring: the `rtl_tcp` server panel, share switch,
//! status polling, and mDNS advertiser.

use gtk4::prelude::*;
use libadwaita::prelude::*;

mod auth;
pub(in crate::window) use auth::ensure_server_auth_key;
use auth::wire_share_auth_controls;

mod status;
pub(in crate::window) use status::{
    connect_server_status_polling, reset_activity_log, reset_clients_list, reset_status_rows,
};

use super::{
    AdvertiseOptions, Advertiser, DEVICE_RTLSDR, InitialDeviceState, Rc, RefCell, SAMPLE_RATES,
    Server, ServerConfig, SidebarPanels, TxtRecord, adw, gio, glib, local_hostname, plain_toast,
};

/// Owned handle for a running `rtl_tcp` server + optional mDNS
/// advertisement. Drops in reverse order: advertiser first (so
/// peers see the goodbye packet before the server stops), then the
/// server itself (which consumes its accept thread + USB device).
///
/// `Advertiser` is an `Option` because the user can run the server
/// without LAN advertising via the "Announce via mDNS" switch.
pub(super) struct RunningServer {
    server: Server,
    advertiser: Option<Advertiser>,
}

/// Wire the server panel end-to-end: the master share-over-network
/// switch (start/stop + control locking), periodic `Server::stats()`
/// polling (rendered status rows + auto-stop on `has_stopped()`), and
/// the bandwidth advisory that toggles on the device-default sample
/// rate. Errors surface via the `toast_overlay`, and the switch
/// auto-reverts to its off state on start failure so the UI never
/// lies about whether a server is actually running.
///
/// The panel itself is always visible — Share is its own activity on
/// the left activity bar (📡), so the legacy hotplug-gated
/// hide/show timer, the device-count cache, and the
/// `device_row.connect_selected_notify` handler that fed it are gone.
/// The start path still rejects a "local RTL-SDR is the active
/// source" conflict via an exclusivity toast inside the share-switch
/// handler — that guard is independent of the removed machinery.
pub(super) fn connect_server_panel(
    panels: &SidebarPanels,
    toast_overlay: &adw::ToastOverlay,
    server_running: Rc<std::cell::Cell<bool>>,
) {
    let running: Rc<RefCell<Option<RunningServer>>> = Rc::new(RefCell::new(None));

    // Share is now an activity on the left activity bar — always
    // reachable via the 📡 icon. The legacy hotplug-driven
    // hide/show timer + device-count cache + device-row notify
    // were removed with that migration; `librtlsdr_rs::get_device_count`
    // is no longer polled on the GTK main loop for visibility,
    // and the start-server path still rejects the "local dongle is
    // the active source" conflict via its own exclusivity guard.

    // Wire the master share-over-network switch. The handler is the
    // authority on server lifecycle — on toggle we either start a
    // new `Server` (+ optional `Advertiser`) and store the handle,
    // or drop the handle so the accept thread tears down.
    connect_share_switch(panels, toast_overlay, Rc::clone(&running), server_running);

    // Poll `Server::stats()` on a timer, render the status rows,
    // and auto-stop the server if `has_stopped()` becomes true
    // (e.g. USB unplug or accept-thread failure).
    connect_server_status_polling(panels, Rc::clone(&running));

    // Bandwidth advisory — toggled on the device-default sample
    // rate. Unlike the source panel's advisory (which also gates
    // on source type), the server is inherently a network path so
    // only the rate matters.
    let advisory_row_weak = panels.server.bandwidth_advisory_row.downgrade();
    let apply_server_bandwidth_advisory = move |row: &adw::ComboRow| {
        let Some(advisory) = advisory_row_weak.upgrade() else {
            return;
        };
        // Bounds-check the selected index before threshold compare.
        // `ComboRow::selected()` can emit transient out-of-range
        // values during widget-model churn (GTK model repopulate,
        // drag-mid-scroll, etc.) — a bare `>=` would treat those
        // as high-bandwidth and flash the advisory visible against
        // no legal selection. Mirrors the `SAMPLE_RATES.get()`
        // safety pattern used elsewhere in this file.
        let selected = row.selected();
        let is_legal = (selected as usize) < SAMPLE_RATES.len();
        advisory.set_visible(
            is_legal && selected >= crate::sidebar::source_panel::HIGH_BANDWIDTH_SAMPLE_RATE_IDX,
        );
    };
    // Seed initial visibility + subscribe for future changes.
    apply_server_bandwidth_advisory(&panels.server.sample_rate_row);
    panels
        .server
        .sample_rate_row
        .connect_selected_notify(apply_server_bandwidth_advisory);
}

/// Weak refs to every widget the share-switch handler reads or
/// mutates. Mirrors the `ServerStatusWidgetsWeak` pattern: the
/// closure attached to `share_row.connect_active_notify` would
/// otherwise create a self-cycle (`share_row` → closure →
/// `server_panel.share_row` → …) via the previous
/// `clone_server_panel` capture. With this struct we capture weak
/// refs only; strong refs live for the duration of one callback
/// via `upgrade()` and drop at function return, so the widgets can
/// be released on window close.
///
/// `source_device_row` is a sidebar neighbour (not in `ServerPanel`)
/// and comes along for the exclusivity guard read.
#[derive(Clone)]
pub(super) struct ServerSwitchWidgetsWeak {
    nickname_row: glib::WeakRef<adw::EntryRow>,
    port_row: glib::WeakRef<adw::SpinRow>,
    bind_row: glib::WeakRef<adw::ComboRow>,
    advertise_row: glib::WeakRef<adw::SwitchRow>,
    compression_row: glib::WeakRef<adw::ComboRow>,
    listener_cap_row: glib::WeakRef<adw::SpinRow>,
    auth_require_row: glib::WeakRef<adw::SwitchRow>,
    device_defaults_row: glib::WeakRef<adw::ExpanderRow>,
    center_freq_row: glib::WeakRef<adw::SpinRow>,
    sample_rate_row: glib::WeakRef<adw::ComboRow>,
    gain_row: glib::WeakRef<adw::SpinRow>,
    ppm_row: glib::WeakRef<adw::SpinRow>,
    bias_tee_row: glib::WeakRef<adw::SwitchRow>,
    direct_sampling_row: glib::WeakRef<adw::SwitchRow>,
    status_row: glib::WeakRef<adw::ExpanderRow>,
    status_client_row: glib::WeakRef<adw::ActionRow>,
    status_uptime_row: glib::WeakRef<adw::ActionRow>,
    status_data_rate_row: glib::WeakRef<adw::ActionRow>,
    status_commanded_row: glib::WeakRef<adw::ActionRow>,
    activity_log_row: glib::WeakRef<adw::ExpanderRow>,
    activity_log_list: glib::WeakRef<gtk4::ListBox>,
    clients_row: glib::WeakRef<adw::ExpanderRow>,
    clients_list: glib::WeakRef<gtk4::ListBox>,
    source_device_row: glib::WeakRef<adw::ComboRow>,
}

/// Upgraded strong refs held for the duration of a single handler
/// invocation. Field names match `ServerPanel` so the existing
/// helpers (`build_server_config_from_panel`, `set_controls_locked`,
/// etc.) keep working after a simple type rename on their `panel`
/// parameter.
pub(super) struct ServerSwitchWidgets {
    nickname_row: adw::EntryRow,
    port_row: adw::SpinRow,
    bind_row: adw::ComboRow,
    advertise_row: adw::SwitchRow,
    compression_row: adw::ComboRow,
    listener_cap_row: adw::SpinRow,
    auth_require_row: adw::SwitchRow,
    device_defaults_row: adw::ExpanderRow,
    center_freq_row: adw::SpinRow,
    sample_rate_row: adw::ComboRow,
    gain_row: adw::SpinRow,
    ppm_row: adw::SpinRow,
    bias_tee_row: adw::SwitchRow,
    direct_sampling_row: adw::SwitchRow,
    status_row: adw::ExpanderRow,
    status_client_row: adw::ActionRow,
    status_uptime_row: adw::ActionRow,
    status_data_rate_row: adw::ActionRow,
    status_commanded_row: adw::ActionRow,
    activity_log_row: adw::ExpanderRow,
    activity_log_list: gtk4::ListBox,
    clients_row: adw::ExpanderRow,
    clients_list: gtk4::ListBox,
    source_device_row: adw::ComboRow,
}

impl ServerSwitchWidgetsWeak {
    fn from_panels(panels: &SidebarPanels) -> Self {
        let s = &panels.server;
        Self {
            nickname_row: s.nickname_row.downgrade(),
            port_row: s.port_row.downgrade(),
            bind_row: s.bind_row.downgrade(),
            advertise_row: s.advertise_row.downgrade(),
            compression_row: s.compression_row.downgrade(),
            listener_cap_row: s.listener_cap_row.downgrade(),
            auth_require_row: s.auth_require_row.downgrade(),
            device_defaults_row: s.device_defaults_row.downgrade(),
            center_freq_row: s.center_freq_row.downgrade(),
            sample_rate_row: s.sample_rate_row.downgrade(),
            gain_row: s.gain_row.downgrade(),
            ppm_row: s.ppm_row.downgrade(),
            bias_tee_row: s.bias_tee_row.downgrade(),
            direct_sampling_row: s.direct_sampling_row.downgrade(),
            status_row: s.status_row.downgrade(),
            status_client_row: s.status_client_row.downgrade(),
            status_uptime_row: s.status_uptime_row.downgrade(),
            status_data_rate_row: s.status_data_rate_row.downgrade(),
            status_commanded_row: s.status_commanded_row.downgrade(),
            activity_log_row: s.activity_log_row.downgrade(),
            activity_log_list: s.activity_log_list.downgrade(),
            clients_row: s.clients_row.downgrade(),
            clients_list: s.clients_list.downgrade(),
            source_device_row: panels.source.device_row.downgrade(),
        }
    }

    /// Lift every weak ref atomically — any missing widget means
    /// the window's torn down and we skip the callback entirely.
    fn upgrade(&self) -> Option<ServerSwitchWidgets> {
        Some(ServerSwitchWidgets {
            nickname_row: self.nickname_row.upgrade()?,
            port_row: self.port_row.upgrade()?,
            bind_row: self.bind_row.upgrade()?,
            advertise_row: self.advertise_row.upgrade()?,
            compression_row: self.compression_row.upgrade()?,
            listener_cap_row: self.listener_cap_row.upgrade()?,
            auth_require_row: self.auth_require_row.upgrade()?,
            device_defaults_row: self.device_defaults_row.upgrade()?,
            center_freq_row: self.center_freq_row.upgrade()?,
            sample_rate_row: self.sample_rate_row.upgrade()?,
            gain_row: self.gain_row.upgrade()?,
            ppm_row: self.ppm_row.upgrade()?,
            bias_tee_row: self.bias_tee_row.upgrade()?,
            direct_sampling_row: self.direct_sampling_row.upgrade()?,
            status_row: self.status_row.upgrade()?,
            status_client_row: self.status_client_row.upgrade()?,
            status_uptime_row: self.status_uptime_row.upgrade()?,
            status_data_rate_row: self.status_data_rate_row.upgrade()?,
            status_commanded_row: self.status_commanded_row.upgrade()?,
            activity_log_row: self.activity_log_row.upgrade()?,
            activity_log_list: self.activity_log_list.upgrade()?,
            clients_row: self.clients_row.upgrade()?,
            clients_list: self.clients_list.upgrade()?,
            source_device_row: self.source_device_row.upgrade()?,
        })
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "share switch orchestrates server start/stop plus listener-cap + \
              auth-key live-update signals; splitting it would scatter the \
              `running` and `toast_overlay` Rc clones across multiple helpers \
              without improving clarity"
)]
pub(super) fn connect_share_switch(
    panels: &SidebarPanels,
    toast_overlay: &adw::ToastOverlay,
    running: Rc<RefCell<Option<RunningServer>>>,
    server_running: Rc<std::cell::Cell<bool>>,
) {
    use std::cell::Cell;

    // Guards against our own `set_active(false)` (called when the
    // user-initiated start path errors out) re-entering the handler
    // and triggering a spurious stop dispatch on a server that
    // never started.
    let reentry_guard = Rc::new(Cell::new(false));
    let toast_overlay_weak = toast_overlay.downgrade();

    let share_row_weak = panels.server.share_row.downgrade();
    // Weak refs to every row/widget the handler reads or mutates.
    // Replaces the previous `clone_server_panel` strong capture,
    // which bumped share_row's GObject refcount and created a
    // self-cycle with the `connect_active_notify` subscription.
    // Upgraded per-callback so strong refs live for one tick only.
    let widgets_weak = ServerSwitchWidgetsWeak::from_panels(panels);

    // Clone the `running` handle for the listener-cap live-apply
    // closure BEFORE the `share_row` active-notify handler
    // below consumes the outer `running` by move. Both closures
    // share the same `RefCell`; neither holds a borrow past its
    // own tick. Per #395.
    let running_for_cap = Rc::clone(&running);
    // Shared state for the auth-key display row. `current_key`
    // holds the active key bytes while the server is running
    // with auth enabled; `None` when auth is off. `key_revealed`
    // tracks whether the subtitle currently shows the full hex
    // or the masked placeholder — the user toggles this via the
    // reveal button. Both are `Rc<...>` so the four closures
    // (toggle, reveal, copy, regenerate) share the same state
    // without borrow conflicts. Per issue #395.
    let current_auth_key: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
    let auth_key_revealed: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));

    // If auth was restored as ON from config, load the key from
    // the keyring so the key row reflects real state before the
    // user interacts with anything. The server isn't running yet
    // (that requires the share_row flip), so no `set_auth_key`
    // call here — just UI state.
    //
    // The load is a synchronous Secret Service D-Bus round trip
    // (and potentially a WRITE, when the keyring is empty/corrupt)
    // — running it inline here would block window construction on
    // a locked or slow keyring (issue #845). Instead it's pushed
    // onto a `gio::spawn_blocking` worker and applied back on the
    // main context via `glib::spawn_future_local`, same pattern as
    // `sstv_viewer.rs::export_png_async`. Weak refs so a window
    // torn down before the round trip lands is a silent no-op; the
    // key row simply appears a beat later on slow keyrings, which
    // is acceptable and intended.
    //
    // Defense in depth #1 (#845, `CodeRabbit` round 1 on PR #873):
    // while `current_auth_key` is still empty and "Require key" is
    // active, `share_row` is held insensitive too. Without this, a
    // fast click (or scripted input) on Share during the pending
    // window would flip sharing on with `current_auth_key` still
    // `None` — `start_shared_server`'s hard guard
    // (`auth_key_ready_to_start`) backstops that even if this UI
    // gate is ever bypassed, but gating here is what keeps the
    // click from silently no-op'ing/reverting in the user's face.
    if panels.server.auth_require_row.is_active() {
        let current_auth_key_for_seed = Rc::clone(&current_auth_key);
        let key_row_weak = panels.server.auth_key_row.downgrade();
        let share_row_weak_for_seed = panels.server.share_row.downgrade();
        panels.server.share_row.set_sensitive(false);
        glib::spawn_future_local(async move {
            let join = gio::spawn_blocking(ensure_server_auth_key).await;
            // Re-sensitize Share regardless of outcome below — the
            // pending window is over either way.
            let share_row = share_row_weak_for_seed.upgrade();
            if let Some(share_row) = &share_row {
                share_row.set_sensitive(true);
            }
            let Some(key_row) = key_row_weak.upgrade() else {
                // Window closed before the keyring round trip
                // finished — nothing left to seed.
                return;
            };
            let key = match join {
                Ok(key) => key,
                Err(e) => {
                    tracing::warn!("startup rtl_tcp auth-key keyring-load worker panicked: {e:?}");
                    return;
                }
            };
            *current_auth_key_for_seed.borrow_mut() = Some(key);
            key_row.set_visible(true);
            // Leave subtitle as the masked placeholder (widget
            // default) — user clicks Reveal to see the real value.
        });
    }

    // Clone `current_auth_key` for the share_row closure before
    // it consumes local state. The closure reads the cell at
    // server-start time to thread the key into
    // `build_server_config_from_panel` without a second
    // `ensure_server_auth_key()` call. Per `CodeRabbit` round 1
    // on PR #406.
    let current_key_for_share = Rc::clone(&current_auth_key);

    // Clones for the auth-controls wiring below — taken before the
    // share_row closure consumes the outer handles by move.
    let widgets_weak_for_auth = widgets_weak.clone();
    let running_for_auth = Rc::clone(&running);
    panels.server.share_row.connect_active_notify(move |row| {
        on_share_row_toggled(
            row,
            &reentry_guard,
            &widgets_weak,
            &toast_overlay_weak,
            &current_key_for_share,
            &running,
            &server_running,
            &share_row_weak,
        );
    });

    wire_share_auth_controls(
        panels,
        &widgets_weak_for_auth,
        &running_for_auth,
        &current_auth_key,
        &auth_key_revealed,
        toast_overlay,
    );

    wire_listener_cap_live_apply(panels, running_for_cap);
}

/// Body of the share-row toggle: exclusivity guard, then the start /
/// stop halves. Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn on_share_row_toggled(
    row: &adw::SwitchRow,
    reentry_guard: &Rc<std::cell::Cell<bool>>,
    widgets_weak: &ServerSwitchWidgetsWeak,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    current_key_for_share: &Rc<RefCell<Option<Vec<u8>>>>,
    running: &Rc<RefCell<Option<RunningServer>>>,
    server_running: &Rc<std::cell::Cell<bool>>,
    share_row_weak: &glib::WeakRef<adw::SwitchRow>,
) {
    if reentry_guard.get() {
        return;
    }
    let Some(widgets) = widgets_weak.upgrade() else {
        // Window is gone — the signal should stop firing soon.
        // Belt-and-suspenders early return.
        return;
    };
    let active = row.is_active();
    if active {
        // Exclusivity guard: can't claim the dongle for the
        // server while the UI still has RTL-SDR picked as the
        // local source type. Toast + revert the switch without
        // touching `running` or widget lock state.
        if widgets.source_device_row.selected() == DEVICE_RTLSDR {
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(plain_toast(
                    "Switch the source away from local RTL-SDR before sharing over network.",
                ));
            }
            reentry_guard.set(true);
            row.set_active(false);
            reentry_guard.set(false);
            return;
        }
        start_shared_server(
            &widgets,
            current_key_for_share,
            running,
            server_running,
            toast_overlay_weak,
            reentry_guard,
            share_row_weak,
        );
    } else {
        stop_shared_server(&widgets, running, server_running);
    }
}

/// Listener-cap live-apply (issue #395): spin-row changes reach the
/// running server without a restart. Split out per the 50-NLOC gate
/// (#817).
fn wire_listener_cap_live_apply(
    panels: &SidebarPanels,
    running_for_cap: Rc<RefCell<Option<RunningServer>>>,
) {
    // Listener-cap live-apply. Changes on the spin row take effect
    // on the next client accept without restarting the server. The
    // row also persists to sdr_config via a separate signal
    // attached inside `server_panel.rs`; this handler only cares
    // about the running-server case. Per issue #395.
    panels.server.listener_cap_row.connect_value_notify(move |row| {
        let Ok(handle) = running_for_cap.try_borrow() else {
            // Another handler is holding the `RunningServer` borrow
            // (e.g. the share_row active-notify flipping server
            // start/stop). Skip this tick — the spin row's new
            // value is already persisted via the server_panel
            // signal, and the next accept after start will pick
            // it up through `build_server_config_from_panel`.
            return;
        };
        let Some(handle) = handle.as_ref() else {
            // Server not running — the spin row edit is already
            // persisted; nothing to apply live.
            return;
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "spin row bounded to [MIN_LISTENER_CAP, MAX_LISTENER_CAP] at the widget level"
        )]
        let cap = row.value() as usize;
        handle.server.set_listener_cap(cap);
    });
}

/// Stop half of the share switch: drop the advertiser before the
/// server (peers see the mDNS goodbye first), clear the live flag,
/// and reset the panel rows. Split out per the 50-NLOC gate (#817).
fn stop_shared_server(
    widgets: &ServerSwitchWidgets,
    running: &Rc<RefCell<Option<RunningServer>>>,
    server_running: &Rc<std::cell::Cell<bool>>,
) {
    // Drop the handle → Server::drop signals shutdown and
    // joins the accept thread; Advertiser::drop unregisters
    // the mDNS record. Sequence matters (advertiser first
    // so peers see the goodbye packet before the server
    // stops) — field declaration order in `RunningServer`
    // would drop `server` first, so take the advertiser
    // explicitly first to reverse.
    if let Some(mut handle) = running.borrow_mut().take() {
        drop(handle.advertiser.take());
        drop(handle.server);
    }
    // Clear the shared "server is live" flag ahead of the
    // widget-visibility changes so an immediate source-type
    // re-selection triggered by the user's next action sees
    // the coherent post-stop state.
    server_running.set(false);
    set_controls_locked(widgets, false);
    widgets.status_row.set_visible(false);
    widgets.activity_log_row.set_visible(false);
    widgets.clients_row.set_visible(false);
    reset_status_rows(widgets);
    reset_activity_log(widgets);
    reset_clients_list(widgets);
}

/// Start half of the share switch: build the `ServerConfig` from
/// panel state, start the server + optional mDNS advertiser, and
/// lock the panel controls; on failure, toast and revert the switch
/// under the reentry guard. Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn start_shared_server(
    widgets: &ServerSwitchWidgets,
    current_key_for_share: &Rc<RefCell<Option<Vec<u8>>>>,
    running: &Rc<RefCell<Option<RunningServer>>>,
    server_running: &Rc<std::cell::Cell<bool>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    reentry_guard: &Rc<std::cell::Cell<bool>>,
    share_row_weak: &glib::WeakRef<adw::SwitchRow>,
) {
    // Build a ServerConfig from current panel state. Widget
    // readers run on the main thread — safe to block-read
    // the rows synchronously. The pending auth key is
    // read from `current_key_for_share` so a Reveal-and-Copy
    // operation before Play uses the same bytes
    // `Server::start` receives. Per `CodeRabbit` round 1
    // on PR #406.
    let pending_auth_key = current_key_for_share.borrow().clone();

    // Defense in depth #2 (#845, `CodeRabbit` round 1 on PR #873):
    // hard guard at the config-assembly boundary. The UI gating in
    // `connect_share_switch` (site 1) and `enable_auth_requirement_async`
    // (site 2) is meant to keep Share insensitive while a keyring
    // load is pending, but this check is what actually makes a
    // `Server::start` with `auth_key: None` while "Require key" is
    // active IMPOSSIBLE, regardless of any future UI-ordering
    // mistake — an unauthenticated server despite the toggle
    // reading "on" is a CWE-306 missing-authentication bug, not
    // just a UI glitch.
    if !auth_key_ready_to_start(
        widgets.auth_require_row.is_active(),
        pending_auth_key.as_deref(),
    ) {
        tracing::warn!(
            "rtl_tcp share-start blocked: auth required but the key is still loading from the keyring"
        );
        if let Some(overlay) = toast_overlay_weak.upgrade() {
            overlay.add_toast(plain_toast(
                "Auth key still loading — try again in a moment",
            ));
        }
        reentry_guard.set(true);
        if let Some(share) = share_row_weak.upgrade() {
            share.set_active(false);
        }
        reentry_guard.set(false);
        return;
    }

    let config = build_server_config_from_panel(widgets, pending_auth_key);
    match Server::start(config) {
        Ok(server) => {
            // If advertising is on, build the TXT record
            // from the tuner metadata the Server exposes.
            // An Advertiser failure is non-fatal for the
            // server itself (the accept loop keeps running
            // without mDNS), but the user explicitly asked
            // for LAN announcement so they need to KNOW the
            // intent failed — surface a toast and leave
            // `advertiser = None` so the stop path doesn't
            // try to unregister something that never
            // registered.
            let advertiser = if widgets.advertise_row.is_active() {
                match build_advertiser(&server, &widgets.nickname_row.text()) {
                    Ok(adv) => Some(adv),
                    Err(e) => {
                        tracing::warn!(error = %e, "mDNS advertiser failed; server running without LAN advertisement");
                        if let Some(overlay) = toast_overlay_weak.upgrade() {
                            overlay.add_toast(plain_toast(&format!(
                                "Server running, but mDNS advertising failed: {e}"
                            )));
                        }
                        None
                    }
                }
            } else {
                None
            };
            set_controls_locked(widgets, true);
            widgets.status_row.set_visible(true);
            widgets.activity_log_row.set_visible(true);
            widgets.clients_row.set_visible(true);
            *running.borrow_mut() = Some(RunningServer { server, advertiser });
            // Flip the shared "server is live" flag AFTER
            // the handle is stored so the source-panel
            // guard can't race against a mid-construction
            // state.
            server_running.set(true);
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to start rtl_tcp server");
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                overlay.add_toast(plain_toast(&format!("Couldn't share over network: {e}")));
            }
            // Revert the switch without re-entering this
            // same handler — the reentry_guard covers the
            // set_active call below.
            reentry_guard.set(true);
            if let Some(share) = share_row_weak.upgrade() {
                share.set_active(false);
            }
            reentry_guard.set(false);
        }
    }
}

/// Upstream `rtl_tcp`'s `-D` flag accepts 0 = off, 2 = Q-branch
/// direct sampling. Only those two values are meaningful for the
/// UI switch; I-branch (1) is deliberately not exposed because
/// upstream's CLI also hardcodes 2 for `-D`.
pub(super) const DIRECT_SAMPLING_OFF: i32 = 0;

/// See [`DIRECT_SAMPLING_OFF`]. 2 selects the Q branch.
pub(super) const DIRECT_SAMPLING_Q_BRANCH: i32 = 2;

/// Buffer-capacity sentinel passed to `ServerConfig`. `0` tells
/// the server crate to use its internal `DEFAULT_BUFFER_CAPACITY`,
/// keeping the UI honest about "we're not overriding this" rather
/// than pinning a value the server may later tune.
pub(super) const SERVER_BUFFER_CAPACITY_DEFAULT: usize = 0;

/// Pure guard (issue #845, `CodeRabbit` round 1 on PR #873): is it
/// safe to start the server with this `auth_required` / pending-key
/// combination? `false` ONLY for "Require key" active with no key
/// bytes yet — the one combination that must never reach
/// `Server::start` (it would silently start an unauthenticated
/// server, CWE-306). Auth off never needs a key, and auth on with
/// bytes already loaded is the normal path.
///
/// Kept as a standalone pure function (rather than inlined at the
/// call site) so it's unit-testable without a GTK harness — see
/// `share/auth_guard_tests.rs`.
#[must_use]
pub(super) fn auth_key_ready_to_start(auth_required: bool, pending_key: Option<&[u8]>) -> bool {
    !auth_required || pending_key.is_some()
}

/// Read the server panel widget values and build a `ServerConfig`
/// off them. Takes the widget bundle by reference so the arg list
/// stays short and the fn signature documents the "this reads EVERY
/// relevant row" contract clearly.
/// Build a `ServerConfig` from the panel's current widget state.
///
/// **`auth_key` parameter policy**: caller passes the pending
/// key already loaded into the panel's `current_auth_key` cell.
/// This is NOT re-derived inside the function via
/// `ensure_server_auth_key()` — doing so would risk a second
/// generate-and-save call with a different random value if the
/// keyring is unavailable between the UI-seed moment and the
/// server-start moment. Single source of truth: the key shown
/// by the Reveal button is exactly what `Server::start`
/// receives. Per `CodeRabbit` round 1 on PR #406.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "spin-row values are bounded to u16/u32 ranges at the widget level"
)]
pub(super) fn build_server_config_from_panel(
    panel: &ServerSwitchWidgets,
    pending_auth_key: Option<Vec<u8>>,
) -> ServerConfig {
    use std::net::SocketAddr;

    use crate::sidebar::server_panel::{BIND_ALL_INTERFACES_IDX, BIND_LOOPBACK_IDX};

    let port = panel.port_row.value() as u16;
    // Match arm bodies duplicate between `BIND_LOOPBACK_IDX` and the
    // wildcard intentionally: the explicit arm documents the
    // expected value at a glance, and the wildcard catches transient
    // out-of-range indices GTK can emit during widget churn. Folding
    // them loses the at-a-glance enumeration of legal indices next
    // to the feature-flag constants.
    #[allow(
        clippy::match_same_arms,
        reason = "explicit legal-index arms document the rule"
    )]
    let bind = match panel.bind_row.selected() {
        BIND_LOOPBACK_IDX => SocketAddr::from(([127, 0, 0, 1], port)),
        BIND_ALL_INTERFACES_IDX => SocketAddr::from(([0, 0, 0, 0], port)),
        _ => SocketAddr::from(([127, 0, 0, 1], port)),
    };

    read_server_dsp_settings(panel, pending_auth_key, bind)
}

/// Start an mDNS advertiser for the running `Server` using the
/// user's chosen nickname (falling back to `local_hostname()` if
/// the entry is empty or whitespace). Errors propagate to the
/// caller so the UI can toast them — the server itself keeps
/// running regardless, just without LAN advertising.
pub(super) fn build_advertiser(
    server: &Server,
    nickname_raw: &str,
) -> Result<Advertiser, sdr_rtltcp_discovery::DiscoveryError> {
    let nickname = nickname_raw.trim();
    let nickname = if nickname.is_empty() {
        local_hostname()
    } else {
        nickname.to_string()
    };
    let host = local_hostname();
    // DNS-SD instance names must be unique on the LAN. Combine host
    // + nickname the same way the CLI does in
    // `sdr-server-rtltcp/src/bin/sdr-rtl-tcp.rs::announce_over_mdns`.
    let instance_name = if nickname == host {
        nickname.clone()
    } else {
        format!("{host} {nickname}")
    };
    let tuner_info = server.tuner_info();
    let opts = AdvertiseOptions {
        port: server.bind_address().port(),
        instance_name,
        hostname: host.clone(),
        txt: TxtRecord {
            tuner: tuner_info.name.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            gains: tuner_info.gain_count,
            nickname,
            txbuf: None,
            // Advertise the codec bitmask so our own clients
            // know up-front whether to send an extended-protocol
            // hello (`NONE_ONLY` → no hello, vanilla path).
            // Vanilla mDNS consumers (non-sdr-rs clients that
            // don't know this key) just ignore it. #307.
            codecs: Some(server.compression().to_wire()),
            // Advertise `auth_required=true` when the running
            // server has a key configured so clients can prompt
            // for a key BEFORE dispatching connect. Read from
            // `Server::auth_required()` (not the UI's auth-toggle
            // state) because a future live-update via
            // `Server::set_auth_key` is the single source of truth.
            // #394 + #395.
            auth_required: server.auth_required().then_some(true),
        },
    };
    Advertiser::announce(opts)
}

/// Lock or unlock the server-config rows. Called with `true` on
/// start (so the user can't mutate config out from under a live
/// session) and `false` on stop. `share_row` itself stays sensitive
/// — that's how the user turns things off.
pub(super) fn set_controls_locked(panel: &ServerSwitchWidgets, locked: bool) {
    let sensitive = !locked;
    panel.nickname_row.set_sensitive(sensitive);
    panel.port_row.set_sensitive(sensitive);
    panel.bind_row.set_sensitive(sensitive);
    panel.advertise_row.set_sensitive(sensitive);
    panel.compression_row.set_sensitive(sensitive);
    panel.device_defaults_row.set_sensitive(sensitive);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod server_panel_format_tests;

#[cfg(test)]
mod auth_guard_tests;

/// Frequency / sample-rate / gain / DSP portion of the panel read.
/// Split out per the 50-NLOC gate (#817).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "spin-row values are bounded to u16/u32 ranges at the widget level"
)]
fn read_server_dsp_settings(
    panel: &ServerSwitchWidgets,
    pending_auth_key: Option<Vec<u8>>,
    bind: std::net::SocketAddr,
) -> ServerConfig {
    let center_freq_hz = panel.center_freq_row.value() as u32;
    // Sample-rate rows share the SAMPLE_RATES table via
    // `source_panel::build_rtlsdr_rows` ordering. `SAMPLE_RATES`
    // holds f64 values; the server API wants u32 Hz, so round on
    // the way across. Out-of-range selectors fall back on the
    // upstream rtl_tcp.c default.
    let sample_rate_hz = SAMPLE_RATES
        .get(panel.sample_rate_row.selected() as usize)
        .copied()
        .map_or(sdr_server_rtltcp::DEFAULT_SAMPLE_RATE_HZ, |rate| {
            rate.round() as u32
        });

    read_server_gain_settings(
        panel,
        pending_auth_key,
        bind,
        center_freq_hz,
        sample_rate_hz,
    )
}

/// Gain / PPM / bias-T portion of the server-config read.
/// Split out per the 50-NLOC gate (#817).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "spin-row values are bounded to u16/u32 ranges at the widget level"
)]
fn read_server_gain_settings(
    panel: &ServerSwitchWidgets,
    pending_auth_key: Option<Vec<u8>>,
    bind: std::net::SocketAddr,
    center_freq_hz: u32,
    sample_rate_hz: u32,
) -> ServerConfig {
    // UI treats gain = 0.0 as auto (None), matching upstream's
    // `-g 0` semantics. Any positive value becomes tenths-of-dB.
    let gain_db = panel.gain_row.value();
    let gain_tenths_db = if gain_db > 0.0 {
        Some((gain_db * 10.0).round() as i32)
    } else {
        None
    };

    let ppm = panel.ppm_row.value() as i32;
    let bias_tee = panel.bias_tee_row.is_active();
    let direct_sampling = if panel.direct_sampling_row.is_active() {
        DIRECT_SAMPLING_Q_BRANCH
    } else {
        DIRECT_SAMPLING_OFF
    };

    // Compression combo maps index → CodecMask. Unknown / transient
    // indices (GTK can emit garbage during widget-model churn) fall
    // back to `NONE_ONLY` — the wire-safe default that preserves
    // compatibility with every existing rtl_tcp client.
    let compression = match panel.compression_row.selected() {
        crate::sidebar::server_panel::COMPRESSION_LZ4_IDX => {
            sdr_server_rtltcp::codec::CodecMask::NONE_AND_LZ4
        }
        _ => sdr_server_rtltcp::codec::CodecMask::NONE_ONLY,
    };

    ServerConfig {
        bind,
        device_index: 0,
        initial: InitialDeviceState {
            center_freq_hz,
            sample_rate_hz,
            gain_tenths_db,
            ppm,
            bias_tee,
            direct_sampling,
        },
        buffer_capacity: SERVER_BUFFER_CAPACITY_DEFAULT,
        compression,
        // Listener cap pulled from the panel's live widget value so
        // the spin row's current position is the single source of
        // truth at server-start time. Later live-update calls flow
        // through `Server::set_listener_cap` directly. Per #395.
        listener_cap: panel.listener_cap_row.value() as usize,
        // Auth key plumbed from the caller. The panel's
        // `auth_require_row.is_active()` still dictates whether
        // auth is on — caller passes `Some(key)` only when the
        // toggle is active. Caller has already validated the
        // key length via `ensure_server_auth_key()`; `Server::start`
        // re-validates defensively before bind. Per `CodeRabbit`
        // round 1 on PR #406.
        auth_key: if panel.auth_require_row.is_active() {
            pending_auth_key
        } else {
            None
        },
    }
}
