//! Per-connection client setup: the staged handshake → auth gate →
//! role admission → granted-handshake → worker-spawn pipeline that
//! runs on each short-lived `rtl_tcp-setup-*` thread. Split out of
//! `accept.rs` per the 50-NLOC function gate (Codacy round on
//! PR #880) — [`spawn_client_workers`] was a single 358-NLOC
//! function; it is now the orchestrator over one helper per stage,
//! with [`HelloOutcome`] / [`AdmittedClient`] carrying the state
//! between stages. Behavior is unchanged.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use librtlsdr_rs::RtlSdrDevice;

use crate::broadcaster::{ClientRegistry, ClientSlot, RoleDecision};
use crate::codec::{Codec, CodecMask, Encoder};
use crate::extension::{PROTOCOL_VERSION_V1, Role, ServerExtension, Status};
use crate::protocol::{DongleInfo, TunerTypeCode};

mod auth;
use auth::run_auth_gate;

use super::ClientSetupDeps;
use super::handshake::{send_denied_response, send_extension_only, sniff_client_hello};
use crate::server::DATA_WRITE_TIMEOUT;
use crate::server::client::{StatsTrackingWrite, command_worker, tcp_writer};

/// What the RTLX hello sniff decided for this connection. One value
/// per accepted client, threaded through the auth gate, role
/// admission, and the granted-handshake writes so every stage acts
/// on the same negotiated view. Split out of the former inline
/// tuple per the 50-NLOC gate (Codacy round on PR #880).
struct HelloOutcome {
    /// `true` when a valid RTLX hello was consumed — gates
    /// `ServerExtension` emission (vanilla clients don't expect it;
    /// writing one would corrupt their stream).
    seen: bool,
    /// What the client asked for; vanilla clients implicitly request
    /// `Control` since they have no way to ask for Listen (no hello
    /// = no role byte).
    role: Role,
    /// RTLX takeover-request flag; always `false` for vanilla —
    /// takeover is an explicit RTLX action.
    request_takeover: bool,
    /// Client promised an `AuthKeyMessage` follow-up. Always `false`
    /// for vanilla clients (no wire field to carry a key).
    has_auth: bool,
    /// Hello version to echo on responses; nominal
    /// `PROTOCOL_VERSION_V1` on the vanilla path (never written to
    /// the wire there).
    version: u8,
    /// Negotiated codec — the intersection of the client's mask and
    /// ours for RTLX clients, `Codec::None` (always uncompressed)
    /// for vanilla.
    codec: Codec,
}

/// The role-admission result for a granted client: the registered
/// slot plus its receive channel and the takeover bookkeeping the
/// commit phase needs. Split out per the 50-NLOC gate (Codacy round
/// on PR #880).
struct AdmittedClient {
    slot: Arc<ClientSlot>,
    rx: Receiver<Vec<u8>>,
    id: u64,
    /// `Some` when admission happened via takeover — the displaced
    /// controller's id, committed only after the newcomer is fully
    /// viable (#710).
    displaced_id: Option<u64>,
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
/// `AUTH_REPLY_TIMEOUT` = 5 s). Holding these on the accept thread
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
pub(in crate::server) fn spawn_client_workers(
    stream: TcpStream,
    peer: SocketAddr,
    deps: ClientSetupDeps,
) {
    let Ok(auth_key) = snapshot_auth_key(&deps, peer) else {
        return;
    };
    let Some(hello) = negotiate_hello(&stream, peer, deps.compression) else {
        return;
    };
    let Some(dongle_info_sent) =
        run_auth_gate(&stream, peer, &deps.device, auth_key.as_deref(), &hello)
    else {
        return;
    };
    let Some(admitted) = admit_client(&stream, peer, &deps, &hello, dongle_info_sent) else {
        return;
    };
    let AdmittedClient {
        slot,
        rx,
        id,
        displaced_id,
    } = admitted;
    let Some(mut writer) = open_granted_writer(&stream, peer, &deps.registry, &slot) else {
        return;
    };
    if !send_granted_handshake(
        &mut writer,
        peer,
        &deps.device,
        &deps.registry,
        &slot,
        &hello,
        dongle_info_sent,
    ) {
        return;
    }
    if !spawn_worker_threads(writer, stream, &deps, &slot, rx, id, hello.codec, peer) {
        return;
    }
    commit_takeover_if_any(&deps.registry, id, displaced_id);

    // Fire and forget — neither the writer nor the command handle is
    // joined here. Both exit independently when they observe the
    // shutdown flag or the slot's disconnection flag. The slot itself
    // is retained by the registry until it's pruned.
}

/// Snapshot the live-update auth key once at the top of the
/// handshake. Reading the `Arc<Mutex>` on every gate branch would
/// risk a mid-handshake `Server::set_auth_key` call splitting the
/// client's eager path (key bytes already on the wire against the
/// old expected value) vs the lazy-gate follow-up (validated
/// against the new value). Snapshot semantics keep each connection
/// bound to a single key view. Per issue #395 live-update design.
/// `Err(())` = poisoned mutex (already logged), drop the client;
/// the `Ok` payload is the configured key (`None` = auth off).
/// Split out per the 50-NLOC gate (Codacy round on PR #880).
fn snapshot_auth_key(deps: &ClientSetupDeps, peer: SocketAddr) -> Result<Option<Vec<u8>>, ()> {
    if let Ok(guard) = deps.auth_key.lock() {
        Ok(guard.clone())
    } else {
        tracing::error!(
            %peer,
            "auth_key mutex poisoned during handshake — dropping client"
        );
        Err(())
    }
}

/// Takeover phase 2 (#710): only once the newcomer is fully viable
/// — header sent, timeout installed, both workers running — is the
/// incumbent controller displaced. Every early return in
/// [`spawn_client_workers`] leaves it in place. Split out per the
/// 50-NLOC gate (Codacy round on PR #880).
fn commit_takeover_if_any(registry: &Arc<ClientRegistry>, id: u64, displaced_id: Option<u64>) {
    if let Some(displaced_id) = displaced_id
        && !registry.commit_takeover(id, displaced_id)
    {
        tracing::debug!(
            client_id = id,
            displaced_client_id = displaced_id,
            "takeover commit found the displaced controller already gone"
        );
    }
}

/// Extended handshake (#307) — sniff the RTLX hello if the client
/// sent one. The outcome drives both codec negotiation and the role
/// request that feeds the #392 admission gate. `None` = sniff error;
/// the caller drops the client (already logged here). Split out per
/// the 50-NLOC gate (Codacy round on PR #880).
fn negotiate_hello(
    stream: &TcpStream,
    peer: SocketAddr,
    compression_offer: CodecMask,
) -> Option<HelloOutcome> {
    let sniff_outcome = match sniff_client_hello(stream) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(%peer, %e, "rtl_tcp handshake sniff failed — dropping client");
            return None;
        }
    };
    // Split the sniff result into the fields we actually act on —
    // see the [`HelloOutcome`] field docs for what each one gates.
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
        Some(HelloOutcome {
            seen: true,
            role: hello.role,
            request_takeover: takeover,
            has_auth,
            version: hello.version,
            codec,
        })
    } else {
        tracing::debug!(
            %peer,
            "rtl_tcp no extended-handshake hello — legacy client path (implicit Role::Control)"
        );
        // Vanilla clients have no way to set the takeover flag, so
        // the admission gate treats them as "request_takeover =
        // false" — the existing Control client (if any) is protected
        // from vanilla-driven displacement. Takeover is an explicit
        // RTLX action. Same logic applies to `has_auth`: vanilla
        // clients never carry an AuthKeyMessage follow-up, so
        // `false` here routes the auth gate's vanilla+auth-required
        // path to a bare TCP FIN (they can't authenticate). Vanilla
        // clients never receive a `ServerExtension` either, so
        // `version` is nominal here — `PROTOCOL_VERSION_V1` as a
        // neutral default; it's never written to the wire on the
        // vanilla path. `codec` is `Codec::None`: vanilla is always
        // uncompressed.
        Some(HelloOutcome {
            seen: false,
            role: Role::Control,
            request_takeover: false,
            has_auth: false,
            version: PROTOCOL_VERSION_V1,
            codec: Codec::None,
        })
    }
}

/// Allocate an id, build the slot with the requested role + channel,
/// and run the atomic #392 admission + #393 takeover decision. On
/// `Granted` or `GrantedViaTakeover` the slot is now in the registry
/// and the broadcaster can find it on its next tick; on denial the
/// slot is never pushed and drops on scope exit (denial response
/// sent here). Takeover also marks the displaced controller
/// disconnected under the same lock so its writer / command threads
/// exit cleanly.
///
/// Reads the listener cap from the live-update Arc ONCE so a
/// mid-decision `Server::set_listener_cap` call doesn't split the
/// "is there room?" check across two values. Per issue #395. Split
/// out per the 50-NLOC gate (Codacy round on PR #880).
fn admit_client(
    stream: &TcpStream,
    peer: SocketAddr,
    deps: &ClientSetupDeps,
    hello: &HelloOutcome,
    dongle_info_sent: bool,
) -> Option<AdmittedClient> {
    let id = deps.registry.allocate_id();
    let (slot, rx) = ClientSlot::new(
        id,
        peer,
        hello.codec,
        hello.role,
        deps.per_client_buffer_depth,
    );

    let cap = deps.listener_cap.load(Ordering::Relaxed);
    let decision = deps
        .registry
        .register_with_role(slot.clone(), cap, hello.request_takeover);
    let displaced_id = match &decision {
        RoleDecision::Granted => None,
        RoleDecision::GrantedViaTakeover { displaced_id } => Some(*displaced_id),
        RoleDecision::ControllerBusy | RoleDecision::ListenerCapReached => {
            reject_admission(stream, peer, deps, hello, dongle_info_sent, cap, &decision);
            return None;
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
                requested_role = ?hello.role,
                "rtl_tcp registry slots mutex poisoned — closing client without reply"
            );
            return None;
        }
    };
    log_admission_granted(peer, id, hello, displaced_id);
    Some(AdmittedClient {
        slot,
        rx,
        id,
        displaced_id,
    })
}

/// Admission-granted logging — the plain and via-takeover forms.
/// Split out of [`admit_client`] per the 50-NLOC gate (Codacy round
/// on PR #880).
fn log_admission_granted(peer: SocketAddr, id: u64, hello: &HelloOutcome, displaced: Option<u64>) {
    match displaced {
        None => {
            tracing::info!(
                %peer,
                client_id = id,
                requested_role = ?hello.role,
                codec = ?hello.codec,
                "rtl_tcp client admitted"
            );
        }
        Some(displaced_id) => {
            tracing::info!(
                %peer,
                client_id = id,
                displaced_client_id = displaced_id,
                codec = ?hello.codec,
                "rtl_tcp client admitted via takeover — prior Control client kicked"
            );
        }
    }
}

/// Log + respond to a role-admission denial (`ControllerBusy` /
/// `ListenerCapReached`). RTLX clients get the full denial response
/// so their UI can show "controller busy" rather than a bare EOF;
/// vanilla clients get TCP FIN with no bytes — cleanest signal for
/// their "connection refused" UX and avoids handing them a
/// dongle_info_t they'd interpret as admission. Split out of
/// [`admit_client`] per the 50-NLOC gate (Codacy round on PR #880).
fn reject_admission(
    stream: &TcpStream,
    peer: SocketAddr,
    deps: &ClientSetupDeps,
    hello: &HelloOutcome,
    dongle_info_sent: bool,
    cap: usize,
    decision: &RoleDecision,
) {
    let status = if matches!(decision, RoleDecision::ControllerBusy) {
        tracing::info!(
            %peer,
            requested_role = ?hello.role,
            "rtl_tcp Control slot busy — denying client"
        );
        Status::ControllerBusy
    } else {
        tracing::info!(
            %peer,
            requested_role = ?hello.role,
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
            hello.seen,
            "vanilla clients should never land in ListenerCapReached"
        );
        Status::ListenerCapReached
    };
    if hello.seen {
        send_role_denial(
            stream,
            peer,
            &deps.device,
            status,
            dongle_info_sent,
            hello.version,
        );
    }
}

/// Send a role-admission denial to an RTLX client, picking the
/// right wire shape: if the lazy auth path already emitted
/// dongle_info_t (#394 round 3 on PR #405), send only the 8-byte
/// ServerExtension follow-up — a duplicate header would desync the
/// client's parser. Split out per the 50-NLOC gate (Codacy round on
/// PR #880); both denial arms in [`admit_client`] previously
/// duplicated this branch verbatim.
fn send_role_denial(
    stream: &TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    status: Status,
    dongle_info_sent: bool,
    hello_version: u8,
) {
    if dongle_info_sent {
        send_extension_only(stream, peer, status, hello_version);
    } else {
        send_denied_response(stream, peer, device, status, hello_version);
    }
}

/// Granted path — clone the socket for the writer half. The slot is
/// already in the registry, so the broadcaster can begin fan-out as
/// soon as its next tick runs; any chunks that arrive before the
/// writer thread spawns queue in the bounded channel and get
/// recorded as per-client `buffers_dropped` if the channel fills
/// first. Worker spawn is microseconds away so the drop risk is
/// negligible in practice. On clone failure the admission is
/// unwound. Split out per the 50-NLOC gate (Codacy round on
/// PR #880).
fn open_granted_writer(
    stream: &TcpStream,
    peer: SocketAddr,
    registry: &Arc<ClientRegistry>,
    slot: &Arc<ClientSlot>,
) -> Option<TcpStream> {
    match stream.try_clone() {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::error!(
                %peer,
                %e,
                "failed to clone client stream for writer — tearing down client"
            );
            registry.unwind_admission(slot);
            None
        }
    }
}

/// Send the granted-path handshake bytes and install the data-write
/// timeout. Returns `false` (admission already unwound) when the
/// client is gone. Split out per the 50-NLOC gate (Codacy round on
/// PR #880).
///
/// The 12-byte dongle_info_t header (rtl_tcp.c:576-594) is emitted
/// for BOTH granted RTLX and granted vanilla — it's the first thing
/// any rtl_tcp client expects. Lazy-auth (#394) skips this: the
/// header was already emitted alongside the initial
/// `ServerExtension(AuthRequired)` challenge, and writing it again
/// would make the client mis-parse the second dongle_info_t as a
/// second handshake. Per `CodeRabbit` round 3 on PR #405.
///
/// RTLX clients additionally get the ServerExtension(granted)
/// block. Must land immediately after dongle_info_t so the client's
/// magic-peek lands on our bytes and not on IQ samples a racing
/// broadcaster may have queued.
///
/// The write timeout is installed BEFORE the caller wraps the
/// stream in the codec's encoder — the encoder's `write()`
/// delegates to the inner stream's `write()`, which in turn
/// enforces `SO_SNDTIMEO`. Setting after-wrap would lose visibility
/// into the inner stream.
fn send_granted_handshake(
    writer: &mut TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    registry: &Arc<ClientRegistry>,
    slot: &Arc<ClientSlot>,
    hello: &HelloOutcome,
    dongle_info_sent: bool,
) -> bool {
    if !dongle_info_sent && !send_dongle_info(writer, peer, device, registry, slot) {
        return false;
    }

    if hello.seen {
        let ext = ServerExtension {
            codec: hello.codec,
            granted_role: Some(hello.role),
            status: Status::Ok,
            // Echo the client's hello version on the response so
            // v1 clients interoperate with this v2-era server
            // without hitting the peer-side strict version gate.
            // Per `CodeRabbit` round 1 on PR #405.
            version: hello.version,
        };
        if let Err(e) = writer.write_all(&ext.to_bytes()) {
            tracing::warn!(%peer, %e, "failed to send RTLX server extension — client gone");
            registry.unwind_admission(slot);
            return false;
        }
    }

    if let Err(e) = writer.set_write_timeout(Some(DATA_WRITE_TIMEOUT)) {
        tracing::warn!(
            %peer,
            %e,
            "set_write_timeout on data channel failed; tearing down client"
        );
        registry.unwind_admission(slot);
        return false;
    }
    true
}

/// Build + write the 12-byte dongle_info_t header. `false`
/// (admission unwound) on a poisoned device mutex or a dead peer.
/// Split out of [`send_granted_handshake`] per the 50-NLOC gate
/// (Codacy round on PR #880).
fn send_dongle_info(
    writer: &mut TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    registry: &Arc<ClientRegistry>,
    slot: &Arc<ClientSlot>,
) -> bool {
    let header = {
        let Ok(dev) = device.lock() else {
            tracing::error!(%peer, "device mutex poisoned, aborting client");
            registry.unwind_admission(slot);
            return false;
        };
        DongleInfo {
            tuner: TunerTypeCode::from(dev.tuner_type()),
            gain_count: dev.tuner_gains().len() as u32,
        }
    };
    if let Err(e) = writer.write_all(&header.to_bytes()) {
        tracing::warn!(%peer, %e, "failed to send dongle_info_t — client gone");
        registry.unwind_admission(slot);
        return false;
    }
    true
}

/// Spawn the writer + command threads for a granted client and park
/// both handles on the registry so `Server::drop` can join any
/// still running at shutdown — without this, the threads'
/// `Arc<Mutex<RtlSdrDevice>>` clones could outlive
/// `has_stopped() == true` and leave the dongle claimed for a
/// follow-up `Server::start`. During normal runtime the broadcaster
/// calls `reap_finished_worker_handles()` on its prune cadence so
/// completed handles from disconnected clients get joined promptly
/// and don't accumulate under connection churn. Per `CodeRabbit`
/// round 1 on PR #402 (shutdown join) + round 5 (runtime reap).
///
/// Pre-#392 spawn-before-register ordering is inverted (register
/// happens during the admission decision) so every failure path
/// here calls `registry.unwind_admission(&slot)` — that marks the
/// slot disconnected AND rolls back the admission so
/// `lifetime_accepted` stays tied to sessions that actually began
/// serving. Per `CodeRabbit` round 1 on PR #403. Returns `false`
/// on spawn failure (admission unwound; a spawned writer is joined
/// so its handle isn't dropped on the floor). Split out per the
/// 50-NLOC gate (Codacy round on PR #880).
#[allow(
    clippy::too_many_arguments,
    reason = "eight is the Codacy limit exactly; the writer/command spawn \
              stage needs the socket pair plus the admitted client's parts, \
              and a second bundling struct for one call site would obscure \
              the move of `rx` into the writer thread"
)]
fn spawn_worker_threads(
    writer: TcpStream,
    command_stream: TcpStream,
    deps: &ClientSetupDeps,
    slot: &Arc<ClientSlot>,
    rx: Receiver<Vec<u8>>,
    id: u64,
    codec: Codec,
    peer: SocketAddr,
) -> bool {
    let Some(writer_handle) = spawn_writer_thread(writer, deps, slot, rx, id, codec, peer) else {
        return false;
    };

    // Spawn the command thread. If it fails, unwind the admission
    // (also marks the slot disconnected so the writer exits) and
    // join the writer here so its handle isn't dropped on the floor.
    let command_slot = slot.clone();
    let command_shutdown = deps.shutdown.clone();
    let command_device = deps.device.clone();
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
            deps.registry.unwind_admission(slot);
            let _ = writer_handle.join();
            return false;
        }
    };

    deps.registry.register_worker_handle(writer_handle);
    deps.registry.register_worker_handle(command_handle);
    true
}

/// Build the tracked + encoded writer chain and spawn the writer
/// thread. `None` (admission unwound) on spawn failure. Split out
/// of [`spawn_worker_threads`] per the 50-NLOC gate (Codacy round
/// on PR #880).
fn spawn_writer_thread(
    writer: TcpStream,
    deps: &ClientSetupDeps,
    slot: &Arc<ClientSlot>,
    rx: Receiver<Vec<u8>>,
    id: u64,
    codec: Codec,
    peer: SocketAddr,
) -> Option<std::thread::JoinHandle<()>> {
    let writer_slot = slot.clone();
    let writer_registry = deps.registry.clone();
    let writer_shutdown = deps.shutdown.clone();
    // Only a pass-through stream can resume a chunk mid-way after a
    // send stall; a compressed stream's encoder state cannot be
    // rewound (#709, CR on PR #807).
    let retry_stalls = codec == Codec::None;
    let tracked_writer = StatsTrackingWrite {
        inner: writer,
        slot: slot.clone(),
        registry: deps.registry.clone(),
    };
    let encoded_writer = Encoder::new(codec, tracked_writer);
    match thread::Builder::new()
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
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!(
                %peer,
                %e,
                "failed to spawn rtl_tcp writer thread — tearing down client"
            );
            deps.registry.unwind_admission(slot);
            None
        }
    }
}
