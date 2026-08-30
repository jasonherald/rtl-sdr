//! `rtl_tcp` client-connect wiring: connection-state toasts,
//! shared connect sequence, controller-busy/auth/connected arms,
//! saved-server state restore, and client-side auth-key keyring
//! plumbing. Split out of `window/source.rs` per the Codacy
//! large-file gate (#846).

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{
    AppState, DEVICE_RTLTCP, NETWORK_PROTOCOL_TCPCLIENT_IDX, Rc, RefCell, SidebarPanels,
    SourceType, TOAST_TIMEOUT_PERSISTENT, TOAST_TIMEOUT_SHORT_SECS, UiToDsp, adw, glib, sidebar,
};
use super::FavoritesMap;
use crate::window::dsp_events::DspEventCtx;

/// Called only from the edge-detection path in
/// `handle_dsp_message`; the caller already verified
/// `prev_disc != now_disc` and stored the new discriminant.
/// Per issue #396.
#[allow(
    clippy::too_many_arguments,
    reason = "toast composition needs read access to multiple panel widgets \
              + a dispatch handle; collapsing into a single context struct \
              would move the same argument count one layer up"
)]
#[allow(
    clippy::doc_markdown,
    reason = "doc references to Connected / ControllerBusy / AuthRequired / \
              AuthFailed are type variants — enum paths would make the prose \
              unreadable; backticks on each would overwhelm the paragraph"
)]
#[allow(
    clippy::too_many_lines,
    reason = "linear arm-by-arm toast + row + state handling for all 8 rtl_tcp connection-state variants; splitting would scatter the shared setup (pending-toasts sweep, edge-log) and obscure the 1:1 mapping from variant to UX gesture"
)]
pub(in crate::window) fn handle_rtl_tcp_state_toast(
    state_val: &sdr_types::RtlTcpConnectionState,
    prev_disc: u8,
    ctx: &DspEventCtx,
) {
    use sdr_types::RtlTcpConnectionState;

    let app_state = &ctx.state;
    let toast_overlay_weak = &ctx.toast_overlay_weak;
    let role_row_weak = &ctx.rtl_tcp_role_row_weak;
    let auth_key_row_weak = &ctx.rtl_tcp_auth_key_row_weak;
    let hostname_row_weak = &ctx.rtl_tcp_hostname_row_weak;
    let port_row_weak = &ctx.rtl_tcp_port_row_weak;
    let pending_controller_busy_toasts = &ctx.pending_controller_busy_toasts;

    // Sweep any still-live ControllerBusy toasts on any
    // transition that isn't re-entering ControllerBusy. Pre-
    // `CodeRabbit` round 11 on PR #408 each ControllerBusy
    // toast's button handler only dismissed itself, so a stale
    // "Take control" / "Connect as Listener" action sat visible
    // after the server went away (Connected directly, Disconnect,
    // Failed, etc.) and could later rebuild the source
    // unexpectedly against a healthy session. The
    // `timeout(0)` persistence is intentional — we WANT these to
    // stick around until the user interacts OR the state
    // resolves itself — but "the state resolved itself" needs
    // its own cleanup pass.
    if !matches!(state_val, RtlTcpConnectionState::ControllerBusy) {
        dismiss_stale_controller_busy_toasts(pending_controller_busy_toasts);
    }

    match state_val {
        RtlTcpConnectionState::ControllerBusy => on_rtl_tcp_controller_busy(
            app_state,
            toast_overlay_weak,
            role_row_weak,
            pending_controller_busy_toasts,
        ),

        RtlTcpConnectionState::AuthRequired => on_rtl_tcp_auth_required(
            app_state,
            toast_overlay_weak,
            auth_key_row_weak,
            hostname_row_weak,
            port_row_weak,
        ),

        RtlTcpConnectionState::AuthFailed => on_rtl_tcp_auth_failed(
            app_state,
            toast_overlay_weak,
            auth_key_row_weak,
            hostname_row_weak,
            port_row_weak,
        ),

        RtlTcpConnectionState::Connected { .. } => on_rtl_tcp_connected(
            prev_disc,
            app_state,
            auth_key_row_weak,
            hostname_row_weak,
            port_row_weak,
        ),

        // Non-toast states (Disconnected / Connecting / Retrying
        // / Failed) just update the status row subtitle via the
        // sibling call in `handle_dsp_message`. No additional
        // UX gesture needed here.
        RtlTcpConnectionState::Disconnected
        | RtlTcpConnectionState::Connecting
        | RtlTcpConnectionState::Retrying { .. }
        | RtlTcpConnectionState::Failed { .. } => {}
    }
}

/// Sweep still-live `ControllerBusy` toasts on any transition that
/// isn't re-entering `ControllerBusy` (CR round 11 on PR #408). Split
/// out per the 50-NLOC gate (#817).
fn dismiss_stale_controller_busy_toasts(
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
) {
    let mut pending = pending_controller_busy_toasts.borrow_mut();
    for weak in pending.drain(..) {
        if let Some(toast) = weak.upgrade() {
            toast.dismiss();
        }
    }
}

/// Record the currently-displayed `rtl_tcp` server's `host:port`
/// on `AppState` so a subsequent successful `Connected` can save
/// the just-entered key to the right per-server keyring entry.
/// Empty on upgrade failure — the save path skips when the
/// cached identity is empty. Per #396.
///
/// **Cache-preserving fallback** (per `CodeRabbit` round 2 on
/// PR #408): if `app_state.rtl_tcp_active_server` is already
/// non-empty, this is a no-op. `apply_rtl_tcp_connect` writes
/// the stable advertised `hostname:port` (same form as
/// `favorite_key(server)`) directly into the cache at
/// connect-setup time, so every downstream per-server lookup
/// (keyring load/save/clear, favorite match) keys off the same
/// identity. Reading `hostname_row.text()` here would overwrite
/// the stable id with whatever the DSP is dialing — for
/// discovery connects that can be a resolved IPv4/IPv6 literal,
/// splitting "shack-pi.local.:1234" (favorites) from
/// "192.168.1.17:1234" (keyring) and breaking round-trip. The
/// widget-read fallback only runs in the manually-typed Play
/// path where `apply_rtl_tcp_connect` never ran.
pub(super) fn record_active_rtl_tcp_server(
    app_state: &Rc<AppState>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    if !app_state.rtl_tcp_active_server.borrow().is_empty() {
        return;
    }
    let Some(host_row) = hostname_row_weak.upgrade() else {
        return;
    };
    let Some(port_row) = port_row_weak.upgrade() else {
        return;
    };
    let host = host_row.text().to_string();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let port = port_row.value() as u16;
    if !host.is_empty() && port != 0 {
        *app_state.rtl_tcp_active_server.borrow_mut() = format!("{host}:{port}");
    }
}

/// Invalidate the cached active `rtl_tcp` server identity when
/// the hostname / port widgets no longer match it. Called from
/// the `hostname_row.connect_changed` + `port_row.connect_value_
/// notify` handlers so a manual edit retargets per-server state
/// (keyring lookups, favorite matches, `rtl_tcp_active_server`)
/// to the newly-typed endpoint.
///
/// Without this, after the startup `LastConnectedServer` restore
/// or an `apply_rtl_tcp_connect` seeded the cache, typing a
/// different host or port in the source row would leave the
/// cache pointing at the old server — the first subsequent
/// `AuthFailed` / `Connected` arm would then
/// clear/save the key under the WRONG server. Per
/// `CodeRabbit` round 4 on PR #408.
///
/// **Comparison guard:** the cache is cleared only when its
/// current value differs from the widget-derived key. That
/// keeps `apply_rtl_tcp_connect`'s own `hostname_row.set_text` /
/// `port_row.set_value` writes (which fire these same handlers)
/// from spuriously clobbering the stable id the caller just
/// wrote. During a caller-driven server switch the cache IS
/// stale at the widget-write moment (old server id, new widget
/// text), so this invalidation fires correctly there too —
/// `apply_rtl_tcp_connect` overwrites the empty cache right
/// afterwards with the new stable id.
///
/// Also clears the auth-key row (visibility + text) so the
/// old server's key bytes can't leak onto a different endpoint.
/// The row's `connect_changed` handler re-dispatches
/// `SetRtlTcpClientConfig { auth_key: None, .. }` so DSP state
/// tracks the invalidation in lockstep with the UI.
pub(super) fn invalidate_rtl_tcp_active_server_on_edit(
    app_state: &Rc<AppState>,
    hostname_row: &adw::EntryRow,
    port_row: &adw::SpinRow,
    auth_key_row: &adw::PasswordEntryRow,
) {
    let hostname = hostname_row.text().to_string();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let port = port_row.value() as u16;
    let current_key = format!("{hostname}:{port}");
    let should_clear = {
        let cached = app_state.rtl_tcp_active_server.borrow();
        !cached.is_empty() && *cached != current_key
    };
    if should_clear {
        app_state.rtl_tcp_active_server.borrow_mut().clear();
        auth_key_row.set_visible(false);
        auth_key_row.set_text("");
    }
}

/// Save the current Server-key-row text to the keyring under
/// the active `rtl_tcp` server's `host:port`. Called on a
/// successful Connected following AuthRequired / AuthFailed.
/// Empty text → clear the saved entry instead of writing empty
/// bytes; invalid hex → log + skip (the live connection
/// obviously accepted the text, but our keyring round-trip
/// demands valid hex). Per #396.
#[allow(
    clippy::doc_markdown,
    reason = "Connected / AuthRequired / AuthFailed are enum variants"
)]
pub(super) fn save_current_auth_key_for_active_server(
    app_state: &Rc<AppState>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
) {
    let active = app_state.rtl_tcp_active_server.borrow().clone();
    if active.is_empty() {
        return;
    }
    let Some((host, port_str)) = active.rsplit_once(':') else {
        return;
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return;
    };
    let Some(row) = auth_key_row_weak.upgrade() else {
        return;
    };
    let text = row.text().to_string();
    if text.is_empty() {
        // User explicitly cleared the field BEFORE this connect
        // succeeded — mirror that intent in the keyring by
        // deleting the saved entry. Pre-CodeRabbit round 3 on
        // PR #408 this branch returned early with a stale
        // "nothing to save" comment, so clearing the row and
        // reconnecting left the old bytes in the keyring and
        // `apply_rtl_tcp_connect` would preload them on the
        // next discovery / favorites / last-connected path,
        // silently undoing the user's clear.
        if let Err(e) = clear_client_auth_key_from_keyring(host, port) {
            tracing::warn!(
                server = %active,
                %e,
                "rtl_tcp: client auth key keyring clear failed (empty row)"
            );
        }
        return;
    }
    let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&text) else {
        tracing::warn!(
            server = %active,
            "rtl_tcp: client auth key hex is invalid — skipping keyring save"
        );
        return;
    };
    if let Err(e) = save_client_auth_key_to_keyring(host, port, &bytes) {
        tracing::warn!(
            server = %active,
            %e,
            "rtl_tcp: client auth key keyring save failed"
        );
    } else {
        tracing::info!(
            server = %active,
            "rtl_tcp: client auth key saved to keyring for next reconnect"
        );
    }
}

pub(in crate::window) fn apply_rtl_tcp_connection_state(
    status_row: &adw::ActionRow,
    disconnect_button: &gtk4::Button,
    retry_button: &gtk4::Button,
    state: &sdr_types::RtlTcpConnectionState,
) {
    use sdr_types::RtlTcpConnectionState;
    status_row.set_subtitle(&sidebar::source_panel::format_rtl_tcp_state(state));
    let is_active = matches!(
        state,
        RtlTcpConnectionState::Connecting
            | RtlTcpConnectionState::Connected { .. }
            | RtlTcpConnectionState::Retrying { .. }
    );
    // "Retry now" is only meaningful when there's an active source
    // to short-circuit out of its backoff wait — Retrying (most
    // common) or any of the four terminal states (Failed +
    // role-denials added in #396). After an explicit Disconnect
    // the controller drops `state.source`, and
    // `UiToDsp::RetryRtlTcpNow` is a no-op (it checks
    // `state.source.as_mut()` → None → early return). Leaving the
    // button visibly enabled in that state misleads the user into
    // thinking they can reconnect in one click; the correct
    // post-Disconnect path is to press Play.
    let can_retry_now = matches!(
        state,
        RtlTcpConnectionState::Retrying { .. } | RtlTcpConnectionState::Failed { .. }
    ) || state.needs_user_action();
    disconnect_button.set_sensitive(is_active);
    retry_button.set_sensitive(can_retry_now);
}

/// Dispatch half of [`apply_rtl_tcp_connect`]: role + auth-key config,
/// device selection, canonical `SetNetworkConfig`, and last-connected
/// persistence. Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn dispatch_rtl_tcp_connect(
    host: &str,
    port: u16,
    nickname: &str,
    role_row: &adw::ComboRow,
    auth_key_row: &adw::PasswordEntryRow,
    device_row: &adw::ComboRow,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use crate::sidebar::source_panel::{FavoriteRole, RTL_TCP_ROLE_LISTEN_IDX};

    let requested_role = match role_row.selected() {
        RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
        _ => FavoriteRole::Control,
    }
    .as_wire_role();
    let key_text = auth_key_row.text().to_string();
    let auth_key: Option<Vec<u8>> = if key_text.is_empty() {
        None
    } else {
        crate::sidebar::server_panel::auth_key_from_hex(&key_text)
    };
    state.send_dsp(UiToDsp::SetRtlTcpClientConfig {
        requested_role,
        auth_key,
    });
    device_row.set_selected(DEVICE_RTLTCP);
    state.send_dsp(UiToDsp::SetNetworkConfig {
        hostname: host.to_string(),
        port,
        protocol: sdr_types::Protocol::TcpClient,
    });
    crate::sidebar::source_panel::save_last_connected(
        config,
        &crate::sidebar::source_panel::LastConnectedServer {
            host: host.to_string(),
            port,
            nickname: nickname.to_string(),
        },
    );
}

/// Execute the shared RTL-TCP connect sequence — used by both the
/// discovery-row Connect button and the favorites-popover Connect
/// button. Centralizes the ordering-sensitive steps so a future
/// fix can't land on one caller and miss the other:
///
/// 1. **Snapshot** `already_rtl_tcp` before touching `device_row`.
///    If the selector was ALREADY on RTL-TCP, `set_selected` is a
///    no-op and the device-row notify handler won't fire — we
///    need to dispatch `SetSourceType` ourselves to force the
///    controller to reopen the source against the new endpoint.
///    If it was on a different source type, the notify handler
///    fires and dispatches `SetSourceType` for us; an explicit
///    send here would double-open.
///
/// 2. **Pin TCP** on `protocol_row` BEFORE writing host / port.
///    `hostname_row.set_text` and `port_row.set_value` fire
///    change handlers that re-read `protocol_row.selected()` to
///    build their `SetNetworkConfig`. If the shared protocol row
///    is still on UDP from a prior raw-Network session, those
///    handlers would dispatch a stale-UDP config against the
///    clicked endpoint before the RTL-TCP switch lands — a
///    transient retarget of any live raw-Network source. `rtl_tcp`
///    is always TCP, so we force TCP unconditionally.
///
/// 3. **Write host / port**, flip `device_row` to RTL-TCP, dispatch
///    the fresh `SetNetworkConfig`, persist a `LastConnectedServer`
///    snapshot so next launch pre-populates the fields without
///    waiting for mDNS.
///
/// 4. **Conditionally** dispatch `SetSourceType(RtlTcp)` — only when
///    `already_rtl_tcp` was true (step 1's rationale).
///
/// Caller-owned follow-ups (popover `popdown`, etc.) happen after
/// this helper returns.
/// Borrowed handles to the six source-panel rows the RTL-TCP connect
/// path rewrites. Both callers (favorites Connect button, discovered-
/// row Connect button) build this shim from the row handles they
/// already own — strong clones or weak-upgraded strongs alike.
#[allow(
    clippy::struct_field_names,
    reason = "fields deliberately mirror the source-panel row names they borrow"
)]
pub(super) struct RtlTcpConnectRows<'a> {
    pub(super) hostname_row: &'a adw::EntryRow,
    pub(super) port_row: &'a adw::SpinRow,
    pub(super) protocol_row: &'a adw::ComboRow,
    pub(super) device_row: &'a adw::ComboRow,
    pub(super) role_row: &'a adw::ComboRow,
    pub(super) auth_key_row: &'a adw::PasswordEntryRow,
}

pub(super) fn apply_rtl_tcp_connect(
    host: &str,
    port: u16,
    nickname: &str,
    rows: &RtlTcpConnectRows<'_>,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    let &RtlTcpConnectRows {
        hostname_row,
        port_row,
        protocol_row,
        device_row,
        role_row,
        auth_key_row,
    } = rows;
    let already_rtl_tcp = device_row.selected() == DEVICE_RTLTCP;
    // Guard the programmatic row rewrites so the per-field
    // handlers don't clobber `KEY_SOURCE_NETWORK_*` (which belong
    // to the user's independent raw-Network selection) with the
    // RTL-TCP endpoint. While the hydration flag is set, the
    // handlers suppress BOTH the persistence write AND the
    // per-edit `SetNetworkConfig` dispatch — three sequential row
    // mutations otherwise fan out to three intermediate
    // reconnects against a partially-rewritten triple. A single
    // canonical `SetNetworkConfig` is dispatched further down
    // (after the flag clears) so the DSP gets the fully-formed
    // endpoint exactly once. Per `CodeRabbit` rounds 1, 2, and 5
    // on PR #558.
    state.rtl_tcp_hydration_in_progress.set(true);
    protocol_row.set_selected(NETWORK_PROTOCOL_TCPCLIENT_IDX);
    hostname_row.set_text(host);
    port_row.set_value(f64::from(port));
    state.rtl_tcp_hydration_in_progress.set(false);
    restore_saved_server_state(host, port, role_row, auth_key_row, state, config);

    dispatch_rtl_tcp_connect(
        host,
        port,
        nickname,
        role_row,
        auth_key_row,
        device_row,
        state,
        config,
    );
    if already_rtl_tcp {
        state.send_dsp(UiToDsp::SetSourceType(SourceType::RtlTcp));
    }
}

/// Keyring-entry prefix for per-server **client** auth keys. The
/// full entry name is `{prefix}-{host}:{port}` — per-server
/// so the user can save distinct keys for distinct servers on
/// the LAN (different owners, different rotation schedules).
/// Kept distinct from `KEYRING_KEY_AUTH_KEY` (which stores the
/// local server's own key, single entry) so neither surface
/// ever reads the other's bytes by accident. Per issue #396.
pub(super) const KEYRING_KEY_CLIENT_AUTH_KEY_PREFIX: &str = "rtl_tcp-client-auth-key-";

/// Build the keyring entry name for a client-side saved key
/// keyed by the server's `host:port` identity. Matches the
/// identity `FavoriteEntry.key` uses, so the keyring entry
/// survives server rename / nickname change. Per issue #396.
pub(super) fn client_auth_key_entry_name(host: &str, port: u16) -> String {
    format!("{KEYRING_KEY_CLIENT_AUTH_KEY_PREFIX}{host}:{port}")
}

/// Load the saved auth key for the given `rtl_tcp` server, if
/// the user previously connected successfully with a key
/// against this `host:port`. Returns `None` for missing /
/// corrupt / keyring-unavailable cases — callers treat that
/// as "ask the user for a key" rather than silently connecting
/// without one. Per issue #396.
#[allow(
    dead_code,
    reason = "wired up in the #396 commit that adds the Server key entry row"
)]
pub(super) fn load_client_auth_key_from_keyring(host: &str, port: u16) -> Option<Vec<u8>> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_SERVICE, auth_key_from_hex};

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    match store.get(&entry) {
        Ok(Some(hex)) => {
            let Some(bytes) = auth_key_from_hex(&hex) else {
                tracing::warn!(
                    entry = %entry,
                    "rtl_tcp client auth key in keyring is malformed hex; treating as missing"
                );
                return None;
            };
            Some(bytes)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%e, entry = %entry, "rtl_tcp client auth key keyring read failed");
            None
        }
    }
}

/// Save a successfully-used client auth key for the given
/// server to the OS keyring. Called AFTER a successful
/// auth-required connect so the user doesn't have to re-enter
/// the key on subsequent reconnects to the same server. A
/// keyring write failure is non-fatal — the current session
/// still works; the next launch will just prompt for the key
/// again. Per issue #396.
#[allow(
    dead_code,
    reason = "wired up in the #396 commit that adds the Server key entry row"
)]
pub(super) fn save_client_auth_key_to_keyring(
    host: &str,
    port: u16,
    bytes: &[u8],
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_SERVICE, auth_key_to_hex};

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    store.set(&entry, &auth_key_to_hex(bytes))
}

/// Delete a saved client auth key for the given server. Called
/// from the UI when the user explicitly clears the key (e.g.
/// the server regenerated on the other end and the old key no
/// longer works; clearing avoids auto-sending the dead key on
/// every reconnect attempt). Missing-entry is treated as
/// success — the goal is "there is no saved key after this
/// call," which a missing entry already satisfies. Per #396.
pub(super) fn clear_client_auth_key_from_keyring(
    host: &str,
    port: u16,
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::KEYRING_SERVICE;

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    store.delete(&entry)
}

/// `ControllerBusy` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_controller_busy(
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    role_row_weak: &glib::WeakRef<adw::ComboRow>,
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
) {
    // Toast with two action buttons: "Connect as
    // Listener" flips the role combo (its change handler
    // re-dispatches SetRtlTcpClientConfig) and fires a
    // normal retry; "Take control" dispatches the one-shot
    // `RetryRtlTcpWithTakeover` message which rebuilds
    // the source with `request_takeover = true` on the
    // hello.
    let Some(overlay) = toast_overlay_weak.upgrade() else {
        return;
    };
    sweep_prior_controller_busy_toasts(pending_controller_busy_toasts);

    let toast = adw::Toast::builder()
        .title("Controller slot is occupied on this server.")
        .timeout(TOAST_TIMEOUT_PERSISTENT)
        .build();
    let listen_toast = adw::Toast::builder()
        .title("Or connect as Listener (read-only).")
        .timeout(TOAST_TIMEOUT_PERSISTENT)
        .build();
    // Cross-dismiss: clicking either action dismisses
    // BOTH toasts, so a stale sibling action can't fire
    // later against a session that's already resolved.
    // `WeakRef` rather than strong clones — the toasts
    // hand out their own strong refs to the overlay
    // internally, and we only need to reach the sibling
    // when it's still live.
    let toast_weak = toast.downgrade();
    let listen_toast_weak = listen_toast.downgrade();

    // Track the two action buttons as separate signals.
    // AdwToast supports a single primary action via
    // `set_button_label` + `connect_button_clicked`; the
    // "Take control" action lands there, and the
    // "Connect as Listener" option lives in the
    // sibling toast below so users still see both
    // choices.
    toast.set_button_label(Some("Take control"));
    let state_for_takeover = Rc::clone(app_state);
    let listen_weak_for_takeover = listen_toast_weak.clone();
    toast.connect_button_clicked(move |t| {
        state_for_takeover.send_dsp(UiToDsp::RetryRtlTcpWithTakeover);
        t.dismiss();
        if let Some(sibling) = listen_weak_for_takeover.upgrade() {
            sibling.dismiss();
        }
    });
    overlay.add_toast(toast);

    wire_listener_fallback_toast(
        app_state,
        role_row_weak,
        &overlay,
        &listen_toast,
        &toast_weak,
    );

    // Record the pair so the non-ControllerBusy state
    // transition at the top of this function can sweep
    // them if the server resolves itself without user
    // interaction.
    {
        let mut pending = pending_controller_busy_toasts.borrow_mut();
        pending.push(toast_weak);
        pending.push(listen_toast_weak);
    }
}

/// `AuthRequired` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_auth_required(
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    // Remember the active server so a subsequent
    // successful Connected can save the user-entered
    // key to the right keyring entry.
    record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
    // Reveal + focus the Server key field so the user
    // can enter the key.
    if let Some(row) = auth_key_row_weak.upgrade() {
        row.set_visible(true);
        row.grab_focus();
    }
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let toast = adw::Toast::builder()
            .title("Server requires an authentication key.")
            .timeout(TOAST_TIMEOUT_SHORT_SECS)
            .build();
        overlay.add_toast(toast);
    }
}

/// `AuthFailed` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_auth_failed(
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
    // Clear the saved per-server key from the keyring
    // too — not just the widget. Pre-CodeRabbit round 2
    // on PR #408 only `row.set_text("")` was called, so
    // the keyring entry survived the rejection and the
    // next discovery / favorites / Play-restart path
    // would auto-load the same rejected bytes into the
    // row via `apply_rtl_tcp_connect` / the startup
    // restore, silently bouncing the user straight back
    // into `AuthFailed`. Now we delete the saved key
    // whenever the server explicitly rejects it; the
    // user has to re-enter (or paste the new) key on
    // the next attempt, which is the only recovery path
    // from a rotated server key anyway. Per issue #396.
    let active = app_state.rtl_tcp_active_server.borrow().clone();
    if let Some((host, port_str)) = active.rsplit_once(':')
        && let Ok(port) = port_str.parse::<u16>()
        && let Err(e) = clear_client_auth_key_from_keyring(host, port)
    {
        tracing::warn!(
            server = %active,
            %e,
            "rtl_tcp: client auth key keyring clear on AuthFailed failed (non-fatal)"
        );
    }
    if let Some(row) = auth_key_row_weak.upgrade() {
        row.set_visible(true);
        row.grab_focus();
        // Clear the entered value so the user doesn't
        // re-submit the same wrong key by reflex on the
        // next Retry.
        row.set_text("");
    }
    if let Some(overlay) = toast_overlay_weak.upgrade() {
        let toast = adw::Toast::builder()
            .title("Key rejected. Check with the server owner.")
            .timeout(TOAST_TIMEOUT_SHORT_SECS)
            .build();
        overlay.add_toast(toast);
    }
}

/// `Connected { .. }` arm of [`handle_rtl_tcp_state_toast`]. Split out per the
/// 50-NLOC gate (#817).
fn on_rtl_tcp_connected(
    prev_disc: u8,
    app_state: &Rc<AppState>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
) {
    use crate::state::{
        RTL_TCP_STATE_DISC_AUTH_FAILED, RTL_TCP_STATE_DISC_AUTH_REQUIRED,
        RTL_TCP_STATE_DISC_CONNECTING, RTL_TCP_STATE_DISC_CONTROLLER_BUSY,
    };

    // Save the user-entered key to the per-server
    // keyring so subsequent reconnects auto-use it.
    // Fires on the edge from any of:
    //
    // - `AuthRequired` / `AuthFailed` — user typed a
    //   key in response to a denial toast;
    // - `Connecting` — user had auth configured up
    //   front (server advertised `auth_required` via
    //   mDNS, key was entered before the first
    //   connect, and the handshake succeeded in a
    //   single `Connecting → Connected` hop);
    // - `ControllerBusy` — user entered a key before
    //   the first connect, server denied with
    //   `ControllerBusy`, and the user's subsequent
    //   Take-control / Listener retry (via
    //   `RetryRtlTcpWithTakeover` or `RetryRtlTcpNow`)
    //   succeeded. Added per `CodeRabbit` round 12 on
    //   PR #408 — without this branch an auth-required
    //   server that's also busy on the first attempt
    //   would accept the key on the takeover reconnect
    //   but never persist it to the keyring.
    //
    // Pre-round-1 on PR #408 only the auth-denial arms
    // triggered the save, so up-front keys never hit the
    // keyring and the user had to re-type them on every
    // reconnect. `save_current_auth_key_for_active_
    // server` is a no-op when the key row is empty, so
    // this is safe to trigger on every qualifying edge
    // even if the server doesn't require auth. Call
    // `record_active_rtl_tcp_server` first so the save-
    // path sees the right `host:port` even when the
    // user never hit an auth-denial arm (which is what
    // previously set the cache).
    if prev_disc == RTL_TCP_STATE_DISC_CONNECTING
        || prev_disc == RTL_TCP_STATE_DISC_CONTROLLER_BUSY
        || prev_disc == RTL_TCP_STATE_DISC_AUTH_REQUIRED
        || prev_disc == RTL_TCP_STATE_DISC_AUTH_FAILED
    {
        record_active_rtl_tcp_server(app_state, hostname_row_weak, port_row_weak);
        save_current_auth_key_for_active_server(app_state, auth_key_row_weak);
    }
}

/// Second `ControllerBusy` toast offering the Listen fallback (`AdwToast` exposes only one action button).
/// Split out per the 50-NLOC gate (#817).
fn wire_listener_fallback_toast(
    app_state: &Rc<AppState>,
    role_row_weak: &glib::WeakRef<adw::ComboRow>,
    overlay: &adw::ToastOverlay,
    listen_toast: &adw::Toast,
    toast_weak: &glib::WeakRef<adw::Toast>,
) {
    // Second toast offering the Listen fallback. Two
    // separate toasts beats a single one because AdwToast
    // exposes only one action button — splitting the two
    // paths keeps both discoverable.
    listen_toast.set_button_label(Some("Connect as Listener"));
    let state_for_listen = Rc::clone(app_state);
    let role_row_for_listen = role_row_weak.clone();
    let toast_weak_for_listen = toast_weak.clone();
    listen_toast.connect_button_clicked(move |t| {
        if let Some(role_row) = role_row_for_listen.upgrade() {
            // Flipping the combo to Listen fires its
            // `selected-notify` handler which dispatches
            // `SetRtlTcpClientConfig` with the new role.
            // Follow with RetryRtlTcpNow so the user
            // doesn't have to click Retry themselves.
            role_row.set_selected(crate::sidebar::source_panel::RTL_TCP_ROLE_LISTEN_IDX);
        }
        state_for_listen.send_dsp(UiToDsp::RetryRtlTcpNow);
        t.dismiss();
        if let Some(sibling) = toast_weak_for_listen.upgrade() {
            sibling.dismiss();
        }
    });
    overlay.add_toast(listen_toast.clone());
}

/// Per-server role + auth-key restore (#396) before the dispatch.
/// Split out per the 50-NLOC gate (#817).
fn restore_saved_server_state(
    host: &str,
    port: u16,
    role_row: &adw::ComboRow,
    auth_key_row: &adw::PasswordEntryRow,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    use crate::sidebar::source_panel::{
        FavoriteRole, KEY_RTL_TCP_CLIENT_LAST_ROLE, RTL_TCP_ROLE_CONTROL_IDX,
        RTL_TCP_ROLE_LISTEN_IDX, load_favorites,
    };
    // Restore saved per-server state (#396) BEFORE the
    // `SetNetworkConfig` / `SetSourceType` dispatch so the DSP
    // thread's first use of the new endpoint already carries the
    // right `requested_role` + `auth_key`. Pre-CodeRabbit round 1
    // on PR #408 this helper only pushed host / port / source,
    // which meant the new favorite metadata (`requested_role`,
    // `auth_required`) and per-server client-key keyring helpers
    // were inert from the discovery + favorites entry points —
    // role always reverted to the global default and keys never
    // auto-filled.
    //
    // Resolution order for role:
    // - If the server is a favorite and that favorite carries a
    //   `requested_role`, use it.
    // - Otherwise fall back to the global
    //   `KEY_RTL_TCP_CLIENT_LAST_ROLE` default (if any).
    // - Otherwise leave the picker alone (Control is the
    //   picker's built-in default for fresh servers).
    //
    // For the auth-key row:
    // - Reveal the row if the favorite's `auth_required` is
    //   `Some(true)` — user doesn't have to hit an
    //   `AuthRequired` denial before seeing the field.
    // - Load any saved keyring hex for this `host:port` and
    //   pre-fill the row so the subsequent connect succeeds in
    //   a single `Connecting → Connected` hop.
    //
    // Both operations are no-ops for servers we've never
    // favorited AND never connected to; the picker stays on
    // Control and the row stays hidden, matching pre-#408
    // behavior.
    // Stable-id rule (per CodeRabbit round 2 on PR #408): all
    // per-server state — keyring entries, favorite matches,
    // `app_state.rtl_tcp_active_server` — keys off the
    // *advertised* `hostname:port`, the same form
    // `favorite_key(server)` produces on mDNS announce. The
    // `host` param threaded into this helper already is that
    // stable value (discovery + favorites both pass the
    // advertised hostname, not a resolved IP), so we build the
    // key from it directly rather than reading it back from
    // `hostname_row.text()` — the row carries the dial target
    // the DSP actually connects to, which could be a resolved
    // IP or an IPv6 literal and would split identity between
    // "favorite shack-pi.local.:1234" and "resolved
    // 192.168.1.17:1234". Cache it on `AppState` so the
    // subsequent auth-flow helpers (`save_current_auth_key_for_
    // active_server`, the keyring-clear on `AuthFailed`, the
    // role-picker's per-favorite update) use this same stable
    // id without re-reading the widget.
    let server_key = format!("{host}:{port}");
    state
        .rtl_tcp_active_server
        .borrow_mut()
        .clone_from(&server_key);
    let favorite_entry = load_favorites(config)
        .into_iter()
        .find(|f| f.key == server_key);
    let favorite_role = favorite_entry
        .as_ref()
        .and_then(|f| f.requested_role)
        .or_else(|| {
            config.read(|v| {
                v.get(KEY_RTL_TCP_CLIENT_LAST_ROLE)
                    .and_then(|rv| serde_json::from_value::<FavoriteRole>(rv.clone()).ok())
            })
        });
    // Always set the role explicitly — never leave the combo
    // showing whatever a prior favorite-restore put there. Pre-
    // `CodeRabbit` round 9 on PR #408 this was `if let Some(
    // fav_role) = favorite_role { ... }`, so a fresh server
    // with no per-favorite role and no global
    // `KEY_RTL_TCP_CLIENT_LAST_ROLE` would silently inherit
    // whatever `Listen` a previous favorite had set — meaning
    // the first connect against a never-seen server could
    // accidentally request Listener instead of the legacy-safe
    // Control default. `unwrap_or(Control)` forces the picker
    // to the right default every time `apply_rtl_tcp_connect`
    // runs.
    let resolved_role = favorite_role.unwrap_or(FavoriteRole::Control);
    let idx = match resolved_role {
        FavoriteRole::Control => RTL_TCP_ROLE_CONTROL_IDX,
        FavoriteRole::Listen => RTL_TCP_ROLE_LISTEN_IDX,
    };
    role_row.set_selected(idx);
    // Auth-row state is driven by two inputs:
    // - `auth_required = Some(true)` on the favorite → the
    //   server advertises a required key, so reveal the row so
    //   the user can enter one (or see a saved one below) BEFORE
    //   the first connect lands — saves the
    //   `AuthRequired` bounce.
    // - A saved key in the per-server keyring → pre-fill the
    //   hex representation so a pre-configured auth connect
    //   succeeds in a single `Connecting → Connected` hop.
    //
    // Pre-CodeRabbit round 2 on PR #408 each of these was a
    // positive-only mutation: on the "no auth / no saved key"
    // path the row kept whatever visibility and text the
    // previous server left behind, so switching from
    // auth-required server A to no-auth server B would leak
    // A's revealed row + pre-filled key bytes into B — the
    // next connect would dispatch `SetRtlTcpClientConfig` with
    // A's key bound to B's endpoint. Now we rewrite both fields
    // deterministically: `set_visible(should_reveal)` and
    // `set_text(saved_hex_or_empty)` fire on every call.
    let has_auth_required = matches!(
        favorite_entry.as_ref().and_then(|f| f.auth_required),
        Some(true)
    );
    let saved_key_bytes = load_client_auth_key_from_keyring(host, port);
    let should_reveal = has_auth_required || saved_key_bytes.is_some();
    auth_key_row.set_visible(should_reveal);
    if let Some(bytes) = saved_key_bytes {
        auth_key_row.set_text(&crate::sidebar::server_panel::auth_key_to_hex(&bytes));
    } else {
        auth_key_row.set_text("");
    }
    // Dispatch a fresh `SetRtlTcpClientConfig` so the DSP
    // thread has the restored role + key in place before the
    // `SetNetworkConfig` + `SetSourceType` below trigger the
    // actual handshake. Without this the DSP would use its
    // last-known values (possibly stale from a prior server)
    // and the first connect could land with the wrong role or
    // a dead auth key from another session.
    // Transient out-of-range ComboRow indices fall back to
    // Control — the legacy-safe default. Collapsed with the
    // explicit Control arm since both produce the same
    // `FavoriteRole::Control`.
}

/// Sweep still-live toasts from a prior `ControllerBusy` entry so the overlay does not stack pairs.
/// Split out per the 50-NLOC gate (#817).
fn sweep_prior_controller_busy_toasts(
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
) {
    // Before creating the new pair, sweep any still-
    // live toasts from a prior `ControllerBusy` entry
    // (e.g. the user hit `Retry` without clicking either
    // action, and the server is still busy on the
    // rebound). Otherwise the overlay would stack two
    // pairs, and dismissing one pair via the cross-
    // dismiss helpers below would leave the other pair
    // orphaned. Per `CodeRabbit` round 11 on PR #408.
    {
        let mut pending = pending_controller_busy_toasts.borrow_mut();
        for weak in pending.drain(..) {
            if let Some(toast) = weak.upgrade() {
                toast.dismiss();
            }
        }
    }
}

/// Startup hydration of hostname/port from the persisted last-connected server (RTL-TCP only).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn restore_last_connected_endpoint(
    state: &Rc<AppState>,
    config_for_discovery: &std::sync::Arc<sdr_config::ConfigManager>,
    hostname_row: &adw::EntryRow,
    port_row: &adw::SpinRow,
    protocol_row: &adw::ComboRow,
) {
    // Populate the hostname / port fields on startup from the last
    // connected server, if any. Runs once before the poller starts
    // so the user sees "the server they were last on" immediately
    // instead of having to wait for a fresh mDNS beacon. No-op on
    // first launch / after a config reset.
    //
    // Protocol row is forced to TCP *before* the hostname / port
    // writes. Those writes fire `connect_changed` / `connect_value_
    // notify` handlers that re-read `protocol_row.selected()` and
    // dispatch `SetNetworkConfig { protocol: ... }`. If the shared
    // protocol row was restored to UDP from a prior raw-Network
    // session, the restore path would otherwise push a UDP
    // `SetNetworkConfig` against the RTL-TCP endpoint on the very
    // first tick. Pinning TCP first keeps the restore both silent
    // to the user and correct end-to-end.
    // Only hydrate the shared host / port / protocol row triple
    // with the last-connected RTL-TCP server when the persisted
    // source type is actually RTL-TCP. If the user was last on
    // raw Network, the values restored by `connect_source_panel`
    // a moment earlier (KEY_SOURCE_NETWORK_*) are the right ones
    // to keep visible — overwriting them with an unrelated
    // RTL-TCP endpoint just because one was once connected would
    // surprise the user on every restart. Per `CodeRabbit` round
    // 2 on PR #558.
    let restored_source_is_rtl_tcp =
        sidebar::source_panel::load_source_device_index(config_for_discovery)
            == sidebar::source_panel::DEVICE_RTLTCP;
    if restored_source_is_rtl_tcp
        && let Some(last) = crate::sidebar::source_panel::load_last_connected(config_for_discovery)
    {
        // Same guarded-rewrite idiom as `apply_rtl_tcp_connect`:
        // hydrating the last-connected RTL-TCP server must not
        // overwrite `KEY_SOURCE_NETWORK_*` (the raw-Network
        // triple). The persistence handlers for those rows
        // observe the flag and skip the disk-write, AND skip
        // the `SetNetworkConfig` dispatch so the three row
        // mutations don't kick three intermediate reconnects
        // against a partially-rewritten triple. Per `CodeRabbit`
        // rounds 1 and 2 on PR #558.
        state.rtl_tcp_hydration_in_progress.set(true);
        protocol_row.set_selected(NETWORK_PROTOCOL_TCPCLIENT_IDX);
        hostname_row.set_text(&last.host);
        port_row.set_value(f64::from(last.port));
        state.rtl_tcp_hydration_in_progress.set(false);
        // Emit the canonical `SetNetworkConfig` for the restored
        // RTL-TCP endpoint *after* the flag clears, mirroring
        // `apply_rtl_tcp_connect`'s own post-hydration dispatch.
        // Without this, the only `SetNetworkConfig` the DSP saw
        // came from `connect_source_panel`'s raw-Network restore
        // a moment earlier — so first Play on a persisted
        // RTL-TCP session would dial the stale raw-Network
        // endpoint until the user nudged a row by hand. Per
        // `CodeRabbit` round 4 on PR #558.
        state.send_dsp(UiToDsp::SetNetworkConfig {
            hostname: last.host.clone(),
            port: last.port,
            protocol: sdr_types::Protocol::TcpClient,
        });
    }

    // Poll the discovery channel from the main thread. Cheap enough
    // to be always-on; discovery events are bursty at start and then
    // idle.
    //
    // Gated on `Some(browser)` so we don't spawn a poller against a
    // dead `disc_rx` when mDNS startup failed. The
    // `DISCOVERY_UNAVAILABLE_SUBTITLE` set in the `Err` branch
    // stays on the expander as the long-term idle state; the
    // restore / favorites paths above already ran unconditionally.
}

/// Startup restore of the `rtl_tcp` client role + auth key (#396).
/// Split out per the 50-NLOC gate (#817).
pub(super) fn restore_rtl_tcp_client_state(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    // Restore the rtl_tcp client's last-used role + auth key
    // (#396). Role resolution uses the standard two-tier
    // lookup: per-favorite `requested_role` first (if the
    // LastConnectedServer matches a favorite entry), falling
    // back to the global `KEY_RTL_TCP_CLIENT_LAST_ROLE` default,
    // and finally to `Control` (legacy-safe). The auth key is
    // loaded directly from the per-server keyring using the
    // LastConnectedServer's `host:port`. Pre-CodeRabbit round 2
    // on PR #408 this path hard-set `auth_key: None` and
    // ignored per-favorite role, so pressing Play right after
    // launch against a previously-auth-configured server would
    // drop the saved key and force a redundant `AuthRequired`
    // bounce before reconnecting. With the keyring preload the
    // DSP carries the right bytes from the first Play.
    {
        use crate::sidebar::source_panel::{
            FavoriteRole, KEY_RTL_TCP_CLIENT_LAST_ROLE, RTL_TCP_ROLE_CONTROL_IDX,
            RTL_TCP_ROLE_LISTEN_IDX, load_favorites, load_last_connected,
        };
        let last_connected = load_last_connected(config);
        let favorite_entry = last_connected.as_ref().and_then(|srv| {
            let key = format!("{}:{}", srv.host, srv.port);
            load_favorites(config).into_iter().find(|f| f.key == key)
        });
        let persisted_role: FavoriteRole = favorite_entry
            .as_ref()
            .and_then(|f| f.requested_role)
            .or_else(|| {
                config.read(|v| {
                    v.get(KEY_RTL_TCP_CLIENT_LAST_ROLE)
                        .and_then(|val| serde_json::from_value(val.clone()).ok())
                })
            })
            .unwrap_or(FavoriteRole::Control);
        let idx = match persisted_role {
            FavoriteRole::Control => RTL_TCP_ROLE_CONTROL_IDX,
            FavoriteRole::Listen => RTL_TCP_ROLE_LISTEN_IDX,
        };
        panels.source.rtl_tcp_role_row.set_selected(idx);
        restore_saved_auth_key(
            panels,
            state,
            last_good_auth_key,
            last_connected.as_ref(),
            favorite_entry.as_ref(),
            persisted_role,
        );
    }
}

/// Auth-key half of the `rtl_tcp` client restore: keyring load, last-good seed, and key-row reveal.
/// Split out per the 50-NLOC gate (#817).
fn restore_saved_auth_key(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
    last_connected: Option<&crate::sidebar::source_panel::LastConnectedServer>,
    favorite_entry: Option<&sidebar::source_panel::FavoriteEntry>,
    persisted_role: crate::sidebar::source_panel::FavoriteRole,
) {
    // Load the saved per-server auth key for the last-
    // connected endpoint, if any. Also cache that server's
    // stable id on `AppState` so the first post-Play
    // `AuthRequired` / `AuthFailed` / `Connected` arm
    // already has it and the keyring save / clear paths
    // target the right entry without waiting on the first
    // `apply_rtl_tcp_connect` call.
    //
    // Auth-row visibility + text is resolved deterministically
    // using the same two-input rule as `apply_rtl_tcp_connect`
    // (per `CodeRabbit` round 5 on PR #408): reveal the row
    // when EITHER the favorite advertises `auth_required ==
    // Some(true)` (server requires a key; user should see the
    // field up-front even on a fresh session with no saved
    // key) OR a saved key exists in the keyring (we want to
    // show the pre-loaded value so the user knows the
    // session will auto-auth). Set text from the saved key,
    // or clear when none — so a prior-session auth-required
    // server whose key the user later cleared doesn't leak
    // stale text into the field on the next launch.
    let mut auth_key: Option<Vec<u8>> = None;
    if let Some(srv) = last_connected {
        *state.rtl_tcp_active_server.borrow_mut() = format!("{}:{}", srv.host, srv.port);
        auth_key = load_client_auth_key_from_keyring(&srv.host, srv.port);
        // Seed the round-9 last-good cache with the
        // startup-restored bytes so a subsequent malformed-
        // hex role flip (round 9's fallback path) preserves
        // the auth DSP just received. Without this the
        // cache would stay `None` until the user first
        // edited the auth field, opening a window where a
        // role flip with malformed text in the row silently
        // clears DSP auth. Per `CodeRabbit` round 10 on
        // PR #408.
        last_good_auth_key.borrow_mut().clone_from(&auth_key);
        let has_auth_required = matches!(favorite_entry.and_then(|f| f.auth_required), Some(true));
        let should_reveal = has_auth_required || auth_key.is_some();
        panels
            .source
            .rtl_tcp_auth_key_row
            .set_visible(should_reveal);
        if let Some(bytes) = auth_key.as_ref() {
            panels
                .source
                .rtl_tcp_auth_key_row
                .set_text(&crate::sidebar::server_panel::auth_key_to_hex(bytes));
        } else {
            panels.source.rtl_tcp_auth_key_row.set_text("");
        }
    }
    state.send_dsp(UiToDsp::SetRtlTcpClientConfig {
        requested_role: persisted_role.as_wire_role(),
        auth_key,
    });
}

/// Two-tier role persistence: global default + per-favorite override.
/// Split out per the 50-NLOC gate (#817).
pub(super) fn persist_role_preference(
    state_role: &Rc<AppState>,
    config_for_role: &std::sync::Arc<sdr_config::ConfigManager>,
    hostname_for_role: &adw::EntryRow,
    port_for_role: &adw::SpinRow,
    favorites_for_role: &FavoritesMap,
    fav_role: crate::sidebar::source_panel::FavoriteRole,
) {
    use crate::sidebar::source_panel::{KEY_RTL_TCP_CLIENT_LAST_ROLE, save_favorites};

    // Tier 1: global default — always written so a fresh
    // server ("never favorited, never configured") picks
    // this up as the picker seed.
    config_for_role.write(|v| {
        v[KEY_RTL_TCP_CLIENT_LAST_ROLE] =
            serde_json::to_value(fav_role).unwrap_or(serde_json::Value::Null);
    });
    // Tier 2: per-favorite override. Resolve the
    // server key from the cached stable identity first
    // (`state.rtl_tcp_active_server`, written by
    // `apply_rtl_tcp_connect` / the startup restore at
    // connect-setup time) and only fall back to reading
    // the `hostname_row` / `port_row` widgets when the
    // cache is empty (manually-typed Play path, no
    // apply_rtl_tcp_connect). Pre-`CodeRabbit` round 10
    // on PR #408 this handler always rebuilt the key
    // from the widgets, so a discovery connect that
    // persisted `shack-pi.local.:1234` as the favorite
    // identity could silently diverge from whatever
    // resolved-IP value the dial path had pushed into
    // `hostname_row` — the lookup below would miss the
    // favorite, and `requested_role` wouldn't round-
    // trip between discovery, favorites, and reconnects.
    //
    // Then update the matching entry's `requested_role`
    // in the SHARED in-memory map
    // (`connect_rtl_tcp_discovery`'s re-announce path
    // also reads + mutates this map), and persist the
    // full snapshot. Pre-round-8 this handler called
    // `load_favorites` on every fire and saved a fresh
    // `Vec`, diverging from the discovery path's in-
    // memory map — a subsequent `ServerAnnounced` would
    // preserve the stale in-memory role and clobber the
    // just-saved selection. Mutating the shared map
    // keeps both paths honest.
    let server_key = {
        let cached = state_role.rtl_tcp_active_server.borrow().clone();
        if cached.is_empty() {
            let host = hostname_for_role.text().to_string();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let port = port_for_role.value() as u16;
            if host.is_empty() || port == 0 {
                return;
            }
            format!("{host}:{port}")
        } else {
            cached
        }
    };
    let dirty = {
        let mut favorites = favorites_for_role.borrow_mut();
        if let Some(fav) = favorites.get_mut(&server_key)
            && fav.requested_role != Some(fav_role)
        {
            fav.requested_role = Some(fav_role);
            true
        } else {
            false
        }
    };
    if dirty {
        let snapshot: Vec<sidebar::source_panel::FavoriteEntry> =
            favorites_for_role.borrow().values().cloned().collect();
        save_favorites(config_for_role, &snapshot);
    }
}
