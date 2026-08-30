//! Network / `rtl_tcp`-client source-panel rows: raw-Network
//! hostname/port/protocol, file path + IQ recording, the Airspy
//! unit selector, and the `rtl_tcp` client role/server-key/restart
//! rows. Split out of `window/source/rows.rs` per the Codacy
//! large-file gate (#846).

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::super::{
    AppState, NETWORK_PROTOCOL_TCPCLIENT_IDX, NETWORK_PROTOCOL_UDP_IDX, Rc, RefCell, SidebarPanels,
    UiToDsp, adw, recording_path, sidebar,
};
use super::super::FavoritesMap;
use super::super::connect::{
    invalidate_rtl_tcp_active_server_on_edit, persist_role_preference, restore_rtl_tcp_client_state,
};

/// Startup restore of the `rtl_tcp` client's last-used role + auth
/// key (#396) — a thin wrapper over `restore_rtl_tcp_client_state`.
/// The role/server-key ROW WIRING itself lives in
/// `wire_role_and_server_key_rows`. Split out per the 50-NLOC gate
/// (#817).
pub(in crate::window::source) fn wire_rtl_tcp_client_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    restore_rtl_tcp_client_state(panels, state, config, last_good_auth_key);
}

/// Airspy unit selector (#848 phase 5): restore-then-wire. The
/// persisted serial is dispatched at startup so the next Play opens
/// the chosen unit even before the combo has real entries; the
/// enumeration answer (`AirspyDeviceList`) later rebuilds the combo
/// and re-selects it. Selection changes persist the serial and
/// dispatch it — taking effect at the next Play, matching how the
/// RTL/Airspy device switch itself behaves. Split out of
/// `window/source/rows.rs` per the Codacy large-file gate (#846).
pub(in crate::window::source) fn wire_airspy_device_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    {
        let persisted = sidebar::source_panel::load_airspy_serial(config);
        if persisted.is_some() {
            state.send_dsp(UiToDsp::SetAirspyDeviceSerial(persisted));
        }
        // Enumerate at startup when the panel restores to Airspy so
        // the combo has real entries without a device-switch nudge.
        let device = sidebar::source_panel::load_source_device_index(config);
        if device == sidebar::source_panel::DEVICE_AIRSPY {
            state.send_dsp(UiToDsp::RefreshAirspyDevices);
        }
    }
    let state_serial = Rc::clone(state);
    let config_serial = std::sync::Arc::clone(config);
    panels
        .source
        .airspy_device_row
        .connect_selected_notify(move |row| {
            // Programmatic rebuilds from the device-list event must
            // not round-trip into config writes — see
            // `AppState::suppress_airspy_unit_notify`.
            if state_serial.suppress_airspy_unit_notify.get() {
                return;
            }
            let idx = row.selected();
            // "First available" vs an enumerated serial parsed back
            // from its label. A transient out-of-range index during
            // model churn parses to None and is discarded.
            let serial = if idx == sidebar::source_panel::AIRSPY_FIRST_AVAILABLE_INDEX {
                None
            } else {
                let Some(label) = row
                    .model()
                    .and_then(|m| m.downcast::<gtk4::StringList>().ok())
                    .and_then(|m| m.string(idx))
                else {
                    return;
                };
                let Some(serial) = sdr_source_airspy::parse_device_serial(&label) else {
                    return;
                };
                Some(serial)
            };
            sidebar::source_panel::save_airspy_serial(&config_serial, serial);
            state_serial.send_dsp(UiToDsp::SetAirspyDeviceSerial(serial));
        });
}

/// IQ correction, PPM, and the stop/start buttons.
/// Split out per the 50-NLOC gate (#817).
pub(in crate::window::source) fn wire_iq_ppm_and_restart_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // IQ correction toggle. Restore-then-wire (#552).
    {
        let persisted = sidebar::source_panel::load_source_iq_correction(config);
        panels.source.iq_correction_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetIqCorrection(persisted));
    }
    let state_iq_corr = Rc::clone(state);
    let config_iq_corr = std::sync::Arc::clone(config);
    panels
        .source
        .iq_correction_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_iq_correction(&config_iq_corr, enabled);
            state_iq_corr.send_dsp(UiToDsp::SetIqCorrection(enabled));
        });

    // PPM correction. Restore persisted value before wiring
    // the notify handler — same idiom as bias-T / gain. Per
    // #551.
    {
        let persisted_ppm = sidebar::source_panel::load_source_rtl_ppm(config);
        panels.source.ppm_row.set_value(f64::from(persisted_ppm));
        state.send_dsp(UiToDsp::SetPpmCorrection(persisted_ppm));
    }
    let state_ppm = Rc::clone(state);
    let config_ppm = std::sync::Arc::clone(config);
    panels.source.ppm_row.connect_value_notify(move |row| {
        #[allow(clippy::cast_possible_truncation)]
        let ppm = row.value() as i32;
        sidebar::source_panel::save_source_rtl_ppm(&config_ppm, ppm);
        state_ppm.send_dsp(UiToDsp::SetPpmCorrection(ppm));
    });

    // rtl_tcp connection controls — Disconnect + Retry now.
    // Both route to the DSP controller which owns the active
    // Source and performs the stop/start teardown. Buttons are
    // sensitive-gated by the state-change handler in
    // `handle_dsp_message`, so clicks should only ever reach here
    // on legal transitions.
    let state_disconnect = Rc::clone(state);
    panels
        .source
        .rtl_tcp_disconnect_button
        .connect_clicked(move |_| {
            state_disconnect.send_dsp(UiToDsp::DisconnectRtlTcp);
        });
    let state_retry = Rc::clone(state);
    panels
        .source
        .rtl_tcp_retry_button
        .connect_clicked(move |_| {
            state_retry.send_dsp(UiToDsp::RetryRtlTcpNow);
        });
}

/// Raw-Network hostname/port/protocol rows (atomic restore, per-edit dispatch).
/// Split out per the 50-NLOC gate (#817).
pub(in crate::window::source) fn wire_network_source_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Raw-Network source config (hostname / port / protocol).
    // Restore all three widgets atomically BEFORE wiring the
    // change-notify handlers, then dispatch one
    // `SetNetworkConfig` with the loaded values so Play picks up
    // the right destination on first launch. Per #552. (rtl_tcp
    // client maintains its own per-server hostname/port via the
    // favorites list — these keys are for the raw IQ-stream
    // Network source only; on a launch where the user was last
    // on rtl_tcp the favorites system also restores its own
    // hostname/port and the two are independent.)
    {
        let hostname = sidebar::source_panel::load_source_network_hostname(config);
        let port = sidebar::source_panel::load_source_network_port(config);
        let protocol_idx = sidebar::source_panel::load_source_network_protocol_index(config);
        panels.source.hostname_row.set_text(&hostname);
        panels.source.port_row.set_value(f64::from(port));
        if protocol_idx == NETWORK_PROTOCOL_UDP_IDX
            || protocol_idx == NETWORK_PROTOCOL_TCPCLIENT_IDX
        {
            panels.source.protocol_row.set_selected(protocol_idx);
        }
        let protocol = if protocol_idx == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        state.send_dsp(UiToDsp::SetNetworkConfig {
            hostname,
            port,
            protocol,
        });
    }

    // Explicit dispatch — NOT a tail-call chain — so the full row
    // set this entry point wires is visible in one place instead of
    // buried across nested calls. Order matches the original chain
    // exactly: hostname, then port/protocol, then protocol alone.
    // Split out per CR round 2 on #846.
    wire_network_hostname_row(panels, state, config);
    wire_network_port_and_protocol(panels, state, config);
    wire_network_protocol_row(panels, state, config);
}

/// Connection-role picker + server-key entry (#394/#396).
/// Split out per the 50-NLOC gate (#817).
pub(in crate::window::source) fn wire_role_and_server_key_rows(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites: &FavoritesMap,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    // Connection-role picker (#396). The selector flips between
    // `Role::Control` (index 0) and `Role::Listen` (index 1); we
    // dispatch a fresh `SetRtlTcpClientConfig` with the new role
    // plus the current auth key (unchanged by a role flip). The
    // role takes effect on the NEXT connect — already-running
    // sessions keep their admitted role because the wire
    // protocol ties role to the hello and doesn't support
    // mid-stream role changes. Persistence has two tiers:
    //
    // - Global `KEY_RTL_TCP_CLIENT_LAST_ROLE` — fallback default
    //   for NEW servers that haven't been favorited yet. The
    //   Connect-from-discovery path reads this to seed the
    //   picker before the user has expressed a per-server
    //   preference. Pre-CodeRabbit round 1 on PR #408 this was
    //   the ONLY persistence tier, which meant changing
    //   Server B's role clobbered Server A's preference.
    // - Per-favorite `FavoriteEntry.requested_role` — wins for
    //   favorited servers. When the current server identity
    //   matches a favorite key, update that entry's role and
    //   save_favorites so the next connect from this favorite
    //   restores the right picker state without touching other
    //   servers.
    let state_role = Rc::clone(state);
    let auth_key_for_role = panels.source.rtl_tcp_auth_key_row.clone();
    let config_for_role = std::sync::Arc::clone(config);
    let hostname_for_role = panels.source.hostname_row.clone();
    let port_for_role = panels.source.port_row.clone();
    let favorites_for_role = Rc::clone(favorites);
    let last_good_for_role = Rc::clone(last_good_auth_key);
    panels
        .source
        .rtl_tcp_role_row
        .connect_selected_notify(move |row| {
            on_rtl_tcp_role_selected(
                row,
                &state_role,
                &auth_key_for_role,
                &config_for_role,
                &hostname_for_role,
                &port_for_role,
                &favorites_for_role,
                &last_good_for_role,
            );
        });

    // Explicit dispatch — NOT a tail-call chain — so both row
    // groups this entry point wires are visible in one place.
    // Order matches the original chain exactly: server-key entry,
    // then file path. Split out per CR round 2 on #846.
    wire_server_key_entry(panels, state, last_good_auth_key);
    wire_file_path_row(panels, state, config);
}

/// Role-picker dispatch (#396): resolve the auth key (empty → None,
/// valid hex → bytes, malformed → last-good cache), push the new role
/// to DSP, then persist the two-tier role preference (global default
/// + per-favorite override). Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn on_rtl_tcp_role_selected(
    row: &adw::ComboRow,
    state_role: &Rc<AppState>,
    auth_key_for_role: &adw::PasswordEntryRow,
    config_for_role: &std::sync::Arc<sdr_config::ConfigManager>,
    hostname_for_role: &adw::EntryRow,
    port_for_role: &adw::SpinRow,
    favorites_for_role: &FavoritesMap,
    last_good_for_role: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    use crate::sidebar::source_panel::{
        FavoriteRole, RTL_TCP_ROLE_CONTROL_IDX, RTL_TCP_ROLE_LISTEN_IDX,
    };
    let fav_role = match row.selected() {
        RTL_TCP_ROLE_CONTROL_IDX => FavoriteRole::Control,
        RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
        _ => return, // transient out-of-range indices
    };
    let requested_role = fav_role.as_wire_role();
    // Resolve the auth_key for this dispatch:
    // - Empty text → `None` (intentional clear).
    // - Valid hex → `Some(bytes)`.
    // - Malformed non-empty text → the cached last-good
    //   bytes (which the auth handler maintains). This
    //   means a role flip with bad hex in the auth field
    //   still pushes the new role to DSP — pre-
    //   `CodeRabbit` round 9 on PR #408 we'd skip the
    //   dispatch entirely, so a user could switch to
    //   Listener, hit Retry / ControllerBusy-toast-
    //   Takeover, and still end up as Controller because
    //   DSP never saw the new role. The auth_key-row
    //   handler still drives the `error` CSS class on
    //   the row so the user sees the malformed input.
    let key_text = auth_key_for_role.text().to_string();
    let auth_key: Option<Vec<u8>> = if key_text.is_empty() {
        None
    } else if let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&key_text) {
        Some(bytes)
    } else {
        last_good_for_role.borrow().clone()
    };
    state_role.send_dsp(UiToDsp::SetRtlTcpClientConfig {
        requested_role,
        auth_key,
    });
    persist_role_preference(
        state_role,
        config_for_role,
        hostname_for_role,
        port_for_role,
        favorites_for_role,
        fav_role,
    );
}

/// Network port + protocol rows.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_port_and_protocol(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Network port
    let state_port = Rc::clone(state);
    let config_port = std::sync::Arc::clone(config);
    let host_for_port = panels.source.hostname_row.clone();
    let proto_for_port = panels.source.protocol_row.clone();
    let port_row_for_port = panels.source.port_row.clone();
    let auth_key_for_port = panels.source.rtl_tcp_auth_key_row.clone();
    panels.source.port_row.connect_value_notify(move |row| {
        // Skip the invalidation during RTL-TCP hydration; see
        // hostname handler above for the rationale. Per
        // `CodeRabbit` round 3 on PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            invalidate_rtl_tcp_active_server_on_edit(
                &state_port,
                &host_for_port,
                &port_row_for_port,
                &auth_key_for_port,
            );
        }
        let hostname = host_for_port.text().to_string();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = row.value() as u16;
        // Skip the raw-Network disk-write during RTL-TCP
        // hydration; see hostname handler above. Per CodeRabbit
        // round 1 on PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            sidebar::source_panel::save_source_network_port(&config_port, port);
        }
        let protocol = if proto_for_port.selected() == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        // Suppress per-edit dispatch during hydration; see
        // hostname handler above. Per `CodeRabbit` round 2 on
        // PR #558.
        if !state_port.rtl_tcp_hydration_in_progress.get() {
            state_port.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        }
    });
}

/// Server key entry (#394/#396): per-edit config rebuild + last-good cache.
/// Split out per the 50-NLOC gate (#817).
fn wire_server_key_entry(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    last_good_auth_key: &Rc<RefCell<Option<Vec<u8>>>>,
) {
    // Server key entry (#394 + #396). On every edit we rebuild
    // the `SetRtlTcpClientConfig` message with the current role
    // + the new key bytes, so the NEXT connect carries the
    // latest value. The entry accepts hex input (matching what
    // `openssl rand -hex 32` produces and what the server UI's
    // Copy button writes to the clipboard); an empty field
    // clears the key (`auth_key: None`). The key is also saved
    // to the per-server keyring on a successful auth-required
    // connect (wired in the toast-flow commit) — this handler
    // only threads the current-session value through to the
    // DSP.
    let state_auth = Rc::clone(state);
    let role_for_auth = panels.source.rtl_tcp_role_row.clone();
    let last_good_for_auth = Rc::clone(last_good_auth_key);
    panels
        .source
        .rtl_tcp_auth_key_row
        .connect_changed(move |row| {
            use crate::sidebar::source_panel::{
                FavoriteRole, RTL_TCP_ROLE_CONTROL_IDX, RTL_TCP_ROLE_LISTEN_IDX,
            };
            // Transient out-of-range indices on `ComboRow` can
            // occur during widget teardown; fall back to the
            // legacy-safe `Control` default in that case (same
            // treatment the role_row handler gives with an
            // `early return`, but auth_key edits happen often
            // enough that swallowing one rare transient is
            // fine).
            #[allow(
                clippy::match_same_arms,
                reason = "explicit catch-all matches the Control default"
            )]
            let fav_role = match role_for_auth.selected() {
                RTL_TCP_ROLE_CONTROL_IDX => FavoriteRole::Control,
                RTL_TCP_ROLE_LISTEN_IDX => FavoriteRole::Listen,
                _ => FavoriteRole::Control,
            };
            let text = row.text().to_string();
            // Malformed hex must NOT collapse to `auth_key: None`.
            // Pre-`CodeRabbit` round 7 on PR #408 a bad paste fell
            // into the `auth_key_from_hex(..) -> None` branch and
            // silently cleared DSP auth state — the next Retry /
            // Play would then dispatch an unauthenticated connect,
            // bounce through `AuthRequired`, and the user had to
            // fix the text before realizing the previous saved key
            // had been clobbered. Three cases now:
            //
            // - Empty text: intentional clear. Drop the error
            //   class, dispatch `auth_key: None`, cache `None`.
            // - Valid hex: parsed bytes. Drop the error class,
            //   dispatch `Some(bytes)`, cache `Some(bytes)`.
            // - Malformed non-empty text: add the libadwaita
            //   `error` CSS class so the row reads as invalid,
            //   and RETURN without dispatching or updating the
            //   cache — keeping DSP's last-good auth state
            //   (and the `last_good_auth_key` cache the role
            //   handler reads from) intact until the user
            //   either fixes the text or clears the field.
            //
            // `auth_key_from_hex` treats empty as `None` too, but
            // we handle the empty branch explicitly above so the
            // malformed case is cleanly separable.
            let auth_key: Option<Vec<u8>> = if text.is_empty() {
                row.remove_css_class("error");
                None
            } else if let Some(bytes) = crate::sidebar::server_panel::auth_key_from_hex(&text) {
                row.remove_css_class("error");
                Some(bytes)
            } else {
                row.add_css_class("error");
                return;
            };
            // Update the last-good cache alongside the dispatch
            // so the role handler's fallback path (malformed
            // hex at role-flip time) has a coherent value to
            // dispatch. See `last_good_auth_key` declaration
            // above. Per `CodeRabbit` round 9 on PR #408.
            last_good_for_auth.borrow_mut().clone_from(&auth_key);
            state_auth.send_dsp(UiToDsp::SetRtlTcpClientConfig {
                requested_role: fav_role.as_wire_role(),
                auth_key,
            });
        });
}

/// Network hostname — per-edit dispatch so Play always has the current value.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_hostname_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Network hostname — send on every edit so Play always has current value
    let state_host = Rc::clone(state);
    let config_host = std::sync::Arc::clone(config);
    let port_for_host = panels.source.port_row.clone();
    let proto_for_host = panels.source.protocol_row.clone();
    let hostname_for_host = panels.source.hostname_row.clone();
    let auth_key_for_host = panels.source.rtl_tcp_auth_key_row.clone();
    panels.source.hostname_row.connect_changed(move |row| {
        // Invalidate the cached `rtl_tcp_active_server` when
        // the widget no longer matches the cached stable id
        // (typically a manual edit; harmless no-op for
        // `apply_rtl_tcp_connect`'s programmatic writes when
        // those match the cache). Per CodeRabbit round 4 on
        // PR #408.
        //
        // Skip the invalidation during RTL-TCP hydration: the
        // startup hydration in `connect_rtl_tcp_discovery`
        // rewrites this row from the last-connected RTL-TCP
        // server (only when the persisted source type is
        // RTL-TCP), and `apply_rtl_tcp_connect` writes the
        // cache *after* the row writes — so an unguarded
        // invalidate would clear the cache the hydration just
        // restored AND blank the auth row before the auth-row
        // handler had a chance to push the saved key. The
        // `apply_rtl_tcp_connect` path handles cache and auth
        // row deterministically itself; we just need to stay
        // out of its way here. Per `CodeRabbit` round 3 on PR
        // #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            invalidate_rtl_tcp_active_server_on_edit(
                &state_host,
                &hostname_for_host,
                &port_for_host,
                &auth_key_for_host,
            );
        }
        let hostname = row.text().to_string();
        // Skip the raw-Network disk-write when this change came
        // from an RTL-TCP hydration. The user's independent
        // raw-Network hostname stays in `KEY_SOURCE_NETWORK_*`
        // and round-trips across restart on its own. Per
        // CodeRabbit round 1 on PR #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            sidebar::source_panel::save_source_network_hostname(&config_host, &hostname);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let port = port_for_host.value() as u16;
        let protocol = if proto_for_host.selected() == NETWORK_PROTOCOL_UDP_IDX {
            sdr_types::Protocol::Udp
        } else {
            sdr_types::Protocol::TcpClient
        };
        // Suppress per-edit `SetNetworkConfig` dispatch while a
        // hydration is rewriting all three rows in sequence. The
        // sequence would otherwise cause three intermediate
        // reconnect attempts (one per row), each against a
        // partially-rewritten triple. `apply_rtl_tcp_connect`
        // dispatches a single canonical `SetNetworkConfig` after
        // clearing the flag, so the final state still reaches
        // the DSP. Per `CodeRabbit` round 2 on PR #558.
        if !state_host.rtl_tcp_hydration_in_progress.get() {
            state_host.send_dsp(UiToDsp::SetNetworkConfig {
                hostname,
                port,
                protocol,
            });
        }
    });
}

/// Network protocol selector.
/// Split out per the 50-NLOC gate (#817).
fn wire_network_protocol_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // Network protocol
    let state_proto = Rc::clone(state);
    let config_proto = std::sync::Arc::clone(config);
    let host_for_proto = panels.source.hostname_row.clone();
    let port_for_proto = panels.source.port_row.clone();
    panels
        .source
        .protocol_row
        .connect_selected_notify(move |row| {
            let hostname = host_for_proto.text().to_string();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let port = port_for_proto.value() as u16;
            let selected = row.selected();
            // Validate the selected index BEFORE persisting so a
            // transient out-of-range value during widget churn
            // can't land in config (matches the sample-rate /
            // device / decimation handlers' early-return pattern).
            // Per `CodeRabbit` round 3 on PR #558.
            let protocol = match selected {
                NETWORK_PROTOCOL_TCPCLIENT_IDX => sdr_types::Protocol::TcpClient,
                NETWORK_PROTOCOL_UDP_IDX => sdr_types::Protocol::Udp,
                _ => return, // ignore transient indices
            };
            // Skip the raw-Network disk-write during RTL-TCP
            // hydration; see hostname handler above. Per
            // `CodeRabbit` round 1 on PR #558.
            if !state_proto.rtl_tcp_hydration_in_progress.get() {
                sidebar::source_panel::save_source_network_protocol_index(&config_proto, selected);
            }
            // Suppress per-edit dispatch during hydration; see
            // hostname handler above. Per `CodeRabbit` round 2 on
            // PR #558.
            if !state_proto.rtl_tcp_hydration_in_progress.get() {
                state_proto.send_dsp(UiToDsp::SetNetworkConfig {
                    hostname,
                    port,
                    protocol,
                });
            }
        });
}

/// File path — per-edit dispatch.
/// Split out per the 50-NLOC gate (#817).
fn wire_file_path_row(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
    // File path — send on every edit so Play always has current
    // value. Restore-then-wire (#552). Empty saved string is the
    // default and means "no file selected" — re-set the widget
    // to empty too so the placeholder stays correct.
    {
        let persisted = sidebar::source_panel::load_source_file_path(config);
        panels.source.file_path_row.set_text(&persisted);
        state.send_dsp(UiToDsp::SetFilePath(std::path::PathBuf::from(&persisted)));
    }
    let state_file = Rc::clone(state);
    let config_file = std::sync::Arc::clone(config);
    panels.source.file_path_row.connect_changed(move |row| {
        let text = row.text().to_string();
        sidebar::source_panel::save_source_file_path(&config_file, &text);
        state_file.send_dsp(UiToDsp::SetFilePath(std::path::PathBuf::from(text)));
    });

    // IQ recording toggle
    let state_iq_rec = Rc::clone(state);
    panels
        .source
        .record_iq_row
        .connect_active_notify(move |row| {
            if row.is_active() {
                let path = recording_path("iq");
                tracing::info!(?path, "starting IQ recording");
                state_iq_rec.send_dsp(UiToDsp::StartIqRecording(path));
            } else {
                tracing::info!("stopping IQ recording");
                state_iq_rec.send_dsp(UiToDsp::StopIqRecording);
            }
        });
}
