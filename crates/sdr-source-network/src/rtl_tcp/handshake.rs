//! Connection + handshake for the `rtl_tcp` client (issue #818):
//! cancellable TCP connect, the RTLX extension negotiation
//! (`ClientHello` → `dongle_info_t` → `ServerExtension`), and the
//! socket-level helpers the handshake path uses. Split out of
//! `rtl_tcp.rs` per the Codacy 500-NLOC file gate — behavior is
//! unchanged.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use sdr_server_rtltcp::codec::{Codec, CodecMask};
use sdr_server_rtltcp::extension::{
    CLIENT_HELLO_FLAGS_NONE, ClientHello, EXTENSION_MAGIC, SERVER_EXTENSION_LEN, ServerExtension,
    Status,
};
use sdr_server_rtltcp::protocol::{DONGLE_INFO_LEN, DongleInfo};
use sdr_types::SourceError;

use super::manager::{clear_pending_stream, set_state};
use super::{CONNECT_SHUTDOWN_POLL, ConnectionState, RtlTcpConfig, SharedState, TunerInfo};

/// Read the server's 8-byte [`ServerExtension`] block, invoked
/// immediately after the legacy 12-byte `dongle_info_t`.
///
/// **Only call this when the client has sent a `ClientHello`.**
/// Once we've committed to the extended protocol the server is
/// contractually obligated to respond with an 8-byte block whose
/// first four bytes are [`EXTENSION_MAGIC`]. This function reads
/// that block with `read_exact` — not `peek` — so partial TCP
/// deliveries (magic-only first, body later) can't race us into
/// parsing zero-padded scratch memory and silently mis-negotiating
/// as `Codec::None` while the server is actually streaming LZ4.
///
/// A short read, a magic mismatch, or a malformed body all surface
/// as [`std::io::ErrorKind::InvalidData`]; the caller promotes
/// these to `SourceError::Protocol` and aborts the connection
/// rather than falling back to a legacy path that would read
/// compressed bytes as raw I/Q. Per CodeRabbit round 1 on PR #399.
fn read_server_extension(stream: &TcpStream) -> std::io::Result<ServerExtension> {
    let mut buf = [0u8; SERVER_EXTENSION_LEN];
    // `Read` is implemented for `&TcpStream`, so shadow into a
    // local mut binding and call `read_exact` through the trait
    // directly — cleaner than a fully-qualified turbofish and
    // avoids the temporary-rvalue gotcha of `(&*stream).read_exact(…)`.
    (&mut &*stream).read_exact(&mut buf)?;
    if buf[..EXTENSION_MAGIC.len()] != EXTENSION_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "rtl_tcp extension handshake: expected `RTLX` magic after dongle_info_t, got {:02x?}",
                &buf[..EXTENSION_MAGIC.len()]
            ),
        ));
    }
    ServerExtension::from_bytes(&buf).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "rtl_tcp extension handshake: `RTLX` magic matched but server-extension body failed to parse",
        )
    })
}

/// Outcome of an extended-protocol handshake. `stream` is the
/// TCP socket (still plain `TcpStream`; the caller wraps it in a
/// [`Decoder`] for reads). `codec` tells the caller which
/// decoder to use for the IQ stream.
///
/// `ServerExtension.granted_role` isn't carried here — it's
/// already published inside `attempt_connect` on the
/// `ConnectionState::Connected` state transition, which is the
/// single consumer the UI needs. Threading it through the
/// outcome struct too would require the manager's data-pump
/// branch to also read and forward it, which it has no use for
/// (no mid-stream role changes exist in the wire protocol).
/// Per `CodeRabbit` round 1 on PR #408.
pub(super) struct HandshakeOutcome {
    pub(super) stream: TcpStream,
    pub(super) codec: Codec,
}

/// One TCP connect + full handshake, orchestrated over one helper
/// per stage — resolve/connect, hello emission, `dongle_info_t`,
/// `ServerExtension` negotiation, and the connected-state publish.
/// The former single-function form carried a `too_many_lines` allow;
/// carved per the 50-NLOC gate (Codacy precedent from PR #880's
/// server-side `spawn_client_workers` staging). Behavior unchanged:
/// every stage propagates the same errors from the same points, and
/// the publish ordering (tuner cache → `Connected` state → command
/// sink) is preserved inside [`publish_connected`].
pub(super) fn attempt_connect(
    host: &str,
    port: u16,
    shared: &Arc<SharedState>,
    config: &RtlTcpConfig,
) -> Result<HandshakeOutcome, SourceError> {
    let stream = resolve_and_connect(host, port, shared, config)?;
    let extension_enabled = send_client_hello_if_enabled(&stream, config)?;
    let tuner = read_dongle_header(&stream)?;
    let (codec, granted_role) = negotiate_extension(&stream, extension_enabled)?;
    publish_connected(shared, &stream, config, tuner, codec, granted_role)?;
    // `pending_stream` stays populated for the session as the lock-free
    // cancellation handle (see its field doc).
    Ok(HandshakeOutcome { stream, codec })
}

/// Resolve `host:port`, run the cancellable connect, publish the
/// stop-cancellation handle, and install the socket options the
/// session needs. Split out per the 50-NLOC gate (PR #880 Codacy
/// precedent).
fn resolve_and_connect(
    host: &str,
    port: u16,
    shared: &Arc<SharedState>,
    config: &RtlTcpConfig,
) -> Result<TcpStream, SourceError> {
    // `(host, port).to_socket_addrs()` handles both IPv4 dotted
    // quads AND IPv6 literals like `::1` correctly — the naïve
    // `format!("{host}:{port}")` that we had before would build
    // `::1:1234` for IPv6, which SocketAddr::from_str then rejects.
    //
    // Resolution itself is ~instant on localhost; the slow path is the
    // actual `connect_timeout` call, which is offloaded below.
    use std::net::ToSocketAddrs;
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(SourceError::Io)?
        .collect();

    // Run the blocking connect on a helper thread so the manager can
    // respond to `shutdown` within `CONNECT_SHUTDOWN_POLL` instead of
    // being wedged for the full `config.connect_timeout` window when a
    // destination is blackholed. `TcpStream::connect_timeout` has no
    // cancellation hook, so we let the helper finish naturally after
    // shutdown and just ignore its result.
    let stream = connect_cancellable(addrs, config.connect_timeout, &shared.shutdown)?;

    // Let `stop_manager` cut the handshake reads short (#745).
    match stream.try_clone() {
        Ok(clone) => {
            if let Ok(mut pending) = shared.pending_stream.lock() {
                *pending = Some(clone);
            }
        }
        Err(e) => {
            tracing::warn!(%e, "rtl_tcp: could not clone the socket for stop(); a silent peer may delay stop by the read timeout");
        }
    }
    // `stop_manager` sets `shutdown` and then takes `pending_stream`. If
    // it ran between `connect_cancellable`'s last poll and the publish
    // above, it found nothing to close and is already waiting in
    // `join()` — so re-check here rather than walking into the handshake
    // reads with a clone nobody will shut down.
    if shared.shutdown.load(Ordering::Relaxed) {
        clear_pending_stream(shared);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return Err(SourceError::Io(std::io::Error::from(
            std::io::ErrorKind::Interrupted,
        )));
    }

    stream.set_read_timeout(Some(config.data_read_timeout))?;
    if let Err(e) = set_keepalive(&stream, true) {
        tracing::warn!(%e, "SO_KEEPALIVE not applied (non-fatal)");
    }

    Ok(stream)
}

/// Emit the extended-protocol `ClientHello` (+ eager-auth follow-up)
/// when any config field carries non-default RTLX state. Returns
/// whether the extension path was taken so the caller knows to read
/// the `ServerExtension` block. Split out per the 50-NLOC gate
/// (PR #880 Codacy precedent).
fn send_client_hello_if_enabled(
    stream: &TcpStream,
    config: &RtlTcpConfig,
) -> Result<bool, SourceError> {
    // Send the extended-protocol `ClientHello` if the caller
    // opted into either compression (#307) or takeover (#393) —
    // both are RTLX features that require the server to parse
    // the hello. A hello sent to a vanilla `rtl_tcp` server
    // straddles its 5-byte command-read framing (hello is 8
    // bytes = 1.6 commands) and can cause garbage dispatches,
    // so we only send it when we have out-of-band evidence the
    // server speaks the extension (e.g., mDNS TXT `codecs=3` or
    // an explicit per-server profile setting — see the
    // `RtlTcpConfig.compression` doc for the full signal). The
    // same signal gates takeover: if the user clicks "Take
    // control" against a server whose mDNS record says legacy-
    // only, the UI should gray out the option or toast the user,
    // because sending a hello there corrupts the legacy stream.
    // Default `compression = NONE_ONLY && request_takeover =
    // false` → no hello → wire-compatible with every rtl_tcp
    // server on earth. Per #307 / #393.
    // Hello needed when ANY field carries non-default state:
    // compression opt-in, takeover opt-in, auth, or role
    // (Listen != Control default). Per #396, a `Role::Listen`
    // request also has to surface on the wire, so widen the
    // gate here — same RTLX-only hazard as the other fields,
    // same mDNS `codecs=3` out-of-band evidence requirement.
    let extension_enabled = config.compression != CodecMask::NONE_ONLY
        || config.request_takeover
        || config.auth_key.is_some()
        || config.requested_role != sdr_server_rtltcp::extension::Role::Control;
    if extension_enabled {
        // Compose the flags byte. Each bit is independently set
        // based on config — takeover and auth can co-exist on
        // the same hello. Servers without role / auth support
        // ignore the whole hello, but the RTLX-only hazard still
        // applies (see the field docs for the mDNS `codecs=`
        // gate).
        let mut flags = CLIENT_HELLO_FLAGS_NONE;
        if config.request_takeover {
            // Bit 0: `FLAG_REQUEST_TAKEOVER`. Tells #392+ servers
            // to kick the existing Control client (when the slot
            // is busy) and admit us instead. #393.
            flags |= sdr_server_rtltcp::extension::FLAG_REQUEST_TAKEOVER;
        }
        if config.auth_key.is_some() {
            // Bit 1: `FLAG_HAS_AUTH`. Announces that an
            // `AuthKeyMessage` follow-up lands immediately after
            // this hello. Server reads the follow-up in the
            // same receive-buffer position without a round-trip
            // to request it. #394.
            flags |= sdr_server_rtltcp::extension::FLAG_HAS_AUTH;
        }
        // Pre-build AND validate the auth payload BEFORE any
        // writes hit the socket. Two reasons (`CodeRabbit` round 1
        // on PR #405):
        //
        // 1. An invalid `auth_key` (empty or > `MAX_AUTH_KEY_LEN`)
        //    caught at `AuthKeyMessage::to_bytes` must surface
        //    as a config error BEFORE the hello lands on the
        //    wire. Otherwise we'd send the hello, discover the
        //    key is bad, and abort — leaving a pre-#394 server
        //    stuck reading bytes we never sent (or mis-parsing
        //    the hello as a legacy command prefix).
        // 2. All-or-nothing semantics — the server either sees
        //    the full extended handshake (hello + auth) or
        //    nothing at all. No partial-write state.
        let auth_payload: Option<Vec<u8>> = if let Some(key) = &config.auth_key {
            let msg = sdr_server_rtltcp::extension::AuthKeyMessage { key: key.clone() };
            Some(msg.to_bytes().ok_or_else(|| {
                SourceError::Protocol(format!(
                    "RtlTcpConfig.auth_key length {} invalid for AuthKeyMessage (must be \
                     1..={MAX}; 32-byte URL-safe base64 is the canonical server-\
                     generated shape)",
                    key.len(),
                    MAX = sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN,
                ))
            })?)
        } else {
            None
        };

        let hello = ClientHello {
            codec_mask: config.compression,
            // Threaded from `RtlTcpConfig.requested_role` — the
            // UI's Connection-role picker (#396) feeds this
            // directly. Default `Role::Control` matches the
            // pre-#392 single-client behavior every legacy
            // `rtl_tcp` client assumes; opting into
            // `Role::Listen` needs the out-of-band evidence
            // that the server is #392-aware (mDNS TXT
            // `codecs=3` or saved-profile knowledge).
            role: config.requested_role,
            flags,
            // Pick the MINIMUM viable protocol version for this
            // hello's feature set. Compression-only / takeover-
            // only / plain hellos emit v1 (back-compat with
            // pre-#394 servers that haven't widened their version
            // gate). Auth-bearing hellos emit v2 so pre-#394
            // servers reject at parse time instead of accepting
            // the hello and then mis-reading the queued
            // `AuthKeyMessage` bytes as legacy commands. Per
            // `CodeRabbit` round 1 on PR #405.
            version: sdr_server_rtltcp::extension::required_protocol_version(flags),
        };
        if let Err(e) = (&mut &*stream).write_all(&hello.to_bytes()) {
            return Err(SourceError::Io(e));
        }
        // Auth follow-up (#394 eager path). Pre-built above so
        // local validation precedes any socket write; server
        // reads these bytes in the same receive-buffer position
        // without a round-trip to request them.
        if let Some(wire) = auth_payload {
            if let Err(e) = (&mut &*stream).write_all(&wire) {
                return Err(SourceError::Io(e));
            }
        }
    }
    Ok(extension_enabled)
}

/// Read + verify the legacy 12-byte `dongle_info_t` header. Split
/// out per the 50-NLOC gate (PR #880 Codacy precedent).
fn read_dongle_header(stream: &TcpStream) -> Result<TunerInfo, SourceError> {
    // Read and verify the 12-byte dongle_info_t header.
    let mut header_buf = [0u8; DONGLE_INFO_LEN];
    read_exact_with_context(stream, &mut header_buf)?;

    let Some(info) = DongleInfo::from_bytes(&header_buf) else {
        return Err(SourceError::Protocol(
            "not an rtl_tcp server: dongle_info_t magic prefix mismatch".into(),
        ));
    };
    let tuner = TunerInfo::from(info);
    // NOTE: `shared.tuner` is NOT published here. Writing it
    // before the extension read would expose stale tuner metadata
    // via `tuner_info()` if the extension fails or the server
    // rejects with a non-OK status — callers would see a
    // "tuner = R820T" readback for a session that never actually
    // reached `Connected`. The cache write now lives next to the
    // `set_state(Connected)` call below, so the tuner is visible
    // only once the handshake has fully succeeded. Per CodeRabbit
    // round 5 on PR #399.
    Ok(tuner)
}

/// Read the `ServerExtension` block (when a hello was sent) and
/// route non-OK statuses to their dedicated error variants; the
/// legacy path lands on `Codec::None` with no granted role. Split
/// out per the 50-NLOC gate (PR #880 Codacy precedent).
fn negotiate_extension(
    stream: &TcpStream,
    extension_enabled: bool,
) -> Result<(Codec, Option<sdr_server_rtltcp::extension::Role>), SourceError> {
    // Read the server's `ServerExtension` block BEFORE publishing
    // `Connected` state — the codec is part of the state the UI
    // renders, and landing in `Connected { codec: None }` first and
    // then updating would cause a subtitle flicker.
    //
    // Only runs when we sent a hello. Once we've committed to the
    // extended protocol the server MUST respond with an 8-byte block
    // starting with `RTLX`; any short read, magic mismatch, or
    // malformed body is a protocol error (not a legacy fallback) —
    // silently falling back would let a server that picked LZ4
    // stream compressed bytes into our IQ decoder. Per CodeRabbit
    // round 1 on PR #399.
    // Track the server's `granted_role` so it can flow through
    // to `ConnectionState::Connected`. `None` on the legacy path
    // (we never read the extension block) OR on an extended
    // server that predates #392 and leaves the field unset —
    // the UI treats both as "unknown" and hides the role badge.
    // Per CodeRabbit round 1 on PR #408.
    let mut granted_role: Option<sdr_server_rtltcp::extension::Role> = None;
    let codec = if extension_enabled {
        match read_server_extension(stream) {
            Ok(ext) => {
                // A non-OK status means the server parsed our hello
                // but rejected the session. Each flavor needs a
                // distinct error variant so the connection manager
                // can route to the right `RtlTcpConnectionState`
                // without string-parsing the reason:
                //
                // - `ControllerBusy` (#392) → `SourceError::ControllerBusy`.
                //   User must decide: retry with `request_takeover`
                //   or switch to `Role::Listen`. No auto-retry; the
                //   UI surfaces a toast with action buttons.
                //   Pre-#396 this folded into `TemporarilyUnavailable`
                //   with silent auto-retry, which hid the decision
                //   point. Per #396.
                // - `AuthRequired` (#394) → `SourceError::AuthRequired`.
                //   User must enter a key. No auto-retry.
                // - `AuthFailed` (#394) → `SourceError::AuthFailed`.
                //   User must enter the RIGHT key. No auto-retry.
                // - Anything else → `SourceError::Protocol` (generic
                //   terminal). Covers future status codes we haven't
                //   seen yet, plus `ListenerCapReached` (which the
                //   UI can treat similarly to ControllerBusy at the
                //   toast level, but surfacing as generic Protocol
                //   is acceptable until a dedicated #396 follow-up
                //   fleshes out listener-cap UX).
                match ext.status {
                    Status::Ok => {}
                    Status::ControllerBusy => {
                        return Err(SourceError::ControllerBusy);
                    }
                    Status::AuthRequired => {
                        return Err(SourceError::AuthRequired);
                    }
                    Status::AuthFailed => {
                        return Err(SourceError::AuthFailed);
                    }
                    other @ Status::ListenerCapReached => {
                        return Err(SourceError::Protocol(format!(
                            "rtl_tcp extension rejected by server: status={:?} (wire={})",
                            other,
                            other.to_wire()
                        )));
                    }
                }
                tracing::info!(
                    codec = %ext.codec,
                    status = ext.status.to_wire(),
                    granted_role = ?ext.granted_role,
                    "rtl_tcp extended-handshake accepted by server"
                );
                granted_role = ext.granted_role;
                ext.codec
            }
            Err(e) => {
                return Err(SourceError::Protocol(format!(
                    "rtl_tcp extension handshake failed after sending ClientHello: {e}"
                )));
            }
        }
    } else {
        Codec::None
    };
    Ok((codec, granted_role))
}

/// Publish the handshake result: tuner cache first, then the
/// `Connected` state transition, then the command-sink clone — the
/// ordering contract documented on each block below. Split out per
/// the 50-NLOC gate (PR #880 Codacy precedent).
fn publish_connected(
    shared: &Arc<SharedState>,
    stream: &TcpStream,
    config: &RtlTcpConfig,
    tuner: TunerInfo,
    codec: Codec,
    granted_role: Option<sdr_server_rtltcp::extension::Role>,
) -> Result<(), SourceError> {
    // Publish tuner metadata + Connected state together — both
    // reflect the same "handshake fully succeeded" point. Order
    // matters: tuner cache first, then state transition, so any
    // UI listener that observes Connected and immediately reads
    // `tuner_info()` sees the fresh value rather than a None
    // (initial) or stale (previous-session) snapshot.
    if let Ok(mut slot) = shared.tuner.lock() {
        *slot = Some(tuner);
    }
    set_state(
        shared,
        ConnectionState::Connected {
            tuner,
            codec,
            granted_role,
        },
    );

    // Publish a clone of the stream for the command sender. Install a
    // write timeout on the clone so `send_command`'s blocking
    // `write_all` can't hang indefinitely if a zero-window peer
    // saturates our kernel send buffer — tune/gain changes must stay
    // responsive. Socket options propagate across `try_clone` on the
    // same underlying fd, so this applies to every subsequent write.
    //
    // The command sink is ALWAYS the raw TCP stream regardless of
    // negotiated codec — rtl_tcp commands are 5-byte fixed-width and
    // are not encoded under the `"RTLX"` extension. Only the
    // server→client data direction is compressed.
    let sink = stream.try_clone().map_err(SourceError::Io)?;
    if let Err(e) = sink.set_write_timeout(Some(config.data_read_timeout)) {
        tracing::warn!(%e, "set_write_timeout on command sink failed — command sends may block");
    }
    if let Ok(mut slot) = shared.command_sink.lock() {
        *slot = Some(sink);
    }
    Ok(())
}

/// Run `TcpStream::connect_timeout` on a helper thread, polling a
/// channel from the manager thread so shutdown is noticed promptly.
///
/// Iterates `addrs` in order (covers hostnames that resolve to multiple
/// A/AAAA records), first successful connect wins. On shutdown the
/// helper is abandoned — its `tx.send` becomes a no-op when `rx` drops
/// at return, and the helper thread dies naturally once its `connect`
/// call returns or times out.
pub(super) fn connect_cancellable(
    addrs: Vec<SocketAddr>,
    timeout: Duration,
    shutdown: &AtomicBool,
) -> Result<TcpStream, SourceError> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<TcpStream, std::io::Error>>();
    thread::Builder::new()
        .name("rtl_tcp-connect".into())
        .spawn(move || {
            // `rx` may already have dropped if the manager shut down
            // during our blocking connect — that's fine, the helper
            // just exits with its result thrown away.
            let _ = tx.send(connect_first_addr(addrs, timeout));
        })
        .map_err(SourceError::Io)?;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            // Abandon the helper. On return `rx` drops, the helper's
            // next `tx.send` becomes a no-op, and the helper thread
            // exits on its own once the current connect completes.
            return Err(SourceError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "manager shutdown during connect",
            )));
        }
        match rx.recv_timeout(CONNECT_SHUTDOWN_POLL) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => return Err(SourceError::Io(e)),
            // Timeout: loop back and re-check shutdown. (Empty arm —
            // fall through to the next iteration.)
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(SourceError::Io(std::io::Error::other(
                    "connect helper thread disconnected unexpectedly",
                )));
            }
        }
    }
}

/// Helper-thread body of [`connect_cancellable`]: try each resolved
/// address in order (covers hostnames with multiple A/AAAA records),
/// first successful connect wins. Split out per the 50-NLOC gate
/// (PR #880 Codacy precedent).
fn connect_first_addr(
    addrs: Vec<SocketAddr>,
    timeout: Duration,
) -> Result<TcpStream, std::io::Error> {
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no socket addresses resolved",
        )
    }))
}

fn read_exact_with_context(stream: &TcpStream, buf: &mut [u8]) -> Result<(), SourceError> {
    let mut filled = 0;
    let mut s = stream;
    while filled < buf.len() {
        match Read::read(&mut s, &mut buf[filled..]) {
            Ok(0) => {
                return Err(SourceError::Io(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            Ok(n) => filled += n,
            Err(e) => return Err(SourceError::Io(e)),
        }
    }
    Ok(())
}

/// `SO_KEEPALIVE` through `socket2` — the same option the server
/// tunes, without a raw `setsockopt` / `unsafe` block (#715).
fn set_keepalive(stream: &TcpStream, on: bool) -> std::io::Result<()> {
    socket2::SockRef::from(stream).set_keepalive(on)
}
