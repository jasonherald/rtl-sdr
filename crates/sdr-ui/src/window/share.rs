//! Share activity wiring: the `rtl_tcp` server panel, share switch,
//! status polling, and mDNS advertiser.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{
    AdvertiseOptions, Advertiser, DEVICE_RTLSDR, Duration, InitialDeviceState, Instant, Rc,
    RefCell, SAMPLE_RATES, Server, ServerConfig, SidebarPanels, TxtRecord, adw, glib,
    local_hostname, plain_toast, sidebar,
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

/// Read the `rtl_tcp` server auth key from the OS keyring, if
/// present. Returns `Some(bytes)` for a well-formed hex-encoded
/// entry, `None` for a missing key, keyring unavailable, empty
/// entry, or corrupt hex. Corrupt entries are logged at `warn`
/// so operators can diagnose without the UI silently regenerating
/// over their paste. Per issue #395.
pub(super) fn load_server_auth_key_from_keyring() -> Option<Vec<u8>> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_KEY_AUTH_KEY, KEYRING_SERVICE, auth_key_from_hex};

    let store = KeyringStore::new(KEYRING_SERVICE);
    match store.get(KEYRING_KEY_AUTH_KEY) {
        Ok(Some(hex)) => {
            let Some(bytes) = auth_key_from_hex(&hex) else {
                tracing::warn!(
                    "rtl_tcp server auth key in keyring is malformed hex; regenerating on next toggle-on"
                );
                return None;
            };
            Some(bytes)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%e, "rtl_tcp server auth key keyring read failed");
            None
        }
    }
}

/// Write the `rtl_tcp` server auth key to the OS keyring as
/// lowercase hex. Returns the underlying keyring error so
/// callers can surface it via toast — the caller is responsible
/// for deciding UX fallback (e.g. revert the toggle, show a
/// banner). Per issue #395.
pub(super) fn save_server_auth_key_to_keyring(
    bytes: &[u8],
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_KEY_AUTH_KEY, KEYRING_SERVICE, auth_key_to_hex};

    let store = KeyringStore::new(KEYRING_SERVICE);
    store.set(KEYRING_KEY_AUTH_KEY, &auth_key_to_hex(bytes))
}

/// Load the persisted server auth key, generating + saving a
/// fresh one when the keyring is either empty or corrupt. The
/// caller gets the fresh bytes regardless — a write failure
/// leaves the key in memory so the current session works, and
/// the next session's toggle-on retries the save path. Per
/// issue #395.
pub(super) fn ensure_server_auth_key() -> Vec<u8> {
    if let Some(existing) = load_server_auth_key_from_keyring() {
        return existing;
    }
    let fresh = sdr_server_rtltcp::auth::generate_random_auth_key();
    if let Err(e) = save_server_auth_key_to_keyring(&fresh) {
        tracing::warn!(%e, "rtl_tcp server auth key keyring write failed — in-memory only");
    }
    fresh
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

/// Extracted out of `connect_server_panel` so the parent function
/// stays under clippy's `too_many_lines` limit. Handles exactly one
/// thing: the `share_row.connect_active_notify` wiring, with its
/// downstream start/stop effects (build `ServerConfig`, call
/// `Server::start`, optionally attach an `Advertiser`, lock or
/// unlock the panel controls, reapply visibility, and surface any
/// error via a toast while flipping the switch back to off).
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

    // If auth was restored as ON from config, eagerly load the
    // key from the keyring so the key row reflects real state
    // before the user interacts with anything. The server isn't
    // running yet (that requires the share_row flip), so no
    // `set_auth_key` call here — just UI state.
    if panels.server.auth_require_row.is_active() {
        let key = ensure_server_auth_key();
        *current_auth_key.borrow_mut() = Some(key);
        panels.server.auth_key_row.set_visible(true);
        // Leave subtitle as the masked placeholder (widget
        // default) — user clicks Reveal to see the real value.
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

/// Auth controls of the Share panel (#394/#395): require-key toggle,
/// reveal, copy, and regenerate. All four closures share
/// `current_auth_key` / `auth_key_revealed` and the running-server
/// handle. Split out of [`connect_share_switch`] per the 50-NLOC
/// gate (#817).
fn wire_share_auth_controls(
    panels: &SidebarPanels,
    widgets_weak: &ServerSwitchWidgetsWeak,
    running: &Rc<RefCell<Option<RunningServer>>>,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
    toast_overlay: &adw::ToastOverlay,
) {
    let toast_overlay_weak = toast_overlay.downgrade();

    // ====================================================
    // Auth controls (#394/#395) — toggle + reveal + copy +
    // regenerate. All four closures share `current_auth_key`
    // and `auth_key_revealed` via `Rc` + the running-server
    // handle via `running_for_auth_{toggle,regen}`.
    // ====================================================

    wire_auth_require_toggle(
        panels,
        running,
        current_auth_key,
        auth_key_revealed,
        &toast_overlay_weak,
        widgets_weak,
    );

    wire_auth_reveal_copy(
        panels,
        current_auth_key,
        auth_key_revealed,
        &toast_overlay_weak,
    );

    wire_auth_regenerate(
        panels,
        running,
        current_auth_key,
        auth_key_revealed,
        &toast_overlay_weak,
    );
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

/// Cadence for the server-stats poll that renders the "Server
/// status" rows. 500 ms is fast enough that "connected / waiting"
/// transitions feel instant while keeping the `ServerStats` clone +
/// row-subtitle churn off the critical path.
pub(super) const SERVER_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Bits-per-byte conversion used in the Mbps formatter. Kept behind
/// a named constant so the arithmetic at the call site reads as
/// unit math ("bytes * `BITS_PER_BYTE` / duration / MEGA") instead
/// of opaque `8`s and `1_000_000`s.
pub(super) const BITS_PER_BYTE: u64 = 8;

/// Megabits divisor for rendering Mbps. `1_000_000` matches
/// telecom/carrier conventions for transport rates.
pub(super) const BITS_PER_MEGABIT: f64 = 1_000_000.0;

/// Weak references to every widget the server-status poll tick
/// touches. Held by the poll closure INSTEAD of a strong
/// `ServerPanel` clone so the closure doesn't bump the widgets'
/// `GObject` refcounts past window lifetime.
///
/// The original design cloned the whole `ServerPanel` into the
/// closure and relied on a single `widget_weak.upgrade().is_none()`
/// break gate — but the clone held strong refs to every widget,
/// including the group itself, so the weak check could never fire
/// and the 500 ms timer leaked past window close. Every
/// panel-touching closure in this file now uses weak refs for the
/// same reason (see `connect_rtl_tcp_discovery`'s pattern).
pub(super) struct ServerStatusWidgetsWeak {
    status_row: glib::WeakRef<adw::ExpanderRow>,
    status_client_row: glib::WeakRef<adw::ActionRow>,
    status_uptime_row: glib::WeakRef<adw::ActionRow>,
    status_data_rate_row: glib::WeakRef<adw::ActionRow>,
    status_commanded_row: glib::WeakRef<adw::ActionRow>,
    activity_log_row: glib::WeakRef<adw::ExpanderRow>,
    activity_log_list: glib::WeakRef<gtk4::ListBox>,
    clients_row: glib::WeakRef<adw::ExpanderRow>,
    clients_list: glib::WeakRef<gtk4::ListBox>,
}

/// Snapshot of upgraded strong references held for the duration of
/// a single poll tick. All nine widgets upgrade together or we
/// `Break` the timer — render functions then read these fields
/// directly without needing their own weak-ref fallbacks.
pub(super) struct ServerStatusWidgets {
    status_row: adw::ExpanderRow,
    status_client_row: adw::ActionRow,
    status_uptime_row: adw::ActionRow,
    status_data_rate_row: adw::ActionRow,
    status_commanded_row: adw::ActionRow,
    activity_log_row: adw::ExpanderRow,
    activity_log_list: gtk4::ListBox,
    clients_row: adw::ExpanderRow,
    clients_list: gtk4::ListBox,
}

impl ServerStatusWidgetsWeak {
    fn from_panel(panel: &sidebar::ServerPanel) -> Self {
        Self {
            status_row: panel.status_row.downgrade(),
            status_client_row: panel.status_client_row.downgrade(),
            status_uptime_row: panel.status_uptime_row.downgrade(),
            status_data_rate_row: panel.status_data_rate_row.downgrade(),
            status_commanded_row: panel.status_commanded_row.downgrade(),
            activity_log_row: panel.activity_log_row.downgrade(),
            activity_log_list: panel.activity_log_list.downgrade(),
            clients_row: panel.clients_row.downgrade(),
            clients_list: panel.clients_list.downgrade(),
        }
    }

    /// Upgrade every weak ref atomically. Returns `None` if any
    /// one widget has been destroyed — the caller breaks its
    /// timer instead of rendering against a partially-dead panel.
    fn upgrade(&self) -> Option<ServerStatusWidgets> {
        Some(ServerStatusWidgets {
            status_row: self.status_row.upgrade()?,
            status_client_row: self.status_client_row.upgrade()?,
            status_uptime_row: self.status_uptime_row.upgrade()?,
            status_data_rate_row: self.status_data_rate_row.upgrade()?,
            status_commanded_row: self.status_commanded_row.upgrade()?,
            activity_log_row: self.activity_log_row.upgrade()?,
            activity_log_list: self.activity_log_list.upgrade()?,
            clients_row: self.clients_row.upgrade()?,
            clients_list: self.clients_list.upgrade()?,
        })
    }
}

/// Poll `Server::stats()` on a fixed cadence, render the four
/// status rows from the snapshot, and auto-stop the server if
/// `has_stopped()` becomes true (e.g. USB dongle unplugged or
/// accept-thread error).
///
/// Auto-stop flips the `share_row` back off, which re-enters the
/// switch's `connect_active_notify` handler — that branch drops the
/// `RunningServer` handle and releases the dongle for subsequent
/// reopens. Without this the UI would lie about the server's
/// running state indefinitely.
///
/// Data-rate is computed from the delta in `bytes_sent` between
/// consecutive poll ticks. Counter resets (on disconnect) produce
/// negative deltas which we clamp to zero so the row reads "0 bps"
/// instead of a bogus megabit-scale number during the transient.
pub(super) fn connect_server_status_polling(
    panels: &SidebarPanels,
    running: Rc<RefCell<Option<RunningServer>>>,
) {
    use std::cell::Cell;

    let widgets_weak = ServerStatusWidgetsWeak::from_panel(&panels.server);
    let share_row_weak = panels.server.share_row.downgrade();
    let last_bytes_sent = Rc::new(Cell::new(0u64));
    // Activity-log diff key: (ring_len, newest_instant). Rendering
    // is cheap but clearing the ListBox resets any user scroll
    // position, so we short-circuit on unchanged ticks.
    let last_activity_key: Rc<Cell<(usize, Option<Instant>)>> = Rc::new(Cell::new((0, None)));
    // Clients-list diff key. Hashes `(id, peer, role, drops,
    // elapsed_secs)` per client so a stable connected set with
    // ticking uptime / incrementing drop counters still triggers
    // a rebuild — the previous id-set-only hash froze row
    // subtitles once the set stabilized, so a 10-minute session
    // would show "0s" uptime forever. `Option<u64>` so the
    // stop/start reset path can invalidate the cache by setting
    // `None`; without that, an "empty set → empty set" transition
    // across stop/start would short-circuit the first post-start
    // render and leave the expander blank (the placeholder row
    // was removed by `reset_clients_list`). Per `CodeRabbit`
    // round 2 on PR #406.
    let last_clients_key: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));

    // Separate subscription on the Stop button. Flipping the switch
    // off is the single canonical stop path — pointing the button
    // there avoids a second teardown codepath that could drift.
    let stop_share_row_weak = share_row_weak.clone();
    panels.server.status_stop_button.connect_clicked(move |_| {
        if let Some(share) = stop_share_row_weak.upgrade() {
            share.set_active(false);
        }
    });

    let _ = glib::timeout_add_local(SERVER_STATUS_POLL_INTERVAL, move || {
        // Upgrade all the status widgets in one shot. If any is gone
        // (window closed → sidebar dropped → widgets orphaned), tear
        // the timer down. Strong refs live only for the duration of
        // this tick — dropped at function return — so they never
        // contribute to the long-running GObject refcount.
        let Some(widgets) = widgets_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // Snapshot `Server::stats()` under the borrow. `stats()`
        // internally locks a Mutex — the return is a Clone, so the
        // borrow scope is tight.
        let snapshot = running
            .borrow()
            .as_ref()
            .map(|h| (h.server.stats(), h.server.has_stopped()));
        let Some((stats, stopped)) = snapshot else {
            // No server running — nothing to render, keep ticking
            // (the share switch handler will spin us up again).
            return glib::ControlFlow::Continue;
        };

        // If the accept thread exited on its own (USB unplug,
        // fatal error), auto-flip the share switch off. Re-enters
        // the switch handler, which drops the server handle.
        if stopped {
            tracing::warn!("rtl_tcp server stopped on its own — flipping share switch off");
            if let Some(share) = share_row_weak.upgrade() {
                share.set_active(false);
            }
            return glib::ControlFlow::Continue;
        }

        render_status_rows(&widgets, &stats, &last_bytes_sent);
        render_activity_log(&widgets, &stats, &last_activity_key);
        render_clients_list(&widgets, &stats, &last_clients_key);
        glib::ControlFlow::Continue
    });
}

/// Write the current `ServerStats` snapshot into the four status
/// rows. Uses `last_bytes_sent` to compute a rolling data-rate from
/// delta-over-poll-interval. Takes upgraded `ServerStatusWidgets`
/// — strong refs held only for this call's duration — so the poll
/// closure itself doesn't contribute to the long-running `GObject`
/// refcount.
///
/// Renders the FIRST connected client in the per-session rows
/// (client peer, uptime, commanded state, activity log). Multi-
/// client per-client UI rows land in PR B of #391; this commit
/// just wires the new `Vec<ClientInfo>` shape into the existing
/// single-client row layout so the server-panel keeps working.
/// The data-rate row switches to the aggregate
/// `total_bytes_sent` so operators see the full server throughput
/// even before PR B's per-client rows arrive.
pub(super) fn render_status_rows(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_bytes_sent: &Rc<std::cell::Cell<u64>>,
) {
    use crate::sidebar::server_panel::STATUS_WAITING_FOR_CLIENT_SUBTITLE;

    let first = stats.connected_clients.first();
    let extra = stats.connected_clients.len().saturating_sub(1);

    // Client row + expander subtitle. When there are N > 1 clients,
    // append "(+N-1 more)" so the row makes the multi-client state
    // visible even before PR B's per-client list exists.
    if let Some(info) = first {
        let peer_str = info.peer.to_string();
        let client_subtitle = if extra > 0 {
            format!("{peer_str} (+{extra} more)")
        } else {
            peer_str.clone()
        };
        widgets.status_client_row.set_subtitle(&client_subtitle);
        let expander_subtitle = if stats.connected_clients.len() == 1 {
            format!("Connected: {peer_str}")
        } else {
            format!("{} clients connected", stats.connected_clients.len())
        };
        widgets.status_row.set_subtitle(&expander_subtitle);
    } else {
        widgets
            .status_client_row
            .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
        widgets
            .status_row
            .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
    }

    render_uptime_and_rate_rows(widgets, stats, last_bytes_sent);
}

/// Select the client whose most recent `last_command` timestamp is
/// newest. Falls back to the first connected client when nobody
/// has issued a command yet, and to `None` when no clients are
/// connected.
///
/// Shared between the commanded-state row and the activity-log
/// renderer so both surfaces track the same "who's actually
/// driving the dongle" peer. Pre-#392 this matters because any
/// client can command; post-#392 role-gated dispatch will make
/// this resolve to the controller every time.
pub(super) fn pick_most_recent_commander(
    clients: &[sdr_server_rtltcp::ClientInfo],
) -> Option<&sdr_server_rtltcp::ClientInfo> {
    clients
        .iter()
        .filter_map(|c| c.last_command.map(|(_, t)| (c, t)))
        .max_by_key(|&(_, t)| t)
        .map(|(c, _)| c)
        .or_else(|| clients.first())
}

/// Render a `Duration` as `Nh Nm Ns` / `Nm Ns` / `Ns` depending on
/// magnitude. Keeps the row readable at a glance without fighting a
/// full clock component.
pub(super) fn format_uptime(elapsed: Duration) -> String {
    let total_secs = elapsed.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Render bytes/interval as a human-readable data rate. Picks the
/// right unit automatically: kbps when we're below 1 Mbps (quiet
/// clients), Mbps otherwise. `rtl_tcp` IQ streams at 2.4 MS/s × 2
/// bytes per sample = ~4.8 Mbps, so the Mbps case dominates in
/// practice.
#[allow(
    clippy::cast_precision_loss,
    reason = "intermediate f64 conversion for rate math; Mbps precision is cosmetic"
)]
pub(super) fn format_data_rate(bytes: u64, interval: Duration) -> String {
    let secs = interval.as_secs_f64();
    if secs <= 0.0 {
        return "—".to_string();
    }
    let bits_per_sec = (bytes as f64 * BITS_PER_BYTE as f64) / secs;
    if bits_per_sec < BITS_PER_MEGABIT {
        format!("{:.1} kbps", bits_per_sec / 1_000.0)
    } else {
        format!("{:.2} Mbps", bits_per_sec / BITS_PER_MEGABIT)
    }
}

/// Render the "Tuned to" row subtitle for the first connected
/// client. Combines frequency, sample rate and gain into one
/// line. Unset `current_*` fields on the client fall back to the
/// server's **configured** `initial` state (what the user set up
/// in the server panel or CLI args), NOT the library's upstream
/// `rtl_tcp.c` defaults. `None` input (no clients connected)
/// renders as the idle placeholder. Per `CodeRabbit` round 1 on
/// PR #402.
pub(super) fn format_commanded_state(
    info: Option<&sdr_server_rtltcp::ClientInfo>,
    initial: &sdr_server_rtltcp::InitialDeviceState,
) -> String {
    let Some(info) = info else {
        return crate::sidebar::server_panel::STATUS_IDLE_VALUE_SUBTITLE.to_string();
    };
    let freq_hz = info.current_freq_hz.unwrap_or(initial.center_freq_hz);
    let sample_rate_hz = info
        .current_sample_rate_hz
        .unwrap_or(initial.sample_rate_hz);
    let gain_text = match (info.current_gain_auto, info.current_gain_tenths_db) {
        (Some(true), _) => "auto".to_string(),
        (_, Some(gain_tenths)) => {
            #[allow(clippy::cast_precision_loss, reason = "gain tenths-of-dB, cosmetic")]
            let db = f64::from(gain_tenths) / 10.0;
            format!("{db:.1} dB")
        }
        // Client hasn't sent a gain command yet — show whatever
        // the server started with. `initial.gain_tenths_db = None`
        // encodes upstream's "automatic" mode (CLI `-g 0`).
        _ => match initial.gain_tenths_db {
            None => "auto".to_string(),
            Some(gain_tenths) => {
                #[allow(clippy::cast_precision_loss, reason = "gain tenths-of-dB, cosmetic")]
                let db = f64::from(gain_tenths) / 10.0;
                format!("{db:.1} dB")
            }
        },
    };
    format!(
        "{} @ {} • gain {}",
        format_hz(freq_hz),
        format_hz(sample_rate_hz),
        gain_text
    )
}

/// Short Hz formatter — kHz / MHz / GHz depending on magnitude.
/// Kept local to this module because the status row's formatting
/// needs differ from the header-bar frequency selector (which has
/// its own 12-digit grid display).
pub(super) fn format_hz(hz: u32) -> String {
    let hz_f = f64::from(hz);
    if hz >= 1_000_000_000 {
        format!("{:.3} GHz", hz_f / 1_000_000_000.0)
    } else if hz >= 1_000_000 {
        format!("{:.3} MHz", hz_f / 1_000_000.0)
    } else if hz >= 1_000 {
        format!("{:.3} kHz", hz_f / 1_000.0)
    } else {
        format!("{hz} Hz")
    }
}

/// Rebuild the activity-log list from the most-recently-commanding
/// client's `recent_commands` ring if it has actually changed since
/// the last render. The "changed?" check uses the ring length + the
/// timestamp of the newest entry so we skip the clear-and-rebuild
/// on idle ticks — preserves any scroll position the user has in
/// the `ListBox`.
///
/// Uses [`pick_most_recent_commander`] rather than just the first
/// connected client because pre-#392 any client can send commands
/// — the oldest client would shadow a newer peer's activity. Per
/// `CodeRabbit` round 2 on PR #402. PR B of #391 replaces this with
/// a per-client log tab so every client's commands show under
/// their own row; until then, tracking "whoever's driving right
/// now" is the right single-row compromise.
pub(super) fn render_activity_log(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_rendered: &Rc<std::cell::Cell<(usize, Option<Instant>)>>,
) {
    use crate::sidebar::server_panel::ACTIVITY_LOG_EMPTY_SUBTITLE;

    let Some(commander) = pick_most_recent_commander(&stats.connected_clients) else {
        // No connected client → clear + show empty subtitle if
        // we're not already in that state. Track the idle cache
        // key as (0, None) so the render skips on subsequent
        // idle ticks.
        let current_key = (0usize, None::<Instant>);
        if current_key == last_rendered.get() {
            return;
        }
        last_rendered.set(current_key);
        while let Some(child) = widgets.activity_log_list.first_child() {
            widgets.activity_log_list.remove(&child);
        }
        widgets
            .activity_log_row
            .set_subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE);
        return;
    };
    let ring: &std::collections::VecDeque<(sdr_server_rtltcp::CommandOp, Instant)> =
        &commander.recent_commands;

    let newest = ring.back().map(|(_, t)| *t);
    let current_key = (ring.len(), newest);
    if current_key == last_rendered.get() {
        return;
    }
    last_rendered.set(current_key);

    // Clear the ListBox children. GTK4 ListBox has no mass-remove,
    // so walk the child list.
    while let Some(child) = widgets.activity_log_list.first_child() {
        widgets.activity_log_list.remove(&child);
    }

    if ring.is_empty() {
        widgets
            .activity_log_row
            .set_subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE);
        return;
    }

    widgets
        .activity_log_row
        .set_subtitle(&format!("{} commands", ring.len()));
    // Newest first so the user doesn't have to scroll to see the
    // most recent activity.
    let now = Instant::now();
    for (op, at) in ring.iter().rev() {
        let row = adw::ActionRow::builder()
            .title(format!("{op:?}"))
            .subtitle(format_log_age(now.saturating_duration_since(*at)))
            .activatable(false)
            .build();
        widgets.activity_log_list.append(&row);
    }
}

/// Render the "Connected clients" list — one row per client
/// with peer, role badge, duration, and drops counter. Empty
/// state: single "No clients connected" placeholder row plus
/// matching expander subtitle.
///
/// **Rebuild trigger.** Hashes `(id, peer, role, drops,
/// elapsed_secs)` for every connected client; rebuilds when
/// the hash changes. That covers both accept/disconnect
/// transitions AND per-row field churn (ticking uptime,
/// incrementing drop counters), so the displayed subtitles
/// stay live throughout a session. Scroll / hover state is
/// preserved on unchanged ticks. Per issue #395 +
/// `CodeRabbit` round 2 on PR #406.
///
/// **Stop/start invalidation.** On server stop
/// `reset_clients_list` empties the `ListBox` but can't reach
/// the cache cell across function boundaries; instead,
/// `render_clients_list` treats `first_child().is_none()` as
/// "reset has run, force rebuild" so an empty→empty session
/// transition still repaints the placeholder. Per `CodeRabbit`
/// round 2 on PR #406.
pub(super) fn render_clients_list(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_rendered: &Rc<std::cell::Cell<Option<u64>>>,
) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Compute a diff key that bumps on *any* rendered-field
    // change — not just accept / disconnect. Including peer,
    // role, drops, and (rounded-seconds) uptime in the hash
    // means a stable connected set with ticking uptime or
    // incrementing drops still triggers rebuilds. The previous
    // id-set-only key froze row subtitles once the client set
    // stabilized. Per `CodeRabbit` round 2 on PR #406.
    //
    // Rebuild cost is ~N widget builds at 2 Hz (N ≤ 32 at the
    // listener cap); trivial vs. the USB / DSP hot path.
    let now = Instant::now();
    let mut key_fields: Vec<(sdr_server_rtltcp::ClientId, String, u8, u64, u64)> = stats
        .connected_clients
        .iter()
        .map(|c| {
            let role_disc = match c.role {
                sdr_server_rtltcp::extension::Role::Control => 0u8,
                sdr_server_rtltcp::extension::Role::Listen => 1u8,
            };
            let elapsed_secs = now.saturating_duration_since(c.connected_since).as_secs();
            (
                c.id,
                c.peer.to_string(),
                role_disc,
                c.buffers_dropped,
                elapsed_secs,
            )
        })
        .collect();
    key_fields.sort_unstable_by_key(|(id, _, _, _, _)| *id);
    let mut hasher = DefaultHasher::new();
    key_fields.hash(&mut hasher);
    let current_key = hasher.finish();

    rebuild_client_rows(widgets, stats, last_rendered, current_key, now);
}

/// Reset activity-log list + subtitle on stop. Without this the
/// list would persist after the server stopped — misleading users
/// into thinking the log reflects a currently-running session.
pub(super) fn reset_activity_log(panel: &ServerSwitchWidgets) {
    use crate::sidebar::server_panel::ACTIVITY_LOG_EMPTY_SUBTITLE;
    while let Some(child) = panel.activity_log_list.first_child() {
        panel.activity_log_list.remove(&child);
    }
    panel
        .activity_log_row
        .set_subtitle(ACTIVITY_LOG_EMPTY_SUBTITLE);
}

/// Reset the connected-clients list to its empty state. Called on
/// server stop so the next start doesn't surface stale client rows
/// before the first poll tick repopulates. Per issue #395.
pub(super) fn reset_clients_list(panel: &ServerSwitchWidgets) {
    use crate::sidebar::server_panel::CLIENTS_LIST_EMPTY_SUBTITLE;
    while let Some(child) = panel.clients_list.first_child() {
        panel.clients_list.remove(&child);
    }
    panel.clients_row.set_subtitle(CLIENTS_LIST_EMPTY_SUBTITLE);
}

/// Render an elapsed duration as a compact "age" string for the
/// activity-log rows. Narrower set of buckets than the discovery
/// formatter — commands arrive in bursts during a session, so the
/// "just now" / seconds-ago distinction matters but hours isn't
/// common in a single session.
pub(super) fn format_log_age(elapsed: Duration) -> String {
    const JUST_NOW_THRESHOLD: Duration = Duration::from_secs(2);
    let secs = elapsed.as_secs();
    if elapsed < JUST_NOW_THRESHOLD {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Reset status rows to their idle-no-client state. Called when the
/// server stops so the user doesn't see stale "connected at 127.0.0.1"
/// / "uptime 5m" data after they flipped the share switch off.
pub(super) fn reset_status_rows(panel: &ServerSwitchWidgets) {
    use crate::sidebar::server_panel::STATUS_IDLE_VALUE_SUBTITLE;
    use crate::sidebar::server_panel::STATUS_WAITING_FOR_CLIENT_SUBTITLE;
    panel
        .status_row
        .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
    panel
        .status_client_row
        .set_subtitle(STATUS_WAITING_FOR_CLIENT_SUBTITLE);
    panel
        .status_uptime_row
        .set_subtitle(STATUS_IDLE_VALUE_SUBTITLE);
    panel
        .status_data_rate_row
        .set_subtitle(STATUS_IDLE_VALUE_SUBTITLE);
    panel
        .status_commanded_row
        .set_subtitle(STATUS_IDLE_VALUE_SUBTITLE);
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

/// Read the server panel widget values and build a `ServerConfig`
/// off them. Takes the full `ServerPanel` by reference so the arg
/// list stays short and the fn signature documents the "this reads
/// EVERY relevant row" contract clearly.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "spin-row values are bounded to u16/u32 ranges at the widget level"
)]
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

/// Apply an auth-key change to the running server and refresh
/// the mDNS advertiser atomically. Returns `true` iff the
/// server actually holds the new state; `false` means the
/// caller must revert the UI so it stays in sync.
///
/// **Success cases:**
/// - No server is running (`running` cell contains `None`): no
///   server-side change to apply; caller can proceed with UI.
/// - Server is running and `set_auth_key(new)` returns `Ok`.
///   The advertiser is then rebuilt via
///   `refresh_advertiser_for_auth_change` so the TXT record
///   reflects the new `auth_required` state.
///
/// **Failure cases:**
/// - `try_borrow_mut` on the running-server cell fails (another
///   handler holds a mutable borrow — rare, mid-click race).
///   Caller reverts the switch; next click usually wins.
/// - `set_auth_key` returns `Err` (e.g., mutex poisoned). The
///   toast surfaces the error and the caller reverts UI state.
///
/// Does NOT touch UI state — caller owns the UI mutation gate.
/// Per `CodeRabbit` round 1 on PR #406.
pub(super) fn apply_live_auth_change(
    running: &Rc<RefCell<Option<RunningServer>>>,
    new_key: Option<Vec<u8>>,
    widgets: Option<&ServerSwitchWidgets>,
    toast_overlay: &glib::WeakRef<adw::ToastOverlay>,
) -> bool {
    let Ok(mut handle_cell) = running.try_borrow_mut() else {
        tracing::warn!("auth change skipped — running-server cell busy");
        return false;
    };
    let Some(handle) = handle_cell.as_mut() else {
        // Server not running — UI-only change is always fine.
        return true;
    };
    if let Err(e) = handle.server.set_auth_key(new_key) {
        tracing::warn!(%e, "Server::set_auth_key failed on live auth change");
        if let Some(overlay) = toast_overlay.upgrade() {
            overlay.add_toast(plain_toast(&format!(
                "Couldn't update auth on the running server: {e}"
            )));
        }
        return false;
    }
    // mDNS TXT refresh. Only meaningful when we have widget refs
    // (caller upgraded `widgets_weak` before the call).
    if let Some(widgets) = widgets {
        refresh_advertiser_for_auth_change(handle, widgets, toast_overlay);
    }
    true
}

/// Tear down and rebuild the running server's mDNS advertiser so
/// its TXT record reflects the current `Server::auth_required()`
/// state. Called after every successful live auth toggle so
/// discovery clients see the new `auth_required=true|absent`
/// flag without waiting for a server restart.
///
/// **No-op when:**
/// - No server is running (`handle` is `None`).
/// - The user has advertising turned off (`advertise_row`
///   inactive). Honors the user's choice — we don't bring
///   advertising back online just because auth flipped.
///
/// **Error path:** `build_advertiser` failures log + toast
/// (same pattern as the initial server-start advertise failure).
/// The server itself keeps running without a fresh TXT; worst
/// case, clients see stale auth metadata until the server is
/// restarted. Never panics, never leaves a half-registered
/// advertiser in place. Per `CodeRabbit` round 1 on PR #406.
pub(super) fn refresh_advertiser_for_auth_change(
    handle: &mut RunningServer,
    widgets: &ServerSwitchWidgets,
    toast_overlay: &glib::WeakRef<adw::ToastOverlay>,
) {
    if !widgets.advertise_row.is_active() {
        // User turned advertising off — don't sneak it back on.
        return;
    }
    // Drop the old advertiser FIRST so its Drop-based unregister
    // fires before we re-announce under the same instance name.
    // mdns-sd allows back-to-back registers with the same name
    // but cleanly bracketed unregister/register avoids a window
    // where duplicate records briefly coexist on the LAN.
    drop(handle.advertiser.take());
    match build_advertiser(&handle.server, &widgets.nickname_row.text()) {
        Ok(adv) => {
            handle.advertiser = Some(adv);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "mDNS advertiser rebuild after auth toggle failed; TXT auth_required will be stale until next start"
            );
            if let Some(overlay) = toast_overlay.upgrade() {
                overlay.add_toast(plain_toast(&format!(
                    "Couldn't refresh mDNS advertisement after auth toggle: {e}"
                )));
            }
        }
    }
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

/// Master "Require key" toggle: apply to the running server first, refresh the advertiser, then mutate UI state (CR round 1 on PR #406).
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_require_toggle(
    panels: &SidebarPanels,
    running: &Rc<RefCell<Option<RunningServer>>>,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    widgets_weak: &ServerSwitchWidgetsWeak,
) {
    let running_for_auth_toggle = Rc::clone(running);
    let toast_overlay_for_auth_toggle = toast_overlay_weak.clone();
    let widgets_weak_for_auth_toggle = widgets_weak.clone();
    let auth_toggle_reentry_guard = Rc::new(std::cell::Cell::new(false));
    // Master "Require key" toggle.
    //
    // Order of operations (per `CodeRabbit` round 1 on PR #406):
    // 1. Apply the change to the running server FIRST.
    // 2. Refresh the mDNS advertiser so discovery TXT reflects
    //    the new `auth_required` flag.
    // 3. Only mutate UI state (current_auth_key, row visibility,
    //    subtitle, reveal button) after steps 1 and 2 succeeded.
    //
    // On any failure: revert the switch to its pre-toggle state
    // via `auth_toggle_reentry_guard` so UI ↔ server parity is
    // preserved. Discovery clients never see "auth advertised"
    // while the server is unauthed, or vice versa.
    //
    // When the server isn't running, steps 1+2 are no-ops and UI
    // mutation always proceeds — toggling auth with the switch
    // off is a config-only change and the next Start path
    // honors it via the pending-key plumbing.
    let key_row_for_toggle = panels.server.auth_key_row.downgrade();
    let reveal_button_for_toggle = panels.server.auth_key_reveal_button.downgrade();
    let current_key_for_toggle = Rc::clone(current_auth_key);
    let revealed_for_toggle = Rc::clone(auth_key_revealed);
    let auth_toggle_guard_for_handler = Rc::clone(&auth_toggle_reentry_guard);
    panels
        .server
        .auth_require_row
        .connect_active_notify(move |row| {
            on_auth_require_toggled(
                row,
                &auth_toggle_guard_for_handler,
                &key_row_for_toggle,
                &widgets_weak_for_auth_toggle,
                &running_for_auth_toggle,
                &toast_overlay_for_auth_toggle,
                &current_key_for_toggle,
                &revealed_for_toggle,
                &reveal_button_for_toggle,
            );
        });
}

/// Body of the require-key toggle handler: reentry guard, widget
/// upgrades, then the enable / disable halves with switch-revert on
/// failure. Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn on_auth_require_toggled(
    row: &adw::SwitchRow,
    auth_toggle_guard_for_handler: &Rc<std::cell::Cell<bool>>,
    key_row_for_toggle: &glib::WeakRef<adw::ActionRow>,
    widgets_weak_for_auth_toggle: &ServerSwitchWidgetsWeak,
    running_for_auth_toggle: &Rc<RefCell<Option<RunningServer>>>,
    toast_overlay_for_auth_toggle: &glib::WeakRef<adw::ToastOverlay>,
    current_key_for_toggle: &Rc<RefCell<Option<Vec<u8>>>>,
    revealed_for_toggle: &Rc<std::cell::Cell<bool>>,
    reveal_button_for_toggle: &glib::WeakRef<gtk4::Button>,
) {
    if auth_toggle_guard_for_handler.get() {
        // Re-entered from our own `set_active` revert
        // path — let the signal settle without running
        // the handler again.
        return;
    }
    let Some(key_row) = key_row_for_toggle.upgrade() else {
        return;
    };
    let widgets = widgets_weak_for_auth_toggle.upgrade();

    if row.is_active() {
        let ok = enable_auth_requirement(
            widgets.as_ref(),
            running_for_auth_toggle,
            toast_overlay_for_auth_toggle,
            current_key_for_toggle,
            revealed_for_toggle,
            &key_row,
            reveal_button_for_toggle,
        );
        if !ok {
            // Revert the switch. UI stays on the pre-toggle
            // state; the user can click again after resolving
            // the server issue.
            auth_toggle_guard_for_handler.set(true);
            row.set_active(false);
            auth_toggle_guard_for_handler.set(false);
        }
    } else {
        let ok = disable_auth_requirement(
            widgets.as_ref(),
            running_for_auth_toggle,
            toast_overlay_for_auth_toggle,
            current_key_for_toggle,
            revealed_for_toggle,
            &key_row,
        );
        if !ok {
            auth_toggle_guard_for_handler.set(true);
            row.set_active(true);
            auth_toggle_guard_for_handler.set(false);
        }
    }
}

/// Toggle-OFF half of the require-key handler — same order as the ON
/// half: server call first, UI mutation only after success. Returns
/// `false` when the caller must revert the switch. Split out per the
/// 50-NLOC gate (#817).
fn disable_auth_requirement(
    widgets: Option<&ServerSwitchWidgets>,
    running_for_auth_toggle: &Rc<RefCell<Option<RunningServer>>>,
    toast_overlay_for_auth_toggle: &glib::WeakRef<adw::ToastOverlay>,
    current_key_for_toggle: &Rc<RefCell<Option<Vec<u8>>>>,
    revealed_for_toggle: &Rc<std::cell::Cell<bool>>,
    key_row: &adw::ActionRow,
) -> bool {
    let server_result = apply_live_auth_change(
        running_for_auth_toggle,
        None,
        widgets,
        toast_overlay_for_auth_toggle,
    );
    if !server_result {
        return false;
    }
    *current_key_for_toggle.borrow_mut() = None;
    key_row.set_visible(false);
    // Zero the revealed flag too so a next toggle-on starts masked
    // regardless of the prior reveal state.
    revealed_for_toggle.set(false);
    true
}

/// Toggle-ON half of the require-key handler: generate/load the key,
/// apply it to the live server + advertiser, then reveal the key row
/// in masked state. Returns `false` when the live-server apply failed
/// and the caller must revert the switch. Split out per the 50-NLOC
/// gate (#817).
fn enable_auth_requirement(
    widgets: Option<&ServerSwitchWidgets>,
    running_for_auth_toggle: &Rc<RefCell<Option<RunningServer>>>,
    toast_overlay_for_auth_toggle: &glib::WeakRef<adw::ToastOverlay>,
    current_key_for_toggle: &Rc<RefCell<Option<Vec<u8>>>>,
    revealed_for_toggle: &Rc<std::cell::Cell<bool>>,
    key_row: &adw::ActionRow,
    reveal_button_for_toggle: &glib::WeakRef<gtk4::Button>,
) -> bool {
    // Pending key is the single source of truth for
    // both the server and any subsequent Reveal /
    // Copy. Generate / load once, reuse everywhere.
    let key = ensure_server_auth_key();

    // Step 1+2: apply to live server + refresh mDNS.
    let server_result = apply_live_auth_change(
        running_for_auth_toggle,
        Some(key.clone()),
        widgets,
        toast_overlay_for_auth_toggle,
    );
    if !server_result {
        return false;
    }

    // Step 3: UI mutation AFTER successful server change.
    *current_key_for_toggle.borrow_mut() = Some(key);
    key_row.set_visible(true);
    // Reset to masked state on every toggle-on so the
    // key row doesn't surface a previously-revealed
    // value across sessions.
    revealed_for_toggle.set(false);
    key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
    if let Some(rb) = reveal_button_for_toggle.upgrade() {
        rb.set_icon_name("view-reveal-symbolic");
        rb.set_tooltip_text(Some("Reveal key"));
        rb.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
    }
    true
}

/// Reveal/conceal + copy buttons for the auth-key row.
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_reveal_copy(
    panels: &SidebarPanels,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
) {
    // Reveal / conceal button — flips the subtitle between the
    // masked placeholder and the full hex-encoded key. Pure UI
    // state; doesn't touch keyring or server.
    let key_row_for_reveal = panels.server.auth_key_row.downgrade();
    let current_key_for_reveal = Rc::clone(current_auth_key);
    let revealed_for_reveal = Rc::clone(auth_key_revealed);
    panels
        .server
        .auth_key_reveal_button
        .connect_clicked(move |btn| {
            let Some(key_row) = key_row_for_reveal.upgrade() else {
                return;
            };
            let Ok(key_opt) = current_key_for_reveal.try_borrow() else {
                return;
            };
            let Some(bytes) = key_opt.as_ref() else {
                return;
            };
            let now_revealed = !revealed_for_reveal.get();
            revealed_for_reveal.set(now_revealed);
            if now_revealed {
                key_row.set_subtitle(&crate::sidebar::server_panel::auth_key_to_hex(bytes));
                btn.set_icon_name("view-conceal-symbolic");
                btn.set_tooltip_text(Some("Hide key"));
                // Flip the accessible label alongside the icon /
                // tooltip so screen readers announce the current
                // action rather than the stale build-time label.
                // Per `CodeRabbit` round 1 on PR #406.
                btn.update_property(&[gtk4::accessible::Property::Label("Hide key")]);
            } else {
                key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
                btn.set_icon_name("view-reveal-symbolic");
                btn.set_tooltip_text(Some("Reveal key"));
                btn.update_property(&[gtk4::accessible::Property::Label("Reveal key")]);
            }
        });

    wire_auth_copy_button(panels, current_auth_key, toast_overlay_weak);
}

/// Regenerate button: new key to keyring + live server + advertiser refresh.
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_regenerate(
    panels: &SidebarPanels,
    running: &Rc<RefCell<Option<RunningServer>>>,
    current_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    auth_key_revealed: &Rc<std::cell::Cell<bool>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
) {
    let running_for_auth_regen = Rc::clone(running);
    let toast_overlay_for_regen = toast_overlay_weak.clone();
    // Regenerate button — generates a fresh 32-byte key,
    // applies it to the live server, persists to keyring, and
    // updates the display row subtitle (preserving the current
    // revealed state so the user can verify the new value
    // immediately).
    //
    // Order of operations (per `CodeRabbit` round 2 on PR #406):
    // 1. Apply to the running server via
    //    `apply_live_auth_change` — shared with the toggle path.
    //    On failure (mutex poisoned, borrow race), toast + return
    //    BEFORE touching keyring or UI.
    // 2. Persist to keyring. Failure here is non-fatal (the
    //    in-memory key still works this session; next launch
    //    would read the OLD keyring value, which now forces the
    //    user to click Regenerate again — better than the old
    //    order where a keyring success + server failure would
    //    leave next-launch using a key the server never
    //    accepted).
    // 3. UI mutation (`current_auth_key`, subtitle, toast).
    //
    // Regenerate keeps `auth_required = true`, so the mDNS TXT
    // doesn't change — `apply_live_auth_change` skips the
    // advertiser rebuild when passed `widgets = None`.
    let key_row_for_regen = panels.server.auth_key_row.downgrade();
    let current_key_for_regen = Rc::clone(current_auth_key);
    let revealed_for_regen = Rc::clone(auth_key_revealed);
    panels
        .server
        .auth_key_regenerate_button
        .connect_clicked(move |_btn| {
            let Some(key_row) = key_row_for_regen.upgrade() else {
                return;
            };
            let fresh = sdr_server_rtltcp::auth::generate_random_auth_key();

            // Step 1: live server apply. `widgets = None` because
            // regenerate doesn't flip `auth_required`, so no
            // advertiser rebuild is needed.
            if !apply_live_auth_change(
                &running_for_auth_regen,
                Some(fresh.clone()),
                None,
                &toast_overlay_for_regen,
            ) {
                return;
            }

            // Step 2: persist to keyring. Failure is tolerable —
            // current in-memory key still works this session; the
            // user can click Regenerate again later when the
            // keyring recovers. Toast so they know, but don't
            // roll back the server (it already accepted the key).
            if let Err(e) = save_server_auth_key_to_keyring(&fresh) {
                tracing::warn!(%e, "rtl_tcp auth-key regenerate keyring write failed");
                if let Some(overlay) = toast_overlay_for_regen.upgrade() {
                    overlay.add_toast(plain_toast(&format!(
                        "Couldn't save new key to keyring: {e}"
                    )));
                }
            }

            // Step 3: UI mutation after server + persistence
            // settled.
            *current_key_for_regen.borrow_mut() = Some(fresh.clone());
            if revealed_for_regen.get() {
                key_row.set_subtitle(&crate::sidebar::server_panel::auth_key_to_hex(&fresh));
            } else {
                key_row.set_subtitle(crate::sidebar::server_panel::AUTH_KEY_MASKED_PLACEHOLDER);
            }
            if let Some(overlay) = toast_overlay_for_regen.upgrade() {
                overlay.add_toast(plain_toast("New key generated"));
            }
        });
}

/// Uptime / data-rate half of the status rows.
/// Split out per the 50-NLOC gate (#817).
fn render_uptime_and_rate_rows(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_bytes_sent: &Rc<std::cell::Cell<u64>>,
) {
    use crate::sidebar::server_panel::STATUS_IDLE_VALUE_SUBTITLE;
    let first = stats.connected_clients.first();
    // Uptime row — first client's uptime. PR B will show one row
    // per client, each with its own uptime.
    widgets.status_uptime_row.set_subtitle(&first.map_or_else(
        || STATUS_IDLE_VALUE_SUBTITLE.to_string(),
        |info| format_uptime(info.connected_since.elapsed()),
    ));

    // Data-rate row. Uses the cumulative `total_bytes_sent`
    // counter, which is monotonic within a single Server lifetime.
    // After a stop+start cycle the counter resets to 0 while
    // `last_bytes_sent` still holds the previous server's final
    // value — in that case `current < previous` is the restart
    // signal: rebase `last_bytes_sent` to the new counter and
    // report 0 bytes this tick rather than a bogus huge delta or
    // a long "0.0 kbps" flatline until the new server catches up
    // past the old final byte count. Per `CodeRabbit` round 2 on
    // PR #402.
    let current_bytes = stats.total_bytes_sent;
    let previous_bytes = last_bytes_sent.get();
    let delta = if current_bytes < previous_bytes {
        // Restart detected — the new server has already
        // accumulated `current_bytes` worth of traffic since its
        // start, so that's the best available estimate for
        // "bytes this tick". Reporting 0 or the saturating sub
        // would flatline the row until the new server exceeds
        // the old final count. Per `CodeRabbit` round 2 on
        // PR #402.
        current_bytes
    } else {
        current_bytes - previous_bytes
    };
    last_bytes_sent.set(current_bytes);
    widgets
        .status_data_rate_row
        .set_subtitle(&format_data_rate(delta, SERVER_STATUS_POLL_INTERVAL));

    // Commanded-state row — the most-recently-commanding client's
    // state. Pre-#392 any connected client can send `SetX`
    // commands, so picking the oldest client would let a later
    // peer's tune show up as the oldest peer's "stale" state.
    // `pick_most_recent_commander` resolves this by finding the
    // client whose `last_command` timestamp is newest (falls back
    // to the first connected client when nobody has commanded
    // yet). Post-#392, role-gated dispatch means only the
    // controller can record a command, so this helper naturally
    // resolves to the controller. Per `CodeRabbit` round 2 on
    // PR #402.
    let commander = pick_most_recent_commander(&stats.connected_clients);
    widgets
        .status_commanded_row
        .set_subtitle(&format_commanded_state(commander, &stats.initial));
}

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

/// Row rebuild for the clients list (diff key already decided a rebuild).
/// Split out per the 50-NLOC gate (#817).
fn rebuild_client_rows(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    last_rendered: &Rc<std::cell::Cell<Option<u64>>>,
    current_key: u64,
    now: Instant,
) {
    use crate::sidebar::server_panel::CLIENTS_LIST_EMPTY_SUBTITLE;
    // Invalidate the cache when the ListBox has been cleared
    // externally (by `reset_clients_list` on server stop). Without
    // this, an "empty set → empty set" transition across stop/start
    // would match the prior hash and short-circuit the first-tick
    // render, leaving the expander visually blank. The empty
    // state's placeholder row is a single child, so
    // `first_child().is_none()` distinguishes the reset state from
    // the rendered-empty state. Per `CodeRabbit` round 2 on PR #406.
    let list_was_reset = widgets.clients_list.first_child().is_none();
    if !list_was_reset && last_rendered.get() == Some(current_key) {
        return;
    }
    last_rendered.set(Some(current_key));

    // Clear the ListBox. GTK4 ListBox has no mass-remove.
    while let Some(child) = widgets.clients_list.first_child() {
        widgets.clients_list.remove(&child);
    }

    if stats.connected_clients.is_empty() {
        widgets
            .clients_row
            .set_subtitle(CLIENTS_LIST_EMPTY_SUBTITLE);
        let empty_row = adw::ActionRow::builder()
            .title(CLIENTS_LIST_EMPTY_SUBTITLE)
            .activatable(false)
            .css_classes(["dim-label"])
            .build();
        widgets.clients_list.append(&empty_row);
        return;
    }

    // Expander subtitle shows the count so a collapsed expander
    // still communicates whether the server has activity.
    let count = stats.connected_clients.len();
    widgets.clients_row.set_subtitle(&if count == 1 {
        "1 client".to_string()
    } else {
        format!("{count} clients")
    });

    append_client_rows(widgets, stats, now);
}

/// Copy button — always copies the FULL hex key.
/// Split out per the 50-NLOC gate (#817).
fn wire_auth_copy_button(
    panels: &SidebarPanels,
    current_key: &Rc<RefCell<Option<Vec<u8>>>>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
) {
    let toast_overlay_for_copy = toast_overlay_weak.clone();
    // Copy button — always copies the FULL hex key regardless of
    // reveal state. Users typically click Copy without clicking
    // Reveal first.
    let current_key_for_copy = Rc::clone(current_key);
    panels
        .server
        .auth_key_copy_button
        .connect_clicked(move |btn| {
            let Ok(key_opt) = current_key_for_copy.try_borrow() else {
                return;
            };
            let Some(bytes) = key_opt.as_ref() else {
                return;
            };
            let hex = crate::sidebar::server_panel::auth_key_to_hex(bytes);
            // Grab the display's clipboard via the button's widget
            // ancestry. `clipboard()` on a widget returns the
            // primary clipboard for the display it's attached to.
            let clipboard = btn.clipboard();
            clipboard.set_text(&hex);
            if let Some(overlay) = toast_overlay_for_copy.upgrade() {
                overlay.add_toast(plain_toast("Key copied to clipboard"));
            }
        });
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

/// Per-client row build: controller first, listeners in registry order.
/// Split out per the 50-NLOC gate (#817).
fn append_client_rows(
    widgets: &ServerStatusWidgets,
    stats: &sdr_server_rtltcp::ServerStats,
    now: Instant,
) {
    // Build per-client rows. Controller first (if any) so the
    // accent-colored row sits at the top; listeners render below
    // in the order the registry has them (acceptance order, per
    // `ClientRegistry`). Order isn't a hard contract — if a
    // future registry reorders for its own reasons, this just
    // changes visual order.
    let mut ordered: Vec<&sdr_server_rtltcp::ClientInfo> = stats.connected_clients.iter().collect();
    ordered.sort_by_key(|c| match c.role {
        sdr_server_rtltcp::extension::Role::Control => 0u8,
        sdr_server_rtltcp::extension::Role::Listen => 1u8,
    });

    // Reuse the `now` captured for the diff-key hash so the
    // displayed duration and the hashed `elapsed_secs` are
    // sampled from the same instant — avoids a split where
    // the hash matches but the render shows a one-tick-newer
    // duration (or vice-versa).
    for info in ordered {
        let (role_label, role_css) = match info.role {
            sdr_server_rtltcp::extension::Role::Control => ("Controller", "accent"),
            sdr_server_rtltcp::extension::Role::Listen => ("Listener", "dim-label"),
        };
        let duration = format_uptime(now.saturating_duration_since(info.connected_since));
        let subtitle = if info.buffers_dropped > 0 {
            format!(
                "{role_label} · {duration} · {drops} drops",
                drops = info.buffers_dropped
            )
        } else {
            format!("{role_label} · {duration}")
        };
        let row = adw::ActionRow::builder()
            .title(info.peer.to_string())
            .subtitle(&subtitle)
            .activatable(false)
            .build();
        // Prefix badge: a colored dot (accent for Control, dim
        // for Listen). Small and unobtrusive but enough to
        // distinguish the controller at a glance in a dense list.
        let badge = gtk4::Image::from_icon_name("media-record-symbolic");
        badge.add_css_class(role_css);
        row.add_prefix(&badge);
        widgets.clients_list.append(&row);
    }
}
