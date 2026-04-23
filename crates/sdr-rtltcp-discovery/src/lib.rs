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

pub use advertiser::{AdvertiseOptions, Advertiser, local_hostname};
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
    fn local_hostname_returns_bare_non_empty_name_without_local_suffix() {
        // Contract: non-empty, no trailing `.local.` / `.local`.
        // `libc::gethostname` on CI runners and dev machines returns a
        // real name; if the syscall ever failed it'd fall back to
        // "localhost" which still satisfies the contract.
        let host = local_hostname();
        assert!(!host.is_empty(), "local_hostname() returned empty string");
        // clippy::case_sensitive_file_extension_comparisons wants
        // `.rsplit('.').next()` — but this is a hostname-suffix check
        // that is genuinely case-sensitive per DNS labels (though
        // mDNS normalizes case in practice, our local_hostname()
        // contract is byte-exact). Allow the lint locally.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        let ends_bad = host.ends_with(".local.") || host.ends_with(".local");
        assert!(
            !ends_bad,
            "local_hostname() must return bare name, not mDNS-qualified: {host:?}"
        );
        // mDNS DNS-SD instance-name components aren't allowed to
        // contain NUL bytes. gethostname should never produce one, but
        // our UTF-8 trim path should have stripped any interior NUL
        // regardless.
        assert!(!host.contains('\0'));
    }

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

        /// Arbitrary high port for the fake advertisement — outside
        /// the upstream rtl_tcp default (1234) so a stray real server
        /// doesn't alias this test.
        const MDNS_ROUNDTRIP_PORT: u16 = 31_234;
        /// How long to wait for mDNS propagation across the loopback
        /// multicast path. Typical resolution is 1-2 s; 5 s is slack
        /// for slower loopbacks / loaded machines.
        const MDNS_PROPAGATION_TIMEOUT: Duration = Duration::from_secs(5);
        /// Poll cadence while waiting. Short enough that the test
        /// doesn't sleep meaningfully past the actual resolution.
        const MDNS_POLL_INTERVAL: Duration = Duration::from_millis(100);
        /// Expected gain count in the TXT payload — R820T standard
        /// step count.
        const R820T_GAIN_COUNT: u32 = 29;

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
            port: MDNS_ROUNDTRIP_PORT,
            instance_name: "sdr-rtltcp-integration-test".into(),
            hostname: String::new(),
            txt: TxtRecord {
                tuner: "R820T".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                gains: R820T_GAIN_COUNT,
                nickname: "integration-test-nick".into(),
                txbuf: None,
                codecs: None,
                auth_required: None,
            },
        })
        .expect("announce");

        let deadline = Instant::now() + MDNS_PROPAGATION_TIMEOUT;
        while Instant::now() < deadline {
            if !observed.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(MDNS_POLL_INTERVAL);
        }

        browser.stop();

        let seen = observed.lock().unwrap();
        assert!(
            !seen.is_empty(),
            "browser never observed the advertised service"
        );
        let server = &seen[0];
        assert_eq!(server.port, MDNS_ROUNDTRIP_PORT);
        assert_eq!(server.txt.tuner, "R820T");
        assert_eq!(server.txt.nickname, "integration-test-nick");
        assert_eq!(server.txt.gains, R820T_GAIN_COUNT);
    }
}
