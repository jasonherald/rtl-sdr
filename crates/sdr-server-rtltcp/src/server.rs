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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use librtlsdr_rs::RtlSdrDevice;

use crate::broadcaster::{ClientRegistry, ClientSlot, RoleDecision};
use crate::codec::{Codec, CodecMask, Encoder};
use crate::dispatch::dispatch;
use crate::error::ServerError;
use crate::extension::{
    AUTH_KEY_HEADER_LEN, AUTH_REPLY_TIMEOUT, AuthKeyMessage, CLIENT_HELLO_LEN, ClientHello,
    EXTENSION_MAGIC, PROTOCOL_VERSION_V1, Role, ServerExtension, Status,
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

/// `SO_SNDTIMEO` on each client's data socket: how long one `write`
/// may block on a closed receive window before the writer gets
/// control back to shed its backlog. Separate from
/// [`WRITER_RECV_TIMEOUT`] (#709): a stall is a signal to drop
/// queued chunks, not the client.
const DATA_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Consecutive send timeouts (each [`DATA_WRITE_TIMEOUT`] long) a
/// client may accumulate on one chunk before it is dropped — about
/// 10 s, enough for an AP roam or a laptop waking from power-save,
/// which upstream rtl_tcp's 1 s `select` loop also rides out by
/// dropping buffers rather than the connection (#709).
const MAX_CONSECUTIVE_WRITE_STALLS: u32 = 10;

/// Consecutive failed USB bulk reads before the broadcaster gives
/// up and stops the server. Mirrors librtlsdr's `xfer_errors`
/// budget of `xfer_buf_num` (`DEFAULT_BUF_NUMBER` = 15) consecutive
/// failed transfers: one `Overflow` / `Pipe` / `Io` under EMI or a
/// `set_sample_rate` race is retried, not fatal (#711). A
/// `NoDevice` still stops immediately.
const MAX_CONSECUTIVE_USB_ERRORS: u32 = 15;

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
    /// (i.e., `1..=256`). [`Server::start`] validates the length
    /// BEFORE binding the listener or opening the USB device and
    /// returns [`ServerError::InvalidAuthKeyLength`] immediately
    /// if the key is empty or oversize, so the operator sees a
    /// single clear configuration error at startup rather than
    /// every client failing at handshake time. Per `CodeRabbit`
    /// round 2 on PR #405. #394.
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
    /// Listener cap shared with the accept thread via [`AtomicUsize`]
    /// so the UI can live-update it via [`Server::set_listener_cap`]
    /// without restarting the server. Accept path does a
    /// `Relaxed` load on each admission decision; the atomic's cost
    /// is negligible relative to the `TcpListener::accept` syscall.
    /// Per #395.
    listener_cap: Arc<AtomicUsize>,
    /// Auth key shared with the accept thread via `Mutex` so the UI
    /// can live-update it via [`Server::set_auth_key`] without
    /// restarting the server. The handshake path snapshots the value
    /// once per connection (cloning the `Option<Vec<u8>>`) so a
    /// mid-handshake `set_auth_key` never splits a single client's
    /// eager-vs-lazy gate across two keys. Per #395.
    auth_key: Arc<Mutex<Option<Vec<u8>>>>,
    /// Cached "auth is required" flag used by [`Server::auth_required`]
    /// when the `auth_key` mutex is poisoned. Without this, a poisoned
    /// mutex would silently advertise "no auth" via mDNS (discovery
    /// clients would stop prompting for a key) while the handshake
    /// path and [`Server::set_auth_key`] still treat the poison as
    /// fatal. The atomic is updated inside [`Server::set_auth_key`]
    /// on every successful mutation (AFTER the mutex write succeeds)
    /// and initialized from `ServerConfig.auth_key.is_some()` at
    /// construction. Per `CodeRabbit` round 1 on PR #406.
    auth_required_cache: Arc<AtomicBool>,
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
        // Validate `auth_key` BEFORE bind or USB open so a bad
        // config doesn't leave a half-initialized server + a
        // claimed dongle. `ServerConfig` is public, so library
        // callers can construct `Some(vec![])` or an oversize
        // key directly (bypassing the FFI + wire-format guards).
        // Catch that here rather than deferring to per-handshake
        // failures — otherwise the server appears to start fine
        // but every client gets rejected. Per `CodeRabbit`
        // round 2 on PR #405.
        validate_auth_key_length(config.auth_key.as_deref())?;

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

        let device_count = librtlsdr_rs::get_device_count();
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

        // Wrap `listener_cap` and `auth_key` in shared atomics so the
        // UI can live-update them via `Server::set_listener_cap` /
        // `Server::set_auth_key` without restarting the server. The
        // accept thread holds `Arc` clones and re-reads on every
        // admission so the next-accept-after-change sees the new
        // value. Per issue #395: "change takes effect on next accept".
        let listener_cap = Arc::new(AtomicUsize::new(config.listener_cap));
        let auth_key = Arc::new(Mutex::new(config.auth_key.clone()));
        // Seed the auth-required cache from the starting config.
        // Kept in lockstep with `auth_key` updates by `set_auth_key`.
        // Per `CodeRabbit` round 1 on PR #406.
        let auth_required_cache = Arc::new(AtomicBool::new(config.auth_key.is_some()));

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
            listener_cap.clone(),
            auth_key.clone(),
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
            listener_cap,
            auth_key,
            auth_required_cache,
            initial: config.initial,
        })
    }

    /// Replace the listener cap while the server is running. Takes
    /// effect on the **next accept** — already-connected listeners
    /// are never kicked, even when the new cap is lower than the
    /// currently-connected count (surprise disconnection is rude;
    /// per issue #395). Cheap — single `Relaxed` atomic store. The
    /// accept thread reads via `Relaxed` load on each admission.
    pub fn set_listener_cap(&self, cap: usize) {
        self.listener_cap.store(cap, Ordering::Relaxed);
    }

    /// Replace the auth key while the server is running. Takes
    /// effect on the **next accept** — already-authenticated clients
    /// keep their sessions (auth runs once per handshake, not
    /// per-message). Useful for the #395 "Regenerate" button: the
    /// old key stops working for future reconnects without
    /// disturbing anyone currently streaming.
    ///
    /// Validates length up-front (same rule as [`Server::start`]):
    /// `Some(key)` must have `1..=MAX_AUTH_KEY_LEN` bytes. Rejects
    /// empty / oversize inputs with [`ServerError::InvalidAuthKeyLength`]
    /// — the live-update surface cannot silently accept a config
    /// value that `Server::start` itself would have refused.
    ///
    /// `None` disables the auth gate entirely (matches the
    /// "Require key" switch flipping off in the #395 UI).
    ///
    /// Poisoned mutex surfaces as
    /// [`ServerError::Io(ErrorKind::Other)`] — the only way the
    /// mutex gets poisoned is if a prior auth-gate panic left it
    /// locked, which should be unreachable in practice; the server
    /// is effectively broken at that point and the UI should
    /// surface the error so the operator can restart it.
    pub fn set_auth_key(&self, key: Option<Vec<u8>>) -> Result<(), ServerError> {
        validate_auth_key_length(key.as_deref())?;
        let mut guard = self.auth_key.lock().map_err(|_| {
            ServerError::Io(std::io::Error::other(
                "auth_key mutex poisoned — server is in a broken state",
            ))
        })?;
        let will_be_required = key.is_some();
        *guard = key;
        // Drop the lock BEFORE the cache store so the cache write
        // is never ordered-after a still-held mutex observer.
        drop(guard);
        // Update the poison-survival cache. Readers of
        // `auth_required()` fall back to this value when the mutex
        // is poisoned, so the cache must reflect the latest
        // successful state change. Per `CodeRabbit` round 1 on
        // PR #406.
        self.auth_required_cache
            .store(will_be_required, Ordering::Relaxed);
        Ok(())
    }

    /// Current listener cap. Reflects the most recent
    /// [`Server::set_listener_cap`] call, or the starting
    /// `ServerConfig.listener_cap` if never changed.
    pub fn listener_cap(&self) -> usize {
        self.listener_cap.load(Ordering::Relaxed)
    }

    /// Whether the server currently requires auth. Returns `true`
    /// iff [`Server::set_auth_key`] has been called with `Some(_)`
    /// (or the starting `ServerConfig.auth_key` was `Some`). Does
    /// not leak the key itself — useful for stamping the mDNS TXT
    /// `auth_required=true` field without handing the caller the
    /// raw key bytes.
    ///
    /// Falls back to `auth_required_cache` on a poisoned mutex.
    /// The handshake path and `set_auth_key` treat poisoning as
    /// fatal; downgrading the mDNS advertisement to "no auth" in
    /// the same scenario would make discovery clients stop
    /// prompting for a key exactly when the server is broken. The
    /// cache holds the last-known good value so the TXT stays
    /// honest even after the mutex is unusable. Per `CodeRabbit`
    /// round 1 on PR #406.
    pub fn auth_required(&self) -> bool {
        if let Ok(g) = self.auth_key.lock() {
            g.is_some()
        } else {
            tracing::warn!(
                "auth_key mutex poisoned — auth_required() falling back to cached value"
            );
            self.auth_required_cache.load(Ordering::Relaxed)
        }
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
        // Loop-drain: setup threads (post-PR #405 round 4) and
        // the writer/command threads they spawn share the same
        // registry bucket. A setup thread that's still inside
        // `spawn_client_workers` when the FIRST drain runs will
        // register its `writer_handle` + `command_handle` AFTER
        // the drain returned — those late handles would never
        // be joined under a single-pass drain, leaving the
        // writer/command workers alive with their
        // `Arc<Mutex<RtlSdrDevice>>` clones past the
        // `has_stopped()` transition. Keep draining until the
        // bucket stays empty across a full pass. Termination is
        // guaranteed: the accept thread was joined first so no
        // new setup threads can spawn; each live setup thread
        // has a bounded lifetime (≤ `HELLO_SNIFF_TIMEOUT` +
        // `AUTH_REPLY_TIMEOUT` ≈ 5.1 s) and registers its
        // writer/command handles before exiting, so after at
        // most one round of joins + drains the bucket converges
        // to empty. Per `CodeRabbit` round 5 on PR #405.
        loop {
            let handles = self.registry.drain_worker_handles();
            if handles.is_empty() {
                break;
            }
            for h in handles {
                let _ = h.join();
            }
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

/// Shared validation for `ServerConfig.auth_key` /
/// `Server::set_auth_key` inputs: `None` is always fine; `Some(key)`
/// must have `1..=MAX_AUTH_KEY_LEN` bytes. Empty or oversize Vecs
/// return [`ServerError::InvalidAuthKeyLength`] with the out-of-range
/// len so the caller's error message can name the exact failure.
/// Centralized here so `Server::start` and `Server::set_auth_key`
/// enforce the same contract — the live-update path cannot silently
/// accept inputs that the construction path would refuse. Per #395.
fn validate_auth_key_length(key: Option<&[u8]>) -> Result<(), ServerError> {
    if let Some(bytes) = key {
        let max = crate::extension::MAX_AUTH_KEY_LEN;
        if bytes.is_empty() || bytes.len() > max {
            return Err(ServerError::InvalidAuthKeyLength {
                len: bytes.len(),
                max,
            });
        }
    }
    Ok(())
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
fn spawn_client_workers(
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
fn send_extension_only(stream: &TcpStream, peer: SocketAddr, status: Status, hello_version: u8) {
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
const TCP_KEEPALIVE_IDLE_SECS: u32 = 60;
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
const TCP_KEEPALIVE_INTERVAL_SECS: u32 = 10;
/// How many unanswered probes before the kernel drops the socket.
/// `TCP_KEEPCNT`. 3 keeps the total dead-peer detection window at
/// roughly 90 s (idle 60 s + 3 × 10 s probes). Per #393.
///
/// Linux-only — same rationale as `TCP_KEEPALIVE_INTERVAL_SECS`.
#[cfg(target_os = "linux")]
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
fn sniff_auth_key_message(mut stream: &TcpStream) -> std::io::Result<AuthKeyMessage> {
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
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
    retry_stalls: bool,
) {
    // Write timeout (`DATA_WRITE_TIMEOUT`) installed by the caller on
    // the underlying `TcpStream` before wrapping in the codec — see
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
                let outcome = write_chunk_shedding_backlog(
                    &mut stream,
                    &buf,
                    &rx,
                    &slot,
                    &registry,
                    retry_stalls,
                );
                if outcome == ChunkOutcome::Closed {
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

/// Result of pushing one chunk to a client socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkOutcome {
    /// The whole chunk (and the flush) went out.
    Sent,
    /// The socket is gone, or it stalled past the budget.
    Closed,
}

/// Is this the stall signal `SO_SNDTIMEO` raises when the peer's
/// receive window is closed?
fn is_stall(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// Write `chunk` to `stream`, riding out send stalls the way
/// upstream rtl_tcp does (#709): a timed-out write sheds the chunks
/// that queued up behind it (counted as drops), then resumes the
/// *same* chunk at the byte where it stopped so the I/Q byte
/// alignment on the wire survives. [`MAX_CONSECUTIVE_WRITE_STALLS`]
/// stalls in a row (the count resets on any progress), a zero-length
/// write, or any other error close the client.
///
/// `retry_stalls` is `false` for compressed streams: the LZ4 frame
/// encoder may have emitted part of a block before the inner socket
/// timed out, and its state cannot be rewound, so a stall there is
/// terminal rather than retried (CR on PR #807).
///
/// Every chunk is flushed so the LZ4 frame encoder (when active)
/// doesn't hold a partial block waiting for the next USB chunk to
/// fill it out to the 64 KiB frame-block size — on low-rate streams
/// that buffering adds minutes of latency and can trip the client's
/// stall-detection timeout. Pass-through `Codec::None` flushes to
/// `TcpStream::flush()`, a no-op on Linux. Per CodeRabbit round 1 on
/// PR #399.
fn write_chunk_shedding_backlog<W: Write>(
    stream: &mut W,
    chunk: &[u8],
    rx: &Receiver<Vec<u8>>,
    slot: &ClientSlot,
    registry: &ClientRegistry,
    retry_stalls: bool,
) -> ChunkOutcome {
    let mut offset = 0;
    let mut stalls: u32 = 0;
    let mut flushed = false;
    while !flushed {
        let step = if offset < chunk.len() {
            stream.write(&chunk[offset..]).inspect(|n| offset += n)
        } else {
            stream.flush().map(|()| {
                flushed = true;
                1
            })
        };
        match step {
            Ok(0) => {
                tracing::debug!(client_id = slot.id, "rtl_tcp client socket closed by peer");
                return ChunkOutcome::Closed;
            }
            Ok(_) => stalls = 0,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if is_stall(&e) && retry_stalls => {
                stalls += 1;
                if stalls > MAX_CONSECUTIVE_WRITE_STALLS {
                    tracing::debug!(
                        client_id = slot.id,
                        stalls,
                        "rtl_tcp client stalled past the write budget, closing"
                    );
                    return ChunkOutcome::Closed;
                }
                shed_backlog(rx, slot, registry, stalls);
            }
            Err(e) => {
                tracing::debug!(%e, client_id = slot.id, "rtl_tcp client socket write failed, closing");
                return ChunkOutcome::Closed;
            }
        }
    }
    ChunkOutcome::Sent
}

/// Discard every chunk queued behind a stalled write and count the
/// drops against the client (#709).
fn shed_backlog(rx: &Receiver<Vec<u8>>, slot: &ClientSlot, registry: &ClientRegistry, stalls: u32) {
    let shed = u64::try_from(rx.try_iter().count()).unwrap_or(u64::MAX);
    if shed > 0 {
        registry.record_buffers_dropped(slot, shed);
    }
    tracing::trace!(
        client_id = slot.id,
        stalls,
        shed,
        "rtl_tcp client send stalled; backlog shed, retrying"
    );
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
        // A takeover may have displaced this controller while it
        // waited for the device lock; its command must not land on
        // the new controller's dongle state (#710).
        if slot.is_disconnected() {
            tracing::debug!(
                client_id = slot.id,
                "rtl_tcp command dropped — client displaced while waiting for the device"
            );
            return;
        }
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
    let Some(handle) = usb_handle_or_shutdown(&device, &shutdown) else {
        return;
    };
    // Scratch buffer reused across iterations — only the Vec<u8>
    // that the registry clones per-client gets a fresh allocation,
    // sized to the data the USB read actually returned.
    let mut scratch = vec![0u8; READ_BUFFER_LEN as usize];
    let mut ticks_since_prune: u32 = 0;
    let mut usb_errors: u32 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        let read = handle.read_bulk(RtlSdrDevice::BULK_ENDPOINT, &mut scratch, USB_READ_TIMEOUT);
        match classify_usb_read(read, &mut usb_errors) {
            UsbReadOutcome::Data(n) => {
                registry.broadcast(&scratch[..n]);
                ticks_since_prune = ticks_since_prune.saturating_add(1);
                if ticks_since_prune >= BROADCASTER_PRUNE_EVERY_N_TICKS {
                    prune_and_reap(&registry);
                    ticks_since_prune = 0;
                }
            }
            UsbReadOutcome::Idle => {
                // No data — loop and re-check shutdown.
            }
            UsbReadOutcome::Retry(e) => {
                tracing::warn!(
                    %e,
                    consecutive = usb_errors,
                    "rtl_tcp bulk read error — retrying"
                );
            }
            UsbReadOutcome::Stop(e) => {
                // Dongle unplug (or a sustained run of failed
                // transfers) is unrecoverable at the server
                // level. Escalate to a global shutdown so the
                // accept thread exits, the CLI sees
                // `has_stopped() == true`, and connected
                // clients' command / writer loops observe the
                // flag and tear down.
                tracing::error!(
                    %e,
                    consecutive = usb_errors,
                    "rtl_tcp: USB read failure is terminal, stopping server"
                );
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

/// Pull the USB handle once so the broadcaster doesn't lock the
/// device mutex on every bulk read. The handle is Arc-cloneable and
/// thread-safe for bulk reads; the mutex-guarded device is still
/// required for command dispatch and configuration changes, which
/// run on per-client command workers. A poisoned mutex is
/// unrecoverable: signal server shutdown and return `None`.
fn usb_handle_or_shutdown(
    device: &Mutex<RtlSdrDevice>,
    shutdown: &AtomicBool,
) -> Option<Arc<rusb::DeviceHandle<rusb::GlobalContext>>> {
    let Ok(dev) = device.lock() else {
        tracing::error!(
            "device mutex poisoned, broadcaster aborting and signalling server shutdown"
        );
        shutdown.store(true, Ordering::SeqCst);
        return None;
    };
    Some(dev.usb_handle())
}

/// Broadcaster housekeeping on its prune cadence: drop disconnected
/// slots and reap finished per-client worker handles — otherwise a
/// long-lived server with connection churn would keep every
/// completed writer/command handle alive until shutdown (OS thread
/// resources + TLS linger). Per `CodeRabbit` round 5 on PR #402.
fn prune_and_reap(registry: &ClientRegistry) {
    let removed = registry.prune_disconnected();
    if removed > 0 {
        tracing::debug!(removed, "rtl_tcp pruned disconnected client slots");
    }
    let reaped = registry.reap_finished_worker_handles();
    if reaped > 0 {
        tracing::debug!(reaped, "rtl_tcp reaped finished per-client worker threads");
    }
}

/// What the broadcaster does with one USB bulk-read result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsbReadOutcome {
    /// `n` bytes to fan out.
    Data(usize),
    /// Nothing to do: zero-length read or timeout.
    Idle,
    /// Transient failure: counted, read again.
    Retry(rusb::Error),
    /// Device gone, or [`MAX_CONSECUTIVE_USB_ERRORS`] failures in a
    /// row (this error being the last of them): stop the server.
    Stop(rusb::Error),
}

/// librtlsdr's `xfer_errors` rule (#711 / #808): any successful read
/// — including a zero-length one — resets `consecutive_errors`; a
/// `Timeout` is neutral; `NoDevice` is terminal at once; any other
/// error is counted and retried until the budget is spent.
fn classify_usb_read(
    result: Result<usize, rusb::Error>,
    consecutive_errors: &mut u32,
) -> UsbReadOutcome {
    match result {
        Ok(n) => {
            *consecutive_errors = 0;
            if n > 0 {
                UsbReadOutcome::Data(n)
            } else {
                UsbReadOutcome::Idle
            }
        }
        Err(rusb::Error::Timeout) => UsbReadOutcome::Idle,
        Err(e @ rusb::Error::NoDevice) => UsbReadOutcome::Stop(e),
        Err(e) => {
            *consecutive_errors = consecutive_errors.saturating_add(1);
            if *consecutive_errors >= MAX_CONSECUTIVE_USB_ERRORS {
                UsbReadOutcome::Stop(e)
            } else {
                UsbReadOutcome::Retry(e)
            }
        }
    }
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
mod tests;
