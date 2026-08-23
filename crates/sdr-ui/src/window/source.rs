//! Source panel wiring: device controls, `rtl_tcp` client discovery,
//! favorites, and client-side auth-key keyring plumbing.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{
    AppState, Browser, DECIMATION_FACTORS, DEVICE_FILE, DEVICE_NETWORK, DEVICE_RTLSDR,
    DEVICE_RTLTCP, DiscoveredServer, DiscoveryEvent, Duration, FavoritesHeaderHandle,
    NETWORK_PROTOCOL_TCPCLIENT_IDX, NETWORK_PROTOCOL_UDP_IDX, Rc, RefCell, SAMPLE_RATES,
    SidebarPanels, SourceType, TOAST_TIMEOUT_PERSISTENT, TOAST_TIMEOUT_SHORT_SECS, UiToDsp, adw,
    glib, mpsc, plain_toast, recording_path, sidebar,
};

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
pub(super) fn handle_rtl_tcp_state_toast(
    state_val: &sdr_types::RtlTcpConnectionState,
    prev_disc: u8,
    app_state: &Rc<AppState>,
    toast_overlay_weak: &glib::WeakRef<adw::ToastOverlay>,
    role_row_weak: &glib::WeakRef<adw::ComboRow>,
    auth_key_row_weak: &glib::WeakRef<adw::PasswordEntryRow>,
    hostname_row_weak: &glib::WeakRef<adw::EntryRow>,
    port_row_weak: &glib::WeakRef<adw::SpinRow>,
    pending_controller_busy_toasts: &Rc<RefCell<Vec<glib::WeakRef<adw::Toast>>>>,
) {
    use sdr_types::RtlTcpConnectionState;

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

/// Sweep still-live ControllerBusy toasts on any transition that
/// isn't re-entering ControllerBusy (CR round 11 on PR #408). Split
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

pub(super) fn apply_rtl_tcp_connection_state(
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

/// Widget/state dependencies the discovery poller needs to build a
/// discovered-server row. Bundled so the `ServerAnnounced` arm can
/// live in its own function (#817).
struct DiscoveredRowDeps {
    hostname_row: adw::EntryRow,
    port_row: adw::SpinRow,
    protocol_row: adw::ComboRow,
    device_row: adw::ComboRow,
    role_row: adw::ComboRow,
    auth_key_row: adw::PasswordEntryRow,
    state: Rc<AppState>,
    config: std::sync::Arc<sdr_config::ConfigManager>,
    favorite_row_ctx: Rc<FavoriteRowContext>,
    discovered_star_buttons:
        Rc<RefCell<std::collections::HashMap<String, glib::WeakRef<gtk4::ToggleButton>>>>,
    expander_weak: glib::WeakRef<adw::ExpanderRow>,
}

/// Grace period before a discovered-server row is pruned when the
/// responder stops re-announcing (crashed or partitioned peers never
/// send `ServerWithdrawn`).
const STALE_ROW_GRACE: std::time::Duration = std::time::Duration::from_mins(3);

/// Connect source panel controls to DSP commands.
#[allow(clippy::too_many_lines)]
/// Spawn an mDNS browser for `_rtl_tcp._tcp.local.` services and wire
/// its events into the `rtl_tcp_discovered_row` expander. Each
/// discovered server gets an `AdwActionRow` with a Connect button that
/// populates hostname/port and switches the source type.
///
/// The `Browser` handle is moved into the `timeout_add_local` closure
/// so it lives for the lifetime of the main context (= the app), and
/// mDNS discovery runs continuously whether or not the RTL-TCP source
/// is currently selected. That's fine — discovery is cheap and having
/// the list pre-populated when the user switches to RTL-TCP makes the
/// UX immediate instead of "wait 5 s for the first advertisement."
pub(super) fn connect_rtl_tcp_discovery(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites_header: &FavoritesHeaderHandle,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
) {
    use std::collections::HashMap;

    /// Grace window after which a server that has stopped
    /// re-announcing gets pruned from the UI list. A healthy mDNS
    /// responder re-announces well before its TTL (default 120 s on
    /// most daemons) expires; 3 minutes without a refresh means the
    /// responder is either dead or network-partitioned.
    ///
    /// Defense-in-depth: mdns-sd's daemon SHOULD fire
    /// `ServiceRemoved` on TTL expiry, but a crashed server that
    /// vanishes without a goodbye may leave the cache entry around
    /// longer than the client wants. Expiring client-side keeps the
    /// Connect button from offering a dead endpoint.

    /// Poll cadence for the mDNS discovery event channel. 200 ms is
    /// fast enough that newly-announced servers appear "instantly" to
    /// the user and cheap enough to be always-on even when RTL-TCP is
    /// not the selected source type.
    const DISCOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

    /// Subtitle shown on the discovered-servers expander when mDNS
    /// discovery is non-functional (either `Browser::start` failed or
    /// the browser thread exited at runtime). Distinguishes "nothing
    /// to see yet" from "we gave up listening" — without this the UI
    /// would lie by showing the idle "No servers discovered…" state.
    const DISCOVERY_UNAVAILABLE_SUBTITLE: &str = "Discovery unavailable on this system.";

    // "Manage favorites…" button inside the discovered-servers
    // expander — a second entry point into the same popover as
    // the header-bar star button. Wired here because the
    // `MenuButton` whose `popup()` we trigger lives in the
    // header. Weak ref on the button keeps the closure drop-safe
    // if the header is torn down before the source panel (though
    // in practice the window owns both and they drop together).
    let favorites_menu_weak = favorites_header.button.downgrade();
    panels
        .source
        .manage_favorites_button
        .connect_clicked(move |_| {
            if let Some(btn) = favorites_menu_weak.upgrade() {
                // `MenuButton::popup` activates the attached
                // popover anchored to the menu button itself, so
                // the slide-out appears from the header regardless
                // of which entry point the user clicked.
                btn.popup();
            }
        });

    let (disc_tx, disc_rx) = mpsc::channel::<DiscoveryEvent>();
    // `Option<Browser>` — `None` on mDNS startup failure. We still
    // need the rest of this function to run so the *manually*-
    // persisted `last_connected` / favorites restore can repopulate
    // the client UI. Only the discovery poller is skipped in the
    // `None` branch (there'd be nothing to poll, and `disc_tx` is
    // already dropped so `disc_rx` would immediately return
    // `TryRecvError::Disconnected` and spin forever).
    let browser = match Browser::start(move |event| {
        // Ignore send errors — means the UI thread dropped the rx,
        // which only happens on shutdown.
        let _ = disc_tx.send(event);
    }) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(%e, "mDNS browser failed to start — discovery disabled");
            panels
                .source
                .rtl_tcp_discovered_row
                .set_subtitle(DISCOVERY_UNAVAILABLE_SUBTITLE);
            None
        }
    };

    // Tracks the `AdwActionRow` per-server so we can remove it on
    // `ServerWithdrawn` OR when the row goes stale past
    // `STALE_ROW_GRACE`. Keyed by full DNS-SD instance name (stable
    // across nickname changes). Value carries the row widget + the
    // last `DiscoveredServer` payload seen for that instance —
    // `server.last_seen` drives both staleness pruning and the
    // per-tick freshness indicator rendered in the row subtitle.
    let displayed_rows: Rc<RefCell<HashMap<String, (adw::ActionRow, DiscoveredServer)>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Auxiliary map: favorite_key (hostname:port) → weak ref on
    // the currently-rendered discovery-row star `ToggleButton`.
    // Let the favorites-popover Unstar handler find and flip the
    // matching discovery toggle immediately rather than waiting
    // for the next mDNS re-announce — without this, the filled
    // star would stay rendered while the map says otherwise, and
    // the first user click on the stale star would fire
    // `toggled` with `active=false` (wasted click from the
    // user's perspective: they wanted to re-pin).
    //
    // Weak refs only — the `ToggleButton`s are strongly owned by
    // their parent `AdwActionRow`s (as prefix widgets) which are
    // strongly owned by `displayed_rows`. Stale entries
    // (rows that have since been removed from `displayed_rows`)
    // fail to upgrade and self-clean at lookup time; no explicit
    // prune necessary at the <50-server scale this map is sized
    // for.
    let discovered_star_buttons: Rc<RefCell<HashMap<String, glib::WeakRef<gtk4::ToggleButton>>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Weak ref on the expander so the timeout closure doesn't keep
    // the window alive after close — upgrade() returns None on a
    // destroyed widget and the poller breaks out.
    let expander_weak = panels.source.rtl_tcp_discovered_row.downgrade();
    let hostname_row = panels.source.hostname_row.clone();
    let port_row = panels.source.port_row.clone();
    let protocol_row = panels.source.protocol_row.clone();
    let device_row = panels.source.device_row.clone();
    let role_row = panels.source.rtl_tcp_role_row.clone();
    let auth_key_row = panels.source.rtl_tcp_auth_key_row.clone();
    let state = Rc::clone(state);
    // Shared config handle — the Connect button on each discovered
    // row clones it once more inside the closure so it can persist
    // a `LastConnectedServer` snapshot on click.
    let config_for_discovery = std::sync::Arc::clone(config);

    // Favorites map — key (stable hostname:port) → rich
    // `FavoriteEntry` record. Created by the parent
    // `connect_sidebar_panels` so the role-picker handler in
    // `connect_source_panel` can mutate the SAME map this
    // function's re-announce path reads. Per CodeRabbit round 8
    // on PR #408: pre-fix the role-picker reloaded favorites
    // from disk, mutated a local `Vec`, and saved — a
    // later `ServerAnnounced` would preserve the stale
    // in-memory role from this map and clobber the just-saved
    // selection on next disk flush. Sharing keeps both paths
    // honest. The clone we hold here is a cheap `Rc::clone`; the
    // parent retains the original so the Arc-count stays > 0
    // for the lifetime of both handlers.
    let favorites = Rc::clone(favorites);

    // Weak refs to the favorites popover's contents. The star-
    // toggle closure (attached to each row's `ToggleButton`) and
    // the discovery poll timer both need to refresh the popover
    // when the favorites map mutates. Strong captures would create
    // the same closure-cycle pattern the #329 / #335 lessons
    // taught us to avoid — per-callback atomic upgrade + drop
    // keeps the popover widgets releasable on window close.
    let favorites_popover_weak = FavoritesPopoverWeak::from_header(favorites_header);
    // Bundle of per-row action dependencies. Built once, cloned
    // into the three rebuild call sites (startup seed, star
    // toggle, re-announce refresh). `rebuild_favorites_popover`
    // hands a clone to each row's Connect / Copy / Unstar
    // closure, so each button ends up with a single `Rc` clone
    // instead of nine weak-ref captures.
    let favorite_row_ctx: Rc<FavoriteRowContext> = Rc::new(FavoriteRowContext {
        popover: favorites_popover_weak.clone(),
        favorites: Rc::clone(&favorites),
        config: std::sync::Arc::clone(&config_for_discovery),
        state: Rc::clone(&state),
        hostname_row: hostname_row.downgrade(),
        port_row: port_row.downgrade(),
        protocol_row: protocol_row.downgrade(),
        device_row: device_row.downgrade(),
        role_row: role_row.downgrade(),
        auth_key_row: auth_key_row.downgrade(),
        expander_weak: expander_weak.clone(),
        // Weak refs — see `FavoriteRowContext.displayed_rows`
        // docstring for the retain-cycle reasoning.
        displayed_rows: Rc::downgrade(&displayed_rows),
        discovered_star_buttons: Rc::downgrade(&discovered_star_buttons),
    });
    // Seed the popover's content from the restored favorites so
    // the list is ready when the user first clicks the header
    // star, without waiting for a mutation to trigger a rebuild.
    rebuild_favorites_popover(&favorite_row_ctx, &favorites.borrow());

    // Rebuild on every popover show so the "seen Xm ago" subtitles
    // reflect current wall-clock time. Without this, the ages
    // captured by `format_favorite_subtitle` at startup / star
    // toggle / re-announce freeze between popover openings — a
    // user who closes the popover and reopens it 10 minutes later
    // would still see "seen just now" for servers that actually
    // went offline during that gap.
    //
    // `favorite_row_ctx.popover.popover` is the same weak ref the
    // per-row Connect closure uses to dismiss the popover, so no
    // new capture shape is introduced. The closure holds
    // `Rc<FavoriteRowContext>`; no retain cycle because
    // `FavoriteRowContext.popover` is weak.
    {
        let ctx_for_show = Rc::clone(&favorite_row_ctx);
        favorites_header.popover.connect_show(move |_| {
            rebuild_favorites_popover(&ctx_for_show, &ctx_for_show.favorites.borrow());
        });
    }

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
        sidebar::source_panel::load_source_device_index(&config_for_discovery)
            == sidebar::source_panel::DEVICE_RTLTCP;
    if restored_source_is_rtl_tcp
        && let Some(last) = crate::sidebar::source_panel::load_last_connected(&config_for_discovery)
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
    let row_deps = Rc::new(DiscoveredRowDeps {
        hostname_row: hostname_row.clone(),
        port_row: port_row.clone(),
        protocol_row: protocol_row.clone(),
        device_row: device_row.clone(),
        role_row: role_row.clone(),
        auth_key_row: auth_key_row.clone(),
        state: Rc::clone(&state),
        config: config_for_discovery.clone(),
        favorite_row_ctx: Rc::clone(&favorite_row_ctx),
        discovered_star_buttons: Rc::clone(&discovered_star_buttons),
        expander_weak: expander_weak.clone(),
    });
    let Some(browser) = browser else {
        return;
    };
    let _ = glib::timeout_add_local(DISCOVERY_POLL_INTERVAL, move || {
        // Keep the Browser alive as long as the timeout closure is
        // attached.
        let _keep_browser = &browser;
        // If the window / expander has been destroyed, stop polling
        // and let the browser + closure captures drop. Prevents leaked
        // pollers after a hypothetical close-and-reopen of the main
        // window.
        let Some(expander) = expander_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // Prune stale rows before processing incoming events. A
        // responder that crashed or network-partitioned won't send
        // ServerWithdrawn, so without this pass the Connect button
        // for a dead server keeps showing until mDNS cache TTL fires
        // (if it fires at all). 3-minute grace is long enough that
        // a healthy responder's re-announce keeps its row alive.
        prune_stale_discovery_rows(&displayed_rows, &expander);

        loop {
            let event = match disc_rx.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Browser thread exited — `disc_tx` dropped. Stop
                    // polling and surface the degraded state; without
                    // the Break this timeout would spin forever and
                    // the UI would keep claiming "No servers
                    // discovered yet" when we've in fact given up.
                    tracing::warn!(
                        "mDNS discovery channel disconnected — stopping discovery poller"
                    );
                    // Drain any previously announced rows before we
                    // break out. Without this, they'd linger in the
                    // expander indefinitely — no more
                    // `ServerWithdrawn` events will arrive, and the
                    // stale-age pruner at the top of the tick is
                    // also about to stop firing. Users would see
                    // rows that look Connect-able for endpoints
                    // the UI has already declared unavailable.
                    let mut rows = displayed_rows.borrow_mut();
                    for (_, (row, _)) in rows.drain() {
                        expander.remove(&row);
                    }
                    drop(rows);
                    expander.set_subtitle(DISCOVERY_UNAVAILABLE_SUBTITLE);
                    return glib::ControlFlow::Break;
                }
            };
            match event {
                DiscoveryEvent::ServerAnnounced(server) => {
                    on_server_announced(server, &displayed_rows, &expander, &favorites, &row_deps);
                }
                DiscoveryEvent::ServerWithdrawn { instance_name } => {
                    let mut rows = displayed_rows.borrow_mut();
                    if let Some((row, _)) = rows.remove(&instance_name) {
                        expander.remove(&row);
                    }
                    if rows.is_empty() {
                        expander.set_subtitle("No servers discovered on the local network yet.");
                    } else {
                        expander.set_subtitle(&format!("{} server(s) visible", rows.len()));
                    }
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

/// `ServerAnnounced` arm of the discovery poller: build or refresh the
/// row for this instance, wire its Connect/copy/star actions, and keep
/// favorites-first ordering. Split out per the 50-NLOC gate (#817).
fn on_server_announced(
    server: DiscoveredServer,
    displayed_rows: &Rc<
        RefCell<std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>>,
    >,
    expander: &adw::ExpanderRow,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
    deps: &Rc<DiscoveredRowDeps>,
) {
    use std::time::Instant;

    let mut rows = displayed_rows.borrow_mut();
    let title = if server.txt.nickname.is_empty() {
        server.instance_name.clone()
    } else {
        server.txt.nickname.clone()
    };
    // Identity host — the advertised mDNS
    // hostname, matching `favorite_key(&server)`.
    // `apply_rtl_tcp_connect` uses its `host`
    // argument as the stable id for
    // `rtl_tcp_active_server`, keyring lookups,
    // favorite matches, and
    // `LastConnectedServer`. Pre-`CodeRabbit`
    // round 6 on PR #408 this preferred
    // `server.addresses.first()` (a resolved
    // IPv4/IPv6 literal when mDNS had resolved
    // one), which split per-server state
    // between `shack-pi.local.:1234` (what
    // favorites store) and `192.168.1.17:1234`
    // (what the discovery connect path
    // persisted) — role / auth round-tripping
    // through discovery + favorites + startup
    // restore broke silently. The DSP's actual
    // dial path (`RtlTcpSource::with_config` →
    // `(host, port).to_socket_addrs()`) resolves
    // the hostname at connect time, so keeping
    // identity on the advertised name is
    // strictly better: stable across IP
    // changes AND correct by the
    // favorite-key contract.
    let host = server.hostname.clone();
    // Age is effectively 0 here — `server.last_seen` was
    // stamped by the browser thread a few ms ago —
    // `format_age` will render "just now". Subsequent
    // poll ticks refresh this with the actual age.
    let elapsed = Instant::now().saturating_duration_since(server.last_seen);
    let subtitle = format_discovery_subtitle(&server, elapsed);

    // Re-announce for a known instance_name: remove the
    // old row and fall through to build a fresh one.
    // Rebuilding captures the current (host, port) in
    // the new Connect closure; otherwise the stale
    // values from first-announce would stick. See the
    // displayed_rows docstring above.
    if let Some((existing_row, _)) = rows.remove(&server.instance_name) {
        expander.remove(&existing_row);
    }

    let row = adw::ActionRow::builder()
        .title(&title)
        .subtitle(&subtitle)
        .build();

    // Star toggle — prefix icon, pinning this
    // server to the top of the discovered list and
    // persisting the choice across app launches.
    // Using the outlined / filled star icon pair
    // so the toggle state reads clearly without
    // extra CSS.
    build_discovery_star_button(&row, &server, favorites, deps);

    wire_discovered_connect_button(&row, &server, &host, deps);
    expander.add_row(&row);

    refresh_favorite_metadata(&server, favorites, deps);

    rows.insert(server.instance_name.clone(), (row, server));
    // Reorder after insert so favorites float to
    // the top of the new view.
    reorder_discovered_rows(&expander, &rows, &favorites.borrow());

    expander.set_subtitle(&format!("{} server(s) visible", rows.len()));
}

/// Toggle body of a discovered-server star button: flip the icon +
/// accessible name, insert/remove + persist the favorite, and refresh
/// row order + the header popover. Split out per the 50-NLOC gate
/// (#817).
#[allow(clippy::too_many_arguments)]
fn on_discovery_star_toggled(
    btn: &gtk4::ToggleButton,
    star_key: &str,
    star_nickname: &str,
    star_tuner_name: &Option<String>,
    star_gain_count: Option<u32>,
    star_auth_required: Option<bool>,
    star_favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
    star_config: &std::sync::Arc<sdr_config::ConfigManager>,
    star_expander_weak: &glib::WeakRef<adw::ExpanderRow>,
    star_row_ctx: &Rc<FavoriteRowContext>,
) {
    let active = btn.is_active();
    btn.set_icon_name(if active {
        FAVORITE_ICON_FILLED
    } else {
        FAVORITE_ICON_OUTLINE
    });
    // Keep the accessible name in sync with
    // the new state so AT announces the next
    // action ("Unpin from favorites" after the
    // user just pinned it, and vice versa).
    set_favorite_toggle_accessible_name(btn, active);
    {
        let mut favs = star_favorites.borrow_mut();
        if active {
            // Build a fresh entry with the
            // current metadata. Replaces any
            // older entry with the same key
            // (= metadata refresh on re-star).
            favs.insert(
                star_key.to_string(),
                sidebar::source_panel::FavoriteEntry {
                    key: star_key.to_string(),
                    nickname: star_nickname.to_string(),
                    tuner_name: star_tuner_name.clone(),
                    gain_count: star_gain_count,
                    last_seen_unix: Some(sidebar::source_panel::now_unix_seconds()),
                    // Fresh star — no role preference
                    // yet; `auth_required` is captured
                    // from the current mDNS announce's
                    // TXT record above so
                    // `apply_rtl_tcp_connect` + the
                    // startup restore can pre-reveal
                    // the key row immediately, without
                    // waiting on a mDNS re-announce.
                    // Per `CodeRabbit` round 6 on
                    // PR #408 and issue #396.
                    requested_role: None,
                    auth_required: star_auth_required,
                },
            );
        } else {
            favs.remove(star_key);
        }
        // Persist immediately. Order within
        // the persisted list is unspecified —
        // the slide-out sorts on read.
        let snapshot: Vec<sidebar::source_panel::FavoriteEntry> = favs.values().cloned().collect();
        crate::sidebar::source_panel::save_favorites(&star_config, &snapshot);
    }
    // Rebuild the expander so the row moves
    // to/from the top per the new favorite
    // state. Reuses the `displayed_rows` map
    // (strong refs on the AdwActionRow
    // widgets) — ordering is the only thing
    // that changes. The map is held Weak via
    // `FavoriteRowContext`; upgrade fails
    // silently if the discovery timer has
    // already torn down, which means there's
    // nothing to reorder anyway.
    if let (Some(expander), Some(rows)) = (
        star_expander_weak.upgrade(),
        star_row_ctx.displayed_rows.upgrade(),
    ) {
        reorder_discovered_rows(&expander, &rows.borrow(), &star_favorites.borrow());
    }
    // Refresh the header-bar favorites popover
    // so the star-toggle reflects there too.
    // Upgrade-and-drop inside the rebuild keeps
    // the closure leak-free per the #329
    // weak-ref pattern.
    rebuild_favorites_popover(&star_row_ctx, &star_favorites.borrow());
}

/// Per-tick stale-row prune + "seen N ago" subtitle refresh for the
/// discovery expander (3-minute grace; healthy responders re-announce
/// well within it). Split out per the 50-NLOC gate (#817).
fn prune_stale_discovery_rows(
    displayed_rows: &Rc<
        RefCell<std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>>,
    >,
    expander: &adw::ExpanderRow,
) {
    use std::time::Instant;

    let mut rows = displayed_rows.borrow_mut();
    let now = Instant::now();
    let stale_names: Vec<String> = rows
        .iter()
        .filter(|(_, (_, server))| {
            now.saturating_duration_since(server.last_seen) > STALE_ROW_GRACE
        })
        .map(|(name, _)| name.clone())
        .collect();
    for name in stale_names {
        if let Some((row, _)) = rows.remove(&name) {
            tracing::debug!(instance = %name, "pruning stale rtl_tcp discovery row");
            expander.remove(&row);
        }
    }
    // Refresh each surviving row's subtitle with a fresh
    // "seen N ago" stamp. Without this per-tick refresh the
    // age text would freeze at whatever it said when the row
    // was built (or last re-announced) and silently mislead
    // the user about how recent a server is. GTK short-
    // circuits the set_subtitle call when the string is
    // unchanged, so this is nearly free on quiescent rows.
    for (row, server) in rows.values() {
        let elapsed = now.saturating_duration_since(server.last_seen);
        row.set_subtitle(&format_discovery_subtitle(server, elapsed));
    }
    if rows.is_empty() {
        expander.set_subtitle("No servers discovered on the local network yet.");
    } else {
        expander.set_subtitle(&format!("{} server(s) visible", rows.len()));
    }
}

/// Icon name for the un-filled ("not pinned") star on discovery
/// rows. GNOME Symbolic icon set — `non-starred-symbolic` renders
/// the outline glyph, which is visually distinct from the filled
/// pinned state so the affordance reads clearly without relying
/// on the `ToggleButton::is_active` styling alone.
pub(super) const FAVORITE_ICON_OUTLINE: &str = "non-starred-symbolic";

/// Icon name for the filled ("pinned") star. Paired with
/// `FAVORITE_ICON_OUTLINE` so toggling swaps the glyph, not just
/// the button chrome.
pub(super) const FAVORITE_ICON_FILLED: &str = "starred-symbolic";

/// Stable persistence key for a discovered server's favorite
/// state. We key by **advertised hostname + port**, not by the
/// DNS-SD `instance_name`, because `instance_name` is derived
/// from the user-editable TXT nickname — renaming the server
/// would silently drop the saved favorite on the next announce.
/// Hostname is the machine's mDNS identity (e.g. `shack-pi.local.`)
/// which stays put across nickname changes; paired with port it's
/// unique enough that two servers on the same host (different
/// ports) remain distinct favorites. A full machine rename breaks
/// the favorite — acceptable, since a rename semantically IS a
/// different host.
pub(super) fn favorite_key(server: &DiscoveredServer) -> String {
    format!("{}:{}", server.hostname, server.port)
}

/// Order favorites for popover display: primary key lowercased
/// nickname (alphabetical, case-insensitive), secondary key the
/// stable `FavoriteEntry.key` (hostname:port).
///
/// The secondary key is load-bearing — `HashMap::values()`
/// iteration order is non-deterministic, and two favorites with
/// the same nickname would otherwise reshuffle across inserts /
/// removals / app restarts (tie-broken by whatever the hash
/// state happened to be that tick). Tying to `key` pins the
/// order across all three.
pub(super) fn sort_favorites_for_display(entries: &mut [&sidebar::source_panel::FavoriteEntry]) {
    entries.sort_by(|a, b| {
        a.nickname
            .to_lowercase()
            .cmp(&b.nickname.to_lowercase())
            .then_with(|| a.key.cmp(&b.key))
    });
}

/// Update the `GtkAccessible` `Label` on the discovery-row star
/// toggle. The label describes the action the next click will
/// take (NOT the icon's current appearance), so a screen reader
/// announces "Unpin from favorites" when the row is currently
/// pinned and "Pin as favorite" when it isn't. Called once at
/// row-build time and again inside the toggled closure so the
/// name stays in sync with state.
pub(super) fn set_favorite_toggle_accessible_name(btn: &gtk4::ToggleButton, is_favorite: bool) {
    let label = if is_favorite {
        "Unpin from favorites"
    } else {
        "Pin as favorite"
    };
    btn.update_property(&[gtk4::accessible::Property::Label(label)]);
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
#[allow(
    clippy::too_many_arguments,
    reason = "each arg is a distinct widget / state handle the caller owns in its own shape (strong Rc clone vs weak-upgraded strong). Bundling into a struct would duplicate FavoriteRowContext for the favorites caller and invent a mirror struct for the discovery caller, trading argument count for two near-identical shim types."
)]
pub(super) fn apply_rtl_tcp_connect(
    host: &str,
    port: u16,
    nickname: &str,
    hostname_row: &adw::EntryRow,
    port_row: &adw::SpinRow,
    protocol_row: &adw::ComboRow,
    device_row: &adw::ComboRow,
    role_row: &adw::ComboRow,
    auth_key_row: &adw::PasswordEntryRow,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
) {
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

/// Re-add rows to an `AdwExpanderRow` in a deterministic order:
/// favorites (alphabetical by instance name) first, then
/// non-favorites (same alpha order). Called after any mutation
/// that could change the sort — new announce, favorite toggle —
/// so the user's pinned entries stay glued to the top. GTK4 gives
/// us no in-place reorder API for expander children, so we
/// remove-and-re-add. At the expected scale (<50 servers on any
/// realistic LAN) the reparenting is invisible.
pub(super) fn reorder_discovered_rows(
    expander: &adw::ExpanderRow,
    rows: &std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>,
    favorites: &std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>,
) {
    // Remove every row from the expander — widgets live in the
    // HashMap, so no drop happens.
    for (row, _) in rows.values() {
        expander.remove(row);
    }
    // Sort keys: favorites first, then alpha. Favorite check goes
    // through `favorite_key(server)` (hostname+port) so it matches
    // what the star-toggle persists. Alpha tiebreak uses the
    // `instance_name` (HashMap key) so rendering order stays
    // predictable across re-announces.
    let mut keys: Vec<&String> = rows.keys().collect();
    keys.sort_by(|a, b| {
        let a_fav = rows
            .get(a.as_str())
            .is_some_and(|(_, srv)| favorites.contains_key(&favorite_key(srv)));
        let b_fav = rows
            .get(b.as_str())
            .is_some_and(|(_, srv)| favorites.contains_key(&favorite_key(srv)));
        match (a_fav, b_fav) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });
    for key in keys {
        if let Some((row, _)) = rows.get(key) {
            expander.add_row(row);
        }
    }
}

/// Weak references to the widgets inside the header-bar favorites
/// popover. The discovery-flow closures (star toggles, re-announce
/// refresh) refresh popover contents whenever the favorites map
/// mutates; strong captures here would hold the list / label / popover
/// alive for the closure's lifetime, defeating window-close
/// cleanup. Same per-tick-upgrade pattern established in
/// `ServerStatusWidgetsWeak` on #329.
///
/// `Clone` so we can hand a copy to each per-row action closure;
/// `glib::WeakRef` is Rc-like internally, so cloning is cheap.
#[derive(Clone)]
pub(super) struct FavoritesPopoverWeak {
    list: glib::WeakRef<gtk4::ListBox>,
    empty_label: glib::WeakRef<gtk4::Label>,
    popover: glib::WeakRef<gtk4::Popover>,
}

impl FavoritesPopoverWeak {
    fn from_header(handle: &FavoritesHeaderHandle) -> Self {
        Self {
            list: handle.list.downgrade(),
            empty_label: handle.empty_label.downgrade(),
            popover: handle.popover.downgrade(),
        }
    }
}

/// Bundle of dependencies that per-row action closures (Connect /
/// Copy / Unstar) need to capture. Passed by `Rc<FavoriteRowContext>`
/// through `rebuild_favorites_popover` and `attach_favorite_row_actions`
/// so each row-button closure only clones the `Rc` instead of
/// re-capturing nine individual weak refs. All widget handles are
/// `glib::WeakRef` to keep the closures leak-free per the
/// `ServerStatusWidgetsWeak` pattern on #329.
///
/// `displayed_rows` is stored as `std::rc::Weak` specifically to
/// break a retain cycle: the `AdwActionRow` values inside the map
/// own their `connect_toggled` / `connect_clicked` closures, and
/// those closures capture this `FavoriteRowContext`. A strong
/// `Rc<RefCell<HashMap<...>>>` here would close the loop (map →
/// row → signal closure → context → map) and keep the widgets
/// alive past window close. The primary owner of the map — the
/// discovery-polling `glib::timeout_add_local` timer — retains
/// the strong `Rc`, so the upgrade at use-time is reliable while
/// the timer is running and correctly fails when it isn't.
pub(super) struct FavoriteRowContext {
    popover: FavoritesPopoverWeak,
    favorites: Rc<RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>>,
    config: std::sync::Arc<sdr_config::ConfigManager>,
    state: Rc<AppState>,
    hostname_row: glib::WeakRef<adw::EntryRow>,
    port_row: glib::WeakRef<adw::SpinRow>,
    protocol_row: glib::WeakRef<adw::ComboRow>,
    device_row: glib::WeakRef<adw::ComboRow>,
    /// Role picker — `apply_rtl_tcp_connect` needs it so the
    /// per-server `requested_role` can be restored before
    /// the new endpoint's first connect dispatch. Per
    /// `CodeRabbit` round 1 on PR #408.
    role_row: glib::WeakRef<adw::ComboRow>,
    /// Auth-key row — `apply_rtl_tcp_connect` reveals it
    /// when the favorite advertises `auth_required` and
    /// pre-fills any saved key from the keyring so a
    /// pre-configured auth connect lands in a single
    /// `Connecting → Connected` hop. Per `CodeRabbit` round 1
    /// on PR #408.
    auth_key_row: glib::WeakRef<adw::PasswordEntryRow>,
    expander_weak: glib::WeakRef<adw::ExpanderRow>,
    displayed_rows: std::rc::Weak<
        RefCell<std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>>,
    >,
    /// Keyed by `favorite_key(server)` (hostname:port), maps to
    /// a weak ref on the star `ToggleButton` in the currently-
    /// rendered discovery row for that server (if any). Weak
    /// here for the same retain-cycle reason as `displayed_rows`:
    /// the per-row Unstar closure captures this context, and a
    /// strong `Rc` field would close the loop back through the
    /// inner `WeakRef`s to the rows themselves.
    discovered_star_buttons: std::rc::Weak<
        RefCell<std::collections::HashMap<String, glib::WeakRef<gtk4::ToggleButton>>>,
    >,
}

/// Clear the `ListBox` and rebuild one row per `FavoriteEntry`,
/// sorted alphabetically by nickname. Toggles the empty-state
/// label visibility so the popover reads cleanly in both the
/// no-favorites and has-favorites states.
///
/// Silent no-op when either popover widget is gone (window torn
/// down). Each row gets Connect / Copy / Unstar suffix buttons via
/// `attach_favorite_row_actions`.
pub(super) fn rebuild_favorites_popover(
    ctx: &Rc<FavoriteRowContext>,
    favorites: &std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>,
) {
    let (Some(list), Some(empty)) = (
        ctx.popover.list.upgrade(),
        ctx.popover.empty_label.upgrade(),
    ) else {
        return;
    };
    // Clear existing rows. `ListBox::remove` detaches without
    // dropping the widgets past us — the HashMap has already
    // gone through its mutation above this call.
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let has_any = !favorites.is_empty();
    empty.set_visible(!has_any);
    list.set_visible(has_any);
    if !has_any {
        return;
    }
    let now = sidebar::source_panel::now_unix_seconds();
    let mut entries: Vec<&sidebar::source_panel::FavoriteEntry> = favorites.values().collect();
    sort_favorites_for_display(&mut entries);
    for entry in entries {
        let row = adw::ActionRow::builder()
            .title(&entry.nickname)
            .subtitle(format_favorite_subtitle(entry, now))
            .activatable(false)
            .build();
        attach_favorite_row_actions(&row, entry, ctx);
        list.append(&row);
    }
}

/// Build the three suffix buttons on a favorites-popover row:
/// Connect (suggested-action, pins TCP + dispatches to DSP), Copy
/// (writes `host:port` to the clipboard), and Unstar (removes from
/// favorites, persists, reorders discovery, rebuilds the popover).
///
/// Dependencies flow through `FavoriteRowContext` so each closure
/// only clones the `Rc` — not nine individual weak refs. The
/// Connect-button ordering (`protocol_row.set_selected(TCP)`
/// BEFORE `hostname_row.set_text` / `port_row.set_value`) mirrors
/// the discovery-row Connect handler established in PR #335: the
/// hostname / port writes fire change handlers that read the
/// protocol row, so the row must already be on TCP or those
/// handlers will dispatch a stale-UDP `SetNetworkConfig`.
pub(super) fn attach_favorite_row_actions(
    row: &adw::ActionRow,
    entry: &sidebar::source_panel::FavoriteEntry,
    ctx: &Rc<FavoriteRowContext>,
) {
    attach_favorite_connect_button(row, entry, ctx);

    attach_favorite_copy_unstar_buttons(row, entry, ctx);
}

/// Parse a `hostname:port` favorite key back into its two fields.
/// Uses `rsplit_once(':')` so IPv6 literals with multiple colons
/// round-trip if we ever start producing them (today's
/// `favorite_key` only emits the DNS hostname, but the parser
/// should be the conservative half of that contract).
///
/// Returns `None` when the key lacks a colon or the port field
/// doesn't parse as `u16` — callers log and swallow.
pub(super) fn parse_host_port(key: &str) -> Option<(String, u16)> {
    let (host, port_str) = key.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Render a `FavoriteEntry` into the one-line subtitle shown on
/// its row. Joined with ` • ` separators — matches the discovery-
/// row subtitle format so the two lists read consistently.
pub(super) fn format_favorite_subtitle(
    entry: &sidebar::source_panel::FavoriteEntry,
    now_unix: u64,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(entry.key.clone());
    if let (Some(tuner), Some(gains)) = (entry.tuner_name.as_deref(), entry.gain_count) {
        parts.push(format!("{tuner} · {gains} gains"));
    }
    let seen = match entry.last_seen_unix {
        Some(ts) if ts > 0 => format!("seen {}", format_seen_age(now_unix, ts)),
        _ => "offline".to_string(),
    };
    parts.push(seen);
    parts.join(" • ")
}

/// Bucket boundaries for [`format_seen_age`]. Raw Unix-seconds
/// arithmetic (not `std::time::Duration`) because `last_seen_unix`
/// is stored as `u64` seconds in the favorites JSON and stays in
/// that domain end-to-end.
pub(super) const SECONDS_PER_MINUTE: u64 = 60;

pub(super) const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;

pub(super) const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;

/// Bucket a `now - last_seen` difference into a short human
/// string. Coarser buckets than the discovery-row's `format_age`
/// because favorites ages are typically much larger (minutes to
/// days) and the row subtitle has limited horizontal real estate.
pub(super) fn format_seen_age(now_unix: u64, last_seen_unix: u64) -> String {
    if last_seen_unix >= now_unix {
        // Clock skew or freshly-stamped — render as the latest
        // bucket rather than a garbage negative value.
        return "just now".to_string();
    }
    let secs = now_unix - last_seen_unix;
    if secs < SECONDS_PER_MINUTE {
        "just now".to_string()
    } else if secs < SECONDS_PER_HOUR {
        format!("{}m ago", secs / SECONDS_PER_MINUTE)
    } else if secs < SECONDS_PER_DAY {
        format!("{}h ago", secs / SECONDS_PER_HOUR)
    } else {
        format!("{}d ago", secs / SECONDS_PER_DAY)
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

/// Subtitle text shown on AGC-mutexed rows in the grayed-out
/// state so the reason for the lock is inline — without it, an
/// insensitive row is easy to mistake for a bug rather than
/// intentional behavior.
pub(super) const AGC_MUTEX_SUBTITLE: &str = "Disabled while AGC is on";

/// Enforce the tuner AGC ↔ manual gain mutual exclusion on the UI
/// side: when AGC is on, the gain spin row becomes insensitive
/// (grayed out, non-interactive). When AGC is off, the row is
/// fully editable.
///
/// The mutex exists because librtlsdr's `rtlsdr_set_tuner_gain`
/// silently no-ops when AGC mode is active on most RTL variants,
/// and on some oscillates between the manual target and the AGC
/// target in a loop that produces audible artifacts. Preventing
/// the user from editing the control while it would silently fail
/// is the discoverable fix (see #332). Bookmarks restore the full
/// tuning profile with AGC-first-then-gain ordering already, so
/// the restore path still updates `gain_row.set_value` cleanly
/// even when the row is insensitive — the value displays but the
/// user can't edit it until AGC is turned off.
pub(super) fn apply_agc_gain_mutex(gain_row: &adw::SpinRow, agc_active: bool) {
    gain_row.set_sensitive(!agc_active);
    gain_row.set_subtitle(if agc_active { AGC_MUTEX_SUBTITLE } else { "" });
}

/// Enforce the tuner AGC ↔ squelch mutual exclusion on the UI
/// side: when AGC is on, the squelch controls (manual enable,
/// manual level, auto-squelch enable) become insensitive.
///
/// The mutex exists because RTL-SDR's hardware tuner AGC auto-
/// normalizes the IF signal amplitude — the tuner's internal
/// VGA pushes toward a target level regardless of actual RF
/// input. `PowerSquelch` reads mean IF amplitude and gates
/// against a threshold, so with AGC on every signal (including
/// noise on an empty channel) looks like "above threshold" and
/// the gate stays open. Users see this as "all static all the
/// time" the moment they enable AGC while squelch is on.
///
/// Same UX pattern as `apply_agc_gain_mutex`: gray the rows,
/// set a subtitle on the first row explaining why, restore
/// sensitivity when AGC turns off. Both mutexes share the
/// `AGC_MUTEX_SUBTITLE` string so the explanation reads
/// identically across the panel.
pub(super) fn apply_agc_squelch_mutex(
    squelch_enabled_row: &adw::SwitchRow,
    squelch_level_row: &adw::SpinRow,
    auto_squelch_row: &adw::SwitchRow,
    agc_active: bool,
) {
    squelch_enabled_row.set_sensitive(!agc_active);
    squelch_level_row.set_sensitive(!agc_active);
    auto_squelch_row.set_sensitive(!agc_active);
    // Only one subtitle — the squelch-enabled row is the
    // "header" of this group in the Radio panel, so that's
    // where the explanation lands. The other two rows stay
    // grayed without extra text to avoid repeating the
    // message three times in a row.
    squelch_enabled_row.set_subtitle(if agc_active { AGC_MUTEX_SUBTITLE } else { "" });
}

/// Interval for refreshing the source combo's RTL-SDR slot label
/// against the live USB bus. Low-frequency enough to be
/// negligible CPU-wise; fast enough that a user plugging in their
/// dongle after app launch sees the slot update to the real
/// device name within a few seconds without having to restart.
///
/// Previously shared cadence with a server-panel hotplug poll that
/// drove panel visibility — that poll was removed when Share became
/// its own activity icon, but this source-combo poller's 3 s cadence
/// was tuned for the same reason (user plugs in a dongle, sees the
/// slot update by the time they reach for the sidebar) so the value
/// remains a good fit on its own.
pub(super) const SOURCE_RTLSDR_PROBE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(3);

/// Install a hotplug poller on the source panel that keeps the
/// RTL-SDR slot label (`device_row` entry 0) in sync with the
/// live USB bus. Seeded once at build-time (inside
/// `build_source_panel`); this helper adds the ongoing refresh.
///
/// Compared against a cached last-seen label so the `splice` fires
/// only on real edges — plugging in, unplugging, or USB string
/// changing. Without the edge gate we'd churn the combo's model
/// every 3 s and risk transient selection flicker (though GTK's
/// `ComboRow` is robust to same-value splices, the no-op is
/// cheaper to skip than to perform).
///
/// Weak ref on the source panel's `widget` so the poller tears
/// down cleanly on window close — upgrade returns `None` and the
/// `ControlFlow::Break` arm fires.
pub(super) fn connect_source_rtlsdr_probe(panels: &SidebarPanels) {
    let widget_weak = panels.source.widget.downgrade();
    let model_weak = panels.source.device_model.downgrade();
    // Cached label from the last tick so we only rewrite on a
    // real edge. Seed from the model's current `DEVICE_RTLSDR`
    // entry — NOT from a fresh probe — so we're comparing
    // subsequent probes against what the UI is actually showing.
    //
    // A second probe here would race the USB state: if the user
    // unplugs their dongle between `build_source_panel` (which
    // ran the initial probe + seed) and this wiring point, a
    // second probe would read the new bus state, cache it as
    // `last_label`, and then every subsequent tick's probe would
    // match the cache — the combo would stay on the stale plugged-
    // in name forever (or until the NEXT plug / unplug edge
    // briefly desynced them again). Reading the model directly
    // guarantees first-tick reconciliation.
    let seed_label = panels
        .source
        .device_model
        .string(DEVICE_RTLSDR)
        .map_or_else(String::new, |s| s.to_string());
    let last_label: Rc<RefCell<String>> = Rc::new(RefCell::new(seed_label));
    let _ = glib::timeout_add_local(SOURCE_RTLSDR_PROBE_INTERVAL, move || {
        if widget_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        let Some(model) = model_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let current = sidebar::source_panel::probe_rtlsdr_device_label();
        let mut last = last_label.borrow_mut();
        if *last != current {
            tracing::debug!(
                previous = %*last,
                current = %current,
                "source panel: RTL-SDR slot label updated",
            );
            // Replace the RTL-SDR slot in the StringList.
            // `splice(pos, n, additions)` removes `n` items at
            // `pos` and inserts `additions` — so `(DEVICE_RTLSDR,
            // 1, &[&current])` is a single-entry in-place swap.
            // Using the shared `DEVICE_RTLSDR` constant instead
            // of a literal `0` keeps the probe aligned with the
            // rest of the source-row selection logic; all four
            // `DEVICE_*` indices are the one source of truth for
            // slot positions. Leaves Network / File / RTL-TCP
            // entries untouched.
            model.splice(DEVICE_RTLSDR, 1, &[&current]);
            *last = current;
        }
        glib::ControlFlow::Continue
    });
}

#[allow(
    clippy::too_many_lines,
    reason = "GTK signal-wiring panel; splitting would fragment the control mapping"
)]
pub(super) fn connect_source_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    toast_overlay: &adw::ToastOverlay,
    server_running: Rc<std::cell::Cell<bool>>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
) {
    // Sample rate selector + bandwidth advisory re-render.
    // The advisory visibility depends on BOTH the sample-rate
    // selection AND the device-type selection (only network paths
    // care about wire bandwidth). We clone the helper closure into
    // both notify handlers so either trigger re-evaluates.
    // All three widgets the advisory closure touches are weak-
    // ref'd. The closure is attached to both `sample_rate_row` and
    // `device_row`'s `connect_selected_notify` — strong captures
    // here would create the same self-cycle pattern flagged in
    // `connect_share_switch` / `connect_server_status_polling`:
    // `row → closure → row.clone()` keeps the widget alive forever.
    let advisory_row_weak = panels.source.bandwidth_advisory_row.downgrade();
    let device_row_weak = panels.source.device_row.downgrade();
    let sample_rate_row_weak = panels.source.sample_rate_row.downgrade();
    let apply_source_bandwidth_advisory = {
        let advisory_row_weak = advisory_row_weak.clone();
        let device_row_weak = device_row_weak.clone();
        let sample_rate_row_weak = sample_rate_row_weak.clone();
        move || {
            // Any missing widget means the window has been torn
            // down; skip the render — subsequent notify events
            // won't fire against dead widgets.
            let (Some(advisory), Some(device_row), Some(sample_rate_row)) = (
                advisory_row_weak.upgrade(),
                device_row_weak.upgrade(),
                sample_rate_row_weak.upgrade(),
            ) else {
                return;
            };
            // Raw Network (TCP/UDP IQ) has the same wire-bandwidth
            // cost profile as rtl_tcp — a high-sample-rate pull
            // across the network will saturate a 100 Mbit link
            // either way. The advisory applies equally to both
            // network-backed source types.
            let is_network_path = matches!(device_row.selected(), DEVICE_NETWORK | DEVICE_RTLTCP);
            // Bounds-check the sample-rate index: transient
            // out-of-range values from widget-model churn would
            // otherwise satisfy the `>= threshold` compare and
            // flash the advisory visible with no legal selection.
            // Same safety pattern as the server-panel advisory
            // above.
            let selected = sample_rate_row.selected();
            let is_high_rate = (selected as usize) < SAMPLE_RATES.len()
                && selected >= crate::sidebar::source_panel::HIGH_BANDWIDTH_SAMPLE_RATE_IDX;
            advisory.set_visible(is_network_path && is_high_rate);
        }
    };
    // Seed the advisory visibility once at wire-up. Without this,
    // the caption stays hidden until the user nudges one of the
    // two rows — which hides it even when the restored config
    // already has RTL-TCP + a high sample rate selected.
    apply_source_bandwidth_advisory();

    // Sample rate selector. Restore-then-wire (#552).
    {
        let persisted_idx = sidebar::source_panel::load_source_sample_rate_index(config);
        if (persisted_idx as usize) < SAMPLE_RATES.len() {
            panels.source.sample_rate_row.set_selected(persisted_idx);
            if let Some(&rate) = SAMPLE_RATES.get(persisted_idx as usize) {
                state.send_dsp(UiToDsp::SetSampleRate(rate));
            }
        }
    }
    let state_sr = Rc::clone(state);
    let config_sr = std::sync::Arc::clone(config);
    let apply_on_sr = apply_source_bandwidth_advisory.clone();
    panels
        .source
        .sample_rate_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting. GTK can briefly emit
            // out-of-range values during widget-model churn (e.g.
            // teardown / rebuild on style changes); persisting
            // those would corrupt the config file across restart.
            // Mirror the protocol_row pattern further down: bail
            // when the index doesn't map to a real sample rate.
            // Per CodeRabbit round 1 on PR #558.
            let Some(&rate) = SAMPLE_RATES.get(idx as usize) else {
                return;
            };
            sidebar::source_panel::save_source_sample_rate_index(&config_sr, idx);
            state_sr.send_dsp(UiToDsp::SetSampleRate(rate));
            apply_on_sr();
        });
    // Source-type (device) selector. Restore-then-wire (#552).
    // The restore SETs the row's selected index, which fires
    // `connect_selected_notify` and thus re-applies the bandwidth
    // advisory; that's intentional (it wires up the correct
    // visibility for the persisted source type at startup). The
    // source-type swap itself is handled by an UPSTREAM
    // `connect_selected_notify` (around the per-source-type
    // visibility block); this handler only wires the persistence
    // save + bandwidth-advisory refresh. The dedicated swap
    // dispatch lives at the end of `connect_source_panel`.
    {
        let persisted_idx = sidebar::source_panel::load_source_device_index(config);
        // Bound check via `DEVICE_RTLTCP` (the highest valid
        // index) — fails closed if a stale config carries an
        // out-of-range value (e.g. a future build added more
        // source types and the user rolled back).
        if persisted_idx <= sidebar::source_panel::DEVICE_RTLTCP {
            panels.source.device_row.set_selected(persisted_idx);
            // Dispatch the restored source type to the DSP so a
            // saved Network / File / RTL-TCP selection takes
            // effect at startup. The change-notify handler that
            // dispatches `SetSourceType` from user clicks is
            // wired AFTER this restore block runs, and even if it
            // were wired first, programmatic `set_selected` to a
            // value that already matches the row's default (0 =
            // RTL-SDR) wouldn't fire it. Explicit dispatch closes
            // both gaps. Per CodeRabbit round 1 on PR #558.
            let source_type = match persisted_idx {
                sidebar::source_panel::DEVICE_RTLSDR => Some(SourceType::RtlSdr),
                sidebar::source_panel::DEVICE_NETWORK => Some(SourceType::Network),
                sidebar::source_panel::DEVICE_FILE => Some(SourceType::File),
                sidebar::source_panel::DEVICE_RTLTCP => Some(SourceType::RtlTcp),
                _ => None,
            };
            if let Some(source_type) = source_type {
                state.send_dsp(UiToDsp::SetSourceType(source_type));
            }
        }
    }
    let config_device = std::sync::Arc::clone(config);
    let apply_on_device = apply_source_bandwidth_advisory;
    panels
        .source
        .device_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (same rationale as the
            // sample-rate row above). `DEVICE_RTLTCP` is the
            // highest valid index. Per CodeRabbit round 1 on
            // PR #558.
            if idx > sidebar::source_panel::DEVICE_RTLTCP {
                return;
            }
            sidebar::source_panel::save_source_device_index(&config_device, idx);
            apply_on_device();
        });

    // DC blocking toggle. Restore-then-wire (#552). Same idiom
    // as bias-T / gain / PPM: programmatic `set_active` fires
    // `connect_active_notify`, which would re-save the loaded
    // value AND re-dispatch `SetDcBlocking` — both cheap, but
    // the duplicate dispatch in tracing logs is misleading. So
    // restore first, then wire.
    {
        let persisted = sidebar::source_panel::load_source_dc_blocking(config);
        panels.source.dc_blocking_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetDcBlocking(persisted));
    }
    let state_dc_block = Rc::clone(state);
    let config_dc_block = std::sync::Arc::clone(config);
    panels
        .source
        .dc_blocking_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_dc_blocking(&config_dc_block, enabled);
            state_dc_block.send_dsp(UiToDsp::SetDcBlocking(enabled));
        });

    // Bias-T toggle (#537). Powers an inline LNA over the
    // RTL-SDR's coax. The startup restore must run BEFORE
    // wiring the change-notify handler — same idiom as the
    // satellites-panel auto-record toggle: a programmatic
    // `set_active` fires `connect_active_notify`, which would
    // otherwise re-save the just-loaded value (cheap) AND
    // dispatch a redundant `SetBiasTee` (also cheap, but
    // misleading in tracing logs).
    {
        let persisted = sidebar::source_panel::load_source_rtl_bias_tee(config);
        panels.source.bias_tee_row.set_active(persisted);
        // Dispatch the persisted value once at startup so the
        // dongle's GPIO matches the UI from the first source
        // open, not just after the user toggles. The
        // `SetBiasTee` handler stores the value in `DspState`
        // up-front, and `open_source` re-applies it to the
        // freshly-opened RTL-SDR source — so this dispatch
        // works regardless of whether a source is open at
        // startup. Per CR on PR #550.
        state.send_dsp(UiToDsp::SetBiasTee(persisted));
    }
    let state_bias_tee = Rc::clone(state);
    let config_bias_tee = std::sync::Arc::clone(config);
    panels
        .source
        .bias_tee_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_rtl_bias_tee(&config_bias_tee, enabled);
            state_bias_tee.send_dsp(UiToDsp::SetBiasTee(enabled));
        });

    // Direct sampling combo (#538). Same restore-then-wire idiom
    // as bias-T above. The persisted value is the combo index
    // (0/1/2), which is also the `rtlsdr_set_direct_sampling`
    // mode argument — cast straight to `i32` for the dispatch.
    {
        let persisted = sidebar::source_panel::load_source_rtl_direct_sampling_mode(config);
        if persisted <= sidebar::source_panel::DIRECT_SAMPLING_MAX_IDX {
            panels.source.direct_sampling_row.set_selected(persisted);
            #[allow(clippy::cast_possible_wrap, reason = "u32 <= 2 fits in i32 trivially")]
            state.send_dsp(UiToDsp::SetDirectSampling(persisted as i32));
        }
    }
    let state_direct = Rc::clone(state);
    let config_direct = std::sync::Arc::clone(config);
    let toast_overlay_direct = toast_overlay.downgrade();
    panels
        .source
        .direct_sampling_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (mirrors the
            // protocol_row / sample-rate / device / decimation
            // early-return-on-invalid pattern). GTK can briefly
            // emit out-of-range values during widget-model
            // churn; persisting them would leave the next
            // restart pinned to a non-existent direct-sampling
            // mode. Per `CodeRabbit` round 3 on PR #558.
            if idx > sidebar::source_panel::DIRECT_SAMPLING_MAX_IDX {
                return;
            }
            sidebar::source_panel::save_source_rtl_direct_sampling_mode(&config_direct, idx);
            #[allow(clippy::cast_possible_wrap, reason = "idx <= 2 fits in i32 trivially")]
            state_direct.send_dsp(UiToDsp::SetDirectSampling(idx as i32));
            // Surface a tune-guidance toast: enabling direct
            // sampling routes the antenna straight to the ADC,
            // which silences VHF/UHF (the R820T tuner is now
            // bypassed); disabling it puts the tuner back in
            // path, which silences HF. Either direction needs a
            // manual retune to be useful, and a toast saves the
            // user from staring at noise wondering why. Per
            // `CodeRabbit` round 1 on PR #559 / closes #538
            // objective.
            if let Some(overlay) = toast_overlay_direct.upgrade() {
                let msg = if idx == sidebar::source_panel::DIRECT_SAMPLING_DISABLED_IDX {
                    "Direct Sampling off — retune to VHF/UHF."
                } else {
                    // No `<` here: `adw::Toast` titles are Pango markup and
                    // "(< 28 MHz)" failed to parse (GTK-WARNING, blank toast).
                    "Direct Sampling on — retune to an HF frequency (below 28 MHz)."
                };
                overlay.add_toast(plain_toast(msg));
            }
        });

    // Offset tuning toggle (#539). Same restore-then-wire idiom
    // as bias-T above. The controller bridge
    // (`UiToDsp::SetOffsetTuning`) was already plumbed; only
    // wiring is new here.
    //
    // Only DISPATCH the persisted value when it's `true`. The
    // librtlsdr R820T-family branch returns `InvalidParameter`
    // for every `set_offset_tuning` call regardless of value —
    // dispatching `false` at startup (the default for users
    // who've never touched the toggle) generates a spurious
    // "Offset tuning failed" toast on the vast majority of
    // dongles. The driver default already matches `false`, so
    // skipping the dispatch is semantically a no-op. Per issue
    // #564.
    {
        let persisted = sidebar::source_panel::load_source_rtl_offset_tuning(config);
        panels.source.offset_tuning_row.set_active(persisted);
        if persisted {
            state.send_dsp(UiToDsp::SetOffsetTuning(true));
        }
    }
    let state_offset = Rc::clone(state);
    let config_offset = std::sync::Arc::clone(config);
    panels
        .source
        .offset_tuning_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_rtl_offset_tuning(&config_offset, enabled);
            state_offset.send_dsp(UiToDsp::SetOffsetTuning(enabled));
        });

    // IQ inversion toggle. Restore-then-wire (#552).
    {
        let persisted = sidebar::source_panel::load_source_iq_inversion(config);
        panels.source.iq_inversion_row.set_active(persisted);
        state.send_dsp(UiToDsp::SetIqInversion(persisted));
    }
    let state_iq_inv = Rc::clone(state);
    let config_iq_inv = std::sync::Arc::clone(config);
    panels
        .source
        .iq_inversion_row
        .connect_active_notify(move |row| {
            let enabled = row.is_active();
            sidebar::source_panel::save_source_iq_inversion(&config_iq_inv, enabled);
            state_iq_inv.send_dsp(UiToDsp::SetIqInversion(enabled));
        });

    // Decimation selector. Restore-then-wire (#552). The
    // decimation index also feeds the bandwidth-advisory
    // recompute via `apply_source_bandwidth_advisory`, so
    // restoring here BEFORE wiring keeps the advisory pristine
    // on first launch.
    {
        let persisted_idx = sidebar::source_panel::load_source_decimation_index(config);
        if (persisted_idx as usize) < DECIMATION_FACTORS.len() {
            panels.source.decimation_row.set_selected(persisted_idx);
            if let Some(&factor) = DECIMATION_FACTORS.get(persisted_idx as usize) {
                state.send_dsp(UiToDsp::SetDecimation(factor));
            }
        }
    }
    let state_decim = Rc::clone(state);
    let config_decim = std::sync::Arc::clone(config);
    panels
        .source
        .decimation_row
        .connect_selected_notify(move |row| {
            let idx = row.selected();
            // Validate before persisting (same rationale as the
            // sample-rate row above). Per CodeRabbit round 1 on
            // PR #558.
            let Some(&factor) = DECIMATION_FACTORS.get(idx as usize) else {
                return;
            };
            sidebar::source_panel::save_source_decimation_index(&config_decim, idx);
            state_decim.send_dsp(UiToDsp::SetDecimation(factor));
        });

    // Gain control. Sensitivity is gated by AGC — see the `AGC
    // toggle` handler below and `apply_agc_gain_mutex` for the
    // reasoning (librtlsdr silently ignores gain writes when
    // tuner AGC is on; some variants also oscillate between
    // manual and AGC targets on mixed writes).
    //
    // The notify handler checks the AGC state and skips the
    // DSP dispatch when AGC is not Off. `set_sensitive(false)`
    // blocks user interaction but does NOT suppress the notify
    // signal on programmatic `set_value` calls (bookmark
    // restore, future preset-apply paths, etc.), so a pure-
    // sensitivity gate would still let a stream of no-op
    // `SetGain` commands hit the DSP every time a non-Off-AGC
    // bookmark loads. The AGC-state check short-circuits those
    // at the source — both hardware and software AGC
    // renormalize the signal, so any gain write during those
    // modes is discarded downstream anyway.
    // Restore persisted manual gain BEFORE wiring the notify
    // handler — otherwise the programmatic `set_value` fires
    // `connect_value_notify` and the in-flight `set_value`
    // re-dispatches with the freshly-loaded value redundantly.
    // Same idiom as the bias-T restore. Per #551.
    {
        let persisted_gain = sidebar::source_panel::load_source_rtl_gain_db(config);
        panels.source.gain_row.set_value(persisted_gain);
        state.send_dsp(UiToDsp::SetGain(persisted_gain));
    }
    let state_gain = Rc::clone(state);
    let agc_row_for_gain = panels.source.agc_row.downgrade();
    let config_gain = std::sync::Arc::clone(config);
    panels.source.gain_row.connect_value_notify(move |row| {
        // Persist the slider value even when AGC is on — the
        // user's last manual gain should survive an AGC-on /
        // restart / AGC-off cycle. Per #551.
        sidebar::source_panel::save_source_rtl_gain_db(&config_gain, row.value());
        if let Some(agc_row) = agc_row_for_gain.upgrade() {
            let agc_type = sidebar::source_panel::agc_type_from_selected(agc_row.selected());
            if !matches!(agc_type, Some(sidebar::source_panel::AgcType::Off)) {
                return;
            }
        }
        state_gain.send_dsp(UiToDsp::SetGain(row.value()));
    });

    // AGC type selector (Off / Hardware / Software). Dispatches
    // the right `UiToDsp::SetAgc` / `UiToDsp::SetSoftwareAgc`
    // pair on every selection and also fires two mutexes so
    // the UI doesn't lie about controls that EITHER AGC type
    // disables:
    //
    // 1. Gain row — `rtlsdr_set_tuner_gain` silently no-ops on
    //    most RTL variants when hardware AGC is on; software
    //    AGC makes manual gain pointless because the DSP stage
    //    would renormalize it immediately.
    // 2. Squelch rows — both AGC types auto-normalize IF
    //    amplitude, so amplitude-based squelch can't distinguish
    //    signal from noise and the gate just stays open. Without
    //    this mutex users see "all static all the time" the
    //    moment they enable AGC with squelch on.
    //
    // Register the AGC notify handler BEFORE restoring the
    // persisted selection. `set_selected` only fires
    // `selected-notify` when the new index differs from the
    // current one, so the startup-restore path relies on the
    // handler being registered first to dispatch the persisted
    // mode. Without this ordering, fresh installs (persisted
    // matches build-time default) or config match would leave
    // DSP stuck in its all-off default state until the user
    // touched the selector.
    //
    // Handler drops transient out-of-range indices —
    // `agc_type_from_selected` now returns `Option<AgcType>`
    // and we early-return on `None` rather than coercing them
    // to a fallback and persisting a bogus config write during
    // widget-teardown churn.
    let state_agc = Rc::clone(state);
    let config_for_agc = std::sync::Arc::clone(config);
    let gain_row_for_agc = panels.source.gain_row.clone();
    let squelch_enabled_for_agc = panels.radio.squelch_enabled_row.clone();
    let squelch_level_for_agc = panels.radio.squelch_level_row.clone();
    let auto_squelch_for_agc = panels.radio.auto_squelch_row.clone();
    panels.source.agc_row.connect_selected_notify(move |row| {
        let Some(agc_type) = sidebar::source_panel::agc_type_from_selected(row.selected()) else {
            // Transient GTK value (e.g., `INVALID_LIST_POSITION`
            // during model swap). Skip dispatch AND persistence
            // — we'll pick up the next real selection from the
            // follow-up notify event.
            tracing::trace!(
                selected = row.selected(),
                "AGC combo notify with out-of-range index, ignoring"
            );
            return;
        };

        // Dispatch both messages every time so exactly one
        // enable path is active and the other is cleanly off.
        // The engine treats hardware and software AGC as
        // independent flags; the UI is the policy layer that
        // mutually excludes them.
        let (hw, sw) = match agc_type {
            sidebar::source_panel::AgcType::Off => (false, false),
            sidebar::source_panel::AgcType::Hardware => (true, false),
            sidebar::source_panel::AgcType::Software => (false, true),
        };
        state_agc.send_dsp(UiToDsp::SetAgc(hw));
        state_agc.send_dsp(UiToDsp::SetSoftwareAgc(sw));

        // Persist the new selection so the choice sticks
        // across restarts. Cheap — `ConfigManager::write` is an
        // in-memory update with a debounced flush to disk.
        sidebar::source_panel::save_agc_type(&config_for_agc, agc_type);

        let agc_active = !matches!(agc_type, sidebar::source_panel::AgcType::Off);
        apply_agc_gain_mutex(&gain_row_for_agc, agc_active);
        apply_agc_squelch_mutex(
            &squelch_enabled_for_agc,
            &squelch_level_for_agc,
            &auto_squelch_for_agc,
            agc_active,
        );
    });

    // Restore persisted AGC type from config now that the
    // notify handler is wired up. Two scenarios:
    //
    // 1. Persisted index differs from the combo's build-time
    //    default (Software) — `set_selected` fires
    //    `selected-notify`, the handler runs, DSP is
    //    dispatched, mutexes applied.
    // 2. Persisted index matches the default (fresh install
    //    or user previously selected Software) —
    //    `set_selected` is a no-op and `selected-notify`
    //    does NOT fire. We explicitly dispatch so DSP still
    //    gets the initial-state sync and mutexes are applied
    //    against the seeded selection.
    //
    // Both paths run the same dispatch logic; the explicit
    // post-`set_selected` call is idempotent with the notify
    // handler (both `SetAgc` and `SetSoftwareAgc` are
    // idempotent at the controller), so the double-dispatch
    // in scenario 1 is cheap and correct.
    {
        let persisted = sidebar::source_panel::load_agc_type(config);
        panels
            .source
            .agc_row
            .set_selected(sidebar::source_panel::selected_from_agc_type(persisted));

        let (hw, sw) = match persisted {
            sidebar::source_panel::AgcType::Off => (false, false),
            sidebar::source_panel::AgcType::Hardware => (true, false),
            sidebar::source_panel::AgcType::Software => (false, true),
        };
        state.send_dsp(UiToDsp::SetAgc(hw));
        state.send_dsp(UiToDsp::SetSoftwareAgc(sw));
        let agc_active = !matches!(persisted, sidebar::source_panel::AgcType::Off);
        apply_agc_gain_mutex(&panels.source.gain_row, agc_active);
        apply_agc_squelch_mutex(
            &panels.radio.squelch_enabled_row,
            &panels.radio.squelch_level_row,
            &panels.radio.auto_squelch_row,
            agc_active,
        );
    }

    // Shared "last-good auth bytes" cache between the auth-key
    // handler (primary writer) and the role-picker handler
    // (reader). Populated whenever the auth row parses as empty
    // (`None`, intentional clear) or valid hex (`Some(bytes)`);
    // NOT updated on malformed hex. The role handler uses this
    // snapshot when the live auth text is unparseable so it can
    // still propagate the new role to DSP with a coherent
    // auth_key value — without this, flipping role while the
    // key field held a bad paste would skip the whole
    // `SetRtlTcpClientConfig` dispatch and leave DSP on the
    // previous role. Per `CodeRabbit` round 9 on PR #408.
    //
    // `Rc<RefCell<Option<Vec<u8>>>>` on GTK's single-threaded
    // main loop — no lock contention. Declared BEFORE the
    // startup last-connected restore below so that block can
    // seed the cache with the keyring-loaded bytes — per
    // `CodeRabbit` round 10 on PR #408, leaving the cache
    // empty after startup would let a subsequent malformed-hex
    // role flip clear DSP's working auth instead of preserving
    // the startup-restored bytes.
    let last_good_auth_key: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));

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
        if let Some(srv) = last_connected.as_ref() {
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
            let has_auth_required = matches!(
                favorite_entry.as_ref().and_then(|f| f.auth_required),
                Some(true)
            );
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

    // Source type selector — guard against transient out-of-range
    // indices AND enforce mutual exclusivity with the rtl_tcp server
    // (the dongle can only serve one master; re-selecting RTL-SDR
    // while the server's accept thread has the USB device would
    // trigger a double-open at the next Play).
    let state_source = Rc::clone(state);
    let toast_overlay_weak = toast_overlay.downgrade();
    // Last-known legal selection. Seeded from the current row state
    // so the revert path on first illegal transition lands on the
    // value the UI already shows. Updated every time the guard
    // accepts a new selection.
    let last_legal_selection: Rc<std::cell::Cell<u32>> =
        Rc::new(std::cell::Cell::new(panels.source.device_row.selected()));
    // Re-entry guard against our own `set_selected` (the revert).
    // Without it the revert would re-enter this handler, see the
    // previous illegal value as "new", and endlessly toggle.
    let reverting: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
    panels
        .source
        .device_row
        .connect_selected_notify(move |row| {
            if reverting.get() {
                // Our own revert fired this notify — drop it.
                return;
            }
            let selected = row.selected();
            // Exclusivity guard: can't re-enter the local-source
            // world while the rtl_tcp server has the dongle claimed.
            if selected == DEVICE_RTLSDR && server_running.get() {
                if let Some(overlay) = toast_overlay_weak.upgrade() {
                    overlay.add_toast(plain_toast(
                        "Stop the network server first before switching to local RTL-SDR.",
                    ));
                }
                reverting.set(true);
                row.set_selected(last_legal_selection.get());
                reverting.set(false);
                return;
            }
            let source_type = match selected {
                DEVICE_RTLSDR => SourceType::RtlSdr,
                DEVICE_NETWORK => SourceType::Network,
                DEVICE_FILE => SourceType::File,
                DEVICE_RTLTCP => SourceType::RtlTcp,
                _ => return, // ignore transient indices
            };
            last_legal_selection.set(selected);
            state_source.send_dsp(UiToDsp::SetSourceType(source_type));
        });

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
    let last_good_for_role = Rc::clone(&last_good_auth_key);
    panels
        .source
        .rtl_tcp_role_row
        .connect_selected_notify(move |row| {
            use crate::sidebar::source_panel::{
                FavoriteRole, KEY_RTL_TCP_CLIENT_LAST_ROLE, RTL_TCP_ROLE_CONTROL_IDX,
                RTL_TCP_ROLE_LISTEN_IDX, save_favorites,
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
                save_favorites(&config_for_role, &snapshot);
            }
        });

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
    let last_good_for_auth = Rc::clone(&last_good_auth_key);
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

/// Format the subtitle string for a discovered `rtl_tcp` server row.
///
/// Emits three pieces separated by ` • `:
///
/// 1. `{connect_target}:{port}` — the address the Connect button will
///    dial (IPv4 address if we have one, otherwise the advertised
///    hostname).
/// 2. advertised mDNS hostname — only when it's non-empty AND
///    genuinely different from the connect target (i.e., we have an
///    IP and want to show the friendly name alongside it). The
///    hostname is stripped of any trailing `.local.` so we show
///    `shack-pi` instead of `shack-pi.local.`.
/// 3. `{tuner} · {gains} gains · seen {age}` — hardware info plus
///    the freshness indicator from `format_age`.
///
/// Kept as a free function (not a method on `DiscoveredServer`) so the
/// age-stamp convention stays a UI concern and the discovery crate
/// doesn't need to think about human-readable timestamps.
pub(super) fn format_discovery_subtitle(server: &DiscoveredServer, elapsed: Duration) -> String {
    let connect_target = server
        .addresses
        .first()
        .map_or_else(|| server.hostname.clone(), ToString::to_string);
    let bare_hostname = bare_local_host(&server.hostname);
    // Compare `bare_hostname` against a similarly-trimmed view of the
    // connect target so the no-IP fallback (target = hostname) doesn't
    // render "shack-pi.local.:1234 • shack-pi • …" — one name twice.
    let bare_connect_target = bare_local_host(&connect_target);
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(format!("{connect_target}:{}", server.port));
    if !bare_hostname.is_empty() && bare_hostname != bare_connect_target {
        parts.push(bare_hostname.to_string());
    }
    parts.push(format!(
        "{} · {} gains · seen {}",
        server.txt.tuner,
        server.txt.gains,
        format_age(elapsed)
    ));
    parts.join(" • ")
}

/// Strip a trailing `.local.` / `.local` / `.` suffix from an mDNS
/// hostname so the user sees `shack-pi` instead of `shack-pi.local.`.
/// Purely presentational — resolution still happens against the full
/// name in the Connect button's dial path.
pub(super) fn bare_local_host(host: &str) -> &str {
    host.trim_end_matches('.')
        .trim_end_matches(".local")
        .trim_end_matches('.')
}

/// Render an elapsed duration as a short human-readable age string.
///
/// Buckets:
/// - under 5 s → `"just now"` (avoids flicker on the 200 ms poll tick)
/// - 5 s – 60 s → `"Ns ago"`
/// - 1 m – 60 m → `"Nm ago"`
/// - 60 m and up → `"Nh ago"`
///
/// Coarse by design — the point is to tell "freshly re-announced" from
/// "cached and possibly dead", not to replace an NTP timestamp.
pub(super) fn format_age(elapsed: Duration) -> String {
    const FRESH_THRESHOLD: Duration = Duration::from_secs(5);
    let secs = elapsed.as_secs();
    if elapsed < FRESH_THRESHOLD {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(test)]
mod parse_host_port_tests {
    use super::parse_host_port;

    #[test]
    fn round_trips_a_simple_hostname_port_pair() {
        // The mainline case — `favorite_key(server)` today
        // produces exactly this shape, so Connect-from-popover
        // depends on this round-trip working.
        assert_eq!(
            parse_host_port("shack-pi:1234"),
            Some(("shack-pi".to_string(), 1234))
        );
    }

    #[test]
    fn ipv6_literal_with_embedded_colons_splits_on_last_colon() {
        // We don't emit bracketed IPv6 in `favorite_key` today,
        // but the parser should be the conservative half of the
        // contract: `rsplit_once(':')` keeps everything up to the
        // last colon as the host so an IPv6 literal round-trips
        // if we ever start persisting one.
        assert_eq!(
            parse_host_port("fe80::1:8080"),
            Some(("fe80::1".to_string(), 8080))
        );
    }

    #[test]
    fn rejects_missing_colon() {
        assert_eq!(parse_host_port("shack-pi"), None);
    }

    #[test]
    fn rejects_non_numeric_port() {
        assert_eq!(parse_host_port("shack-pi:abc"), None);
    }

    #[test]
    fn rejects_out_of_range_port() {
        // 65536 overflows u16; parse must fail rather than
        // silently truncating.
        assert_eq!(parse_host_port("shack-pi:65536"), None);
    }

    #[test]
    fn rejects_empty_host() {
        // ":1234" shouldn't round-trip as a valid endpoint —
        // callers would dispatch `SetNetworkConfig { hostname:
        // "" }` which is garbage.
        assert_eq!(parse_host_port(":1234"), None);
    }
}

#[cfg(test)]
mod favorite_sort_tests {
    use super::sort_favorites_for_display;
    use crate::sidebar::source_panel::FavoriteEntry;

    fn entry(key: &str, nickname: &str) -> FavoriteEntry {
        FavoriteEntry {
            key: key.into(),
            nickname: nickname.into(),
            tuner_name: None,
            gain_count: None,
            last_seen_unix: None,
            requested_role: None,
            auth_required: None,
        }
    }

    #[test]
    fn primary_order_is_lowercased_nickname() {
        let a = entry("a.local.:1234", "Zeta");
        let b = entry("b.local.:1234", "alpha");
        let c = entry("c.local.:1234", "Beta");
        let mut entries = vec![&a, &b, &c];
        sort_favorites_for_display(&mut entries);
        // Case-insensitive: "alpha" < "Beta" < "Zeta".
        assert_eq!(
            entries.iter().map(|e| &e.key[..]).collect::<Vec<_>>(),
            ["b.local.:1234", "c.local.:1234", "a.local.:1234",]
        );
    }

    #[test]
    fn tie_breaks_on_key_when_nicknames_match() {
        // Duplicate nickname across two servers — the secondary
        // key must pin the order deterministically so two app
        // launches (or two inserts against an unstable HashMap
        // iteration order) render the popover the same way.
        let a = entry("attic-pi.local.:1234", "Shack");
        let b = entry("shack-pi.local.:1234", "Shack");
        let c = entry("basement-pi.local.:1234", "Shack");
        let mut entries = vec![&a, &b, &c];
        sort_favorites_for_display(&mut entries);
        // Alphabetical by `key` — attic < basement < shack.
        assert_eq!(
            entries.iter().map(|e| &e.key[..]).collect::<Vec<_>>(),
            [
                "attic-pi.local.:1234",
                "basement-pi.local.:1234",
                "shack-pi.local.:1234",
            ]
        );
    }

    #[test]
    fn idempotent_when_already_sorted() {
        let a = entry("a.local.:1234", "alpha");
        let b = entry("b.local.:1234", "beta");
        let mut entries = vec![&a, &b];
        sort_favorites_for_display(&mut entries);
        assert_eq!(
            entries.iter().map(|e| &e.key[..]).collect::<Vec<_>>(),
            ["a.local.:1234", "b.local.:1234",]
        );
    }
}

#[cfg(test)]
mod favorite_subtitle_format_tests {
    use super::{format_favorite_subtitle, format_seen_age};
    use crate::sidebar::source_panel::FavoriteEntry;

    /// Fixed "wall-clock now" for the subtitle + age tests. Pinning
    /// this keeps the expected output deterministic; the actual
    /// seconds value is arbitrary (2023-11-14T22:13:20Z) — what
    /// matters is that all test inputs derive their `last_seen`
    /// offsets from here.
    const NOW_UNIX: u64 = 1_700_000_000;

    fn sample_entry(
        tuner: Option<&str>,
        gains: Option<u32>,
        last_seen: Option<u64>,
    ) -> FavoriteEntry {
        FavoriteEntry {
            key: "shack-pi.local.:1234".into(),
            nickname: "Shack Pi".into(),
            tuner_name: tuner.map(str::to_string),
            gain_count: gains,
            last_seen_unix: last_seen,
            requested_role: None,
            auth_required: None,
        }
    }

    #[test]
    fn seen_age_just_now_under_60_seconds() {
        // Sub-minute gap renders as "just now" — avoids "0m ago"
        // churn on freshly-stamped entries.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 30), "just now");
    }

    #[test]
    fn seen_age_minute_bucket() {
        // Integer division, not rounding: 179s → 2m (not 3m).
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 179), "2m ago");
    }

    #[test]
    fn seen_age_hour_bucket() {
        // 3599s → 59m (last second of minute bucket), 3600s → 1h.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 3_600), "1h ago");
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 3_599), "59m ago");
    }

    #[test]
    fn seen_age_day_bucket() {
        // 86_399s → 23h, 86_400s → 1d.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 86_400), "1d ago");
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX - 86_399), "23h ago");
    }

    #[test]
    fn seen_age_clock_skew_renders_just_now() {
        // `last_seen > now` means the entry was stamped against a
        // clock that was ahead of ours — shouldn't underflow into
        // a garbage value.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX + 60), "just now");
        // Equal case.
        assert_eq!(format_seen_age(NOW_UNIX, NOW_UNIX), "just now");
    }

    #[test]
    fn subtitle_includes_all_three_parts_when_metadata_present() {
        // Canonical "rich" entry: key + tuner·gains + seen age,
        // joined by middle-dot separators.
        let entry = sample_entry(Some("R820T"), Some(29), Some(NOW_UNIX - 7_200));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • R820T · 29 gains • seen 2h ago",
        );
    }

    #[test]
    fn subtitle_drops_tuner_segment_when_tuner_missing() {
        // Legacy-upgraded entry with no tuner metadata — the
        // "tuner · gains" middle segment is omitted entirely
        // rather than rendering empty "— · 0 gains" placeholder.
        let entry = sample_entry(None, None, Some(NOW_UNIX - 300));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • seen 5m ago",
        );
    }

    #[test]
    fn subtitle_drops_tuner_segment_when_only_gains_missing() {
        // Partial metadata is still incomplete — `if let (Some,
        // Some)` means both must be present or neither renders.
        let entry = sample_entry(Some("R820T"), None, Some(NOW_UNIX - 300));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • seen 5m ago",
        );
    }

    #[test]
    fn subtitle_shows_offline_when_last_seen_is_none() {
        // Never seen this session → "offline" in the seen slot.
        let entry = sample_entry(Some("R820T"), Some(29), None);
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • R820T · 29 gains • offline",
        );
    }

    #[test]
    fn subtitle_shows_offline_when_last_seen_is_zero() {
        // Zero is treated as "no real stamp" — `format_favorite_
        // subtitle` explicitly gates on `ts > 0` so a corrupt /
        // default-valued timestamp doesn't render as "seen 55y
        // ago" (the 1970 epoch).
        let entry = sample_entry(Some("R820T"), Some(29), Some(0));
        assert_eq!(
            format_favorite_subtitle(&entry, NOW_UNIX),
            "shack-pi.local.:1234 • R820T · 29 gains • offline",
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod rtl_tcp_discovery_format_tests {
    use std::net::IpAddr;
    use std::time::{Duration, Instant};

    use sdr_rtltcp_discovery::{DiscoveredServer, TxtRecord};

    use super::{format_age, format_discovery_subtitle};

    fn sample_server(addresses: Vec<IpAddr>, hostname: &str) -> DiscoveredServer {
        DiscoveredServer {
            instance_name: "shack-pi weather._rtl_tcp._tcp.local.".into(),
            hostname: hostname.into(),
            port: 1234,
            addresses,
            txt: TxtRecord {
                tuner: "R820T".into(),
                version: "0.1.0".into(),
                gains: 29,
                nickname: "weather".into(),
                txbuf: None,
                codecs: None,
                auth_required: None,
            },
            last_seen: Instant::now(),
        }
    }

    #[test]
    fn format_age_buckets_seconds_minutes_hours() {
        // < 5 s bucket → "just now" (debounces the 200 ms refresh
        // from showing "0s ago / 1s ago" noise).
        assert_eq!(format_age(Duration::from_millis(0)), "just now");
        assert_eq!(format_age(Duration::from_secs(4)), "just now");
        // 5 s – 59 s → "Ns ago"
        assert_eq!(format_age(Duration::from_secs(5)), "5s ago");
        assert_eq!(format_age(Duration::from_secs(59)), "59s ago");
        // 1 m – 59 m → "Nm ago"
        assert_eq!(format_age(Duration::from_mins(1)), "1m ago");
        assert_eq!(format_age(Duration::from_secs(125)), "2m ago");
        assert_eq!(format_age(Duration::from_secs(3599)), "59m ago");
        // 1 h+ → "Nh ago"
        assert_eq!(format_age(Duration::from_hours(1)), "1h ago");
        assert_eq!(format_age(Duration::from_hours(2)), "2h ago");
    }

    #[test]
    fn subtitle_with_ip_shows_hostname_and_freshness() {
        // When we have a resolved IP, the subtitle includes both the
        // IP (the Connect button's target) AND the advertised
        // hostname (the friendly name the user recognises).
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        let server = sample_server(vec![ip], "shack-pi.local.");
        let subtitle = format_discovery_subtitle(&server, Duration::from_secs(12));
        assert!(
            subtitle.contains("192.168.1.5:1234"),
            "subtitle missing connect target: {subtitle}"
        );
        assert!(
            subtitle.contains("shack-pi"),
            "subtitle missing advertised hostname: {subtitle}"
        );
        assert!(
            !subtitle.contains(".local"),
            "subtitle should strip .local suffix: {subtitle}"
        );
        assert!(
            subtitle.contains("R820T"),
            "subtitle missing tuner: {subtitle}"
        );
        assert!(
            subtitle.contains("29 gains"),
            "subtitle missing gain count: {subtitle}"
        );
        assert!(
            subtitle.contains("seen 12s ago"),
            "subtitle missing freshness: {subtitle}"
        );
    }

    #[test]
    fn subtitle_without_ip_omits_duplicate_hostname_segment() {
        // No resolved addresses: connect target falls back to the
        // hostname itself. Showing it twice (once as target, once as
        // hostname segment) would be noise, so the hostname segment
        // is suppressed when it would duplicate the target.
        let server = sample_server(vec![], "shack-pi.local.");
        let subtitle = format_discovery_subtitle(&server, Duration::from_secs(1));
        assert!(
            subtitle.starts_with("shack-pi.local.:1234"),
            "subtitle should use hostname as target: {subtitle}"
        );
        // Exactly two ` • ` separators: target + hardware/freshness.
        assert_eq!(
            subtitle.matches(" • ").count(),
            1,
            "expected one bullet separator when hostname segment is suppressed: {subtitle}"
        );
    }

    #[test]
    fn subtitle_fresh_announce_reads_just_now() {
        // On the initial announce, elapsed is effectively 0 — the
        // subtitle should say "just now" rather than "0s ago".
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let server = sample_server(vec![ip], "radio.local.");
        let subtitle = format_discovery_subtitle(&server, Duration::from_millis(50));
        assert!(
            subtitle.ends_with("seen just now"),
            "subtitle should read 'seen just now' for sub-5s age: {subtitle}"
        );
    }
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

/// Second ControllerBusy toast offering the Listen fallback (AdwToast exposes only one action button).
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

/// Connect button — pins TCP, loads host/port, switches to RTL-TCP.
/// Split out per the 50-NLOC gate (#817).
fn attach_favorite_connect_button(
    row: &adw::ActionRow,
    entry: &sidebar::source_panel::FavoriteEntry,
    ctx: &Rc<FavoriteRowContext>,
) {
    // Connect button — pins TCP, loads host/port, switches to RTL-TCP.
    let connect_btn = gtk4::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
    connect_btn.set_valign(gtk4::Align::Center);
    let connect_ctx = Rc::clone(ctx);
    let connect_key = entry.key.clone();
    let connect_nickname = entry.nickname.clone();
    connect_btn.connect_clicked(move |_| {
        on_favorite_connect_clicked(&connect_ctx, &connect_key, &connect_nickname);
    });
    row.add_suffix(&connect_btn);

    // Copy button — writes `host:port` to the clipboard. Lets
    // the user grab the endpoint for pasting into another tool
    // without having to hand-transcribe the subtitle.
}

/// Click body of the favorite Connect button: parse the key, upgrade
/// the row-context widgets, and run [`apply_rtl_tcp_connect`]. Split
/// out per the 50-NLOC gate (#817).
fn on_favorite_connect_clicked(
    connect_ctx: &Rc<FavoriteRowContext>,
    connect_key: &str,
    connect_nickname: &str,
) {
    let Some((host, port)) = parse_host_port(&connect_key) else {
        // Corrupt key shouldn't happen in practice —
        // `favorite_key(server)` always produces
        // `hostname:port`. Log rather than silently dropping
        // the click, so a future schema drift is discoverable.
        tracing::warn!(
            key = %connect_key,
            "favorites popover: Connect clicked on un-parseable key, ignoring",
        );
        return;
    };
    let (
        Some(hostname_row),
        Some(port_row),
        Some(protocol_row),
        Some(device_row),
        Some(role_row),
        Some(auth_key_row),
    ) = (
        connect_ctx.hostname_row.upgrade(),
        connect_ctx.port_row.upgrade(),
        connect_ctx.protocol_row.upgrade(),
        connect_ctx.device_row.upgrade(),
        connect_ctx.role_row.upgrade(),
        connect_ctx.auth_key_row.upgrade(),
    )
    else {
        return;
    };
    // Shared ordering-sensitive flow lives in
    // `apply_rtl_tcp_connect`. The popover-specific follow-up
    // (popdown) happens after this returns.
    apply_rtl_tcp_connect(
        &host,
        port,
        &connect_nickname,
        &hostname_row,
        &port_row,
        &protocol_row,
        &device_row,
        &role_row,
        &auth_key_row,
        &connect_ctx.state,
        &connect_ctx.config,
    );
    // Dismiss the popover once the connection is dispatched
    // so the user sees the source row update underneath.
    if let Some(popover) = connect_ctx.popover.popover.upgrade() {
        popover.popdown();
    }
}

/// Copy-address + unstar buttons for a favorite row.
/// Split out per the 50-NLOC gate (#817).
fn attach_favorite_copy_unstar_buttons(
    row: &adw::ActionRow,
    entry: &sidebar::source_panel::FavoriteEntry,
    ctx: &Rc<FavoriteRowContext>,
) {
    let copy_btn = gtk4::Button::from_icon_name("edit-copy-symbolic");
    copy_btn.set_tooltip_text(Some("Copy host:port"));
    copy_btn.add_css_class("flat");
    copy_btn.set_valign(gtk4::Align::Center);
    // Icon-only button — give it an explicit accessible name so
    // screen readers don't fall back to the icon filename.
    copy_btn.update_property(&[gtk4::accessible::Property::Label("Copy server address")]);
    let copy_key = entry.key.clone();
    copy_btn.connect_clicked(move |btn| {
        // `WidgetExt::clipboard` reaches the display clipboard
        // via the button's realized display. If the popover has
        // been torn down the button isn't reachable anyway, so
        // we just use the button itself as the anchor widget.
        btn.clipboard().set_text(&copy_key);
    });
    row.add_suffix(&copy_btn);

    // Unstar button — removes from the favorites map, persists,
    // and rebuilds both the discovery expander (so the row moves
    // out of the pinned section) and the popover list (so the
    // row disappears from here).
    attach_favorite_unstar_button(row, entry, ctx);
}

/// Sweep still-live toasts from a prior ControllerBusy entry so the overlay does not stack pairs.
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

/// Unstar button for a favorite row.
/// Split out per the 50-NLOC gate (#817).
fn attach_favorite_unstar_button(
    row: &adw::ActionRow,
    entry: &sidebar::source_panel::FavoriteEntry,
    ctx: &Rc<FavoriteRowContext>,
) {
    let unstar_btn = gtk4::Button::from_icon_name("starred-symbolic");
    unstar_btn.set_tooltip_text(Some("Remove from favorites"));
    unstar_btn.add_css_class("flat");
    unstar_btn.set_valign(gtk4::Align::Center);
    // Icon-only button — matches the tooltip here but stays as
    // a distinct property so screen readers announce it even
    // when tooltips are disabled / long-hover wouldn't fire.
    unstar_btn.update_property(&[gtk4::accessible::Property::Label("Remove from favorites")]);
    let unstar_key = entry.key.clone();
    let unstar_ctx = Rc::clone(ctx);
    unstar_btn.connect_clicked(move |_| {
        {
            let mut favs = unstar_ctx.favorites.borrow_mut();
            if favs.remove(&unstar_key).is_none() {
                // Already gone (e.g., double-click race). Nothing
                // to persist and nothing to rebuild.
                return;
            }
            let snapshot: Vec<sidebar::source_panel::FavoriteEntry> =
                favs.values().cloned().collect();
            crate::sidebar::source_panel::save_favorites(&unstar_ctx.config, &snapshot);
        }

        // If the discovery row for this key is currently rendered,
        // flip its star toggle to the unpinned state. The
        // toggle's own `connect_toggled` handler then does the
        // map cleanup (no-op — we already removed), the persist
        // (redundant but idempotent), the discovery reorder, and
        // the popover rebuild — so we early-return and skip OUR
        // reorder / rebuild below.
        //
        // Without this, the filled star would keep rendering
        // until the next mDNS beacon, which isn't just
        // cosmetic: the first user click on the stale filled
        // star fires `toggled` with `active=false` (the intent
        // was "re-pin"), silently wasting a click.
        if let Some(star_map) = unstar_ctx.discovered_star_buttons.upgrade() {
            let maybe_btn = star_map
                .borrow()
                .get(&unstar_key)
                .and_then(glib::WeakRef::upgrade);
            if let Some(btn) = maybe_btn
                && btn.is_active()
            {
                btn.set_active(false);
                return;
            }
        }

        // No discovery row visible for this key — do the reorder
        // and popover rebuild ourselves.
        //
        // `displayed_rows` is Weak on the context — upgrade fails
        // if the discovery timer has been torn down, which also
        // means there's nothing left to reorder.
        if let (Some(expander), Some(rows)) = (
            unstar_ctx.expander_weak.upgrade(),
            unstar_ctx.displayed_rows.upgrade(),
        ) {
            reorder_discovered_rows(&expander, &rows.borrow(), &unstar_ctx.favorites.borrow());
        }
        // Rebuild the popover so the unstarred row disappears.
        // GTK signal-lifetime guarantees we can `ListBox::remove`
        // our own row from inside this button-clicked handler:
        // GTK retains the signal's source widget for the
        // callback's duration, so the button won't drop under us.
        rebuild_favorites_popover(&unstar_ctx, &unstar_ctx.favorites.borrow());
    });
    row.add_suffix(&unstar_btn);
}

/// Connect button on a discovered row: hydrate the endpoint triple and run apply_rtl_tcp_connect.
/// Split out per the 50-NLOC gate (#817).
fn wire_discovered_connect_button(
    row: &adw::ActionRow,
    server: &DiscoveredServer,
    host: &str,
    deps: &Rc<DiscoveredRowDeps>,
) {
    let DiscoveredRowDeps {
        hostname_row,
        port_row,
        protocol_row,
        device_row,
        role_row,
        auth_key_row,
        state,
        config: config_for_discovery,
        ..
    } = deps.as_ref();

    let connect_btn = gtk4::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
    connect_btn.set_valign(gtk4::Align::Center);

    let click_host = host.to_string();
    let click_port = server.port;
    let hr = hostname_row.clone();
    let pr = port_row.clone();
    let protor = protocol_row.clone();
    let dr = device_row.clone();
    let rr = role_row.clone();
    let akr = auth_key_row.clone();
    let st = Rc::clone(&state);
    let cfg = std::sync::Arc::clone(&config_for_discovery);
    // Friendly nickname for the persisted snapshot.
    // Prefer the TXT nickname if the responder set
    // one, fall back to the DNS-SD instance name.
    let click_nickname = if server.txt.nickname.is_empty() {
        server.instance_name.clone()
    } else {
        server.txt.nickname.clone()
    };
    connect_btn.connect_clicked(move |_| {
        // Shared ordering-sensitive flow lives in
        // `apply_rtl_tcp_connect` — see its doc for
        // why `protocol_row` gets set to TCP before
        // the host/port writes and why
        // `SetSourceType` only fires conditionally.
        apply_rtl_tcp_connect(
            &click_host,
            click_port,
            &click_nickname,
            &hr,
            &pr,
            &protor,
            &dr,
            &rr,
            &akr,
            &st,
            &cfg,
        );
    });
    row.add_suffix(&connect_btn);
    // If this server is already favorited, refresh
    // the persisted metadata (tuner name, gain
    // count, nickname, last-seen) off the fresh
    // announce. Keeps the favorites slide-out's
    // display honest when the user revisits it
    // after the server has been renamed /
    // re-announced with updated TXT records.
}

/// Star toggle for a discovered row: initial pin state, weak-map registration, and the toggle wiring.
/// Split out per the 50-NLOC gate (#817).
fn build_discovery_star_button(
    row: &adw::ActionRow,
    server: &DiscoveredServer,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
    deps: &Rc<DiscoveredRowDeps>,
) {
    let DiscoveredRowDeps {
        config: config_for_discovery,
        favorite_row_ctx,
        discovered_star_buttons,
        expander_weak,
        ..
    } = deps.as_ref();

    let star_btn = gtk4::ToggleButton::builder()
        .icon_name(FAVORITE_ICON_OUTLINE)
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .tooltip_text("Pin as favorite")
        .build();
    // Use the stable hostname+port key, not
    // `instance_name`. `instance_name` comes from
    // the server's TXT nickname, which the operator
    // can edit — keying favorites off it would
    // silently drop the star on any rename.
    let star_key = favorite_key(&server);
    let starred_initially = favorites.borrow().contains_key(&star_key);
    star_btn.set_active(starred_initially);
    if starred_initially {
        star_btn.set_icon_name(FAVORITE_ICON_FILLED);
    }
    // Initial accessible name — state-dependent so
    // screen readers announce the action the click
    // will take, not the icon's current appearance.
    // Updated again inside the toggle closure when
    // the user flips the state.
    set_favorite_toggle_accessible_name(&star_btn, starred_initially);
    // Register the star_btn against its
    // favorite_key so the favorites-popover
    // Unstar handler can find and flip this
    // exact toggle when the user unstars from
    // the popover. `insert` overwrites any
    // prior (stale) weak ref under the same key
    // — e.g. from a re-announce rebuild of the
    // row, where the old button was dropped.
    let star_key_for_map = favorite_key(&server);
    discovered_star_buttons
        .borrow_mut()
        .insert(star_key_for_map, star_btn.downgrade());
    // Capture the display metadata into move-able
    // values so the toggle closure can build a
    // `FavoriteEntry` without holding onto
    // `server` (which is consumed by the HashMap
    // insert further down).
    wire_discovery_star_toggle(&star_btn, server, favorites, deps);
    row.add_prefix(&star_btn);
}

/// Capture set + toggle wiring for a discovered row's star button.
/// Split out per the 50-NLOC gate (#817).
fn wire_discovery_star_toggle(
    star_btn: &gtk4::ToggleButton,
    server: &DiscoveredServer,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
    deps: &Rc<DiscoveredRowDeps>,
) {
    let DiscoveredRowDeps {
        config: config_for_discovery,
        favorite_row_ctx,
        expander_weak,
        ..
    } = deps.as_ref();
    let star_key = favorite_key(server);
    let star_nickname = if server.txt.nickname.is_empty() {
        server.instance_name.clone()
    } else {
        server.txt.nickname.clone()
    };
    let star_tuner_name = Some(server.txt.tuner.clone());
    let star_gain_count = Some(server.txt.gains);
    // Capture the announce-derived auth flag so
    // a fresh star persists it alongside the
    // rest of the metadata. Pre-`CodeRabbit`
    // round 6 on PR #408 this was hard-set to
    // `None` at star time, which meant a newly-
    // starred auth-required server looked
    // "unknown" until the next mDNS refresh —
    // `apply_rtl_tcp_connect` + the startup
    // restore wouldn't reveal the key row
    // ahead of the first `AuthRequired` bounce.
    // The discovery-refresh path below already
    // writes `server.txt.auth_required` on re-
    // announce; this keeps the two entry points
    // consistent so freshly-starred favorites
    // carry the same hint as refreshed ones.
    let star_auth_required = server.txt.auth_required;
    let star_favorites = Rc::clone(&favorites);
    let star_config = std::sync::Arc::clone(&config_for_discovery);
    let star_expander_weak = expander_weak.clone();
    // Closure captures `star_row_ctx` only — reaches
    // `displayed_rows` via its `Weak` field inside.
    // A separate `Rc::clone(&displayed_rows)` capture
    // here would reintroduce the retain cycle the
    // `FavoriteRowContext.displayed_rows` docstring
    // describes (map → row → signal → ctx → map).
    let star_row_ctx = Rc::clone(&favorite_row_ctx);
    star_btn.connect_toggled(move |btn| {
        on_discovery_star_toggled(
            btn,
            &star_key,
            &star_nickname,
            &star_tuner_name,
            star_gain_count,
            star_auth_required,
            &star_favorites,
            &star_config,
            &star_expander_weak,
            &star_row_ctx,
        );
    });
}

/// Metadata refresh for an already-favorited server off the fresh mDNS announce (#396).
/// Split out per the 50-NLOC gate (#817).
fn refresh_favorite_metadata(
    server: &DiscoveredServer,
    favorites: &Rc<
        RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>,
    >,
    deps: &Rc<DiscoveredRowDeps>,
) {
    let DiscoveredRowDeps {
        config: config_for_discovery,
        favorite_row_ctx,
        ..
    } = deps.as_ref();

    let fav_key = favorite_key(&server);
    {
        let mut favs = favorites.borrow_mut();
        if favs.contains_key(&fav_key) {
            let refreshed_nickname = if server.txt.nickname.is_empty() {
                server.instance_name.clone()
            } else {
                server.txt.nickname.clone()
            };
            // Preserve any saved `requested_role`
            // from the previous favorites entry (the
            // user's last pick sticks across
            // re-announces); refresh the
            // `auth_required` hint from the incoming
            // TXT so the UI reveals the key field
            // BEFORE the user clicks Connect. Per #396.
            let preserved_role = favs.get(&fav_key).and_then(|f| f.requested_role);
            favs.insert(
                fav_key.clone(),
                sidebar::source_panel::FavoriteEntry {
                    key: fav_key.clone(),
                    nickname: refreshed_nickname,
                    tuner_name: Some(server.txt.tuner.clone()),
                    gain_count: Some(server.txt.gains),
                    last_seen_unix: Some(sidebar::source_panel::now_unix_seconds()),
                    requested_role: preserved_role,
                    auth_required: server.txt.auth_required,
                },
            );
            let snapshot: Vec<sidebar::source_panel::FavoriteEntry> =
                favs.values().cloned().collect();
            crate::sidebar::source_panel::save_favorites(&config_for_discovery, &snapshot);
            // Refresh the header-bar popover's
            // rendering of this entry (age + tuner
            // metadata). Cheap — it rebuilds the
            // whole list but at favorites scale
            // that's trivial.
            rebuild_favorites_popover(&favorite_row_ctx, &favs);
        }
    }
}
