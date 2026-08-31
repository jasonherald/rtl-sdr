//! Handshake helpers for the accept path: the RTLX hello and
//! auth-key sniffers, the denial / follow-up `ServerExtension`
//! writers, and client-socket tuning (keepalive + nodelay). Split
//! out of `accept.rs` (itself split from `server.rs`) per the
//! file-size refactor (#818); pure moves, no behavior change.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use librtlsdr_rs::RtlSdrDevice;

use crate::codec::Codec;
use crate::extension::{
    AUTH_KEY_HEADER_LEN, AUTH_REPLY_TIMEOUT, AuthKeyMessage, CLIENT_HELLO_LEN, ClientHello,
    EXTENSION_MAGIC, ServerExtension, Status,
};
use crate::protocol::{DongleInfo, TunerTypeCode};

/// Emit a dongle_info_t + denial `ServerExtension` to an RTLX
/// client whose admission the role gate refused, then let the
/// stream drop out of scope so the TCP FIN reaches the peer. Only
/// called for RTLX clients — vanilla peers get a bare TCP close
/// (no bytes) because they'd mis-parse any response we wrote. The
/// dongle_info_t block comes first because the RTLX client
/// protocol expects it at the head of the stream regardless of
/// whether the handshake was accepted; the ServerExtension
/// follows with `granted_role = None` (0xFF wire sentinel) and
/// the caller-supplied `status` (ControllerBusy or
/// ListenerCapReached). Write failures downgrade to debug-level
/// tracing because a refused-handshake peer often tears down the
/// socket before our response lands — noisy warn! would bury
/// real signal. #392.
pub(super) fn send_denied_response(
    stream: &TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    status: Status,
    hello_version: u8,
) {
    let Ok(dev) = device.lock() else {
        tracing::warn!(%peer, "device mutex poisoned during denial response");
        return;
    };
    let header = DongleInfo {
        tuner: TunerTypeCode::from(dev.tuner_type()),
        gain_count: dev.tuner_gains().len() as u32,
    };
    drop(dev);
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                %peer,
                %e,
                "failed to clone stream for denial response — closing without reply"
            );
            return;
        }
    };
    if writer.write_all(&header.to_bytes()).is_err() {
        tracing::debug!(
            %peer,
            ?status,
            "failed to send dongle_info_t during denial — client already gone"
        );
        return;
    }
    let ext = ServerExtension {
        // Codec choice is moot on denial — the client never
        // proceeds to the IQ stream. `Codec::None` is the neutral
        // choice: no allocation, always valid on the wire.
        codec: Codec::None,
        granted_role: None,
        status,
        // Echo the client's hello version so v1 clients can
        // parse this denial without tripping their strict
        // version gate. Per `CodeRabbit` round 1 on PR #405.
        version: hello_version,
    };
    if writer.write_all(&ext.to_bytes()).is_err() {
        tracing::debug!(
            %peer,
            ?status,
            "failed to send denial ServerExtension — client already gone"
        );
    }
}

/// Emit a follow-up `ServerExtension` (8 bytes) to an RTLX client
/// that has already received `dongle_info_t` on this connection —
/// i.e., the lazy #394 auth path sent `dongle_info_t +
/// ServerExtension(AuthRequired)` as its challenge, and the
/// outcome of the auth reply (and any downstream role-admission
/// denial) must now be communicated WITHOUT re-emitting the
/// header. Writing dongle_info_t twice would make the client's
/// magic-peek misfire on the second handshake.
///
/// Used for `Status::AuthFailed` after a bad / missing lazy auth
/// reply, and for role-admission denials
/// (`Status::ControllerBusy`, `Status::ListenerCapReached`)
/// that land AFTER the lazy path has already emitted
/// `dongle_info_t`. Write failures downgrade to debug-level
/// tracing because a refused-handshake peer often tears down the
/// socket before our follow-up lands — noisy warn! would bury
/// real signal. Per `CodeRabbit` round 3 on PR #405. #394.
pub(super) fn send_extension_only(
    stream: &TcpStream,
    peer: SocketAddr,
    status: Status,
    hello_version: u8,
) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                %peer,
                %e,
                "failed to clone stream for follow-up extension — closing without reply"
            );
            return;
        }
    };
    let ext = ServerExtension {
        // Neutral defaults — the client never proceeds to the IQ
        // stream on a denial, and the follow-up is an
        // informational status, not a codec negotiation.
        codec: Codec::None,
        granted_role: None,
        status,
        // Echo the client's hello version so v1 clients can
        // parse this follow-up without tripping their strict
        // version gate. Matches `send_denied_response`'s
        // version-echo contract.
        version: hello_version,
    };
    if writer.write_all(&ext.to_bytes()).is_err() {
        tracing::debug!(
            %peer,
            ?status,
            "failed to send follow-up ServerExtension — client already gone"
        );
    }
}

/// Seconds of socket idleness before the first TCP keepalive probe
/// goes out. `TCP_KEEPIDLE` (Linux) / `TCP_KEEPALIVE` (macOS). Kernel
/// default is 7200 s (2 hours) on most systems — unusable for
/// detecting a zombie controller before the user's patience runs
/// out. 60 s is the upstream `tcp(7)` recommended minimum for
/// interactive sessions and matches the #393 budget for zombie
/// detection (60 + 10 × 3 = 90 s worst case). Per #393.
pub(in crate::server) const TCP_KEEPALIVE_IDLE_SECS: u32 = 60;
/// Seconds between probes once the first one has been sent.
/// `TCP_KEEPINTVL`. 10 s gives the zombie three chances to reply
/// across a ~30 s window — enough to ride out a brief network
/// hiccup without blowing the detection deadline. Per #393.
///
/// Linux-only: macOS exposes only `TCP_KEEPALIVE` (the seconds-
/// until-first-probe knob); per-probe interval and retry count
/// are kernel-wide sysctls there. The constant is gated to the
/// platform that can actually set it so non-Linux targets don't
/// drag a dead-code warning forward.
#[cfg(target_os = "linux")]
pub(in crate::server) const TCP_KEEPALIVE_INTERVAL_SECS: u32 = 10;
/// How many unanswered probes before the kernel drops the socket.
/// `TCP_KEEPCNT`. 3 keeps the total dead-peer detection window at
/// roughly 90 s (idle 60 s + 3 × 10 s probes). Per #393.
///
/// Linux-only — same rationale as `TCP_KEEPALIVE_INTERVAL_SECS`.
#[cfg(target_os = "linux")]
pub(in crate::server) const TCP_KEEPALIVE_RETRIES: u32 = 3;

/// Tune a freshly-accepted client socket: keepalive probe schedule
/// (zombie-controller detection, #393) + `TCP_NODELAY` for snappy
/// 5-byte command frames. Both best-effort — failures log and stream on.
pub(in crate::server) fn configure_client_socket(stream: &TcpStream) {
    // Enable SO_KEEPALIVE and tune the probe schedule so a zombie
    // controller (laptop-lid-closed, wifi-dropped) is detected
    // within ~90 s instead of the kernel default (~2 h on Linux).
    // Takeover (#393) relies on this for "stale slot eventually
    // gets pruned without user intervention"; the takeover handshake
    // itself is the *explicit* path, but keepalive is the fallback
    // that prevents permanent lockout when neither side sends FIN.
    if let Err(e) = set_keepalive_tuned(stream) {
        tracing::warn!(%e, "SO_KEEPALIVE tuning not applied (non-fatal)");
    }
    // Disable Nagle — commands are 5 bytes and we want snappy tuning.
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!(%e, "TCP_NODELAY not applied (non-fatal)");
    }
}

/// Enable `SO_KEEPALIVE` and tune the probe schedule through
/// `socket2` — no raw `setsockopt` / `unsafe` (#715). Idle time is
/// portable; the probe interval and retry count are applied where the
/// platform exposes them (Linux), matching the previous per-OS paths.
fn set_keepalive_tuned(stream: &TcpStream) -> std::io::Result<()> {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(u64::from(TCP_KEEPALIVE_IDLE_SECS)));
    #[cfg(target_os = "linux")]
    let keepalive = keepalive
        .with_interval(Duration::from_secs(u64::from(TCP_KEEPALIVE_INTERVAL_SECS)))
        .with_retries(TCP_KEEPALIVE_RETRIES);
    socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// Budget for the RTLX hello sniff on a fresh connection — long
/// enough for a same-LAN client's first bytes, short enough that a
/// silent legacy peer falls back to the vanilla path promptly.
pub(in crate::server) const HELLO_SNIFF_TIMEOUT: Duration = Duration::from_millis(100);

/// Try to read + parse an extended-protocol [`ClientHello`] from
/// `stream` within [`HELLO_SNIFF_TIMEOUT`].
///
/// Return cases:
///
/// - `Ok(Some(hello))` — valid 8-byte hello, fully consumed.
/// - `Ok(None)` — legacy fallback. Reached on a zero-byte
///   timeout/EOF (idle client never sent anything) OR on a
///   peek whose observed prefix definitively does NOT match
///   [`EXTENSION_MAGIC`] (legacy client sent a command; the
///   bytes stay queued in the receive buffer so `command_worker`
///   can parse the frame). Nothing is consumed in either case.
/// - `Err(_)` — protocol error. Raised when the magic already
///   matched and we committed to reading a full 8 bytes — covers
///   `read_exact` timeout or EOF mid-hello (partial hello, bytes
///   already drained from the stream) and parse failure on a
///   complete 8-byte block (unknown role, unknown protocol
///   version, etc.). Also raised when a 1–3 byte prefix of the
///   magic arrived but the remaining magic bytes never completed
///   within the sniff budget: returning `Ok(None)` there would
///   shift the command reader by the prefix bytes still queued
///   in the receive buffer, corrupting the legacy stream.
///
/// Uses `peek()` for the magic check so legitimate legacy traffic
/// stays intact. A fragmented `RTLX` hello whose bytes arrive
/// across two TCP segments surfaces as a short peek; the poll
/// loop waits for more bytes as long as the observed prefix
/// still matches `EXTENSION_MAGIC[..n]`. Once the full magic
/// matches we commit to reading the full 8 bytes — partial reads
/// are fatal because we can't un-consume bytes already drained.
/// Per `CodeRabbit` round 2 on PR #399 (initial fix),
/// round 3 (doc alignment), and round 5 on PR #402
/// (partial-prefix handling for fragmented RTLX).
pub(in crate::server) fn sniff_client_hello(
    mut stream: &TcpStream,
) -> std::io::Result<Option<ClientHello>> {
    // Poll cadence for the peek retry loop. Small enough that a
    // fragmented `RTLX` hello whose trailing bytes land within a
    // few ms gets re-checked before the sniff deadline; large
    // enough to avoid spinning at 100 % CPU while waiting.
    const PEEK_POLL_INTERVAL: Duration = Duration::from_millis(2);

    let deadline = Instant::now() + HELLO_SNIFF_TIMEOUT;
    let mut peek_buf = [0u8; EXTENSION_MAGIC.len()];
    let mut observed_any_prefix = false;

    // Peek loop. `peek` maps to `recv(…, MSG_PEEK)` — returns as
    // soon as any data is available, so a fragmented 4-byte
    // magic (e.g. `RT` then `LX` across two TCP segments)
    // surfaces here as a short peek. Keep waiting while the
    // observed bytes are still a prefix of `EXTENSION_MAGIC`;
    // only fall back to legacy when we have a definite non-magic
    // prefix, an EOF, or a zero-byte timeout.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stream.set_read_timeout(None)?;
            if observed_any_prefix {
                // Partial magic-prefix observed but full 4 bytes
                // never arrived. Returning `Ok(None)` (legacy
                // fallback) would shift the command reader by
                // the 1–3 prefix bytes still queued in the
                // receive buffer — parsing `R` / `RT` / `RTL`
                // as opcodes corrupts the command stream. Surface
                // as `InvalidData` (not `TimedOut`) to match the
                // post-magic-match `read_exact` and body-parse
                // failure paths below: both are protocol-desync
                // errors from the host's perspective — the socket
                // isn't "idle", it sent bytes that commit to the
                // extended protocol and then stalled. Per
                // `CodeRabbit` round 6 on PR #402.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "RTLX magic prefix observed but full 4-byte magic did not complete \
                     within HELLO_SNIFF_TIMEOUT",
                ));
            }
            // Zero bytes ever observed — idle legacy peer or
            // port scanner. Nothing consumed, safe to fall back.
            return Ok(None);
        }
        // Per-iteration read timeout capped by `PEEK_POLL_INTERVAL`
        // so we wake up to re-check the deadline if the kernel
        // blocks waiting for bytes.
        let this_timeout = remaining.min(PEEK_POLL_INTERVAL);
        stream.set_read_timeout(Some(this_timeout))?;
        match stream.peek(&mut peek_buf) {
            Ok(0) => {
                // Peer closed cleanly. Connection is gone so
                // there's no stream left to desync — safe
                // fallback regardless of whether a prefix had
                // been observed.
                stream.set_read_timeout(None)?;
                return Ok(None);
            }
            Ok(n) if n >= EXTENSION_MAGIC.len() => break,
            Ok(n) => {
                // 0 < n < EXTENSION_MAGIC.len() — partial peek.
                observed_any_prefix = true;
                if peek_buf[..n] != EXTENSION_MAGIC[..n] {
                    // Definite non-magic prefix — legacy command
                    // sender whose opcode byte (plus whatever
                    // arg bytes already arrived) doesn't start
                    // with `R`. Bytes stay queued for
                    // `command_worker`.
                    stream.set_read_timeout(None)?;
                    return Ok(None);
                }
                // Prefix still a candidate `RTLX`. Brief yield
                // so the kernel receive buffer can fill before
                // the next peek.
                thread::sleep(PEEK_POLL_INTERVAL);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // No bytes arrived this slice — loop and
                // re-check the deadline.
            }
            Err(e) => {
                // Other errors (ECONNRESET, etc.) → propagate
                // so the caller tears down cleanly.
                stream.set_read_timeout(None)?;
                return Err(e);
            }
        }
    }
    if peek_buf != EXTENSION_MAGIC {
        // Full 4-byte peek and no match — legacy client. A
        // vanilla `SetCenterFreq` starts with 0x01; no
        // documented opcode begins with 0x52 ('R'), so four
        // non-magic bytes at the head are a legitimate legacy
        // command frame. Bytes stay queued for the command
        // reader.
        stream.set_read_timeout(None)?;
        return Ok(None);
    }
    // Magic matched — commit to consuming 8 bytes. A timeout or
    // EOF here is no longer a safe fallback: we've verified the
    // client started an extended hello and `read_exact` will
    // have eaten whatever bytes arrived before the stall.
    // Returning `Ok(None)` would let the legacy path start
    // against a shifted command stream — exactly the desync
    // `CodeRabbit` round 2 on PR #399 flagged. Treat every
    // failure mode as a protocol error and drop the client.
    stream.set_read_timeout(Some(HELLO_SNIFF_TIMEOUT))?;
    let mut hello_buf = [0u8; CLIENT_HELLO_LEN];
    let read_result = stream.read_exact(&mut hello_buf);
    stream.set_read_timeout(None)?;
    read_result?;
    ClientHello::from_bytes(&hello_buf)
        .map(Some)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RTLX magic matched but ClientHello body failed to parse (unknown role or \
             malformed field)",
            )
        })
}

/// Read + parse an [`AuthKeyMessage`] from `stream` within an
/// **absolute** [`AUTH_REPLY_TIMEOUT`] budget. Caller is
/// responsible for having observed `hello.has_auth() == true`
/// on the preceding hello — this helper assumes the auth message
/// is the next thing on the wire. Two-phase read: 6-byte header
/// (magic + u16 key_len) then `key_len`-byte body.
///
/// **Absolute deadline, not per-read timeout.** The budget caps
/// the whole sniff call, not each `read_exact` syscall
/// independently. Per-read timeouts reset on every successful
/// byte, so a slow peer trickling one byte every 4.9 s could
/// keep both reads inside their individual 5 s windows while
/// wedging the accept thread indefinitely. The deadline-based
/// approach bounds total elapsed time — header + body must
/// complete within `AUTH_REPLY_TIMEOUT` of entry. Per
/// `CodeRabbit` round 2 on PR #405. #394.
pub(in crate::server) fn sniff_auth_key_message(
    mut stream: &TcpStream,
) -> std::io::Result<AuthKeyMessage> {
    let deadline = Instant::now() + AUTH_REPLY_TIMEOUT;

    // Header read. `remaining_until` returns the interval from
    // now to the deadline, or `Duration::ZERO` if the deadline
    // already passed (which signals TimedOut here too — reading
    // with a zero timeout is undefined, so we explicitly error).
    let header_timeout = deadline.saturating_duration_since(Instant::now());
    if header_timeout.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "auth key header read deadline expired before first read",
        ));
    }
    stream.set_read_timeout(Some(header_timeout))?;
    let mut header = [0u8; AUTH_KEY_HEADER_LEN];
    if let Err(e) = stream.read_exact(&mut header) {
        let _ = stream.set_read_timeout(None);
        return Err(e);
    }

    // Header parsed OK → decode `key_len` so we know how much
    // more to read.
    let Some(key_len) = AuthKeyMessage::parse_header_len(&header) else {
        let _ = stream.set_read_timeout(None);
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "auth key message header: bad magic or out-of-range key_len",
        ));
    };

    // Body read with the remaining (shrinking) budget. If the
    // header read burned most of the window, the body read gets
    // whatever's left. A zero-remaining budget short-circuits as
    // TimedOut before we ever call into `read_exact`.
    let body_timeout = deadline.saturating_duration_since(Instant::now());
    if body_timeout.is_zero() {
        let _ = stream.set_read_timeout(None);
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "auth key body read deadline expired before body read",
        ));
    }
    stream.set_read_timeout(Some(body_timeout))?;
    let mut body = vec![0u8; key_len as usize];
    let body_result = stream.read_exact(&mut body);
    let _ = stream.set_read_timeout(None);
    body_result?;

    // Reassemble the full wire buffer for
    // `AuthKeyMessage::from_bytes`. Passing through the decoder
    // rather than constructing the struct directly ensures the
    // same validation runs on BOTH the parse-header-then-body
    // path here and the full-buffer-in-hand path the tests use
    // — keeps round-trip invariants honest.
    let mut full = Vec::with_capacity(AUTH_KEY_HEADER_LEN + body.len());
    full.extend_from_slice(&header);
    full.extend_from_slice(&body);
    AuthKeyMessage::from_bytes(&full).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "auth key message body failed to round-trip through AuthKeyMessage::from_bytes",
        )
    })
}
