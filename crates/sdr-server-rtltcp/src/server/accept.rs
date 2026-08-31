//! Accept-side of the server: the listener loop
//! ([`spawn_accept_thread`]) and the per-connection dispatch into
//! the handshake + auth + role-admission pipeline. Split out of
//! `server.rs` per the file-size refactor (#818); the sniffers,
//! denial writers, and socket tuning live in the [`handshake`]
//! child module, and the staged per-client setup pipeline
//! ([`setup::spawn_client_workers`]) lives in the [`setup`] child
//! module (Codacy 50-NLOC round on PR #880).

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use librtlsdr_rs::RtlSdrDevice;

use crate::broadcaster::ClientRegistry;
use crate::codec::CodecMask;

pub(in crate::server) mod handshake;
mod setup;
use handshake::configure_client_socket;
use setup::spawn_client_workers;

use super::{ACCEPT_ERROR_BACKOFF, ACCEPT_POLL_INTERVAL};

/// Everything the per-connection setup pipeline needs beyond the
/// socket itself: shared device / registry / shutdown handles plus
/// the per-client channel depth, codec offer, and the #395
/// live-update cells. One value is built by `Server::start`, moved
/// into the accept thread, and cloned per accepted connection (all
/// heavy fields are `Arc`s, so a clone is a handful of refcount
/// bumps). Bundling these replaces the former nine-parameter
/// signatures on [`spawn_accept_thread`] and
/// [`setup::spawn_client_workers`] (Codacy parameter-count round on
/// PR #880).
#[derive(Clone)]
pub(in crate::server) struct ClientSetupDeps {
    /// Shared RTL-SDR device — command dispatch and dongle_info_t
    /// reads lock it.
    pub(in crate::server) device: Arc<Mutex<RtlSdrDevice>>,
    /// Client registry: id allocation, role admission, worker-handle
    /// parking, and admission unwind on failed setup.
    pub(in crate::server) registry: Arc<ClientRegistry>,
    /// Global shutdown flag observed by every worker loop.
    pub(in crate::server) shutdown: Arc<AtomicBool>,
    /// Bound on the per-client broadcast channel (chunks, not bytes).
    pub(in crate::server) per_client_buffer_depth: usize,
    /// The server's codec offer for RTLX hello negotiation.
    pub(in crate::server) compression: CodecMask,
    /// Live-update listener cap (#395) — read once per admission.
    pub(in crate::server) listener_cap: Arc<AtomicUsize>,
    /// Live-update auth key (#395) — snapshotted once per handshake.
    pub(in crate::server) auth_key: Arc<Mutex<Option<Vec<u8>>>>,
}

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
pub(super) fn spawn_accept_thread(
    listener: TcpListener,
    stopped: Arc<AtomicBool>,
    deps: ClientSetupDeps,
) -> std::io::Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    thread::Builder::new()
        .name("rtl_tcp-accept".into())
        .spawn(move || {
            while !deps.shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => handle_accepted_connection(stream, peer, &deps),
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

/// One freshly-accepted socket: switch it to blocking mode, tune
/// keepalive/nodelay, and hand it to a short-lived per-connection
/// setup thread. Split out of [`spawn_accept_thread`] per the
/// 50-NLOC gate (Codacy round on PR #880).
///
/// Dispatching the blocking handshake (sniff hello + optional auth
/// follow-up + role admission) to a setup thread matters: holding
/// it inline would let one stalled RTLX client serialize unrelated
/// accepts for up to `HELLO_SNIFF_TIMEOUT` (0.1 s) +
/// `AUTH_REPLY_TIMEOUT` (5 s) — a slow-peer DOS against the
/// listener backlog. Per `CodeRabbit` round 4 on PR #405.
///
/// The setup thread runs to natural completion regardless of
/// shutdown: it either fails its handshake (fast return, no
/// registry entry) or progresses to registering writer + command
/// handles and exits. We register its `JoinHandle` on the same
/// `register_worker_handle` bucket as writer/command threads so
/// `Server::drop` joins it alongside them — bounded shutdown
/// latency of ≤ `HELLO_SNIFF_TIMEOUT` + `AUTH_REPLY_TIMEOUT` per
/// in-flight handshake.
fn handle_accepted_connection(stream: TcpStream, peer: SocketAddr, deps: &ClientSetupDeps) {
    tracing::info!(%peer, "rtl_tcp client connected");
    if let Err(e) = stream.set_nonblocking(false) {
        tracing::error!(%e, "failed to set client socket blocking");
        return;
    }
    configure_client_socket(&stream);
    // Snapshot the deps (all `Arc` clones — cheap) into this
    // accept's setup thread. A mid-handshake `set_listener_cap` /
    // `set_auth_key` call is visible to future accepts but does
    // not split the current client's gate across two values (the
    // setup thread reads the cap once at role-admission, and
    // snapshots the auth key once at the top of
    // `spawn_client_workers`). Per issue #395 live-update design.
    let setup_deps = deps.clone();
    match thread::Builder::new()
        .name(format!("rtl_tcp-setup-{}", peer.port()))
        .spawn(move || {
            spawn_client_workers(stream, peer, setup_deps);
        }) {
        Ok(h) => {
            deps.registry.register_worker_handle(h);
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
