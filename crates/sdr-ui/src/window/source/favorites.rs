//! Favorites popover: the header-bar star list, per-row
//! Connect/Copy/Unstar actions, and the favorite-key/subtitle
//! formatting helpers. Split out of `window/source.rs` per the
//! Codacy large-file gate (#846).

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{
    AppState, DiscoveredServer, FavoritesHeaderHandle, Rc, RefCell, SidebarPanels, adw, glib,
    sidebar,
};
use super::FavoritesMap;
use super::connect::{RtlTcpConnectRows, apply_rtl_tcp_connect};
use super::discovery::reorder_discovered_rows;

/// Seed the favorites popover and rebuild it on every show so the
/// "seen Xm ago" subtitles track wall-clock time. Split out per the
/// 50-NLOC gate (#817).
pub(super) fn wire_favorites_popover_refresh(
    favorites_header: &FavoritesHeaderHandle,
    favorite_row_ctx: &Rc<FavoriteRowContext>,
) {
    rebuild_favorites_popover(favorite_row_ctx, &favorite_row_ctx.favorites.borrow());

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
        let ctx_for_show = Rc::clone(favorite_row_ctx);
        favorites_header.popover.connect_show(move |_| {
            rebuild_favorites_popover(&ctx_for_show, &ctx_for_show.favorites.borrow());
        });
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
    pub(super) fn from_header(handle: &FavoritesHeaderHandle) -> Self {
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
    pub(super) popover: FavoritesPopoverWeak,
    pub(super) favorites: FavoritesMap,
    pub(super) config: std::sync::Arc<sdr_config::ConfigManager>,
    pub(super) state: Rc<AppState>,
    pub(super) hostname_row: glib::WeakRef<adw::EntryRow>,
    pub(super) port_row: glib::WeakRef<adw::SpinRow>,
    pub(super) protocol_row: glib::WeakRef<adw::ComboRow>,
    pub(super) device_row: glib::WeakRef<adw::ComboRow>,
    /// Role picker — `apply_rtl_tcp_connect` needs it so the
    /// per-server `requested_role` can be restored before
    /// the new endpoint's first connect dispatch. Per
    /// `CodeRabbit` round 1 on PR #408.
    pub(super) role_row: glib::WeakRef<adw::ComboRow>,
    /// Auth-key row — `apply_rtl_tcp_connect` reveals it
    /// when the favorite advertises `auth_required` and
    /// pre-fills any saved key from the keyring so a
    /// pre-configured auth connect lands in a single
    /// `Connecting → Connected` hop. Per `CodeRabbit` round 1
    /// on PR #408.
    pub(super) auth_key_row: glib::WeakRef<adw::PasswordEntryRow>,
    pub(super) expander_weak: glib::WeakRef<adw::ExpanderRow>,
    pub(super) displayed_rows: std::rc::Weak<
        RefCell<std::collections::HashMap<String, (adw::ActionRow, DiscoveredServer)>>,
    >,
    /// Keyed by `favorite_key(server)` (hostname:port), maps to
    /// a weak ref on the star `ToggleButton` in the currently-
    /// rendered discovery row for that server (if any). Weak
    /// here for the same retain-cycle reason as `displayed_rows`:
    /// the per-row Unstar closure captures this context, and a
    /// strong `Rc` field would close the loop back through the
    /// inner `WeakRef`s to the rows themselves.
    pub(super) discovered_star_buttons: std::rc::Weak<
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
        // `use_markup(false)`: `nickname` and the subtitle's
        // `key` (hostname:port) segment originate from mDNS
        // TXT-record / hostname data captured at star time —
        // attacker-controlled on a hostile LAN. Same treatment
        // as the discovery-row builder in discovery.rs.
        let row = adw::ActionRow::builder()
            .title(&entry.nickname)
            .subtitle(format_favorite_subtitle(entry, now))
            .activatable(false)
            .use_markup(false)
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
}

/// Click body of the favorite Connect button: parse the key, upgrade
/// the row-context widgets, and run [`apply_rtl_tcp_connect`]. Split
/// out per the 50-NLOC gate (#817).
fn on_favorite_connect_clicked(
    connect_ctx: &Rc<FavoriteRowContext>,
    connect_key: &str,
    connect_nickname: &str,
) {
    let Some((host, port)) = parse_host_port(connect_key) else {
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
        connect_nickname,
        &RtlTcpConnectRows {
            hostname_row: &hostname_row,
            port_row: &port_row,
            protocol_row: &protocol_row,
            device_row: &device_row,
            role_row: &role_row,
            auth_key_row: &auth_key_row,
        },
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
    // Copy button — writes `host:port` to the clipboard. Lets
    // the user grab the endpoint for pasting into another tool
    // without having to hand-transcribe the subtitle.
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

/// "Manage favorites…" button — second entry point into the header favorites popover.
/// Split out per the 50-NLOC gate (#817).
pub(super) fn wire_manage_favorites_button(
    panels: &SidebarPanels,
    favorites_header: &FavoritesHeaderHandle,
) {
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
