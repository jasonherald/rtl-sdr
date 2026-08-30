//! `rtl_tcp` client-connect wiring: the shared connect sequence
//! ([`apply_rtl_tcp_connect`] / [`dispatch_rtl_tcp_connect`]) and
//! saved-server / saved-role / saved-auth-key startup restore.
//! Split out of `window/source.rs` per the Codacy large-file gate
//! (#846). Connection-state toast wiring lives in
//! `connect/toasts.rs`; client auth-key keyring plumbing lives in
//! `connect/keyring.rs`.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{
    AppState, DEVICE_RTLTCP, NETWORK_PROTOCOL_TCPCLIENT_IDX, Rc, RefCell, SidebarPanels,
    SourceType, UiToDsp, adw, sidebar,
};
use super::FavoritesMap;

mod keyring;
mod toasts;

use keyring::load_client_auth_key_from_keyring;
pub(super) use toasts::invalidate_rtl_tcp_active_server_on_edit;
pub(in crate::window) use toasts::{apply_rtl_tcp_connection_state, handle_rtl_tcp_state_toast};

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

    // Transient out-of-range ComboRow indices fall back to
    // Control — the legacy-safe default. Collapsed with the
    // explicit Control arm since both produce the same
    // `FavoriteRole::Control`.
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
    // Dispatch a fresh `SetRtlTcpClientConfig` so the DSP
    // thread has the restored role + key in place before the
    // `SetNetworkConfig` + `SetSourceType` below trigger the
    // actual handshake. Without this the DSP would use its
    // last-known values (possibly stale from a prior server)
    // and the first connect could land with the wrong role or
    // a dead auth key from another session.
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
