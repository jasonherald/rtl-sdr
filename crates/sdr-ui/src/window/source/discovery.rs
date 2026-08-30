//! mDNS discovery wiring: the top-level [`connect_rtl_tcp_discovery`]
//! orchestrator and the per-row dependency bundle it assembles.
//! Split out of `window/source.rs` per the Codacy large-file gate
//! (#846). The mDNS browser + poll-timer lifecycle lives in
//! `discovery/browser.rs`; discovered-row rendering (star toggle,
//! Connect button, stale-row pruning, subtitle formatting) lives in
//! `discovery/row_render.rs`.

use gtk4::prelude::*;

use super::super::{AppState, FavoritesHeaderHandle, Rc, RefCell, SidebarPanels, adw, glib};
use super::connect::restore_last_connected_endpoint;
use super::favorites::{
    FavoriteRowContext, FavoritesPopoverWeak, wire_favorites_popover_refresh,
    wire_manage_favorites_button,
};
use super::{DisplayedRowsMap, FavoritesMap, StarButtonsMap};

mod browser;
mod row_render;

use browser::{arm_discovery_poller, start_discovery_browser};
pub(super) use row_render::reorder_discovered_rows;

/// Widget/state dependencies the discovery poller needs to build a
/// discovered-server row. Bundled so the `ServerAnnounced` arm can
/// live in its own function (#817).
pub(super) struct DiscoveredRowDeps {
    pub(super) hostname_row: adw::EntryRow,
    pub(super) port_row: adw::SpinRow,
    pub(super) protocol_row: adw::ComboRow,
    pub(super) device_row: adw::ComboRow,
    pub(super) role_row: adw::ComboRow,
    pub(super) auth_key_row: adw::PasswordEntryRow,
    pub(super) state: Rc<AppState>,
    pub(super) config: std::sync::Arc<sdr_config::ConfigManager>,
    pub(super) favorite_row_ctx: Rc<FavoriteRowContext>,
    pub(super) discovered_star_buttons: StarButtonsMap,
    pub(super) expander_weak: glib::WeakRef<adw::ExpanderRow>,
}

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
