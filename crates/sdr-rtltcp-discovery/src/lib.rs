#![allow(
    // Service / event names duplicate across docs and code; no gain
    // from backtick-wrapping every mention.
    clippy::doc_markdown,
    // `AdvertiseOptions` is moved-in deliberately; Browser & Advertiser
    // both have owned state paths that don't consume the input fully.
    clippy::needless_pass_by_value,
    // Folding `ServiceEvent::SearchStarted | SearchStopped | ServiceFound => None`
    // with the `_ => None` catch-all loses the explicit variant list we
    // want future-readers to see.
    clippy::match_same_arms
)]
//! mDNS/DNS-SD discovery for `rtl_tcp`-compatible servers.
//!
//! Provides:
//! - [`Advertiser`] — used by [`sdr-server-rtltcp`][server] to announce
//!   a running server on the local network
//! - [`Browser`] — used by [`sdr-source-network`]'s rtl_tcp client to
//!   find servers without manually typing host:port
//!
//! Service type: `_rtl_tcp._tcp.local.` This is not an IANA-registered
//! type — the SDR ecosystem uses it by convention (ShinySDR,
//! rtl_tcp_client, etc.). Picking the same string means interop with
//! those tools where they implement discovery.
//!
//! [server]: https://docs.rs/sdr-server-rtltcp
//!
//! ## Why a separate crate
//!
//! Keeps [`sdr-server-rtltcp`][server] free of the `mdns-sd` dependency
//! tree — a CLI user who wants `sdr-rtl-tcp` without LAN advertising
//! can build without it. The UI composes both crates when it wants
//! discovery.
//!
//! ## Pure-Rust stack
//!
//! Uses `mdns-sd` — no Avahi / Bonjour system dependency, no async
//! runtime. The daemon runs on its own thread internally; the
//! [`Browser`] spawns a second thread that translates `mdns-sd`'s
//! event channel into our domain events.

mod advertiser;
mod browser;
mod error;
mod txt;

pub use advertiser::{AdvertiseOptions, Advertiser};
pub use browser::{Browser, DiscoveredServer, DiscoveryEvent};
pub use error::DiscoveryError;
pub use txt::TxtRecord;

/// Fully-qualified mDNS service type used by every rtl_tcp
/// advertisement. This string is load-bearing for interop — any other
/// tool that wants to browse us (or that we want to browse) must use
/// the same literal.
pub const SERVICE_TYPE: &str = "_rtl_tcp._tcp.local.";

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn service_type_matches_dns_sd_shape() {
        // `_service._transport.domain.` — trailing dot means
        // fully-qualified in DNS. This exact string is used for both
        // registration and browse queries; regressing it silently
        // breaks interop.
        assert_eq!(SERVICE_TYPE, "_rtl_tcp._tcp.local.");
        assert!(SERVICE_TYPE.starts_with("_rtl_tcp."));
        assert!(SERVICE_TYPE.contains("._tcp."));
        assert!(SERVICE_TYPE.ends_with("local."));
    }

    /// Live integration test: start an Advertiser on localhost, start
    /// a Browser, and verify we see our own advertisement come back.
    ///
    /// `#[ignore]` because it requires a functioning mDNS multicast
    /// layer (UDP 5353 on 224.0.0.251) — works fine on dev machines
    /// but unreliable in sandboxed CI environments.
    ///
    /// Run manually with `cargo test --ignored mdns_roundtrip`.
    #[test]
    #[ignore = "needs multicast network; run with --ignored locally"]
    fn mdns_roundtrip() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let observed: Arc<Mutex<Vec<DiscoveredServer>>> = Arc::new(Mutex::new(Vec::new()));
        let obs_clone = observed.clone();
        let browser = Browser::start(move |event| {
            if let DiscoveryEvent::ServerAnnounced(s) = event
                && s.instance_name.contains("sdr-rtltcp-integration-test")
            {
                obs_clone.lock().unwrap().push(s);
            }
        })
        .expect("start browser");

        // Advertise a fake server.
        let _advertiser = Advertiser::announce(AdvertiseOptions {
            port: 31234,
            instance_name: "sdr-rtltcp-integration-test".into(),
            hostname: String::new(),
            txt: TxtRecord {
                tuner: "R820T".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                gains: 29,
                nickname: "integration-test-nick".into(),
                txbuf: None,
            },
        })
        .expect("announce");

        // Give mDNS ~5 s to propagate. Typical resolution is 1-2 s on
        // loopback.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !observed.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        browser.stop();

        let seen = observed.lock().unwrap();
        assert!(
            !seen.is_empty(),
            "browser never observed the advertised service"
        );
        let server = &seen[0];
        assert_eq!(server.port, 31234);
        assert_eq!(server.txt.tuner, "R820T");
        assert_eq!(server.txt.nickname, "integration-test-nick");
        assert_eq!(server.txt.gains, 29);
    }
}
