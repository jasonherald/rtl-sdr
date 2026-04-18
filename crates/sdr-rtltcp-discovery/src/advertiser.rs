//! Advertiser: publish a single `_rtl_tcp._tcp.local.` registration.

use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::SERVICE_TYPE;
use crate::error::DiscoveryError;
use crate::txt::TxtRecord;

/// Options for [`Advertiser::announce`]. All values except `port` have
/// reasonable defaults derivable from the local environment, but the
/// caller usually has richer metadata (tuner, gain count) already at
/// hand from the server that's being advertised.
#[derive(Debug, Clone)]
pub struct AdvertiseOptions {
    /// TCP port the rtl_tcp server is listening on.
    pub port: u16,

    /// Instance name as it appears in DNS-SD. Usually a combination of
    /// hostname + nickname. Must be unique on the LAN — if two servers
    /// advertise the same name, clients can't distinguish them.
    ///
    /// Example: `"jason-desk rtl-sdr"` or `"shack-pi weather"`.
    pub instance_name: String,

    /// mDNS hostname for A/AAAA lookup. Conventionally ends with
    /// `.local.` (note the trailing dot). Passing an empty string
    /// triggers auto-derivation from the local system hostname.
    pub hostname: String,

    /// TXT record payload — tuner / version / gains / nickname.
    pub txt: TxtRecord,
}

/// Active advertisement. Drops → unregisters.
///
/// The underlying `ServiceDaemon` is kept alive inside `Advertiser` so
/// the registration stays valid. Dropping the `Advertiser` both
/// unregisters the service AND shuts the daemon down.
pub struct Advertiser {
    daemon: ServiceDaemon,
    /// Full service name as registered, e.g.
    /// `jason-desk rtl-sdr._rtl_tcp._tcp.local.`. Needed by
    /// `ServiceDaemon::unregister` when we drop.
    full_name: String,
}

impl Advertiser {
    /// Register a new advertisement. On success the service is live and
    /// will respond to mDNS queries from the LAN within seconds.
    pub fn announce(opts: AdvertiseOptions) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let props = opts.txt.to_properties()?;

        let host = if opts.hostname.is_empty() {
            local_hostname()?
        } else {
            opts.hostname.clone()
        };

        // Empty-string host IPs + `enable_addr_auto()` tells mdns-sd to
        // auto-populate A/AAAA records from the machine's interface
        // list. Matches the "announce on all local addresses" pattern
        // we want — users don't have to think about which interface.
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &opts.instance_name,
            &host,
            "",
            opts.port,
            Some(props),
        )?
        .enable_addr_auto();

        let full_name = info.get_fullname().to_string();
        daemon.register(info)?;
        tracing::info!(
            service = %full_name,
            port = opts.port,
            "rtl_tcp mDNS advertisement registered"
        );
        Ok(Self { daemon, full_name })
    }

    /// Stop advertising and shut the daemon down. Equivalent to
    /// dropping the `Advertiser`, but lets the caller propagate errors.
    pub fn stop(self) -> Result<(), DiscoveryError> {
        let rx = self.daemon.unregister(&self.full_name)?;
        // `unregister` returns a Receiver for the completion status so
        // the caller can wait for unregistration to finish. Short
        // timeout is fine; if mDNS is wedged we still want to exit.
        let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        // Shutdown follows the same pattern.
        let _ = self.daemon.shutdown();
        Ok(())
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Best-effort teardown — we don't want Drop to panic even if
        // the mDNS daemon has already shut down or a mutex is poisoned.
        if let Ok(rx) = self.daemon.unregister(&self.full_name) {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }
}

/// Best-effort local hostname lookup. Uses `gethostname` via the
/// `hostname::get` crate? — we avoid a new dependency by reading
/// `/etc/hostname` on Unix and falling back to `localhost` otherwise.
///
/// The returned string is passed to mDNS as the A/AAAA host; mDNS
/// will normalize / append `.local.` if needed.
fn local_hostname() -> Result<String, DiscoveryError> {
    // std's hostname accessor is nightly-only, so we roll our own for
    // Unix. The `"localhost"` fallback keeps the advertisement alive on
    // an unusual system rather than erroring out.
    #[cfg(unix)]
    {
        // Read /proc/sys/kernel/hostname on Linux, /etc/hostname
        // elsewhere. Both are plain text, one line.
        match std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        {
            Ok(s) => {
                // Defensive: strip any trailing `.local` / `.local.`
                // before re-appending, so an `/etc/hostname` that
                // already contains the suffix doesn't produce
                // `foo.local..local.`. mDNS daemons typically
                // normalize this but costing a single `trim_end_matches`
                // pair per registration is cheap insurance.
                let trimmed = s
                    .trim()
                    .trim_end_matches(".local.")
                    .trim_end_matches(".local");
                if trimmed.is_empty() {
                    Ok("localhost.local.".to_string())
                } else {
                    Ok(format!("{trimmed}.local."))
                }
            }
            Err(e) => Err(DiscoveryError::Hostname(e)),
        }
    }
    #[cfg(not(unix))]
    {
        Ok("localhost.local.".to_string())
    }
}
