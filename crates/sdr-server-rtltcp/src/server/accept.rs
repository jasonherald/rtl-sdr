//! Accept-side of the server: the listener loop
//! ([`spawn_accept_thread`]), the per-connection handshake + auth +
//! role-admission setup ([`spawn_client_workers`]), denial responses,
//! client-socket tuning (keepalive / nodelay), and the RTLX hello /
//! auth-key sniffers. Split out of `server.rs` per the file-size
//! refactor (#818); pure moves, no behavior change. The sniffers,
//! denial writers, and socket tuning live in the [`handshake`]
//! child module (same refactor, second carve — the accept surface
//! alone exceeded the 500-NLOC file gate).

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use librtlsdr_rs::RtlSdrDevice;

use crate::broadcaster::{ClientRegistry, ClientSlot, RoleDecision};
use crate::codec::{Codec, CodecMask, Encoder};
use crate::extension::{PROTOCOL_VERSION_V1, Role, ServerExtension, Status};
use crate::protocol::{DongleInfo, TunerTypeCode};

pub(in crate::server) mod handshake;
use handshake::{
    configure_client_socket, send_denied_response, send_extension_only, sniff_auth_key_message,
    sniff_client_hello,
};

use super::client::{StatsTrackingWrite, command_worker, tcp_writer};
use super::{ACCEPT_ERROR_BACKOFF, ACCEPT_POLL_INTERVAL, DATA_WRITE_TIMEOUT};

/// Spawn the outer accept loop. Per accepted client:
///   1. handshake (RTLX sniff + dongle_info_t + optional ServerExtension)
///   2. build `ClientSlot` + register in the `ClientRegistry`
///   3. spawn a writer thread (drains slot.rx → encoded socket)
///   4. spawn a command thread (reads socket → dispatches to device)
///
/// No `busy` flag, no second-connection reject — that was the
/// single-client constraint #391 removes. Client lifecycle is
/// observed by the `ClientSlot::disconnected` flag; the broadcaster
/// prunes disconnected slots on its own schedule.
///
/// Returns `Err` on thread spawn failure (rare — kernel resource
/// exhaustion). Callers propagate up to the user.
#[allow(
    clippy::too_many_arguments,
    reason = "accept thread fans state into per-client workers; \
              refactoring to a context struct would churn every test"
)]
pub(super) fn spawn_accept_thread(
    listener: TcpListener,
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    per_client_buffer_depth: usize,
    compression: CodecMask,
    listener_cap: Arc<AtomicUsize>,
    auth_key: Arc<Mutex<Option<Vec<u8>>>>,
) -> std::io::Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    thread::Builder::new()
        .name("rtl_tcp-accept".into())
        .spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        tracing::info!(%peer, "rtl_tcp client connected");
                        if let Err(e) = stream.set_nonblocking(false) {
                            tracing::error!(%e, "failed to set client socket blocking");
                            continue;
                        }
                        configure_client_socket(&stream);
                        // Dispatch the blocking handshake (sniff hello +
                        // optional auth follow-up + role admission) to a
                        // short-lived per-connection setup thread. Holding
                        // it inline would let one stalled RTLX client
                        // serialize unrelated accepts for up to
                        // `HELLO_SNIFF_TIMEOUT` (0.1 s) + `AUTH_REPLY_TIMEOUT`
                        // (5 s) — a slow-peer DOS against the listener
                        // backlog. Per `CodeRabbit` round 4 on PR #405.
                        //
                        // The setup thread runs to natural completion
                        // regardless of shutdown: it either fails its
                        // handshake (fast return, no registry entry) or
                        // progresses to registering writer + command
                        // handles and exits. We register its `JoinHandle`
                        // on the same `register_worker_handle` bucket as
                        // writer/command threads so `Server::drop`
                        // joins it alongside them — bounded shutdown
                        // latency of ≤ `HELLO_SNIFF_TIMEOUT` +
                        // `AUTH_REPLY_TIMEOUT` per in-flight handshake.
                        let setup_device = device.clone();
                        let setup_registry = registry.clone();
                        let setup_shutdown = shutdown.clone();
                        // Snapshot the live listener cap + auth key
                        // Arcs into this accept's setup thread. Both
                        // are `Arc` clones (cheap), so a mid-handshake
                        // `set_*` call is visible to future accepts
                        // but does not split the current client's
                        // gate across two values (the setup thread
                        // reads the cap once at role-admission, and
                        // snapshots the auth key once at the top of
                        // `spawn_client_workers`). Per issue #395
                        // live-update design.
                        let setup_listener_cap = listener_cap.clone();
                        let setup_auth_key = auth_key.clone();
                        let setup_registry_for_register = registry.clone();
                        match thread::Builder::new()
                            .name(format!("rtl_tcp-setup-{}", peer.port()))
                            .spawn(move || {
                                spawn_client_workers(
                                    stream,
                                    peer,
                                    setup_device,
                                    setup_registry,
                                    setup_shutdown,
                                    per_client_buffer_depth,
                                    compression,
                                    setup_listener_cap,
                                    setup_auth_key,
                                );
                            }) {
                            Ok(h) => {
                                setup_registry_for_register.register_worker_handle(h);
                            }
                            Err(e) => {
                                tracing::error!(
                                    %peer,
                                    %e,
                                    "failed to spawn rtl_tcp setup thread — dropping client"
                                );
                                // `stream` drops here → bare TCP FIN.
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL_INTERVAL);
                    }
                    Err(e) => {
                        tracing::error!(%e, "rtl_tcp accept error");
                        thread::sleep(ACCEPT_ERROR_BACKOFF);
                    }
                }
            }
            // Mark stopped AFTER the loop exits so callers polling
            // `has_stopped()` observe that the accept thread is
            // gone. **Narrow semantic** — this flag is set before
            // `join_all_threads()` runs in `stop()` / `Drop`, so
            // the broadcaster + per-client workers may still be
            // running (and still holding the device mutex) when
            // `has_stopped()` first returns true. Callers that
            // need "dongle is actually free" must wait for
            // `stop()` / `Drop` to return — the CLI's
            // `while !has_stopped() ...; drop(server)` pattern
            // does exactly that. Per `CodeRabbit` round 4 on
            // PR #402 (comment clarity; no behavior change).
            stopped.store(true, Ordering::SeqCst);
            tracing::debug!("rtl_tcp accept thread exiting");
        })
}

/// Do the handshake on a freshly-accepted socket, build a
/// [`ClientSlot`], register it, and spawn this client's writer +
/// command threads. Lifecycle is observed via the slot's
/// disconnection flag.
///
/// **Runs on a per-connection setup thread**, not on the accept
/// thread. The handshake includes two blocking reads —
/// [`sniff_client_hello`] (bounded by `HELLO_SNIFF_TIMEOUT = 100 ms`)
/// and, when auth is configured and the client didn't send
/// `has_auth=true`, [`sniff_auth_key_message`] (bounded by
/// [`AUTH_REPLY_TIMEOUT`] = 5 s). Holding these on the accept thread
/// would let one stalled RTLX client serialize unrelated accepts for
/// up to ~5.1 s and pressure the listener backlog. Per `CodeRabbit`
/// round 4 on PR #405. #394.
///
/// If the handshake fails at any step (sniff error, socket clone
/// fails, header write fails, thread spawn fails), the client is
/// silently dropped — no slot is registered, no stats are updated.
/// The setup thread then exits; its `JoinHandle` is already
/// registered with the `ClientRegistry` so `Server::drop` joins
/// it alongside the per-client writer / command handles.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "accept-time client setup fans state across handshake + registry + \
              two worker threads; refactoring to a context struct would churn the \
              accept path without improving clarity"
)]
pub(super) fn spawn_client_workers(
    stream: TcpStream,
    peer: SocketAddr,
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
    per_client_buffer_depth: usize,
    compression_offer: CodecMask,
    listener_cap: Arc<AtomicUsize>,
    auth_key: Arc<Mutex<Option<Vec<u8>>>>,
) {
    // Snapshot the live-update auth key once at the top of the
    // handshake. Reading the `Arc<Mutex>` on every gate branch
    // would risk a mid-handshake `Server::set_auth_key` call
    // splitting the client's eager path (key bytes already on
    // the wire against the old expected value) vs the lazy-gate
    // follow-up (validated against the new value). Snapshot
    // semantics keep each connection bound to a single key view.
    // Per issue #395 live-update design.
    let auth_key: Option<Vec<u8>> = if let Ok(guard) = auth_key.lock() {
        guard.clone()
    } else {
        tracing::error!(
            %peer,
            "auth_key mutex poisoned during handshake — dropping client"
        );
        return;
    };

    // Extended handshake (#307) — sniff the RTLX hello if the
    // client sent one. The outcome drives both codec negotiation
    // and the role request that feeds the #392 admission gate.
    let sniff_outcome = match sniff_client_hello(&stream) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(%peer, %e, "rtl_tcp handshake sniff failed — dropping client");
            return;
        }
    };
    // Split the sniff result into the fields we actually act on:
    //   - `hello_seen` gates ServerExtension emission (vanilla
    //     clients don't expect it; writing one would corrupt their
    //     stream)
    //   - `requested_role` is what the client asked for; vanilla
    //     clients implicitly request `Control` since they have no
    //     way to ask for Listen (no hello = no role byte)
    //   - `negotiated_codec` is `None` for vanilla (always
    //     uncompressed); `Some(_)` for RTLX clients — the
    //     intersection of their mask and ours
    let (hello_seen, requested_role, request_takeover, has_auth, hello_version, negotiated_codec) =
        if let Some(hello) = &sniff_outcome {
            let codec = compression_offer.pick(hello.codec_mask);
            let takeover = hello.request_takeover();
            let has_auth = hello.has_auth();
            tracing::info!(
                %peer,
                client_mask = hello.codec_mask.to_wire(),
                server_mask = compression_offer.to_wire(),
                chosen = %codec,
                requested_role = ?hello.role,
                request_takeover = takeover,
                has_auth,
                hello_version = hello.version,
                "rtl_tcp extended-handshake negotiated"
            );
            (
                true,
                hello.role,
                takeover,
                has_auth,
                hello.version,
                Some(codec),
            )
        } else {
            tracing::debug!(
                %peer,
                "rtl_tcp no extended-handshake hello — legacy client path (implicit Role::Control)"
            );
            // Vanilla clients have no way to set the takeover
            // flag, so the admission gate treats them as
            // "request_takeover = false" — the existing Control
            // client (if any) is protected from vanilla-driven
            // displacement. Takeover is an explicit RTLX action.
            // Same logic applies to `has_auth`: vanilla clients
            // never carry an AuthKeyMessage follow-up, so `false`
            // here routes the auth gate's vanilla+auth-required
            // path to a bare TCP FIN (they can't authenticate).
            // Vanilla clients never receive a `ServerExtension`
            // either, so `hello_version` is nominal here — use
            // `PROTOCOL_VERSION_V1` as a neutral default; it's
            // never written to the wire on the vanilla path.
            (
                false,
                Role::Control,
                false,
                false,
                PROTOCOL_VERSION_V1,
                None,
            )
        };
    let codec = negotiated_codec.unwrap_or(Codec::None);

    // Auth gate (#394). Runs BEFORE role admission — an
    // unauthenticated client shouldn't even be evaluated for role
    // because the wire response would leak server state (role
    // grants or cap status) to an attacker probing the slot. The
    // four outcomes:
    //
    //   - No auth required: skip entirely, fall through to role
    //     admission as-is.
    //   - Auth required + vanilla client: bare TCP FIN, no bytes.
    //     Vanilla has no wire field to carry a key, so they can't
    //     participate. Same signal as "server not there" from
    //     the legacy client's POV.
    //   - Auth required + RTLX + has_auth (eager): read the
    //     follow-up `AuthKeyMessage` with a bounded timeout,
    //     constant-time compare against the configured key.
    //     - Match: continue to role admission.
    //     - Mismatch / malformed / timeout / eof: send dongle_info_t
    //       + `ServerExtension(status=AuthFailed)` and close.
    //   - Auth required + RTLX + !has_auth (lazy, per #394 spec):
    //     send dongle_info_t + `ServerExtension(status=AuthRequired)`
    //     KEEPING THE SOCKET OPEN, then wait up to
    //     `AUTH_REPLY_TIMEOUT` for the client to deliver an
    //     `AuthKeyMessage` on the same connection. On match,
    //     fall through to role admission; the granted path
    //     then resends the ServerExtension with the actual role
    //     status WITHOUT re-emitting dongle_info_t (the client
    //     would misread it as a second handshake). On mismatch
    //     / timeout / parse error, send a follow-up
    //     `ServerExtension(status=AuthFailed)` (extension-only,
    //     no second dongle_info_t) and close. Per `CodeRabbit`
    //     round 3 on PR #405.
    //
    // If `has_auth` is set but auth isn't configured, we STILL
    // read the AuthKeyMessage from the stream to keep the
    // post-hello byte position in sync (the client doesn't know
    // our config and sent the key based on its own); we just
    // discard it without validation.
    //
    // `dongle_info_sent` threads through the rest of this function
    // so the lazy path's initial dongle_info_t emission is visible
    // to role admission (which must skip the duplicate send in
    // both the granted and denied follow-up branches).
    let mut dongle_info_sent = false;
    if hello_seen && has_auth {
        // Read the AuthKeyMessage follow-up regardless of whether
        // auth is required — need to consume the bytes either way
        // so the stream position stays correct.
        let auth_result = sniff_auth_key_message(&stream);
        match auth_result {
            Ok(msg) => {
                if let Some(expected) = auth_key.as_deref() {
                    if !crate::auth::validate_auth_key(&msg.key, expected) {
                        tracing::info!(
                            %peer,
                            "rtl_tcp auth key mismatch — denying client"
                        );
                        send_denied_response(
                            &stream,
                            peer,
                            &device,
                            Status::AuthFailed,
                            hello_version,
                        );
                        return;
                    }
                    tracing::info!(%peer, "rtl_tcp auth key validated");
                } else {
                    // Client sent a key to a server that doesn't
                    // require one — fine, just ignore it. Keeps
                    // the wire-protocol compat flexible: clients
                    // can always send has_auth=true without
                    // knowing the server's config.
                    tracing::debug!(
                        %peer,
                        "rtl_tcp client sent auth key but server doesn't require one — ignored"
                    );
                }
            }
            Err(e) => {
                tracing::info!(
                    %peer,
                    %e,
                    "rtl_tcp auth key follow-up unreadable — denying client"
                );
                if auth_key.is_some() {
                    send_denied_response(&stream, peer, &device, Status::AuthFailed, hello_version);
                }
                // If auth wasn't required but the client promised
                // a follow-up (has_auth=true) and didn't deliver,
                // the stream position is wrong either way — drop.
                return;
            }
        }
    } else if let Some(expected) = auth_key.as_deref() {
        // Client didn't set has_auth but the server requires auth.
        if hello_seen {
            // RTLX client — lazy path per #394 spec. Send
            // dongle_info_t + `ServerExtension(AuthRequired)`
            // and keep the socket open so a compliant client
            // can reply with `AuthKeyMessage` on the same
            // connection. The peer has
            // `AUTH_REPLY_TIMEOUT` to deliver the key;
            // `sniff_auth_key_message` enforces the bound with
            // an absolute deadline. Per `CodeRabbit` round 3
            // on PR #405.
            tracing::info!(
                %peer,
                "rtl_tcp auth required but client didn't send key — sending AuthRequired (lazy path)"
            );
            send_denied_response(&stream, peer, &device, Status::AuthRequired, hello_version);
            dongle_info_sent = true;

            let auth_ok = match sniff_auth_key_message(&stream) {
                Ok(msg) => {
                    if crate::auth::validate_auth_key(&msg.key, expected) {
                        tracing::info!(
                            %peer,
                            "rtl_tcp lazy auth key validated"
                        );
                        true
                    } else {
                        tracing::info!(
                            %peer,
                            "rtl_tcp lazy auth key mismatch — denying"
                        );
                        false
                    }
                }
                Err(e) => {
                    tracing::info!(
                        %peer,
                        %e,
                        "rtl_tcp lazy auth follow-up unreadable — denying"
                    );
                    false
                }
            };
            if !auth_ok {
                // dongle_info_t already on the wire — send only
                // the 8-byte `ServerExtension(AuthFailed)` so
                // the client doesn't misread a duplicate header
                // as a second handshake.
                send_extension_only(&stream, peer, Status::AuthFailed, hello_version);
                return;
            }
            // Match → fall through to role admission. The granted
            // path below observes `dongle_info_sent = true` and
            // skips the duplicate header write; any role-denial
            // (ControllerBusy / ListenerCapReached) similarly
            // switches to `send_extension_only`.
        } else {
            // Vanilla client: can't authenticate, so there's
            // nothing meaningful to tell them. Bare TCP FIN.
            tracing::info!(
                %peer,
                "rtl_tcp vanilla client denied — auth required"
            );
            return;
        }
    }

    // Allocate id + build slot with the requested role + channel.
    // The slot is not yet registered — `register_with_role` below
    // takes the slots mutex and checks the role/cap atomically
    // before admitting.
    let id = registry.allocate_id();
    let (slot, rx) = ClientSlot::new(id, peer, codec, requested_role, per_client_buffer_depth);

    // Atomic #392 admission + #393 takeover decision. On `Granted`
    // or `GrantedViaTakeover` the slot is now in the registry and
    // the broadcaster can find it on its next tick; on denial the
    // slot is never pushed and drops on scope exit. Takeover also
    // marks the displaced controller disconnected under the same
    // lock so its writer / command threads exit cleanly.
    //
    // Read the listener cap from the live-update Arc ONCE here so
    // a mid-decision `Server::set_listener_cap` call doesn't split
    // the "is there room?" check across two values. Per issue #395.
    let cap = listener_cap.load(Ordering::Relaxed);
    let decision = registry.register_with_role(slot.clone(), cap, request_takeover);
    let displaced_id = match decision {
        RoleDecision::GrantedViaTakeover { displaced_id } => Some(displaced_id),
        _ => None,
    };
    match decision {
        RoleDecision::Granted => {
            tracing::info!(
                %peer,
                client_id = id,
                ?requested_role,
                ?codec,
                "rtl_tcp client admitted"
            );
        }
        RoleDecision::GrantedViaTakeover { displaced_id } => {
            tracing::info!(
                %peer,
                client_id = id,
                displaced_client_id = displaced_id,
                ?codec,
                "rtl_tcp client admitted via takeover — prior Control client kicked"
            );
        }
        RoleDecision::ControllerBusy => {
            tracing::info!(
                %peer,
                ?requested_role,
                "rtl_tcp Control slot busy — denying client"
            );
            // RTLX clients get the full denial response so their UI
            // can show "controller busy" rather than a bare EOF.
            // Vanilla clients get TCP FIN with no bytes — cleanest
            // signal for their "connection refused" UX and avoids
            // handing them a dongle_info_t they'd interpret as
            // admission.
            //
            // If the lazy auth path already emitted dongle_info_t
            // (#394 round 3 on PR #405), send only the 8-byte
            // ServerExtension follow-up — a duplicate header
            // would desync the client's parser.
            if hello_seen {
                if dongle_info_sent {
                    send_extension_only(&stream, peer, Status::ControllerBusy, hello_version);
                } else {
                    send_denied_response(
                        &stream,
                        peer,
                        &device,
                        Status::ControllerBusy,
                        hello_version,
                    );
                }
            }
            return;
        }
        RoleDecision::ListenerCapReached => {
            tracing::info!(
                %peer,
                ?requested_role,
                cap = cap,
                "rtl_tcp listener cap reached — denying client"
            );
            // Vanilla clients never land here — they always request
            // (implicit) Control, which routes through the
            // ControllerBusy path above. Defensive debug_assert
            // catches any future regression that breaks that
            // invariant; runtime behavior stays safe (TCP FIN
            // only) even if the assert fires.
            debug_assert!(
                hello_seen,
                "vanilla clients should never land in ListenerCapReached"
            );
            if hello_seen {
                if dongle_info_sent {
                    send_extension_only(&stream, peer, Status::ListenerCapReached, hello_version);
                } else {
                    send_denied_response(
                        &stream,
                        peer,
                        &device,
                        Status::ListenerCapReached,
                        hello_version,
                    );
                }
            }
            return;
        }
        RoleDecision::RegistryPoisoned => {
            // Terminal server fault — the slot list mutex was
            // poisoned by an earlier panic mid-update. Don't
            // write anything to the peer: RTLX clients treat
            // `ControllerBusy` as transient and retry, so
            // sending that (or any other denial) would invite a
            // reconnect storm against a terminally broken
            // server. Bare TCP FIN is the cleanest signal. Per
            // `CodeRabbit` round 1 on PR #403.
            tracing::error!(
                %peer,
                ?requested_role,
                "rtl_tcp registry slots mutex poisoned — closing client without reply"
            );
            return;
        }
    }

    // Granted path — slot is now in the registry. The broadcaster
    // can begin fan-out as soon as its next tick runs; any chunks
    // that arrive before the writer thread spawns queue in the
    // bounded channel and get recorded as per-client `buffers_dropped`
    // if the channel fills first. Worker spawn is microseconds
    // away so the drop risk is negligible in practice.
    let writer_stream = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(
                %peer,
                %e,
                "failed to clone client stream for writer — tearing down client"
            );
            registry.unwind_admission(&slot);
            return;
        }
    };
    let mut writer = writer_stream;

    // Send the 12-byte dongle_info_t header (rtl_tcp.c:576-594).
    // Emitted for BOTH granted RTLX and granted vanilla — it's the
    // first thing any rtl_tcp client expects.
    //
    // Lazy-auth (#394) skips this: the header was already emitted
    // alongside the initial `ServerExtension(AuthRequired)`
    // challenge, and writing it again would make the client
    // mis-parse the second dongle_info_t as a second handshake.
    // Per `CodeRabbit` round 3 on PR #405.
    if !dongle_info_sent {
        let header = {
            let Ok(dev) = device.lock() else {
                tracing::error!(%peer, "device mutex poisoned, aborting client");
                registry.unwind_admission(&slot);
                return;
            };
            DongleInfo {
                tuner: TunerTypeCode::from(dev.tuner_type()),
                gain_count: dev.tuner_gains().len() as u32,
            }
        };
        if let Err(e) = writer.write_all(&header.to_bytes()) {
            tracing::warn!(%peer, %e, "failed to send dongle_info_t — client gone");
            registry.unwind_admission(&slot);
            return;
        }
    }

    // RTLX clients additionally get the ServerExtension(granted)
    // block. Must land immediately after dongle_info_t so the
    // client's magic-peek lands on our bytes and not on IQ samples
    // a racing broadcaster may have queued.
    if hello_seen {
        let ext = ServerExtension {
            codec,
            granted_role: Some(requested_role),
            status: Status::Ok,
            // Echo the client's hello version on the response so
            // v1 clients interoperate with this v2-era server
            // without hitting the peer-side strict version gate.
            // Per `CodeRabbit` round 1 on PR #405.
            version: hello_version,
        };
        if let Err(e) = writer.write_all(&ext.to_bytes()) {
            tracing::warn!(%peer, %e, "failed to send RTLX server extension — client gone");
            registry.unwind_admission(&slot);
            return;
        }
    }

    // Install the write timeout BEFORE wrapping in the codec's
    // encoder — the encoder's `write()` delegates to the inner
    // stream's `write()`, which in turn enforces `SO_SNDTIMEO`.
    // Setting after-wrap would lose visibility into the inner stream.
    if let Err(e) = writer.set_write_timeout(Some(DATA_WRITE_TIMEOUT)) {
        tracing::warn!(
            %peer,
            %e,
            "set_write_timeout on data channel failed; tearing down client"
        );
        registry.unwind_admission(&slot);
        return;
    }

    // Spawn writer + command threads. Pre-#392 spawn-before-register
    // ordering is inverted here (register happens during the
    // decision above) so every failure path from this point on
    // must call `registry.unwind_admission(&slot)` — that marks
    // the slot disconnected AND rolls back the admission so
    // `lifetime_accepted` stays tied to sessions that actually
    // began serving. Per `CodeRabbit` round 1 on PR #403.
    let writer_slot = slot.clone();
    let writer_registry = registry.clone();
    let writer_shutdown = shutdown.clone();
    // Only a pass-through stream can resume a chunk mid-way after a
    // send stall; a compressed stream's encoder state cannot be
    // rewound (#709, CR on PR #807).
    let retry_stalls = codec == Codec::None;
    let tracked_writer = StatsTrackingWrite {
        inner: writer,
        slot: slot.clone(),
        registry: registry.clone(),
    };
    let encoded_writer = Encoder::new(codec, tracked_writer);
    let writer_handle = match thread::Builder::new()
        .name(format!("rtl_tcp-writer-{id}"))
        .spawn(move || {
            tcp_writer(
                encoded_writer,
                rx,
                writer_slot,
                writer_registry,
                writer_shutdown,
                retry_stalls,
            );
        }) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(
                %peer,
                %e,
                "failed to spawn rtl_tcp writer thread — tearing down client"
            );
            registry.unwind_admission(&slot);
            return;
        }
    };

    // Spawn the command thread. If it fails, unwind the admission
    // (also marks the slot disconnected so the writer exits) and
    // join the writer here so its handle isn't dropped on the floor.
    let command_slot = slot.clone();
    let command_shutdown = shutdown.clone();
    let command_device = device;
    let command_stream = stream;
    let command_handle = match thread::Builder::new()
        .name(format!("rtl_tcp-command-{id}"))
        .spawn(move || {
            command_worker(
                command_stream,
                command_device,
                command_slot,
                command_shutdown,
            );
        }) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(
                %peer,
                %e,
                "failed to spawn rtl_tcp command thread — tearing down client"
            );
            registry.unwind_admission(&slot);
            let _ = writer_handle.join();
            return;
        }
    };

    // Park both worker handles on the registry so `Server::drop`
    // can join any still running at shutdown — without this, the
    // threads' `Arc<Mutex<RtlSdrDevice>>` clones could outlive
    // `has_stopped() == true` and leave the dongle claimed for a
    // follow-up `Server::start`. During normal runtime the
    // broadcaster calls `reap_finished_worker_handles()` on its
    // prune cadence so completed handles from disconnected
    // clients get joined promptly and don't accumulate under
    // connection churn. Per `CodeRabbit` round 1 on PR #402
    // (shutdown join) + round 5 (runtime reap).
    registry.register_worker_handle(writer_handle);
    registry.register_worker_handle(command_handle);

    // Takeover phase 2 (#710): only now that the newcomer is fully
    // viable — header sent, timeout installed, both workers running
    // — is the incumbent controller displaced. Every early return
    // above left it in place.
    if let Some(displaced_id) = displaced_id
        && !registry.commit_takeover(id, displaced_id)
    {
        tracing::debug!(
            client_id = id,
            displaced_client_id = displaced_id,
            "takeover commit found the displaced controller already gone"
        );
    }

    // Fire and forget — neither the writer nor the command handle is
    // joined here. Both exit independently when they observe the
    // shutdown flag or the slot's disconnection flag. The slot itself
    // is retained by the registry until it's pruned.
}
