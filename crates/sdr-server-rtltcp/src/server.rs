//! TCP server — accept loop, shared USB broadcaster, per-client worker threads.
//!
//! Multi-client port of the upstream `rtl_tcp` threading model (#391, epic
//! #390). Upstream's model is strictly single-client: one USB reader
//! decoupled from one TCP writer via a condvar + linked list, gated by
//! `llbuf_num` (default 500). Ours keeps the 500-chunk bound but:
//!
//! - **One USB reader thread** (`broadcaster_worker`) runs for the
//!   server's lifetime. It fans every USB chunk out to N per-client
//!   bounded channels via [`ClientRegistry::broadcast`].
//! - **Per-client writer** drains its own channel to an encoded TCP
//!   socket. A slow listener only drops chunks against its own
//!   counter; other clients keep receiving uninterrupted.
//! - **Per-client command worker** reads 5-byte command frames from
//!   the client's socket and dispatches to the shared device mutex.
//!
//! Pre-#391 upstream layout (`rtl_tcp.c:498-720`):
//!   main: bind → accept → apply defaults → reset_buffer → spawn
//!         tcp_worker + command_worker → rtlsdr_read_async (blocks) →
//!         cancel_async on SIGINT → join → accept again
//!
//! Our layout post-#391:
//!   Server::start: bind → open device → apply defaults → spawn
//!                  broadcaster_worker → spawn accept thread
//!   accept thread: accept → handshake → register ClientSlot → spawn
//!                  per-client writer + command → accept again
//!   broadcaster:   one shared thread, USB bulk read → ClientRegistry::broadcast
//!
//! `apply_initial_state` is called ONCE at [`Server::start`] — not
//! re-applied on every client accept. Previously (single-client), each
//! new client got a fresh tune/gain reset so sequential clients didn't
//! inherit each other's state. In the new multi-client model, every
//! client shares the live device state — a controller tuning to 145 MHz
//! means new listeners join on 145 MHz. Matches broadcast-radio
//! semantics and the epic's "one dongle, shared state" model. Role
//! enforcement (listeners can't tune) lands in #392.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sdr_rtlsdr::device::RtlSdrDevice;

use crate::broadcaster::{ClientRegistry, ClientSlot, RoleDecision};
use crate::codec::{Codec, CodecMask, Encoder};
use crate::dispatch::dispatch;
use crate::error::ServerError;
use crate::extension::{
    CLIENT_HELLO_LEN, ClientHello, EXTENSION_MAGIC, PROTOCOL_VERSION, Role, ServerExtension, Status,
};
use crate::protocol::{COMMAND_LEN, Command, CommandOp, DongleInfo, TunerTypeCode};

/// USB read buffer size (bytes). Matches `DEFAULT_BUF_LENGTH` upstream
/// (`rtl_tcp` inherits `rtlsdr_read_async`'s 16 × 32 KiB = 256 KiB default).
///
/// NOTE: must be a multiple of 512 (USB bulk alignment).
pub const READ_BUFFER_LEN: u32 = 256 * 1024;

/// Maximum number of 256 KiB buffers allowed to queue between the USB
/// reader and the per-client TCP writer. Same bound as upstream's
/// `llbuf_num = 500` (rtl_tcp.c:61) — now per-client after #391 instead
/// of shared. When a client's queue fills, subsequent broadcasts drop
/// for THAT client only; other clients keep draining normally.
///
/// Named `DEFAULT_BUFFER_CAPACITY` historically (single-client crate);
/// preserved as an alias for the `DEFAULT_PER_CLIENT_BUFFER_DEPTH`
/// broadcaster constant so external callers that referenced it by name
/// don't have to rename in the same PR that introduces the refactor.
pub use crate::broadcaster::DEFAULT_PER_CLIENT_BUFFER_DEPTH as DEFAULT_BUFFER_CAPACITY;

/// Socket receive timeout for the command worker read loop. Upstream
/// uses a 1-second select timeout so the loop re-checks `do_exit` even
/// when no commands arrive (rtl_tcp.c:293-304). Ours re-checks the
/// shutdown flag AND the per-slot disconnection flag.
const COMMAND_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// Sleep between non-blocking `accept()` polls. Small enough that the
/// accept thread notices the shutdown flag within ~100 ms of `Drop`.
/// `TcpListener` doesn't expose a per-accept timeout, so we poll with
/// `set_nonblocking(true)` + `thread::sleep`.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Backoff after an `accept()` call returns a non-WouldBlock error.
/// Typically an exhausted-FD / out-of-memory situation — short enough
/// to retry quickly once the transient resolves, long enough to avoid
/// a tight log-spam loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(200);

/// `recv_timeout` in the TCP writer so it notices shutdown even when
/// the broadcaster is starving (dongle unplug, no data incoming).
const WRITER_RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// Timeout on each USB bulk read in the broadcaster thread. Matches
/// upstream's 1-second poll interval in the `rtlsdr_read_async` loop.
/// The broadcaster re-checks the shutdown flag between reads.
const USB_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the broadcaster calls [`ClientRegistry::prune_disconnected`]
/// to reap slots whose workers have exited. Measured in USB-read ticks
/// rather than wall clock — at ~10 ms per tick under normal traffic
/// this prunes every ~2.5 s, which is plenty fast without making the
/// lock + retain work happen per chunk.
const BROADCASTER_PRUNE_EVERY_N_TICKS: u32 = 256;

/// Default cap on concurrent `Role::Listen` clients. Vanilla / Control
/// clients are counted separately (they allocate the single controller
/// slot). 10 is a generous default — real deployments pushing past this
/// are either relay/broadcast scenarios where the UI gives the user an
/// explicit "max listeners" knob, or a test setup. Per #390 decisions.
pub const DEFAULT_LISTENER_CAP: usize = 10;

/// Default sample rate in Hz. Matches upstream `rtl_tcp.c:DEFAULT_SAMPLE_RATE_HZ`.
///
/// Exposed so the CLI can share the same constant instead of hard-coding
/// the literal — keeps CLI and library defaults in lock-step if we ever
/// change it.
pub const DEFAULT_SAMPLE_RATE_HZ: u32 = 2_048_000;

/// Default center frequency in Hz, matching upstream rtl_tcp's
/// `frequency = 100000000` default at rtl_tcp.c:389.
pub const DEFAULT_CENTER_FREQ_HZ: u32 = 100_000_000;

/// Maximum number of recent `(CommandOp, Instant)` entries retained
/// per-client (see `broadcaster::RECENT_COMMANDS_CAPACITY`). Exposed
/// at this path for the `stats()` contract — same 50-entry bound as
/// the pre-#391 server-wide ring.
pub use crate::broadcaster::RECENT_COMMANDS_CAPACITY;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// TCP bind address. **Caller is responsible for setting a safe
    /// default** — this crate does not impose a policy. The CLI and UI
    /// both default to loopback per epic #299 review.
    pub bind: SocketAddr,

    /// Device index (0 = first dongle).
    pub device_index: u32,

    /// Initial device state applied after open.
    pub initial: InitialDeviceState,

    /// Max queued buffers **per connected client** between the shared
    /// USB broadcaster and that client's TCP writer. 0 = use
    /// [`DEFAULT_BUFFER_CAPACITY`]. Per-client after #391: a slow
    /// listener can't stall the controller.
    pub buffer_capacity: usize,

    /// Codecs this server is willing to offer to sdr-rs clients
    /// that speak the extended `"RTLX"` handshake (#307). Per-
    /// connection negotiation is the intersection of this mask
    /// and the client's advertised mask (`CodecMask::pick`):
    /// legacy / vanilla-rtl_tcp clients that don't send a hello
    /// always get `Codec::None`; sdr-rs clients supporting LZ4
    /// get LZ4 iff this mask advertises it. Default:
    /// [`CodecMask::NONE_ONLY`] — compression is opt-in per-
    /// server so existing deployments behave identically.
    pub compression: crate::codec::CodecMask,

    /// Maximum concurrent `Role::Listen` clients. The Control client
    /// is separate (always exactly one when occupied), so the total
    /// live-client ceiling is `listener_cap + 1`. When the cap is
    /// already filled, an RTLX client requesting `Role::Listen`
    /// receives `granted_role=denied, status=ListenerCapReached` and
    /// the connection is closed. Vanilla `rtl_tcp` clients never
    /// enter the listener path — they're always Control-or-denied —
    /// so the cap doesn't apply to them. Default:
    /// [`DEFAULT_LISTENER_CAP`]. #392.
    pub listener_cap: usize,

    /// Pre-shared auth key. `None` disables the auth gate entirely
    /// (default — matches the issue's "LAN-only trust model
    /// continues to work as today"). When `Some(key)`, the server
    /// validates every connecting client's [`AuthKeyMessage`]
    /// against this value using a constant-time compare; clients
    /// that don't produce a matching key are denied with
    /// [`Status::AuthFailed`] and the connection is closed. Auth
    /// runs BEFORE the role / listener-cap gate — an
    /// unauthenticated client isn't even evaluated for role.
    ///
    /// Per-byte threat model: the key travels as cleartext over
    /// the TCP connection. Fine for casual on-LAN cohabitants
    /// and IoT-trust-zone separation; NOT suitable for WAN-grade
    /// security. Deployments that need real confidentiality
    /// should wrap the socket in SSH / WireGuard / Tailscale
    /// first and use the PSK as a second layer. See #394 for the
    /// full threat model discussion.
    ///
    /// Key length must be in `1..=crate::extension::MAX_AUTH_KEY_LEN`
    /// — enforced at [`AuthKeyMessage`] serialization/parse time,
    /// so a zero-length or oversize value here would fail-fast at
    /// handshake rather than silently accepting anyone. #394.
    ///
    /// [`AuthKeyMessage`]: crate::extension::AuthKeyMessage
    /// [`Status::AuthFailed`]: crate::extension::Status::AuthFailed
    pub auth_key: Option<Vec<u8>>,
}

impl ServerConfig {
    /// Config with upstream-like defaults and loopback bind. Caller is
    /// still responsible for overriding `bind` if they want to expose
    /// the server beyond localhost.
    pub fn default_loopback() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], crate::protocol::DEFAULT_PORT)),
            device_index: 0,
            initial: InitialDeviceState::default(),
            buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            compression: crate::codec::CodecMask::NONE_ONLY,
            listener_cap: DEFAULT_LISTENER_CAP,
            auth_key: None,
        }
    }
}

/// Initial device state applied on open, before the first client connects.
/// Each field matches a CLI flag in upstream rtl_tcp.
#[derive(Debug, Clone)]
pub struct InitialDeviceState {
    /// `-f` center frequency in Hz.
    pub center_freq_hz: u32,
    /// `-s` sample rate in Hz.
    pub sample_rate_hz: u32,
    /// `-g` tuner gain in 0.1 dB. `None` = auto (upstream's `gain == 0`).
    pub gain_tenths_db: Option<i32>,
    /// `-P` frequency correction in ppm.
    pub ppm: i32,
    /// `-T` enable bias tee.
    pub bias_tee: bool,
    /// `-D` direct sampling (0 = off, 2 = Q branch — upstream hard-codes 2).
    pub direct_sampling: i32,
}

impl Default for InitialDeviceState {
    fn default() -> Self {
        // Upstream rtl_tcp.c:389-392 defaults.
        Self {
            center_freq_hz: DEFAULT_CENTER_FREQ_HZ,
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            gain_tenths_db: None,
            ppm: 0,
            bias_tee: false,
            direct_sampling: 0,
        }
    }
}

/// Live server statistics for UI consumption.
///
/// Multi-client shape (#391). Every connected client contributes an
/// entry to [`Self::connected_clients`]; per-session counters
/// (bytes_sent, commanded state, etc.) live on each [`ClientInfo`].
/// Aggregate counters at the top level are cumulative over the
/// server's lifetime — never reset — so UI consumers can compute
/// rolling deltas across snapshots without having to sum the
/// per-client vec.
///
/// UI callers snapshot the struct via `Server::stats()` on a timer.
/// Data-rate is the delta in [`Self::total_bytes_sent`] between
/// consecutive snapshots, divided by the poll interval.
#[derive(Debug, Clone, Default)]
pub struct ServerStats {
    /// Live-only snapshot of every currently-connected client.
    /// Disconnected-but-not-yet-pruned slots are filtered out at
    /// the registry layer (see `ClientRegistry::snapshot`), so
    /// this Vec never contains dead sessions — UI and FFI
    /// consumers can treat every entry as a peer that was
    /// actively reachable at snapshot time. Order is oldest-first
    /// by connect time. Per `CodeRabbit` round 2 on PR #402
    /// (switched to live-only filtering) + round 3 (doc
    /// alignment with the new contract).
    pub connected_clients: Vec<crate::broadcaster::ClientInfo>,
    /// Cumulative bytes fanned out across all clients over the
    /// server's lifetime. Monotonic — never reset. UI derives the
    /// rolling data-rate as `(stats[t].total_bytes_sent -
    /// stats[t-1].total_bytes_sent) / poll_interval`.
    pub total_bytes_sent: u64,
    /// Cumulative USB chunks dropped across all clients over the
    /// server's lifetime. A drop is counted when the broadcaster's
    /// `try_send` into a client's channel returns `Full` (that
    /// client's listener stalled). Monotonic — never reset.
    pub total_buffers_dropped: u64,
    /// Cumulative count of clients accepted over the server's
    /// lifetime (including clients that have since disconnected).
    /// UI renders as "N clients served" / "N sessions since start"
    /// style load diagnostics.
    pub lifetime_accepted: u64,
    /// Snapshot of the server's configured initial device state —
    /// the values `apply_initial_state` set at `Server::start`.
    /// UI uses these as the fallback when a client hasn't yet
    /// issued a `SetCenterFreq` / `SetSampleRate` / `SetTunerGain`
    /// command: `current_*` fields on a `ClientInfo` mean "what
    /// the client asked for"; unset means "still on the server's
    /// initial", which is a different rendering than "server's
    /// baked-in crate defaults". Per CodeRabbit round 1 on
    /// PR #402.
    pub initial: InitialDeviceState,
}

/// Tuner metadata captured at open time, exposed for callers that
/// need to advertise it (e.g. the `sdr-rtltcp-discovery` advertiser
/// populating mDNS TXT fields).
#[derive(Debug, Clone)]
pub struct TunerAdvertiseInfo {
    /// Human-readable tuner name, e.g. `"R820T"`. Rendered from the
    /// driver's `TunerType` enum via `Debug`.
    pub name: String,
    /// Number of discrete gain steps the tuner exposes.
    pub gain_count: u32,
}

/// Running server handle.
pub struct Server {
    shutdown: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    broadcaster_thread: Option<JoinHandle<()>>,
    registry: Arc<ClientRegistry>,
    bind: SocketAddr,
    tuner: TunerAdvertiseInfo,
    compression: crate::codec::CodecMask,
    /// Snapshot of the `InitialDeviceState` that `apply_initial_state`
    /// actually applied at start. Cloned from `ServerConfig.initial`
    /// and stashed here so `Server::stats()` can include it without
    /// re-reading the (mutating) live device state. UI consumers use
    /// it as the fallback for unset per-client `current_*` fields.
    initial: InitialDeviceState,
}

impl Server {
    /// Bind the listener, open the RTL-SDR, apply initial defaults, and
    /// start accepting clients.
    ///
    /// The returned handle owns the broadcaster thread and the accept
    /// thread. Dropping it signals shutdown and waits for both — plus
    /// any currently-connected clients — to exit cleanly.
    pub fn start(config: ServerConfig) -> Result<Self, ServerError> {
        // Bind first — surface port-in-use before touching the USB device
        // so we don't leave a dongle claimed after a failed bind.
        let listener = TcpListener::bind(config.bind).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                ServerError::PortInUse(config.bind.to_string())
            } else {
                ServerError::Io(e)
            }
        })?;
        // `config.bind` may request port 0 (OS-assigned); in that case
        // the actual port is only known after bind completes. Read it
        // back from the socket so `bind_address()` returns the real
        // port the UI/logs can show.
        let actual_bind = listener.local_addr().map_err(ServerError::Io)?;

        let device_count = sdr_rtlsdr::get_device_count();
        if device_count == 0 {
            return Err(ServerError::NoDevice);
        }
        if config.device_index >= device_count {
            return Err(ServerError::BadDeviceIndex {
                requested: config.device_index,
                available: device_count,
            });
        }

        let mut device = RtlSdrDevice::open(config.device_index)?;
        apply_initial_state(&mut device, &config.initial)?;

        let tuner = TunerAdvertiseInfo {
            name: format!("{:?}", device.tuner_type()),
            gain_count: device.tuner_gains().len() as u32,
        };
        tracing::info!(
            bind = %actual_bind,
            tuner = %tuner.name,
            gain_count = tuner.gain_count,
            "rtl_tcp server listening"
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(ClientRegistry::new());
        let dev_mutex = Arc::new(Mutex::new(device));
        let per_client_depth = if config.buffer_capacity == 0 {
            DEFAULT_BUFFER_CAPACITY
        } else {
            config.buffer_capacity
        };

        // Broadcaster runs for the server's lifetime regardless of
        // connected-client count. Starting it BEFORE the accept thread
        // means the first client that connects already has a live
        // broadcaster ready to fan their channel's worth of data.
        let broadcaster_thread =
            spawn_broadcaster_thread(dev_mutex.clone(), registry.clone(), shutdown.clone())?;

        let accept_thread = match spawn_accept_thread(
            listener,
            dev_mutex,
            registry.clone(),
            shutdown.clone(),
            stopped.clone(),
            per_client_depth,
            config.compression,
            config.listener_cap,
        ) {
            Ok(h) => h,
            Err(e) => {
                // Accept-thread spawn failed AFTER the broadcaster
                // was already running. Signal global shutdown so
                // the broadcaster exits its USB read loop, join
                // it so its `Arc<Mutex<RtlSdrDevice>>` clone
                // drops, THEN surface the error. Without this the
                // broadcaster would keep reading USB against a
                // dongle the caller expects to be released. Per
                // CodeRabbit round 1 on PR #402.
                shutdown.store(true, Ordering::SeqCst);
                let _ = broadcaster_thread.join();
                return Err(ServerError::Io(e));
            }
        };

        Ok(Server {
            shutdown,
            stopped,
            accept_thread: Some(accept_thread),
            broadcaster_thread: Some(broadcaster_thread),
            registry,
            bind: actual_bind,
            tuner,
            compression: config.compression,
            initial: config.initial,
        })
    }

    /// Current server statistics.
    ///
    /// Snapshots every connected client plus the cumulative
    /// server-lifetime counters from the registry. Cheap — acquires
    /// the registry's slot-list lock briefly, per-slot stats mutex
    /// once each. UI consumers call this on their poll timer (~2 Hz)
    /// and compute data-rate deltas across consecutive snapshots.
    pub fn stats(&self) -> ServerStats {
        ServerStats {
            connected_clients: self.registry.snapshot(),
            total_bytes_sent: self.registry.total_bytes_sent(),
            total_buffers_dropped: self.registry.total_buffers_dropped(),
            lifetime_accepted: self.registry.lifetime_accepted(),
            initial: self.initial.clone(),
        }
    }

    /// The address the server is bound to.
    pub fn bind_address(&self) -> SocketAddr {
        self.bind
    }

    /// Tuner metadata captured at `start()` time. Callers that want to
    /// advertise the server (e.g. via mDNS) read this for the tuner
    /// name + gain-count fields; we don't pull in a discovery dep here
    /// to keep the server crate free of mDNS deps.
    pub fn tuner_info(&self) -> &TunerAdvertiseInfo {
        &self.tuner
    }

    /// Codec mask the server is willing to negotiate. The mDNS
    /// advertiser calls this to stamp a `codecs=` TXT entry so
    /// clients can decide up-front whether to send the extended
    /// `"RTLX"` hello (a vanilla client that doesn't recognize the
    /// key just connects the legacy way — see #307).
    pub fn compression(&self) -> crate::codec::CodecMask {
        self.compression
    }

    /// Has the **accept thread** exited?
    ///
    /// Narrowly scoped signal, despite the name — flipped by the
    /// accept thread itself right before it returns (after
    /// observing the global shutdown flag, typically on dongle
    /// unplug or a caller-initiated stop). Does **not** imply that
    /// the broadcaster and per-client worker threads have joined
    /// or that the RTL-SDR dongle has been released. Full shutdown
    /// only happens inside [`Self::stop`] or `Drop` (both of which
    /// join every owned thread via `join_all_threads`).
    ///
    /// CLI callers poll this alongside their own Ctrl-C handler so
    /// the poll loop exits when serving stops on its own (e.g.,
    /// dongle unplug), then drop the `Server` which blocks until
    /// every worker has joined and the dongle is actually released.
    /// Per `CodeRabbit` round 2 on PR #402 (doc clarified; narrow
    /// semantic preserved to avoid breaking the CLI's
    /// `has_stopped() → drop(server)` coupling).
    pub fn has_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Signal shutdown and wait for every owned thread to exit —
    /// accept, broadcaster, and every per-client worker
    /// (writer + command). Equivalent to dropping the `Server`.
    ///
    /// Joining the per-client workers is **load-bearing**: each
    /// holds an `Arc<Mutex<RtlSdrDevice>>` clone, and dropping
    /// `Server` without joining them would let those Arcs outlive
    /// the reported shutdown — leaving the dongle claimed for the
    /// next consumer. Per `CodeRabbit` round 1 on PR #402.
    ///
    /// Any panic from a worker thread is silently swallowed — if
    /// you need to observe panics, keep the handle yourself
    /// instead of routing through `Server`.
    pub fn stop(mut self) {
        self.initiate_shutdown();
        self.join_all_threads();
    }

    fn initiate_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Drain + join every owned thread. Called by both `stop()`
    /// and `Drop`. The order is:
    ///   1. accept thread — stop accepting new clients first so the
    ///      per-client worker set can't grow mid-shutdown.
    ///   2. per-client workers — their `Arc<Mutex<RtlSdrDevice>>`
    ///      clones must drop before the broadcaster exits so the
    ///      last Arc hits zero and the device is released.
    ///   3. broadcaster thread — exits once the shutdown flag is
    ///      set; owns its own USB handle clone that's dropped
    ///      on return.
    ///
    /// After this returns, no thread the Server spawned is still
    /// running, and the device mutex's strong-ref count is
    /// guaranteed to be zero (the inner `Device` is dropped
    /// with `dev_mutex` when the `Server` itself is dropped).
    fn join_all_threads(&mut self) {
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        for h in self.registry.drain_worker_handles() {
            let _ = h.join();
        }
        if let Some(h) = self.broadcaster_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.initiate_shutdown();
        self.join_all_threads();
    }
}

/// Apply the user's initial settings to the freshly-opened device.
///
/// Mirrors the setup block in rtl_tcp.c:490-520. Called once at
/// `Server::start` so the dongle is in a sane state before any client
/// connects. **Not re-called on client accept** post-#391 — every
/// client shares the device state, so resetting on accept would
/// disrupt clients already listening.
fn apply_initial_state(
    dev: &mut RtlSdrDevice,
    initial: &InitialDeviceState,
) -> Result<(), ServerError> {
    // 0 is a valid direct-sampling state (off) and MUST be applied —
    // not skipped — so the device starts on a known state regardless
    // of whatever mode the previous process (or a crashed prior run)
    // left the dongle in. Previously the `!= 0` guard treated 0 as
    // "leave alone," which broke Server::start's promise of a clean
    // slate per process.
    dev.set_direct_sampling(initial.direct_sampling)?;
    dev.set_freq_correction(initial.ppm)?;
    dev.set_sample_rate(initial.sample_rate_hz)?;
    dev.set_center_freq(initial.center_freq_hz)?;
    match initial.gain_tenths_db {
        None => {
            // Upstream: `gain == 0` → automatic
            dev.set_tuner_gain_mode(false)?;
        }
        Some(g) => {
            dev.set_tuner_gain_mode(true)?;
            dev.set_tuner_gain(g)?;
        }
    }
    dev.set_bias_tee(initial.bias_tee)?;
    dev.reset_buffer()?;
    Ok(())
}

/// Spawn the server-lifetime broadcaster thread. Pulls from USB and
/// calls [`ClientRegistry::broadcast`] once per chunk. Runs even when
/// there are zero connected clients — the dongle streams regardless,
/// matching upstream's always-on async read. When clients connect
/// they join the stream mid-flow (no per-client reset).
fn spawn_broadcaster_thread(
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("rtl_tcp-broadcaster".into())
        .spawn(move || {
            broadcaster_worker(device, registry, shutdown);
        })
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
#[allow(
    clippy::too_many_arguments,
    reason = "accept thread fans state into per-client workers; \
              refactoring to a context struct would churn every test"
)]
fn spawn_accept_thread(
    listener: TcpListener,
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    per_client_buffer_depth: usize,
    compression: CodecMask,
    listener_cap: usize,
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
                        spawn_client_workers(
                            stream,
                            peer,
                            device.clone(),
                            registry.clone(),
                            shutdown.clone(),
                            per_client_buffer_depth,
                            compression,
                            listener_cap,
                        );
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
/// command threads. Fire-and-forget — the accept thread doesn't wait
/// for this client's workers; lifecycle is observed via the slot's
/// disconnection flag.
///
/// If the handshake fails at any step (sniff error, socket clone
/// fails, header write fails, thread spawn fails), the client is
/// silently dropped — no slot is registered, no stats are updated.
/// The caller (accept thread) moves on to the next accept.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "accept-time client setup fans state across handshake + registry + \
              two worker threads; refactoring to a context struct would churn the \
              accept path without improving clarity"
)]
fn spawn_client_workers(
    stream: TcpStream,
    peer: SocketAddr,
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
    per_client_buffer_depth: usize,
    compression_offer: CodecMask,
    listener_cap: usize,
) {
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
    let (hello_seen, requested_role, request_takeover, negotiated_codec) =
        if let Some(hello) = &sniff_outcome {
            let codec = compression_offer.pick(hello.codec_mask);
            let takeover = hello.request_takeover();
            tracing::info!(
                %peer,
                client_mask = hello.codec_mask.to_wire(),
                server_mask = compression_offer.to_wire(),
                chosen = %codec,
                requested_role = ?hello.role,
                request_takeover = takeover,
                "rtl_tcp extended-handshake negotiated"
            );
            (true, hello.role, takeover, Some(codec))
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
            (false, Role::Control, false, None)
        };
    let codec = negotiated_codec.unwrap_or(Codec::None);

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
    let decision = registry.register_with_role(slot.clone(), listener_cap, request_takeover);
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
            if hello_seen {
                send_denied_response(&stream, peer, &device, Status::ControllerBusy);
            }
            return;
        }
        RoleDecision::ListenerCapReached => {
            tracing::info!(
                %peer,
                ?requested_role,
                cap = listener_cap,
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
                send_denied_response(&stream, peer, &device, Status::ListenerCapReached);
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

    // RTLX clients additionally get the ServerExtension(granted)
    // block. Must land immediately after dongle_info_t so the
    // client's magic-peek lands on our bytes and not on IQ samples
    // a racing broadcaster may have queued.
    if hello_seen {
        let ext = ServerExtension {
            codec,
            granted_role: Some(requested_role),
            status: Status::Ok,
            version: PROTOCOL_VERSION,
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
    if let Err(e) = writer.set_write_timeout(Some(WRITER_RECV_TIMEOUT)) {
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
    let writer_shutdown = shutdown.clone();
    let tracked_writer = StatsTrackingWrite {
        inner: writer,
        slot: slot.clone(),
        registry: registry.clone(),
    };
    let encoded_writer = Encoder::new(codec, tracked_writer);
    let writer_handle = match thread::Builder::new()
        .name(format!("rtl_tcp-writer-{id}"))
        .spawn(move || {
            tcp_writer(encoded_writer, rx, writer_slot, writer_shutdown);
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

    // Fire and forget — neither the writer nor the command handle is
    // joined here. Both exit independently when they observe the
    // shutdown flag or the slot's disconnection flag. The slot itself
    // is retained by the registry until it's pruned.
}

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
fn send_denied_response(
    stream: &TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    status: Status,
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
        version: PROTOCOL_VERSION,
    };
    if writer.write_all(&ext.to_bytes()).is_err() {
        tracing::debug!(
            %peer,
            ?status,
            "failed to send denial ServerExtension — client already gone"
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
const TCP_KEEPALIVE_IDLE_SECS: u32 = 60;
/// Seconds between probes once the first one has been sent.
/// `TCP_KEEPINTVL`. 10 s gives the zombie three chances to reply
/// across a ~30 s window — enough to ride out a brief network
/// hiccup without blowing the detection deadline. Per #393.
const TCP_KEEPALIVE_INTERVAL_SECS: u32 = 10;
/// How many unanswered probes before the kernel drops the socket.
/// `TCP_KEEPCNT`. 3 keeps the total dead-peer detection window at
/// roughly 90 s (idle 60 s + 3 × 10 s probes). Per #393.
const TCP_KEEPALIVE_RETRIES: u32 = 3;

fn configure_client_socket(stream: &TcpStream) {
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

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn set_keepalive_tuned(stream: &TcpStream) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    // Enable SO_KEEPALIVE first — the per-tunable options below
    // are no-ops without it.
    let on: libc::c_int = 1;
    set_sockopt_int(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, on)?;

    // Idle time before first probe (seconds).
    set_sockopt_int(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPIDLE,
        TCP_KEEPALIVE_IDLE_SECS as libc::c_int,
    )?;
    // Interval between subsequent probes.
    set_sockopt_int(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPINTVL,
        TCP_KEEPALIVE_INTERVAL_SECS as libc::c_int,
    )?;
    // Probe count before declaring the peer dead.
    set_sockopt_int(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPCNT,
        TCP_KEEPALIVE_RETRIES as libc::c_int,
    )?;
    Ok(())
}

#[cfg(all(unix, target_os = "macos"))]
#[allow(unsafe_code)]
fn set_keepalive_tuned(stream: &TcpStream) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    let on: libc::c_int = 1;
    set_sockopt_int(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, on)?;

    // macOS exposes only `TCP_KEEPALIVE` (seconds-until-first-probe,
    // analogous to Linux's `TCP_KEEPIDLE`). The per-probe interval
    // and count use system-level defaults (`sysctl
    // net.inet.tcp.keepintvl` / `keepcnt`), typically 75 s × 8 =
    // ~10 minute detection. Good enough for takeover — the zombie
    // detection path is the fallback; the explicit takeover
    // handshake is the normal case.
    set_sockopt_int(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPALIVE,
        TCP_KEEPALIVE_IDLE_SECS as libc::c_int,
    )?;
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
#[allow(unsafe_code)]
fn set_keepalive_tuned(stream: &TcpStream) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    // Other unix targets (FreeBSD, etc.) — enable keepalive with
    // system defaults. The per-target `TCP_KEEPIDLE` constant names
    // vary; dedicated handling for those targets can be added here
    // when we actually ship on them.
    let on: libc::c_int = 1;
    set_sockopt_int(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, on)
}

#[cfg(not(unix))]
fn set_keepalive_tuned(stream: &TcpStream) -> std::io::Result<()> {
    // Non-unix targets (Windows, WASI, etc.) enable keepalive with
    // system defaults via socket2 — std::net::TcpStream exposes no
    // keepalive setter, and our primary tunables
    // (`TCP_KEEPIDLE` / `TCP_KEEPINTVL` / `TCP_KEEPCNT`) are
    // Linux-specific. Using `SockRef::from(&stream)` lets us flip
    // `SO_KEEPALIVE` without taking ownership of the TcpStream.
    // The kernel's default probe timings apply (~2 h first probe
    // on Windows), so the #393 zombie-detection fallback is slower
    // than on Linux but still runs. Per `CodeRabbit` round 1 on
    // PR #404. #393.
    let sock = socket2::SockRef::from(stream);
    sock.set_keepalive(true)
}

/// Tiny `setsockopt(fd, level, name, &value)` wrapper for integer
/// options. Extracted so each keepalive tunable is a one-liner
/// instead of five lines of repeated FFI boilerplate. Only used
/// on unix targets.
#[cfg(unix)]
#[allow(unsafe_code)]
fn set_sockopt_int(
    fd: libc::c_int,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
) -> std::io::Result<()> {
    // SAFETY: `fd` is a valid open socket borrowed from the
    // caller's `&TcpStream` for the duration of this call.
    // `value` is a stack-local `c_int`; we pass its address plus
    // the matching size, which is the documented calling
    // convention for `setsockopt`'s integer options.
    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            std::ptr::addr_of!(value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// How long the server waits on a fresh TCP connection for a
/// `ClientHello` before assuming the client is a legacy vanilla
/// `rtl_tcp` peer and falling through to the unchanged legacy
/// path. Short enough to be invisible to the user (RTL-SDR init
/// takes full seconds anyway); long enough to cover LAN RTT
/// jitter. Per #307.
const HELLO_SNIFF_TIMEOUT: Duration = Duration::from_millis(100);

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
fn sniff_client_hello(mut stream: &TcpStream) -> std::io::Result<Option<ClientHello>> {
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

/// `Write` adapter sitting between the negotiated `Encoder` and the
/// raw `TcpStream`. Updates the slot's per-client `bytes_sent`
/// counter AND the registry's aggregate `total_bytes_sent` with
/// the on-wire (post-compression) byte count from each successful
/// write. Counting at this layer (not inside `ClientRegistry::broadcast`)
/// means the aggregate and per-client counters never diverge and
/// both reflect bytes that actually reached the socket. Per
/// CodeRabbit round 1 on PR #402.
struct StatsTrackingWrite {
    inner: TcpStream,
    slot: Arc<ClientSlot>,
    registry: Arc<ClientRegistry>,
}

impl Write for StatsTrackingWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        let delta = n as u64;
        // Poisoned mutex only happens if a stats reader panicked
        // while holding the lock — keep streaming and let the
        // stats drift; a crashed UI thread is worse than a dropped
        // counter bump.
        if let Ok(mut s) = self.slot.stats.lock() {
            s.bytes_sent = s.bytes_sent.saturating_add(delta);
        }
        // Aggregate tracks the sum of every successful on-wire
        // write. Cheap atomic fetch_add; no lock contention with
        // other writers or the UI snapshot path.
        self.registry.record_bytes_sent(delta);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn tcp_writer<W: Write + Send>(
    mut stream: W,
    rx: Receiver<Vec<u8>>,
    slot: Arc<ClientSlot>,
    shutdown: Arc<AtomicBool>,
) {
    // Write timeout installed by the caller on the underlying
    // `TcpStream` before wrapping in the codec — see
    // `spawn_client_workers` where the timeout is set up.
    //
    // `recv_timeout` lets us notice shutdown even when the
    // broadcaster is starving (e.g., dongle unplug).
    loop {
        if shutdown.load(Ordering::Relaxed) || slot.is_disconnected() {
            return;
        }
        match rx.recv_timeout(WRITER_RECV_TIMEOUT) {
            Ok(buf) => {
                if let Err(e) = stream.write_all(&buf) {
                    tracing::debug!(%e, client_id = slot.id, "rtl_tcp client socket write failed, closing");
                    slot.mark_disconnected();
                    return;
                }
                // Flush after every chunk so the LZ4 frame encoder
                // (when active) doesn't hold a partial block in its
                // internal buffer waiting for the next USB chunk to
                // fill it out to the 64 KiB frame-block size. On
                // low-rate streams that buffering adds minutes of
                // audio latency and can trip the client's stall-
                // detection timeout. Pass-through `Codec::None`
                // flushes to `TcpStream::flush()`, which is a no-op
                // on Linux (writes go direct to the kernel send
                // buffer), so the legacy path pays nothing. Per
                // CodeRabbit round 1 on PR #399.
                if let Err(e) = stream.flush() {
                    tracing::debug!(%e, client_id = slot.id, "rtl_tcp client socket flush failed, closing");
                    slot.mark_disconnected();
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Re-check shutdown + slot flags above.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Broadcaster dropped our sender. Only happens when
                // the registry prunes our slot AFTER our sender got
                // dropped, which in turn requires slot.disconnected
                // to be set. The writer exits cleanly.
                return;
            }
        }
    }
}

fn command_worker(
    mut stream: TcpStream,
    device: Arc<Mutex<RtlSdrDevice>>,
    slot: Arc<ClientSlot>,
    shutdown: Arc<AtomicBool>,
) {
    // Upstream loops on a 1 s select() so shutdown is noticed promptly.
    // Our equivalent is the socket read timeout. If we can't install it,
    // `read_full` would block indefinitely in `stream.read()` without
    // ever re-checking the shutdown flag — which would deadlock
    // `Server::drop`. Treat the failure as fatal for this client.
    if let Err(e) = stream.set_read_timeout(Some(COMMAND_READ_TIMEOUT)) {
        tracing::warn!(%e, client_id = slot.id, "set_read_timeout on command channel failed; dropping client");
        slot.mark_disconnected();
        return;
    }
    let mut buf = [0u8; COMMAND_LEN];
    loop {
        if shutdown.load(Ordering::Relaxed) || slot.is_disconnected() {
            return;
        }
        match read_full(&mut stream, &mut buf, &slot, &shutdown) {
            ReadResult::Ok => {}
            ReadResult::Eof => {
                tracing::debug!(client_id = slot.id, "rtl_tcp command channel EOF");
                slot.mark_disconnected();
                return;
            }
            ReadResult::Shutdown => return,
            ReadResult::Err(e) => {
                tracing::warn!(%e, client_id = slot.id, "rtl_tcp command recv error");
                slot.mark_disconnected();
                return;
            }
        }
        let Some(cmd) = Command::from_bytes(&buf) else {
            // Upstream silently drops unknown opcodes (switch has no default).
            tracing::debug!(
                op = buf[0],
                client_id = slot.id,
                "rtl_tcp unknown command opcode, dropping"
            );
            continue;
        };
        // Role gate (#392). Listener clients may send commands —
        // the protocol doesn't stop them — but the server drops
        // them server-side without touching the device. No reply
        // is sent (keeps the wire protocol identical for Control
        // and Listen); the listener's UI simply observes that its
        // tune / gain commands have no effect, which matches the
        // "passive observer" contract they signed up for by
        // requesting Role::Listen.
        if slot.role == Role::Listen {
            tracing::debug!(
                client_id = slot.id,
                op = ?cmd.op,
                param = cmd.param,
                "rtl_tcp listener client attempted command — dropped"
            );
            continue;
        }
        let Ok(mut dev) = device.lock() else {
            // Same rationale as the broadcaster: a poisoned device
            // mutex is unrecoverable, and silently dropping commands
            // here would leave the client driving the UI with no
            // visible effect on the server. Close this client.
            tracing::error!(
                client_id = slot.id,
                "device mutex poisoned, command worker aborting and closing this client"
            );
            slot.mark_disconnected();
            return;
        };
        dispatch(&mut dev, cmd);
        drop(dev);
        if let Ok(mut s) = slot.stats.lock() {
            let now = Instant::now();
            s.record_command(cmd.op, now);
            // Capture the commanded state alongside the
            // last-command stamp. We record what the CLIENT
            // requested (not what the device ultimately applied)
            // because: (a) the dispatch layer already logs device
            // failures at warn!, (b) if a SetCenterFreq request is
            // rejected by the device, the client will re-request,
            // and (c) showing the client's view helps debug
            // client-side bugs ("why is GQRX stuck on 145 MHz?").
            match cmd.op {
                CommandOp::SetCenterFreq => s.current_freq_hz = Some(cmd.param),
                CommandOp::SetSampleRate => s.current_sample_rate_hz = Some(cmd.param),
                CommandOp::SetTunerGain => {
                    #[allow(
                        clippy::cast_possible_wrap,
                        reason = "gain param is signed tenths-of-dB on the wire, u32 is a raw-bits transport"
                    )]
                    let gain = cmd.param as i32;
                    s.current_gain_tenths_db = Some(gain);
                }
                CommandOp::SetGainMode => {
                    // Upstream: 0 = auto, nonzero = manual. Store
                    // the auto bool for the UI status-row renderer.
                    s.current_gain_auto = Some(cmd.param == 0);
                }
                _ => {}
            }
        }
    }
}

fn broadcaster_worker(
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
) {
    // Pull the USB handle once so we don't lock the device mutex on
    // every bulk read. The handle is Arc-cloneable and thread-safe
    // for bulk reads; the mutex-guarded device is still required for
    // command dispatch and configuration changes, which run on
    // per-client command workers.
    let handle = {
        let Ok(dev) = device.lock() else {
            tracing::error!(
                "device mutex poisoned, broadcaster aborting and signalling server shutdown"
            );
            shutdown.store(true, Ordering::SeqCst);
            return;
        };
        dev.usb_handle()
    };
    // Scratch buffer reused across iterations — only the Vec<u8>
    // that the registry clones per-client gets a fresh allocation,
    // sized to the data the USB read actually returned.
    let mut scratch = vec![0u8; READ_BUFFER_LEN as usize];
    let mut ticks_since_prune: u32 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        match handle.read_bulk(
            sdr_rtlsdr::constants::BULK_ENDPOINT,
            &mut scratch,
            USB_READ_TIMEOUT,
        ) {
            Ok(n) if n > 0 => {
                registry.broadcast(&scratch[..n]);
                ticks_since_prune = ticks_since_prune.saturating_add(1);
                if ticks_since_prune >= BROADCASTER_PRUNE_EVERY_N_TICKS {
                    let removed = registry.prune_disconnected();
                    if removed > 0 {
                        tracing::debug!(removed, "rtl_tcp pruned disconnected client slots");
                    }
                    // Reap finished per-client worker handles on
                    // the same cadence — otherwise a long-lived
                    // server with connection churn would keep
                    // every completed writer/command handle alive
                    // until shutdown (OS thread resources + TLS
                    // linger). Per `CodeRabbit` round 5 on PR #402.
                    let reaped = registry.reap_finished_worker_handles();
                    if reaped > 0 {
                        tracing::debug!(
                            reaped,
                            "rtl_tcp reaped finished per-client worker threads"
                        );
                    }
                    ticks_since_prune = 0;
                }
            }
            Ok(_) | Err(rusb::Error::Timeout) => {
                // No data — loop and re-check shutdown.
            }
            Err(rusb::Error::NoDevice) => {
                // Dongle unplug is unrecoverable at the server level.
                // Escalate to a global shutdown so the accept thread
                // exits, the CLI sees `has_stopped() == true`, and
                // connected clients' command / writer loops observe
                // the flag and tear down.
                tracing::error!("rtl_tcp: USB device lost mid-stream, stopping server");
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
            Err(e) => {
                tracing::error!(%e, "rtl_tcp bulk read error — stopping server");
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
        }
    }
    // Final prune on exit so the pruned-slots metric doesn't
    // indefinitely lag behind truth when the server stops with
    // dead slots still registered.
    registry.prune_disconnected();
}

enum ReadResult {
    Ok,
    Eof,
    Shutdown,
    Err(std::io::Error),
}

/// Read exactly `buf.len()` bytes, splitting across multiple `read`s but
/// re-checking the shutdown flag on each timeout. Mirrors the upstream
/// `while(left > 0)` loop in rtl_tcp.c:297-313.
fn read_full(
    stream: &mut TcpStream,
    buf: &mut [u8],
    slot: &Arc<ClientSlot>,
    shutdown: &Arc<AtomicBool>,
) -> ReadResult {
    let mut filled = 0;
    while filled < buf.len() {
        if shutdown.load(Ordering::Relaxed) || slot.is_disconnected() {
            return ReadResult::Shutdown;
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return ReadResult::Eof,
            Ok(n) => filled += n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Timeout — loop to re-check shutdown.
            }
            Err(e) => return ReadResult::Err(e),
        }
    }
    ReadResult::Ok
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // ============================================================
    // Test fixture constants (CodeRabbit round 4 on PR #402).
    // Extracted so each test's intent reads at a glance —
    // `42_001` on its own is noise, `TEST_CLIENT_A_PORT` plus a
    // bounds docstring is self-documenting.
    // ============================================================

    /// Loopback peer port for the "client A" side of two-client
    /// fixtures. Non-privileged, doesn't overlap with anything
    /// well-known, and disjoint from `TEST_CLIENT_B_PORT`.
    const TEST_CLIENT_A_PORT: u16 = 42_001;
    /// Loopback peer port for "client B". Disjoint from
    /// `TEST_CLIENT_A_PORT` so snapshot assertions can verify
    /// ordering / identity.
    const TEST_CLIENT_B_PORT: u16 = 42_002;
    /// Small per-client channel depth used by tests that don't
    /// exercise the full/drop path — just needs to fit the few
    /// chunks a test sends. Anything ≥ the chunk count is fine.
    const TEST_CLIENT_CHANNEL_DEPTH: usize = 4;
    /// Synthetic `bytes_sent` value for client A's stats —
    /// arbitrary small number, just has to differ from B's value
    /// so the per-client readback assertions prove the right
    /// entry landed in `connected_clients[0]`.
    const TEST_CLIENT_A_BYTES: u64 = 100;
    /// Synthetic `bytes_sent` value for client B. Differs from
    /// A's by an order of magnitude so a cross-over bug stands out.
    const TEST_CLIENT_B_BYTES: u64 = 999;
    /// 2-meter amateur band frequency (145.5 MHz) stamped into
    /// client A's `current_freq_hz` — stand-in for "non-default
    /// freq client A commanded".
    const TEST_CLIENT_A_FREQ_HZ: u32 = 145_500_000;
    /// WFM broadcast band frequency (100 MHz) stamped into
    /// client B's `current_freq_hz`. Second distinct sample so
    /// cross-client bugs show up as the wrong freq under
    /// `connected_clients[1]`.
    const TEST_CLIENT_B_FREQ_HZ: u32 = 100_000_000;

    /// Test helper: `getsockopt(fd, level, name)` for integer
    /// options. Panics on failure with the OS errno so test
    /// diagnostics point at the real problem instead of a bare
    /// "assertion failed". Linux-only because the only caller is
    /// the keepalive readback test below and those constant
    /// names are Linux-specific.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn get_sockopt_int(fd: libc::c_int, level: libc::c_int, name: libc::c_int) -> libc::c_int {
        let mut value: libc::c_int = 0;
        let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `fd` is a valid open socket for the call's
        // duration (test-side caller holds the accepted
        // TcpStream). `value` and `len` live on the stack; their
        // pointers are valid for the `getsockopt` call.
        let ret = unsafe {
            libc::getsockopt(
                fd,
                level,
                name,
                std::ptr::addr_of_mut!(value).cast(),
                std::ptr::addr_of_mut!(len),
            )
        };
        assert_eq!(
            ret,
            0,
            "getsockopt(level={level}, name={name}) failed: {}",
            std::io::Error::last_os_error()
        );
        value
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn configure_client_socket_applies_tuned_keepalive_on_linux() {
        // **Regression test for #393.** Verifies that the
        // TCP_KEEPIDLE / TCP_KEEPINTVL / TCP_KEEPCNT constants
        // actually land on the socket — not just that setsockopt
        // returned zero. Calls getsockopt after
        // `configure_client_socket` and asserts the kernel-side
        // values match our per-file constants.
        //
        // Linux-only because the platform constants differ
        // (macOS has only `TCP_KEEPALIVE`, FreeBSD has different
        // names). Other targets are exercised via the Unsupported
        // fallback path in `set_keepalive_tuned`.
        use std::os::unix::io::AsRawFd;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let s = TcpStream::connect(addr).unwrap();
            // Hold the client side alive long enough for the
            // server half to run getsockopt. Drops at thread
            // exit, which cleans up the socket pair.
            thread::sleep(Duration::from_millis(100));
            drop(s);
        });
        let (server_stream, _peer) = listener.accept().unwrap();
        configure_client_socket(&server_stream);

        let fd = server_stream.as_raw_fd();
        assert_ne!(
            get_sockopt_int(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE),
            0,
            "SO_KEEPALIVE should be enabled after configure_client_socket"
        );
        assert_eq!(
            get_sockopt_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE) as u32,
            TCP_KEEPALIVE_IDLE_SECS,
            "TCP_KEEPIDLE should match the tuned constant"
        );
        assert_eq!(
            get_sockopt_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL) as u32,
            TCP_KEEPALIVE_INTERVAL_SECS,
            "TCP_KEEPINTVL should match the tuned constant"
        );
        assert_eq!(
            get_sockopt_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT) as u32,
            TCP_KEEPALIVE_RETRIES,
            "TCP_KEEPCNT should match the tuned constant"
        );

        drop(server_stream);
        let _ = client.join();
    }

    #[test]
    fn start_surfaces_port_conflict_as_typed_error() {
        // Hold a port before calling Server::start — the second bind must
        // surface as ServerError::PortInUse (not a generic IO error), so
        // the UI can fall back without parsing error strings.
        //
        // This test does NOT need a real RTL-SDR dongle present because
        // Server::start binds the listener before touching USB.
        let holder = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        let config = ServerConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], port)),
            device_index: 0,
            initial: InitialDeviceState::default(),
            buffer_capacity: 0,
            compression: CodecMask::NONE_ONLY,
            listener_cap: DEFAULT_LISTENER_CAP,
            auth_key: None,
        };
        match Server::start(config) {
            Err(ServerError::PortInUse(ref addr)) => {
                assert!(addr.contains(&format!("{port}")));
            }
            Err(e) => panic!("expected PortInUse, got {e:?}"),
            Ok(_) => panic!("bind should have failed"),
        }
        drop(holder);
    }

    #[test]
    fn initial_device_state_defaults_match_upstream_rtl_tcp() {
        let d = InitialDeviceState::default();
        // rtl_tcp.c:389-392 — these are the upstream defaults.
        assert_eq!(d.center_freq_hz, 100_000_000);
        assert_eq!(d.sample_rate_hz, 2_048_000);
        assert_eq!(d.ppm, 0);
        assert!(!d.bias_tee);
        assert_eq!(d.direct_sampling, 0);
        assert!(d.gain_tenths_db.is_none());
    }

    #[test]
    fn default_loopback_config_binds_localhost() {
        let cfg = ServerConfig::default_loopback();
        assert_eq!(cfg.bind.ip().to_string(), "127.0.0.1");
        assert_eq!(cfg.bind.port(), crate::protocol::DEFAULT_PORT);
        assert_eq!(cfg.buffer_capacity, DEFAULT_BUFFER_CAPACITY);
    }

    #[test]
    fn server_stats_default_is_empty() {
        let stats = ServerStats::default();
        assert!(stats.connected_clients.is_empty());
        assert_eq!(stats.total_bytes_sent, 0);
        assert_eq!(stats.total_buffers_dropped, 0);
        assert_eq!(stats.lifetime_accepted, 0);
        // Default initial state matches the upstream rtl_tcp defaults.
        assert_eq!(stats.initial.center_freq_hz, DEFAULT_CENTER_FREQ_HZ);
        assert_eq!(stats.initial.sample_rate_hz, DEFAULT_SAMPLE_RATE_HZ);
    }

    #[test]
    fn recent_commands_capacity_matches_documented_bound() {
        // Sanity check on the published const. If the UI side starts
        // depending on a specific size for pagination, changing the
        // constant becomes a contract break this test catches.
        assert_eq!(RECENT_COMMANDS_CAPACITY, 50);
    }

    #[test]
    fn has_stopped_is_false_before_accept_thread_exits() {
        // We can't stand up a real Server without hardware, but we CAN
        // sanity-check the `stopped` flag contract: `has_stopped()`
        // reads the AtomicBool directly. Default state is false.
        let stopped = Arc::new(AtomicBool::new(false));
        assert!(!stopped.load(Ordering::Relaxed));
        // Accept thread setting the flag → has_stopped() observes true.
        stopped.store(true, Ordering::SeqCst);
        assert!(stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn buffer_capacity_zero_uses_default() {
        // ServerConfig exposes `buffer_capacity: 0` as "use default". This
        // is checked during Server::start, but we can sanity-check the
        // DEFAULT_BUFFER_CAPACITY matches upstream's llbuf_num = 500
        // (rtl_tcp.c:61).
        assert_eq!(DEFAULT_BUFFER_CAPACITY, 500);
    }

    #[test]
    fn server_stats_exposes_all_connected_clients() {
        // Multi-client shape: `connected_clients` carries one
        // `ClientInfo` per registered slot. Different from the
        // pre-#391 single-client projection which only exposed the
        // first client's session fields. This test pins the
        // contract that every registered slot is visible to the
        // UI / FFI — critical for the per-client rendering that
        // follows in PR B.
        use crate::broadcaster::ClientSlot;
        let registry = Arc::new(ClientRegistry::new());

        let (slot_a, _rx_a) = ClientSlot::new(
            registry.allocate_id(),
            SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_A_PORT)),
            Codec::None,
            Role::Control,
            TEST_CLIENT_CHANNEL_DEPTH,
        );
        if let Ok(mut s) = slot_a.stats.lock() {
            s.bytes_sent = TEST_CLIENT_A_BYTES;
            s.current_freq_hz = Some(TEST_CLIENT_A_FREQ_HZ);
        }
        registry.register(slot_a);

        let (slot_b, _rx_b) = ClientSlot::new(
            registry.allocate_id(),
            SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_B_PORT)),
            Codec::Lz4,
            Role::Control,
            TEST_CLIENT_CHANNEL_DEPTH,
        );
        if let Ok(mut s) = slot_b.stats.lock() {
            s.bytes_sent = TEST_CLIENT_B_BYTES;
            s.current_freq_hz = Some(TEST_CLIENT_B_FREQ_HZ);
        }
        registry.register(slot_b);

        // Snapshot via the registry directly since we don't have a
        // real Server here — the same code path `Server::stats`
        // uses to build its `ServerStats`.
        let stats = ServerStats {
            connected_clients: registry.snapshot(),
            total_bytes_sent: registry.total_bytes_sent(),
            total_buffers_dropped: registry.total_buffers_dropped(),
            lifetime_accepted: registry.lifetime_accepted(),
            initial: InitialDeviceState::default(),
        };

        assert_eq!(stats.connected_clients.len(), 2);
        assert_eq!(
            stats.connected_clients[0].peer,
            SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_A_PORT))
        );
        assert_eq!(stats.connected_clients[0].bytes_sent, TEST_CLIENT_A_BYTES);
        assert_eq!(
            stats.connected_clients[1].peer,
            SocketAddr::from(([127, 0, 0, 1], TEST_CLIENT_B_PORT))
        );
        assert_eq!(stats.connected_clients[1].codec, Codec::Lz4);
        assert_eq!(stats.lifetime_accepted, 2);
    }

    // ============================================================
    // sniff_client_hello regression tests (`CodeRabbit` round 2 on PR #399)
    //
    // The sniff is the only piece of the per-client handshake that
    // can run without a real RTL-SDR dongle, so unit tests live here.
    // Each test pairs a server-side accept with a client-side TCP
    // connect + controlled write pattern, verifying that
    // `sniff_client_hello` classifies the stream correctly.
    // ============================================================

    /// Accept one TCP client on a loopback listener and hand the
    /// accepted socket to `sniff_client_hello`. Factored out so
    /// each scenario test stays focused on what bytes the client
    /// writes, not the boilerplate of setting up sockets.
    fn run_sniff_against<F>(client_behavior: F) -> std::io::Result<Option<ClientHello>>
    where
        F: FnOnce(TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client_thread = thread::spawn(move || {
            let client = TcpStream::connect(addr).unwrap();
            client_behavior(client);
        });
        let (server_stream, _peer) = listener.accept().unwrap();
        let result = sniff_client_hello(&server_stream);
        // Join best-effort — the client thread may legitimately still
        // be holding the connection open (partial-hello test). Drop
        // the server side first so any pending write on the client
        // side unblocks, then join.
        drop(server_stream);
        let _ = client_thread.join();
        result
    }

    #[test]
    fn sniff_client_hello_full_hello_parses_correctly() {
        // Happy path: client sends a complete 8-byte hello, sniff
        // returns `Ok(Some)` with the parsed struct. Regression
        // guard against a future refactor breaking the common case.
        use crate::codec::CodecMask;
        use crate::extension::{CLIENT_HELLO_FLAGS_NONE, Role};
        let hello = ClientHello {
            codec_mask: CodecMask::NONE_AND_LZ4,
            role: Role::Control,
            flags: CLIENT_HELLO_FLAGS_NONE,
            version: PROTOCOL_VERSION,
        };
        let bytes = hello.to_bytes();
        let result = run_sniff_against(move |mut client| {
            client.write_all(&bytes).unwrap();
            // Let the server finish reading before the client
            // stream drops (which would EOF mid-read).
            thread::sleep(Duration::from_millis(50));
        });
        assert_eq!(result.unwrap(), Some(hello));
    }

    #[test]
    fn sniff_client_hello_idle_client_returns_legacy_fallback() {
        // Legacy rtl_tcp client: connects, then idles waiting for
        // the server's `dongle_info_t`. Zero bytes reach the sniff
        // before the timeout fires, so `Ok(None)` is the safe
        // fallback — nothing consumed, no desync risk.
        let result = run_sniff_against(|client| {
            // Hold the socket open well past the sniff timeout.
            thread::sleep(HELLO_SNIFF_TIMEOUT * 3);
            drop(client);
        });
        match result {
            Ok(None) => {}
            other => panic!("expected Ok(None) for idle client, got {other:?}"),
        }
    }

    #[test]
    fn sniff_client_hello_non_magic_prefix_is_legacy_fallback() {
        // Vanilla client sends a `SetCenterFreq` command
        // immediately after connect (opcode 0x01 + 4-byte arg).
        // Peek reads 4 bytes, magic doesn't match, sniff returns
        // `Ok(None)` without consuming — so the command_worker
        // reads the full 5-byte frame cleanly.
        let result = run_sniff_against(|mut client| {
            // 5-byte vanilla SetCenterFreq command: opcode=0x01,
            // freq=100_000_000 Hz big-endian.
            let cmd: [u8; 5] = [0x01, 0x05, 0xF5, 0xE1, 0x00];
            client.write_all(&cmd).unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        match result {
            Ok(None) => {}
            other => panic!("expected Ok(None) for non-RTLX prefix, got {other:?}"),
        }
    }

    #[test]
    fn sniff_client_hello_partial_hello_is_protocol_error() {
        // **Regression test for `CodeRabbit` round 2 on PR #399.**
        // A client that sends the 4-byte `RTLX` magic and then
        // stalls without sending the remaining 4 hello bytes used
        // to fall back to the legacy path — which desynced the
        // command stream by 4 bytes (those magic bytes were
        // already consumed by `read_exact` before it timed out).
        // The fix promotes partial-hello to `Err` so the client
        // gets dropped instead.
        let result = run_sniff_against(|mut client| {
            // Send magic only; hold the connection open past the
            // sniff timeout so `read_exact` observes partial data.
            client.write_all(&EXTENSION_MAGIC).unwrap();
            thread::sleep(HELLO_SNIFF_TIMEOUT * 5);
            drop(client);
        });
        assert!(
            result.is_err(),
            "partial hello (magic only, body stalled) must surface as Err — \
             got {result:?} which would desync the command stream on fallback"
        );
    }

    #[test]
    fn sniff_client_hello_fragmented_magic_completes_successfully() {
        // **Regression test for `CodeRabbit` round 5 on PR #402.**
        // A well-behaved RTLX client whose `ClientHello` bytes
        // span two TCP segments (e.g. `RT` in one, `LX` + body
        // in the next) previously fell back to legacy on the
        // first short peek — corrupting the command stream for
        // the unlucky RTLX client. The fix retries the peek
        // while the observed bytes are a prefix of
        // `EXTENSION_MAGIC`, so a fragmented magic still reaches
        // the full `read_exact` path.
        use crate::codec::CodecMask;
        use crate::extension::{CLIENT_HELLO_FLAGS_NONE, Role};
        let hello = ClientHello {
            codec_mask: CodecMask::NONE_AND_LZ4,
            role: Role::Control,
            flags: CLIENT_HELLO_FLAGS_NONE,
            version: PROTOCOL_VERSION,
        };
        let bytes = hello.to_bytes();
        let result = run_sniff_against(move |mut client| {
            // Send the first 2 magic bytes, flush, pause briefly
            // to force a short peek on the server side, then
            // send the remaining 6 bytes.
            client.write_all(&bytes[..2]).unwrap();
            client.flush().unwrap();
            thread::sleep(Duration::from_millis(10));
            client.write_all(&bytes[2..]).unwrap();
            // Keep the client alive long enough for the server
            // to finish `read_exact` before we drop and EOF.
            thread::sleep(Duration::from_millis(50));
        });
        assert_eq!(
            result.unwrap(),
            Some(hello),
            "fragmented RTLX hello must not fall back to legacy on the first short peek"
        );
    }

    #[test]
    fn sniff_client_hello_stalled_magic_prefix_is_protocol_error() {
        // **Regression test for `CodeRabbit` round 5 on PR #402**
        // (initial promotion to `Err`) **+ round 6** (pinning
        // the error kind to `InvalidData`, not `TimedOut`).
        //
        // A client that starts sending the magic (e.g. `RT`)
        // and then stalls without completing the remaining 2
        // bytes must NOT fall back to legacy: the prefix bytes
        // are still queued in the receive buffer, and handing
        // them to `command_worker` would start the legacy
        // command stream at `R` (0x52) which isn't any valid
        // opcode — poisoning every subsequent command. Promote
        // to `Err` with `ErrorKind::InvalidData` so it lands in
        // the same classification bucket as `read_exact`
        // timeout mid-hello and a malformed body — all three
        // are protocol-desync errors from the host's POV.
        let result = run_sniff_against(|mut client| {
            client.write_all(&EXTENSION_MAGIC[..2]).unwrap();
            thread::sleep(HELLO_SNIFF_TIMEOUT * 5);
            drop(client);
        });
        let err = result.expect_err(
            "stalled magic prefix must surface as Err (dropping the client) — legacy \
             fallback would desync the command stream",
        );
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "stalled RTLX prefix classifies as InvalidData (protocol desync), not \
             TimedOut (idle-socket semantics) — got {err:?}"
        );
    }

    #[test]
    fn sniff_client_hello_short_non_magic_prefix_is_legacy_fallback() {
        // Legacy client whose first TCP segment carries only 1
        // byte that already disagrees with magic (e.g. 0x01 =
        // `SetCenterFreq` opcode). The sniff loop must recognize
        // this as a definite non-match from the short peek and
        // fall back to legacy immediately — no reason to wait
        // out the full `HELLO_SNIFF_TIMEOUT` when the first byte
        // already rules out RTLX.
        //
        // Times just the sniff call (not the client-thread join)
        // to verify the short-circuit. Inlined instead of using
        // `run_sniff_against` so the client keeps the socket
        // open past the sniff return — otherwise we can't prove
        // the sniff exited on the definite-non-match path vs.
        // on an EOF from the client dropping.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let keep_alive = Arc::new(AtomicBool::new(true));
        let keep_alive_clone = Arc::clone(&keep_alive);
        let client_thread = thread::spawn(move || {
            let mut client = TcpStream::connect(addr).unwrap();
            client.write_all(&[0x01]).unwrap();
            client.flush().unwrap();
            // Hold the socket open so the sniff must decide on
            // the 1-byte prefix alone, not on EOF.
            while keep_alive_clone.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(2));
            }
            drop(client);
        });
        let (server_stream, _peer) = listener.accept().unwrap();
        let sniff_start = std::time::Instant::now();
        let result = sniff_client_hello(&server_stream);
        let elapsed = sniff_start.elapsed();
        keep_alive.store(false, Ordering::Relaxed);
        drop(server_stream);
        let _ = client_thread.join();

        match result {
            Ok(None) => {}
            other => panic!("expected Ok(None) for short non-magic prefix, got {other:?}"),
        }
        assert!(
            elapsed < HELLO_SNIFF_TIMEOUT,
            "short non-magic prefix should short-circuit within HELLO_SNIFF_TIMEOUT, \
             but sniff took {elapsed:?} (HELLO_SNIFF_TIMEOUT = {HELLO_SNIFF_TIMEOUT:?})"
        );
    }

    #[test]
    fn sniff_client_hello_malformed_body_is_protocol_error() {
        // Client sends a full 8 bytes starting with `RTLX` but with
        // an unknown role byte (0x99). Body parses as `None` →
        // protocol error. Previously returned `Ok(None)` (legacy
        // fallback on a shifted stream — desync risk).
        let mut garbled = [0u8; CLIENT_HELLO_LEN];
        garbled[..EXTENSION_MAGIC.len()].copy_from_slice(&EXTENSION_MAGIC);
        garbled[4] = 0x03; // codec mask (NONE+LZ4)
        garbled[5] = 0x99; // invalid role — from_bytes returns None
        garbled[6] = 0x00; // flags
        garbled[7] = PROTOCOL_VERSION;
        let result = run_sniff_against(move |mut client| {
            client.write_all(&garbled).unwrap();
            thread::sleep(Duration::from_millis(50));
        });
        assert!(
            result.is_err(),
            "malformed hello body (magic matched, unknown role) must surface as Err — \
             got {result:?}"
        );
    }
}
