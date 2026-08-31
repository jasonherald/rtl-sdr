//! `rtl_tcp` server status surface: the 500 ms polling loop and the
//! renderers for status / uptime / data-rate / activity-log /
//! client-list rows. Split out of `window/share.rs` per the Codacy
//! 500-NLOC file gate on PR #844.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::super::{Duration, Instant, Rc, SidebarPanels, adw, glib, sidebar};
use super::ServerSwitchWidgets;
use super::handle::RunningServerHandle;
use std::cell::Cell;

/// Cadence for the server-stats poll that renders the "Server
/// status" rows. 500 ms is fast enough that "connected / waiting"
/// transitions feel instant while keeping the `ServerStats` clone +
/// row-subtitle churn off the critical path.
pub(in crate::window) const SERVER_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Bits-per-byte conversion used in the Mbps formatter. Kept behind
/// a named constant so the arithmetic at the call site reads as
/// unit math ("bytes * `BITS_PER_BYTE` / duration / MEGA") instead
/// of opaque `8`s and `1_000_000`s.
pub(in crate::window) const BITS_PER_BYTE: u64 = 8;

/// Megabits divisor for rendering Mbps. `1_000_000` matches
/// telecom/carrier conventions for transport rates.
pub(in crate::window) const BITS_PER_MEGABIT: f64 = 1_000_000.0;

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
pub(in crate::window) struct ServerStatusWidgetsWeak {
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
pub(in crate::window) struct ServerStatusWidgets {
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
pub(in crate::window) fn connect_server_status_polling(
    panels: &SidebarPanels,
    running: RunningServerHandle,
) {
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
        // Snapshot `(Server::stats(), Server::has_stopped())` via
        // the typed handle — the tight-borrow rationale lives on
        // `RunningServerHandle::poll_stats` (issue #847).
        let Some((stats, stopped)) = running.poll_stats() else {
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
pub(in crate::window) fn render_status_rows(
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
pub(in crate::window) fn pick_most_recent_commander(
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
pub(in crate::window) fn format_uptime(elapsed: Duration) -> String {
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
pub(in crate::window) fn format_data_rate(bytes: u64, interval: Duration) -> String {
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
pub(in crate::window) fn format_commanded_state(
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
pub(in crate::window) fn format_hz(hz: u32) -> String {
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
pub(in crate::window) fn render_activity_log(
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
pub(in crate::window) fn render_clients_list(
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
pub(in crate::window) fn reset_activity_log(panel: &ServerSwitchWidgets) {
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
pub(in crate::window) fn reset_clients_list(panel: &ServerSwitchWidgets) {
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
pub(in crate::window) fn format_log_age(elapsed: Duration) -> String {
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
pub(in crate::window) fn reset_status_rows(panel: &ServerSwitchWidgets) {
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
