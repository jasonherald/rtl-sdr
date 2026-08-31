//! Server configuration + lifecycle: [`ServerConfig`] /
//! [`InitialDeviceState`] / [`ServerStats`] / [`TunerAdvertiseInfo`]
//! and the [`Server`] handle itself — start (bind, USB open, initial
//! device state, thread spawns), live-update setters, stats
//! snapshots, and the drop-join shutdown ordering. Split out of
//! `server.rs` per the file-size refactor (#818); pure moves, no
//! behavior change.

use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use librtlsdr_rs::RtlSdrDevice;

use crate::broadcaster::ClientRegistry;
use crate::error::ServerError;

use super::accept::spawn_accept_thread;
use super::broadcast::spawn_broadcaster_thread;
use super::{
    DEFAULT_BUFFER_CAPACITY, DEFAULT_CENTER_FREQ_HZ, DEFAULT_LISTENER_CAP, DEFAULT_SAMPLE_RATE_HZ,
};

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
pub(super) fn validate_auth_key_length(key: Option<&[u8]>) -> Result<(), ServerError> {
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
