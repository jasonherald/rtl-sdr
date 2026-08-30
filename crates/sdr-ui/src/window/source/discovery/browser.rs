//! mDNS browser lifecycle: `Browser::start`, the event channel,
//! the 200 ms poll timer, and per-tick event draining. Split out
//! of `window/source/discovery.rs` per the Codacy large-file gate
//! (#846).

use libadwaita::prelude::*;

use super::super::super::{Browser, DiscoveryEvent, Rc, SidebarPanels, adw, glib, mpsc};
use super::row_render::{on_server_announced, prune_stale_discovery_rows};
use super::{DiscoveredRowDeps, DisplayedRowsMap, FavoritesMap};

/// Subtitle shown on the discovered-servers expander when mDNS
/// discovery is non-functional (either `Browser::start` failed or the
/// browser thread exited at runtime). Distinguishes "nothing to see
/// yet" from "we gave up listening" — without this the UI would lie by
/// showing the idle "No servers discovered…" state.
const DISCOVERY_UNAVAILABLE_SUBTITLE: &str = "Discovery unavailable on this system.";

/// Arm the 200 ms discovery poll timer (skipped when the mDNS browser
/// failed to start). Split out per the 50-NLOC gate (#817).
pub(super) fn arm_discovery_poller(
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

/// Start the mDNS browser + event channel. Returns `None` for the
/// browser on startup failure (the caller still runs the restore /
/// favorites paths; only the poller is skipped). Split out per the
/// 50-NLOC gate (#817).
pub(super) fn start_discovery_browser(
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
