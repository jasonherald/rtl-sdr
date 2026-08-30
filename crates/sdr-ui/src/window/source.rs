//! Source panel wiring: device controls, `rtl_tcp` client discovery,
//! favorites, and client-side auth-key keyring plumbing.

/// Shared favorites map: stable `hostname:port` key -> rich
/// [`sidebar::source_panel::FavoriteEntry`]. Single instance owned by
/// `connect_sidebar_panels`, mutated by the role picker and the
/// discovery re-announce path.
pub(super) type FavoritesMap =
    Rc<RefCell<std::collections::HashMap<String, sidebar::source_panel::FavoriteEntry>>>;

/// Live discovered-server rows keyed by server key, plus the announce
/// snapshot each row was rendered from.
pub(super) type DisplayedRowsMap =
    Rc<RefCell<std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>>>;

/// Weak handles to each discovered row's star `ToggleButton`, keyed by
/// server key, for unstar-refresh sync.
pub(super) type StarButtonsMap =
    Rc<RefCell<std::collections::HashMap<String, glib::WeakRef<gtk4::ToggleButton>>>>;

use super::{AppState, DiscoveredServer, Rc, RefCell, SidebarPanels, adw, glib, sidebar};

mod connect;
mod discovery;
mod favorites;
mod rows;

pub(in crate::window) use connect::{apply_rtl_tcp_connection_state, handle_rtl_tcp_state_toast};
pub(in crate::window) use discovery::connect_rtl_tcp_discovery;
pub(in crate::window) use rows::connect_source_rtlsdr_probe;

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
    favorites: &FavoritesMap,
) {
    rows::wire_sample_rate_and_device_rows(panels, state, config);

    rows::wire_rtl_frontend_toggles(panels, state, toast_overlay, config);

    rows::wire_gain_and_agc_rows(panels, state, config);
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

    rows::wire_rtl_tcp_client_rows(panels, state, config, &last_good_auth_key);

    rows::wire_iq_ppm_and_restart_rows(panels, state, config);

    rows::wire_source_type_guard(panels, state, toast_overlay, server_running, config);

    rows::wire_airspy_device_row(panels, state, config);
    rows::wire_network_source_rows(panels, state, config);

    rows::wire_role_and_server_key_rows(panels, state, config, favorites, &last_good_auth_key);
}
