//! mDNS discovery wiring: the `_rtl_tcp._tcp.local.` browser,
//! discovered-row rendering, star-toggle sync, and stale-row
//! pruning. Split out of `window/source.rs` per the Codacy
//! large-file gate (#846).

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{
    AppState, Browser, DiscoveredServer, DiscoveryEvent, Duration, FavoritesHeaderHandle, Rc,
    RefCell, SidebarPanels, adw, glib, mpsc, sidebar,
};
use super::connect::{RtlTcpConnectRows, apply_rtl_tcp_connect, restore_last_connected_endpoint};
use super::favorites::{
    FAVORITE_ICON_FILLED, FAVORITE_ICON_OUTLINE, FavoriteRowContext, FavoritesPopoverWeak,
    favorite_key, rebuild_favorites_popover, set_favorite_toggle_accessible_name,
    wire_favorites_popover_refresh, wire_manage_favorites_button,
};
use super::{DisplayedRowsMap, FavoritesMap, StarButtonsMap};

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
    discovered_star_buttons: StarButtonsMap,
    expander_weak: glib::WeakRef<adw::ExpanderRow>,
}

/// Subtitle shown on the discovered-servers expander when mDNS
/// discovery is non-functional (either `Browser::start` failed or the
/// browser thread exited at runtime). Distinguishes "nothing to see
/// yet" from "we gave up listening" — without this the UI would lie by
/// showing the idle "No servers discovered…" state.
const DISCOVERY_UNAVAILABLE_SUBTITLE: &str = "Discovery unavailable on this system.";

/// Grace period before a discovered-server row is pruned when the
/// responder stops re-announcing. A healthy mDNS responder
/// re-announces well before its TTL (default 120 s on most daemons)
/// expires; 3 minutes without a refresh means the responder is either
/// dead or network-partitioned. Defense-in-depth: mdns-sd's daemon
/// SHOULD fire `ServiceRemoved` on TTL expiry, but a crashed server
/// that vanishes without a goodbye may leave the cache entry around
/// longer than the client wants — expiring client-side keeps the
/// Connect button from offering a dead endpoint.
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
pub(in crate::window) fn connect_rtl_tcp_discovery(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config: &std::sync::Arc<sdr_config::ConfigManager>,
    favorites_header: &FavoritesHeaderHandle,
    favorites: &FavoritesMap,
) {
    wire_manage_favorites_button(panels, favorites_header);

    let (browser, disc_rx) = start_discovery_browser(panels);

    let (displayed_rows, discovered_star_buttons) = new_discovery_maps();

    // Weak ref on the expander so the timeout closure doesn't keep
    // the window alive after close — upgrade() returns None on a
    // destroyed widget and the poller breaks out.
    let expander_weak = panels.source.rtl_tcp_discovered_row.downgrade();
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
    // Bundle of per-row action dependencies. Built once, cloned
    // into the three rebuild call sites (startup seed, star
    // toggle, re-announce refresh). `rebuild_favorites_popover`
    // hands a clone to each row's Connect / Copy / Unstar
    // closure, so each button ends up with a single `Rc` clone
    // instead of nine weak-ref captures.
    let favorite_row_ctx = build_favorite_row_ctx(
        &FavoritesPopoverWeak::from_header(favorites_header),
        &favorites,
        &config_for_discovery,
        state,
        panels,
        &expander_weak,
        &displayed_rows,
        &discovered_star_buttons,
    );
    // Seed the popover's content from the restored favorites so
    // the list is ready when the user first clicks the header
    // star, without waiting for a mutation to trigger a rebuild.
    wire_favorites_popover_refresh(favorites_header, &favorite_row_ctx);

    restore_last_connected_endpoint(
        state,
        &config_for_discovery,
        &panels.source.hostname_row,
        &panels.source.port_row,
        &panels.source.protocol_row,
    );

    let row_deps = build_row_deps(
        panels,
        state,
        &config_for_discovery,
        &favorite_row_ctx,
        &discovered_star_buttons,
        &expander_weak,
    );
    arm_discovery_poller(
        browser,
        disc_rx,
        &displayed_rows,
        &favorites,
        &row_deps,
        &expander_weak,
    );
}

/// Fresh (rows, star-buttons) map pair for the discovery poller. The
/// row map is keyed by full DNS-SD instance name (stable across
/// nickname changes) and carries the last `DiscoveredServer` payload
/// for staleness pruning; the auxiliary map lets the favorites-popover
/// Unstar handler find and flip the matching in-list star toggle.
/// Weak refs only — the `ToggleButton`s are strongly owned by their
/// rows. Split out per the 50-NLOC gate (#817).
#[allow(clippy::type_complexity)]
fn new_discovery_maps() -> (DisplayedRowsMap, StarButtonsMap) {
    (
        Rc::new(RefCell::new(std::collections::HashMap::new())),
        Rc::new(RefCell::new(std::collections::HashMap::new())),
    )
}

/// Assemble the discovery-row dependency bundle. Split out per the
/// 50-NLOC gate (#817).
fn build_row_deps(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    config_for_discovery: &std::sync::Arc<sdr_config::ConfigManager>,
    favorite_row_ctx: &Rc<FavoriteRowContext>,
    discovered_star_buttons: &StarButtonsMap,
    expander_weak: &glib::WeakRef<adw::ExpanderRow>,
) -> Rc<DiscoveredRowDeps> {
    Rc::new(DiscoveredRowDeps {
        hostname_row: panels.source.hostname_row.clone(),
        port_row: panels.source.port_row.clone(),
        protocol_row: panels.source.protocol_row.clone(),
        device_row: panels.source.device_row.clone(),
        role_row: panels.source.rtl_tcp_role_row.clone(),
        auth_key_row: panels.source.rtl_tcp_auth_key_row.clone(),
        state: Rc::clone(state),
        config: std::sync::Arc::clone(config_for_discovery),
        favorite_row_ctx: Rc::clone(favorite_row_ctx),
        discovered_star_buttons: Rc::clone(discovered_star_buttons),
        expander_weak: expander_weak.clone(),
    })
}

/// Arm the 200 ms discovery poll timer (skipped when the mDNS browser
/// failed to start). Split out per the 50-NLOC gate (#817).
fn arm_discovery_poller(
    browser: Option<Browser>,
    disc_rx: mpsc::Receiver<DiscoveryEvent>,
    displayed_rows: &DisplayedRowsMap,
    favorites: &FavoritesMap,
    row_deps: &Rc<DiscoveredRowDeps>,
    expander_weak: &glib::WeakRef<adw::ExpanderRow>,
) {
    const DISCOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
    let displayed_rows = Rc::clone(displayed_rows);
    let favorites = Rc::clone(favorites);
    let row_deps = Rc::clone(row_deps);
    let expander_weak = expander_weak.clone();
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

        drain_discovery_events(&disc_rx, &displayed_rows, &expander, &favorites, &row_deps)
    });
}

/// Build the per-row action context shared by the discovery rows and
/// the favorites popover. Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn build_favorite_row_ctx(
    favorites_popover_weak: &FavoritesPopoverWeak,
    favorites: &FavoritesMap,
    config_for_discovery: &std::sync::Arc<sdr_config::ConfigManager>,
    state: &Rc<AppState>,
    panels: &SidebarPanels,
    expander_weak: &glib::WeakRef<adw::ExpanderRow>,
    displayed_rows: &DisplayedRowsMap,
    discovered_star_buttons: &StarButtonsMap,
) -> Rc<FavoriteRowContext> {
    Rc::new(FavoriteRowContext {
        popover: favorites_popover_weak.clone(),
        favorites: Rc::clone(favorites),
        config: std::sync::Arc::clone(config_for_discovery),
        state: Rc::clone(state),
        hostname_row: panels.source.hostname_row.downgrade(),
        port_row: panels.source.port_row.downgrade(),
        protocol_row: panels.source.protocol_row.downgrade(),
        device_row: panels.source.device_row.downgrade(),
        role_row: panels.source.rtl_tcp_role_row.downgrade(),
        auth_key_row: panels.source.rtl_tcp_auth_key_row.downgrade(),
        expander_weak: expander_weak.clone(),
        // Weak refs — see `FavoriteRowContext.displayed_rows`
        // docstring for the retain-cycle reasoning.
        displayed_rows: Rc::downgrade(displayed_rows),
        discovered_star_buttons: Rc::downgrade(discovered_star_buttons),
    })
}

/// Start the mDNS browser + event channel. Returns `None` for the
/// browser on startup failure (the caller still runs the restore /
/// favorites paths; only the poller is skipped). Split out per the
/// 50-NLOC gate (#817).
fn start_discovery_browser(
    panels: &SidebarPanels,
) -> (Option<Browser>, mpsc::Receiver<DiscoveryEvent>) {
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

    (browser, disc_rx)
}

/// Drain the mDNS discovery channel for one poll tick. Returns
/// `Break` when the browser thread has exited (rows drained and the
/// degraded-state subtitle set). Split out per the 50-NLOC gate
/// (#817).
fn drain_discovery_events(
    disc_rx: &mpsc::Receiver<DiscoveryEvent>,
    displayed_rows: &DisplayedRowsMap,
    expander: &adw::ExpanderRow,
    favorites: &FavoritesMap,
    row_deps: &Rc<DiscoveredRowDeps>,
) -> glib::ControlFlow {
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
                tracing::warn!("mDNS discovery channel disconnected — stopping discovery poller");
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
                on_server_announced(server, displayed_rows, expander, favorites, row_deps);
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
}

/// `ServerAnnounced` arm of the discovery poller: build or refresh the
/// row for this instance, wire its Connect/copy/star actions, and keep
/// favorites-first ordering. Split out per the 50-NLOC gate (#817).
fn on_server_announced(
    server: DiscoveredServer,
    displayed_rows: &DisplayedRowsMap,
    expander: &adw::ExpanderRow,
    favorites: &FavoritesMap,
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
    reorder_discovered_rows(expander, &rows, &favorites.borrow());

    expander.set_subtitle(&format!("{} server(s) visible", rows.len()));
}

/// Announce-derived metadata of one discovered server, captured at
/// star-button wiring time so a fresh star persists nickname/tuner/
/// gain/auth alongside the key.
struct StarredServerInfo {
    key: String,
    nickname: String,
    tuner_name: Option<String>,
    gain_count: Option<u32>,
    auth_required: Option<bool>,
}

impl StarredServerInfo {
    /// Fresh favorite record for this server, stamped with the
    /// current time as `last_seen`.
    fn to_favorite_entry(&self) -> sidebar::source_panel::FavoriteEntry {
        sidebar::source_panel::FavoriteEntry {
            key: self.key.clone(),
            nickname: self.nickname.clone(),
            tuner_name: self.tuner_name.clone(),
            gain_count: self.gain_count,
            last_seen_unix: Some(sidebar::source_panel::now_unix_seconds()),
            requested_role: None,
            auth_required: self.auth_required,
        }
    }
}

/// Toggle body of a discovered-server star button: flip the icon +
/// accessible name, insert/remove + persist the favorite, and refresh
/// row order + the header popover. Split out per the 50-NLOC gate
/// (#817).
#[allow(clippy::too_many_arguments)]
fn on_discovery_star_toggled(
    btn: &gtk4::ToggleButton,
    info: &StarredServerInfo,
    star_favorites: &FavoritesMap,
    star_config: &std::sync::Arc<sdr_config::ConfigManager>,
    star_expander_weak: &glib::WeakRef<adw::ExpanderRow>,
    star_row_ctx: &Rc<FavoriteRowContext>,
) {
    let star_key = info.key.as_str();
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
            // `to_favorite_entry` stamps the current time and carries
            // the announce-derived auth hint — see its doc and CR
            // round 6 on PR #408 / issue #396.
            favs.insert(star_key.to_string(), info.to_favorite_entry());
        } else {
            favs.remove(star_key);
        }
        // Persist immediately. Order within
        // the persisted list is unspecified —
        // the slide-out sorts on read.
        let snapshot: Vec<sidebar::source_panel::FavoriteEntry> = favs.values().cloned().collect();
        crate::sidebar::source_panel::save_favorites(star_config, &snapshot);
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
    rebuild_favorites_popover(star_row_ctx, &star_favorites.borrow());
}

/// Per-tick stale-row prune + "seen N ago" subtitle refresh for the
/// discovery expander (3-minute grace; healthy responders re-announce
/// well within it). Split out per the 50-NLOC gate (#817).
fn prune_stale_discovery_rows(displayed_rows: &DisplayedRowsMap, expander: &adw::ExpanderRow) {
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

/// Connect button on a discovered row: hydrate the endpoint triple and run `apply_rtl_tcp_connect`.
/// Split out per the 50-NLOC gate (#817).
fn wire_discovered_connect_button(
    row: &adw::ActionRow,
    server: &DiscoveredServer,
    host: &str,
    deps: &Rc<DiscoveredRowDeps>,
) {
    let connect_btn = gtk4::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
    connect_btn.set_valign(gtk4::Align::Center);

    let click_host = host.to_string();
    let click_port = server.port;
    // Friendly nickname for the persisted snapshot.
    // Prefer the TXT nickname if the responder set
    // one, fall back to the DNS-SD instance name.
    let click_nickname = if server.txt.nickname.is_empty() {
        server.instance_name.clone()
    } else {
        server.txt.nickname.clone()
    };
    let deps_c = Rc::clone(deps);
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
            &RtlTcpConnectRows {
                hostname_row: &deps_c.hostname_row,
                port_row: &deps_c.port_row,
                protocol_row: &deps_c.protocol_row,
                device_row: &deps_c.device_row,
                role_row: &deps_c.role_row,
                auth_key_row: &deps_c.auth_key_row,
            },
            &deps_c.state,
            &deps_c.config,
        );
    });
    row.add_suffix(&connect_btn);
}

/// Star toggle for a discovered row: initial pin state, weak-map registration, and the toggle wiring.
/// Split out per the 50-NLOC gate (#817).
fn build_discovery_star_button(
    row: &adw::ActionRow,
    server: &DiscoveredServer,
    favorites: &FavoritesMap,
    deps: &Rc<DiscoveredRowDeps>,
) {
    let DiscoveredRowDeps {
        discovered_star_buttons,
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
    let star_key = favorite_key(server);
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
    let star_key_for_map = favorite_key(server);
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
    favorites: &FavoritesMap,
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
    let star_favorites = Rc::clone(favorites);
    let star_config = std::sync::Arc::clone(config_for_discovery);
    let star_expander_weak = expander_weak.clone();
    // Closure captures `star_row_ctx` only — reaches
    // `displayed_rows` via its `Weak` field inside.
    // A separate `Rc::clone(&displayed_rows)` capture
    // here would reintroduce the retain cycle the
    // `FavoriteRowContext.displayed_rows` docstring
    // describes (map → row → signal → ctx → map).
    let star_row_ctx = Rc::clone(favorite_row_ctx);
    let star_info = StarredServerInfo {
        key: star_key,
        nickname: star_nickname,
        tuner_name: star_tuner_name,
        gain_count: star_gain_count,
        auth_required: star_auth_required,
    };
    star_btn.connect_toggled(move |btn| {
        on_discovery_star_toggled(
            btn,
            &star_info,
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
    favorites: &FavoritesMap,
    deps: &Rc<DiscoveredRowDeps>,
) {
    let DiscoveredRowDeps {
        config: config_for_discovery,
        favorite_row_ctx,
        ..
    } = deps.as_ref();

    let fav_key = favorite_key(server);
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
            crate::sidebar::source_panel::save_favorites(config_for_discovery, &snapshot);
            // Refresh the header-bar popover's
            // rendering of this entry (age + tuner
            // metadata). Cheap — it rebuilds the
            // whole list but at favorites scale
            // that's trivial.
            rebuild_favorites_popover(favorite_row_ctx, &favs);
        }
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
