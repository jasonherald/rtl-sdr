//! Advertiser: publish a single `_rtl_tcp._tcp.local.` registration.

use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::SERVICE_TYPE;
use crate::error::DiscoveryError;
use crate::txt::TxtRecord;

/// How long we wait for mDNS unregister / shutdown completion before
/// proceeding. Short timeout is intentional — if the daemon is wedged
/// we prefer to exit promptly; the registration will time out on the
/// LAN side via its normal TTL regardless of whether our explicit
/// unregister succeeded.
const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(1);

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
///
/// `daemon` is wrapped in `Option` so [`Advertiser::stop`] can take it
/// out explicitly, leaving `Drop::drop` as a no-op for already-stopped
/// instances — otherwise callers who `stop()` would pay for two rounds
/// of unregister + shutdown against the same (potentially already
/// dead) daemon.
pub struct Advertiser {
    daemon: Option<ServiceDaemon>,
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

        // Normalize the mDNS hostname identically for both the
        // auto-derived and caller-provided paths. Previously only the
        // empty-string branch appended `.local.`; a caller that
        // passed this crate's own `local_hostname()` output (which
        // returns the bare name by contract) would have registered
        // under a non-qualified hostname while the auto-derive path
        // registered under `.local.`. Trimming defensively on both
        // paths also lets a caller pass `"foo.local."` without
        // producing `foo.local..local.`.
        let raw_host = if opts.hostname.is_empty() {
            local_hostname()
        } else {
            opts.hostname.clone()
        };
        let trimmed = raw_host
            .trim()
            .trim_end_matches(".local.")
            .trim_end_matches(".local");
        let host = if trimmed.is_empty() {
            String::from("localhost.local.")
        } else {
            format!("{trimmed}.local.")
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
        Ok(Self {
            daemon: Some(daemon),
            full_name,
        })
    }

    /// Stop advertising and shut the daemon down. Equivalent to
    /// dropping the `Advertiser`, but lets the caller propagate errors.
    /// Taking the daemon out of `Option` here means the subsequent
    /// `Drop` call is a no-op for an already-stopped advertiser.
    pub fn stop(mut self) -> Result<(), DiscoveryError> {
        let Some(daemon) = self.daemon.take() else {
            return Ok(());
        };
        // Run shutdown unconditionally. The Option::take above has
        // already disarmed Drop, so if we bailed here via `?` on an
        // unregister error the daemon thread and its mDNS sockets
        // would leak for the rest of the process lifetime.
        // `unregister` returns a Receiver for the completion status;
        // short timeout is fine because if mDNS is wedged we still
        // want to exit.
        let unregister_result = daemon.unregister(&self.full_name);
        if let Ok(rx) = &unregister_result {
            let _ = rx.recv_timeout(UNREGISTER_TIMEOUT);
        }
        let _ = daemon.shutdown();
        // Surface the unregister error (if any) to the caller after
        // the daemon is fully torn down. Discard the Receiver on the
        // Ok path — we already awaited it above.
        unregister_result?;
        Ok(())
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // No-op if `stop()` already consumed the daemon — the Option
        // lets us distinguish "still owned, need teardown" from
        // "already torn down by an explicit stop()."
        let Some(daemon) = self.daemon.take() else {
            return;
        };
        // Best-effort teardown — we don't want Drop to panic even if
        // the mDNS daemon has already shut down or a mutex is poisoned.
        if let Ok(rx) = daemon.unregister(&self.full_name) {
            let _ = rx.recv_timeout(UNREGISTER_TIMEOUT);
        }
        let _ = daemon.shutdown();
    }
}

/// Best-effort local hostname lookup, returning the **bare** hostname
/// without any `.local.` suffix. Useful as a default nickname for
/// advertisement (callers who want the full mDNS form can append
/// `.local.` themselves).
///
/// Uses `libc::gethostname(3)` on Unix — portable across Linux, macOS,
/// and the BSDs, unlike the `/proc/sys/kernel/hostname` + `/etc/hostname`
/// reads we had before (Linux-only). On the exceedingly rare failure
/// path — `gethostname()` cannot actually fail on a modern OS except
/// for `EFAULT` against a buffer we control — we log and return
/// `"localhost"` so a degraded system still gets an advertisement
/// rather than a cryptic mDNS registration error.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn local_hostname() -> String {
    // POSIX caps HOST_NAME_MAX at 255; 256 includes the NUL and
    // leaves room for any OS that returns a non-NUL-terminated
    // buffer at full capacity.
    const BUFFER_LEN: usize = 256;
    let mut buf = [0u8; BUFFER_LEN];
    // SAFETY: `buf` is a fixed-size stack array whose lifetime
    // outlives the syscall. We pass its length via `size_of_val`, so
    // `gethostname()` cannot write past the end. The result is read
    // only after we confirm success.
    let rc = unsafe {
        libc::gethostname(
            buf.as_mut_ptr().cast::<libc::c_char>(),
            std::mem::size_of_val(&buf),
        )
    };
    if rc != 0 {
        tracing::warn!("gethostname() failed, using 'localhost' as nickname default");
        return String::from("localhost");
    }
    // gethostname does NOT guarantee NUL-termination when the name
    // fills the buffer, but POSIX HOST_NAME_MAX is well under 256, so
    // in practice every returned hostname is NUL-terminated and the
    // NUL scan is safe. Still, defensively cap the length.
    let name_len = buf.iter().position(|&b| b == 0).unwrap_or(BUFFER_LEN);
    let Ok(name) = std::str::from_utf8(&buf[..name_len]) else {
        tracing::warn!("gethostname() returned non-UTF-8 bytes, using 'localhost'");
        return String::from("localhost");
    };
    let trimmed = name
        .trim()
        .trim_end_matches(".local.")
        .trim_end_matches(".local");
    if trimmed.is_empty() {
        String::from("localhost")
    } else {
        trimmed.to_string()
    }
}

/// Non-Unix stub. `sdr-rtl-tcp` bin is `compile_error!`-gated to Unix
/// already, so this path is only reachable from library consumers on
/// exotic platforms. Returns `"localhost"` so they still get a valid
/// (if boring) default nickname.
#[cfg(not(unix))]
pub fn local_hostname() -> String {
    String::from("localhost")
}
