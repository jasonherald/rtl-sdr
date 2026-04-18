//! Browser: watch the LAN for `_rtl_tcp._tcp.local.` advertisements.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::SERVICE_TYPE;
use crate::error::DiscoveryError;
use crate::txt::TxtRecord;

/// A server visible on the local network.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    /// Full DNS-SD instance name, e.g.
    /// `jason-desk rtl-sdr._rtl_tcp._tcp.local.`. Use this as the
    /// stable identifier for a discovered entry — the nickname in
    /// `txt` is user-editable and may change.
    pub instance_name: String,

    /// mDNS hostname the service registered with (e.g. `jason-desk.local.`).
    pub hostname: String,

    /// Port the rtl_tcp server is listening on.
    pub port: u16,

    /// Resolved addresses. IPv4 comes first when both are present so
    /// clients that prefer v4 (the rtl_tcp protocol itself is
    /// address-family-agnostic, but some embedded clients only do v4)
    /// have a predictable default.
    pub addresses: Vec<IpAddr>,

    /// TXT record payload (tuner / version / gains / nickname / txbuf).
    pub txt: TxtRecord,

    /// When we last saw a `ServiceResolved` for this entry. Lets the
    /// UI show "last seen 5 s ago" style freshness indicators and
    /// garbage-collect servers that have gone silent but haven't
    /// produced a `ServiceRemoved` yet.
    pub last_seen: Instant,
}

/// Events the browser emits to its callback.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A server appeared or an existing one re-resolved with fresh
    /// metadata. Re-resolution fires as TTLs refresh, typically every
    /// 60-120 s depending on the responder.
    ServerAnnounced(DiscoveredServer),

    /// A server explicitly withdrew (its responder sent a goodbye
    /// packet). Not every disappearance produces this event —
    /// silently-dying servers just stop re-announcing.
    ServerWithdrawn {
        /// Full DNS-SD instance name, same shape as
        /// `DiscoveredServer::instance_name`.
        instance_name: String,
    },
}

/// Live browser. Drops → stops listening and shuts the daemon down.
pub struct Browser {
    daemon: ServiceDaemon,
    shutdown: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
}

impl Browser {
    /// Start browsing for `_rtl_tcp._tcp.local.` services. The callback
    /// runs on a dedicated background thread — it's invoked for every
    /// new / refreshed / withdrawn service.
    ///
    /// Callback errors are not propagated. If your callback can fail,
    /// handle it inline (log, send to a channel, etc.).
    pub fn start<F>(mut on_event: F) -> Result<Self, DiscoveryError>
    where
        F: FnMut(DiscoveryEvent) + Send + 'static,
    {
        let daemon = ServiceDaemon::new()?;
        let receiver = daemon.browse(SERVICE_TYPE)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let listener_shutdown = shutdown.clone();

        let listener = thread::Builder::new()
            .name("rtl_tcp-discovery-browser".into())
            .spawn(move || {
                // Poll with a short timeout so we notice `shutdown`
                // flips within ~100 ms of Browser::stop / Drop.
                //
                // `mdns-sd` re-exports `flume::Receiver`; rather than
                // name its error type explicitly we distinguish the
                // two error cases (Timeout vs. Disconnected) by asking
                // the receiver whether the channel is still live.
                while !listener_shutdown.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(event) => {
                            if let Some(e) = translate(event) {
                                on_event(e);
                            }
                        }
                        Err(_) if receiver.is_disconnected() => {
                            tracing::debug!("mDNS browser channel closed, exiting listener thread");
                            return;
                        }
                        Err(_) => {}
                    }
                }
            })
            .map_err(DiscoveryError::Hostname)?;

        Ok(Self {
            daemon,
            shutdown,
            listener: Some(listener),
        })
    }

    /// Stop browsing and shut down. Equivalent to dropping the `Browser`.
    pub fn stop(mut self) {
        self.initiate_shutdown();
        if let Some(h) = self.listener.take() {
            let _ = h.join();
        }
    }

    fn initiate_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.initiate_shutdown();
        if let Some(h) = self.listener.take() {
            let _ = h.join();
        }
    }
}

/// Translate an `mdns-sd` event into our domain event.
///
/// Returns `None` for events that don't carry actionable info (search
/// started / stopped) — those fire on daemon lifecycle and we only
/// care about resolution / removal.
fn translate(event: ServiceEvent) -> Option<DiscoveryEvent> {
    match event {
        ServiceEvent::ServiceResolved(resolved) => {
            // `ServiceResolved` carries `Box<ResolvedService>` in
            // mdns-sd 0.19. Extract its address set (HashSet<ScopedIp>)
            // and flatten to plain IpAddr via ScopedIp::to_ip_addr(),
            // dropping the scope metadata. Sort IPv4 first so clients
            // that prefer v4 (or have issues with v6 link-local) pick
            // the first address deterministically.
            let mut addresses: Vec<IpAddr> = resolved
                .get_addresses()
                .iter()
                .map(mdns_sd::ScopedIp::to_ip_addr)
                .collect();
            addresses.sort_by_key(|ip| match ip {
                IpAddr::V4(_) => 0,
                IpAddr::V6(_) => 1,
            });

            // TxtProperties exposes an iterator over (&TxtProperty);
            // hand key/val strings to `TxtRecord::from_properties` so
            // missing / unknown fields fall back to sensible defaults.
            let txt = TxtRecord::from_properties(
                resolved
                    .get_properties()
                    .iter()
                    .map(|p| (p.key().to_string(), p.val_str().to_string())),
            );

            Some(DiscoveryEvent::ServerAnnounced(DiscoveredServer {
                instance_name: resolved.get_fullname().to_string(),
                hostname: resolved.get_hostname().to_string(),
                port: resolved.get_port(),
                addresses,
                txt,
                last_seen: Instant::now(),
            }))
        }
        ServiceEvent::ServiceRemoved(_, name) => Some(DiscoveryEvent::ServerWithdrawn {
            instance_name: name,
        }),
        ServiceEvent::SearchStarted(_)
        | ServiceEvent::SearchStopped(_)
        | ServiceEvent::ServiceFound(_, _) => None,
        // `ServiceEvent` is marked `#[non_exhaustive]` upstream — any
        // future variants are unknown-by-design and we ignore them
        // rather than fail to compile on crate upgrades.
        _ => None,
    }
}
