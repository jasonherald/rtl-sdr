//! TCP server — accept loop, per-client data/command/writer threads.
//!
//! Faithful port of the upstream rtl_tcp threading model with one Rust
//! tweak: upstream uses a pthread condvar + linked list to decouple the
//! libusb async callback from the TCP writer (dropping buffers when the
//! list exceeds `llbuf_num`, default 500). We use a bounded
//! `std::sync::mpsc::sync_channel` with identical drop-on-full semantics —
//! simpler and no `unsafe`, same backpressure behavior.
//!
//! Upstream layout (rtl_tcp.c:498-720):
//!   main: bind → accept → apply defaults → reset_buffer → spawn
//!         tcp_worker + command_worker → rtlsdr_read_async (blocks) →
//!         cancel_async on SIGINT → join → accept again
//!
//! Our layout: accept thread owns the outer loop; each accepted client
//! spawns data_worker (USB → channel), writer (channel → TCP send), and
//! command_worker (TCP recv → dispatch). First worker to exit signals
//! the others via the shutdown flag.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sdr_rtlsdr::device::RtlSdrDevice;

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
/// reader and the TCP writer. Matches upstream's default `llbuf_num = 500`
/// (rtl_tcp.c:61). When the queue is full, new USB buffers are dropped and
/// a warning is logged — exactly upstream's drop-on-overflow policy.
pub const DEFAULT_BUFFER_CAPACITY: usize = 500;

/// Socket receive timeout for the command worker select loop. Upstream
/// uses a 1-second select timeout so the loop re-checks `do_exit` even
/// when no commands arrive (rtl_tcp.c:293-304).
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
/// the USB reader is starving (dongle unplug, no data incoming).
const WRITER_RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// Timeout on each USB bulk read in the data worker. Matches upstream's
/// 1-second poll interval in the `rtlsdr_read_async` loop. The data
/// worker re-checks the shutdown flag between reads.
const USB_READ_TIMEOUT: Duration = Duration::from_secs(1);

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
/// in `ServerStats::recent_commands`. 50 covers a typical rtl_tcp
/// session's worth of tuning / gain / mode changes — enough to
/// debug "why didn't my tune command land?" scenarios without
/// unbounded memory growth on long-running servers. Oldest entries
/// are popped when the ring fills.
pub const RECENT_COMMANDS_CAPACITY: usize = 50;

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

    /// Max queued buffers between USB reader and TCP writer. 0 = use
    /// [`DEFAULT_BUFFER_CAPACITY`].
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
/// Each field is either a session-scoped counter (reset when the
/// current client disconnects — `bytes_sent`, `buffers_dropped`,
/// `last_command`, and the `current_*` commanded fields) or a
/// session-identity hint (`connected_client`, `connected_since`).
/// UI callers snapshot the struct via `Server::stats()` on a timer
/// and compute deltas (e.g. data-rate) across consecutive snapshots.
#[derive(Debug, Clone, Default)]
pub struct ServerStats {
    /// Socket address of the currently-connected client. `None`
    /// means the accept loop is waiting.
    pub connected_client: Option<SocketAddr>,
    /// Wall-clock moment the current session began. Paired with
    /// `connected_client`: both are `Some` or both are `None`.
    pub connected_since: Option<Instant>,
    /// Bytes written to the client socket across the current
    /// session. Reset to 0 on connect / disconnect.
    pub bytes_sent: u64,
    /// Buffer-drop count: how many times the USB→TCP queue was
    /// full when a new IQ block arrived, forcing us to discard.
    /// Reset to 0 on connect / disconnect.
    pub buffers_dropped: u64,
    /// Most recent command received from the client, with the
    /// moment it was dispatched. UI renders this as the "activity
    /// log" preview. Reset to `None` on disconnect.
    pub last_command: Option<(CommandOp, Instant)>,
    /// Most recent `SetCenterFreq` value requested by the client,
    /// in Hz. Populated from the wire param so it reflects what
    /// the client asked for even if the device layer rejected it;
    /// UI treats this as "the client thinks we're tuned here."
    /// Reset on disconnect.
    pub current_freq_hz: Option<u32>,
    /// Most recent `SetSampleRate` request, in Hz. Same "client's
    /// view" semantics as `current_freq_hz`.
    pub current_sample_rate_hz: Option<u32>,
    /// Most recent `SetTunerGain` request, in tenths of dB
    /// (negative is legal per upstream). `None` means the client
    /// hasn't requested a manual gain since connecting.
    pub current_gain_tenths_db: Option<i32>,
    /// `true` when the client most recently sent
    /// `SetGainMode(auto)`, `false` on `SetGainMode(manual)`,
    /// `None` when it hasn't sent one this session. UI renders
    /// "auto" vs "manual" accordingly.
    pub current_gain_auto: Option<bool>,
    /// Ring buffer of recent commands received this session, bounded
    /// at `RECENT_COMMANDS_CAPACITY` entries. Newest-first ordering
    /// is the UI's responsibility; this preserves insertion order
    /// (oldest at front, newest at back) so the producer stays cheap
    /// (`push_back` + `pop_front` at cap). Reset on connect /
    /// disconnect along with the other per-session counters.
    pub recent_commands: VecDeque<(CommandOp, Instant)>,
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
    stats: Arc<Mutex<ServerStats>>,
    bind: SocketAddr,
    tuner: TunerAdvertiseInfo,
    compression: crate::codec::CodecMask,
}

impl Server {
    /// Bind the listener, open the RTL-SDR, apply initial defaults, and
    /// start accepting clients.
    ///
    /// The returned handle owns the accept thread. Dropping it signals
    /// shutdown and waits for the current client (if any) to disconnect.
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
        // The listener is already blocking by default from `bind` —
        // no need to force it here. The accept thread flips it to
        // nonblocking immediately on entry.

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
        let stats = Arc::new(Mutex::new(ServerStats::default()));

        let dev_mutex = Arc::new(Mutex::new(device));
        let capacity = if config.buffer_capacity == 0 {
            DEFAULT_BUFFER_CAPACITY
        } else {
            config.buffer_capacity
        };

        let accept_thread = spawn_accept_thread(
            listener,
            dev_mutex,
            shutdown.clone(),
            stopped.clone(),
            stats.clone(),
            capacity,
            config.initial.clone(),
            config.compression,
        )?;

        Ok(Server {
            shutdown,
            stopped,
            accept_thread: Some(accept_thread),
            stats,
            bind: actual_bind,
            tuner,
            compression: config.compression,
        })
    }

    /// Current server statistics.
    pub fn stats(&self) -> ServerStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
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

    /// Has the accept thread exited (either via `stop()` or an
    /// unrecoverable error like USB device loss)?
    ///
    /// CLI callers poll this alongside their own Ctrl-C handler so the
    /// process exits when serving actually stops, instead of sleeping
    /// forever after the dongle is unplugged.
    pub fn has_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Signal shutdown and wait for the accept thread to exit.
    ///
    /// Equivalent to dropping the `Server`. Any panic from the accept
    /// thread is silently swallowed — if you need to observe panics,
    /// keep the `JoinHandle` yourself instead of calling `stop()`.
    pub fn stop(mut self) {
        self.initiate_shutdown();
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }

    fn initiate_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.initiate_shutdown();
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }
}

/// Lock the device and reapply initial state for a new client session.
/// Exists so the accept loop has a small surface that wraps the lock
/// acquisition; `apply_initial_state` itself stays lock-agnostic.
fn reset_device_to_initial(
    device: &Arc<Mutex<RtlSdrDevice>>,
    initial: &InitialDeviceState,
) -> Result<(), ServerError> {
    let mut dev = device
        .lock()
        .map_err(|_| ServerError::Io(std::io::Error::other("device mutex poisoned")))?;
    apply_initial_state(&mut dev, initial)
}

/// Apply the user's initial settings to the freshly-opened device.
///
/// Mirrors the setup block in rtl_tcp.c:490-520. Called once at
/// `Server::start` so the dongle is in a sane state even before any
/// client connects, and again on every new client session via
/// `reset_device_to_initial` so sequential clients don't inherit each
/// other's tuning.
fn apply_initial_state(
    dev: &mut RtlSdrDevice,
    initial: &InitialDeviceState,
) -> Result<(), ServerError> {
    // 0 is a valid direct-sampling state (off) and MUST be applied —
    // not skipped — so a previous session that enabled direct sampling
    // doesn't leak its mode into the next client's session. Previously
    // the `!= 0` guard treated 0 as "leave alone," which broke
    // reset_device_to_initial's promise of a clean slate per client.
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

/// Spawn the outer accept loop. Upstream's main runs this inline; we run
/// it on a thread so `Server::start` can return a handle to the caller.
///
/// `initial_state` is reapplied on every new client session so sequential
/// clients start from a clean slate rather than inheriting the prior
/// client's tuning / gain / direct-sampling state. Matches upstream's
/// `accept → apply defaults → reset_buffer → spawn workers` shape,
/// extended to the multi-client accept loop.
///
/// Returns `Err` on thread spawn failure (rare — kernel resource
/// exhaustion). Callers propagate up to the user.
#[allow(
    clippy::too_many_arguments,
    reason = "#307 grew the accept-thread signature with `compression`; \
              refactoring to a context struct would churn every caller \
              without improving readability"
)]
fn spawn_accept_thread(
    listener: TcpListener,
    device: Arc<Mutex<RtlSdrDevice>>,
    shutdown: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    stats: Arc<Mutex<ServerStats>>,
    buffer_capacity: usize,
    initial_state: InitialDeviceState,
    compression: CodecMask,
) -> std::io::Result<JoinHandle<()>> {
    // Poll-accept cadence means the listener must be nonblocking.
    // Configure it BEFORE spawning so failures surface through
    // `Server::start`'s `?` rather than getting buried inside the
    // spawned thread body — burying it would return `Ok` to the caller
    // and the accept thread would die without ever setting `stopped`,
    // leaving callers polling `has_stopped()` stuck forever.
    listener.set_nonblocking(true)?;
    thread::Builder::new()
        .name("rtl_tcp-accept".into())
        .spawn(move || {
            // Session slot: set to true while a client is being served.
            // Kept in an Arc so the session thread can clear it on exit.
            // Using `swap(true, ...)` to claim the slot atomically — if
            // it returns true, we were already busy and this new accept
            // must be rejected immediately (kernel had queued it in the
            // backlog, but we refuse to hold it).
            let busy = Arc::new(AtomicBool::new(false));
            let mut session_handle: Option<JoinHandle<()>> = None;

            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        if busy.swap(true, Ordering::SeqCst) {
                            tracing::info!(
                                %peer,
                                "rtl_tcp already serving a client — rejecting new connection"
                            );
                            // Close the socket immediately so the client
                            // sees FIN instead of hanging in backlog.
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            // swap set busy=true, but we weren't actually
                            // idle — the already-active session will
                            // eventually clear busy when it exits. Leave
                            // the flag set; don't reset.
                            continue;
                        }
                        // We now own the session slot. Reap the previous
                        // session handle if any (it must be finished,
                        // because the session thread clears busy at its
                        // tail — we got false from the swap, which means
                        // the prior session already cleared it).
                        if let Some(h) = session_handle.take() {
                            let _ = h.join();
                        }

                        tracing::info!(%peer, "rtl_tcp client connected");
                        if let Err(e) = stream.set_nonblocking(false) {
                            tracing::error!(%e, "failed to set client socket blocking");
                            busy.store(false, Ordering::SeqCst);
                            continue;
                        }
                        configure_client_socket(&stream);
                        update_stats_on_connect(&stats, peer);

                        // Reapply initial state BEFORE spawning workers
                        // so this client starts on clean tuning/gain
                        // rather than inheriting the prior session's
                        // state. Matches upstream's per-accept setup
                        // block (rtl_tcp.c:490-520).
                        if let Err(e) = reset_device_to_initial(&device, &initial_state) {
                            tracing::error!(
                                %e, %peer,
                                "rtl_tcp failed to reset device to initial state, dropping client"
                            );
                            busy.store(false, Ordering::SeqCst);
                            update_stats_on_disconnect(&stats);
                            continue;
                        }

                        let session_device = device.clone();
                        let session_shutdown = shutdown.clone();
                        let session_stats = stats.clone();
                        let session_busy = busy.clone();
                        let session_compression = compression;
                        match thread::Builder::new().name("rtl_tcp-session".into()).spawn(
                            move || {
                                handle_client(
                                    stream,
                                    session_device,
                                    session_shutdown,
                                    session_stats.clone(),
                                    buffer_capacity,
                                    session_compression,
                                );
                                update_stats_on_disconnect(&session_stats);
                                tracing::info!(%peer, "rtl_tcp client disconnected");
                                session_busy.store(false, Ordering::SeqCst);
                            },
                        ) {
                            Ok(h) => session_handle = Some(h),
                            Err(e) => {
                                tracing::error!(%e, "failed to spawn session thread");
                                busy.store(false, Ordering::SeqCst);
                                update_stats_on_disconnect(&stats);
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

            // Shutdown: wait for the active session (if any) to finish
            // before returning, so Server::drop sees all workers done.
            if let Some(h) = session_handle.take() {
                let _ = h.join();
            }
            // Signal to CLI / UI callers polling `Server::has_stopped()`
            // that the server is no longer serving. Set AFTER the
            // session join so a caller that observes `has_stopped() ==
            // true` can safely assume all workers have exited.
            stopped.store(true, Ordering::SeqCst);
            tracing::debug!("rtl_tcp accept thread exiting");
        })
}

fn configure_client_socket(stream: &TcpStream) {
    // Keep TCP alive so dead clients (laptop lid closed, wifi dropped) stop
    // wedging the server rather than trickling forever into the void.
    // Per-platform keepalive tuning lives in std behind raw setsockopt; we
    // enable the default which at least lets the kernel eventually notice.
    if let Err(e) = set_keepalive(stream, true) {
        tracing::warn!(%e, "SO_KEEPALIVE not applied (non-fatal)");
    }
    // Disable Nagle — commands are 5 bytes and we want snappy tuning.
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!(%e, "TCP_NODELAY not applied (non-fatal)");
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn set_keepalive(stream: &TcpStream, on: bool) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let value: libc::c_int = libc::c_int::from(on);
    // SAFETY: `fd` is a valid open socket for the duration of this call
    // (we borrow `stream` by reference). `value` is a stack-local
    // `c_int` passed as a pointer along with the matching size — this
    // is the documented shape of `setsockopt(_, SOL_SOCKET,
    // SO_KEEPALIVE, ...)` on every POSIX target.
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
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

#[cfg(not(unix))]
fn set_keepalive(_stream: &TcpStream, _on: bool) -> std::io::Result<()> {
    // Non-unix has no implementation yet. Return Unsupported so the
    // warn path in `configure_client_socket` fires and the log makes
    // the missing keepalive visible — silently returning Ok would
    // leave operators thinking dead-peer detection is active when it
    // isn't.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SO_KEEPALIVE not implemented on this platform",
    ))
}

fn update_stats_on_connect(stats: &Arc<Mutex<ServerStats>>, peer: SocketAddr) {
    if let Ok(mut s) = stats.lock() {
        s.connected_client = Some(peer);
        s.connected_since = Some(Instant::now());
        s.bytes_sent = 0;
        s.buffers_dropped = 0;
        s.last_command = None;
        s.current_freq_hz = None;
        s.current_sample_rate_hz = None;
        s.current_gain_tenths_db = None;
        s.current_gain_auto = None;
        s.recent_commands.clear();
    }
}

fn update_stats_on_disconnect(stats: &Arc<Mutex<ServerStats>>) {
    if let Ok(mut s) = stats.lock() {
        // `update_stats_on_connect` treats every counter below as
        // session-scoped (resets them on new connect), so the
        // disconnect path must clear them too — otherwise a UI polling
        // `ServerStats` while no client is connected would see stale
        // traffic / command data from the previous session.
        s.connected_client = None;
        s.connected_since = None;
        s.bytes_sent = 0;
        s.buffers_dropped = 0;
        s.last_command = None;
        s.current_freq_hz = None;
        s.current_sample_rate_hz = None;
        s.current_gain_tenths_db = None;
        s.current_gain_auto = None;
        s.recent_commands.clear();
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
/// - `Ok(None)` — legacy fallback. Reached either on a zero-byte
///   timeout/EOF (idle client never sent anything) OR on a
///   non-zero peek whose prefix doesn't match [`EXTENSION_MAGIC`]
///   (legacy client sent a command; the bytes stay queued in the
///   receive buffer so `command_worker` can parse the 5-byte
///   frame). Nothing is consumed in either sub-case.
/// - `Err(_)` — protocol error, raised only after the magic
///   already matched and we committed to reading a full 8 bytes.
///   Covers `read_exact` timeout or EOF mid-hello (partial hello,
///   bytes already drained from the stream) and parse failure
///   on a complete 8-byte block (unknown role, unknown protocol
///   version, etc.). Falling back to legacy from either of these
///   states would desync the command stream, so the caller drops
///   the client.
///
/// Uses `peek()` for the initial magic check so legitimate legacy
/// traffic stays intact. Once the magic matches we commit to
/// reading the full 8 bytes; partial reads are fatal because we
/// can't un-consume the half we already read. Per CodeRabbit
/// round 2 on PR #399 (initial fix) + round 3 (doc alignment).
fn sniff_client_hello(mut stream: &TcpStream) -> std::io::Result<Option<ClientHello>> {
    stream.set_read_timeout(Some(HELLO_SNIFF_TIMEOUT))?;
    // Peek the first 4 bytes (magic-only check). `peek` maps to
    // `recv(…, MSG_PEEK)` which respects `SO_RCVTIMEO`, so this
    // returns WouldBlock / TimedOut after the timeout without
    // consuming bytes.
    let mut peek_buf = [0u8; EXTENSION_MAGIC.len()];
    let peeked = match stream.peek(&mut peek_buf) {
        Ok(n) => n,
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            // Pure timeout with zero bytes observed — the client
            // never sent anything, so this is an idle legacy peer
            // (or a port scanner). Safe to fall back; no bytes
            // were consumed.
            stream.set_read_timeout(None)?;
            return Ok(None);
        }
        Err(e) => {
            // Other errors (ECONNRESET, etc.) → propagate so the
            // caller tears down cleanly.
            stream.set_read_timeout(None)?;
            return Err(e);
        }
    };
    if peeked == 0 {
        // Peer closed cleanly before sending anything. Same
        // safety as a timeout-with-zero-bytes: no bytes consumed,
        // nothing to desync.
        stream.set_read_timeout(None)?;
        return Ok(None);
    }
    if peeked < EXTENSION_MAGIC.len() || peek_buf[..EXTENSION_MAGIC.len()] != EXTENSION_MAGIC {
        // Legacy client — either sent fewer than 4 bytes but
        // something (so we can't tell if they might still be
        // sending a hello), or the first bytes aren't the
        // sdr-rs magic. Preserving bytes for the command reader
        // is only safe when we know they're the start of a
        // command: a vanilla `SetCenterFreq` starts with 0x01,
        // no documented opcode begins with 'R' (0x52), so a
        // mismatch on a full 4-byte peek is a legitimate legacy
        // command. A short prefix that doesn't match magic is
        // ambiguous but benign — the command reader will parse
        // it as a 5-byte command frame and dispatch or log
        // unknown-opcode.
        stream.set_read_timeout(None)?;
        return Ok(None);
    }
    // Magic matched — commit to consuming 8 bytes. A timeout or
    // EOF here is no longer a safe fallback: we've verified the
    // client started an extended hello and consumed `read_exact`
    // will have eaten whatever bytes arrived before the stall.
    // Returning `Ok(None)` would let the legacy path start
    // against a shifted command stream — exactly the desync
    // CodeRabbit round 2 flagged. Treat every failure mode as a
    // protocol error and drop the client.
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

/// Serve exactly one client. Spawns the three worker threads, waits for
/// the first to exit, signals the others, joins all.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "#307 grew the session signature with compression_offer; \
              refactoring to a context struct would churn every rtl_tcp \
              server test without improving readability"
)]
fn handle_client(
    stream: TcpStream,
    device: Arc<Mutex<RtlSdrDevice>>,
    global_shutdown: Arc<AtomicBool>,
    stats: Arc<Mutex<ServerStats>>,
    buffer_capacity: usize,
    compression_offer: CodecMask,
) {
    // Extended handshake (#307). Must happen BEFORE we write the
    // legacy `dongle_info_t` — if the client sent an `"RTLX"`
    // hello, we want to write the server response block
    // immediately after the legacy header, all in one atomic
    // stretch, so the client's `peek` for the `"RTLX"` magic
    // lands on our bytes and not on IQ samples the data worker
    // has queued up.
    let negotiated_codec = match sniff_client_hello(&stream) {
        Ok(Some(hello)) => {
            let codec = compression_offer.pick(hello.codec_mask);
            tracing::info!(
                client_mask = hello.codec_mask.to_wire(),
                server_mask = compression_offer.to_wire(),
                chosen = %codec,
                "rtl_tcp extended-handshake negotiated"
            );
            Some(codec)
        }
        Ok(None) => {
            tracing::debug!("rtl_tcp no extended-handshake hello — legacy client path");
            None
        }
        Err(e) => {
            tracing::warn!(%e, "rtl_tcp handshake sniff failed — dropping client");
            return;
        }
    };

    // Send the 12-byte dongle_info_t header first (rtl_tcp.c:576-594).
    let header = {
        let Ok(dev) = device.lock() else {
            tracing::error!("device mutex poisoned, aborting client");
            return;
        };
        DongleInfo {
            tuner: TunerTypeCode::from(dev.tuner_type()),
            gain_count: dev.tuner_gains().len() as u32,
        }
    };
    let header_bytes = header.to_bytes();
    let writer_stream = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(%e, "failed to clone client stream for writer — dropping client");
            return;
        }
    };
    let mut writer = writer_stream;
    if let Err(e) = writer.write_all(&header_bytes) {
        tracing::warn!(%e, "failed to send dongle_info_t — client gone");
        return;
    }

    // If we negotiated the extended protocol, emit the
    // `ServerExtension` block immediately after `dongle_info_t`.
    // Must land before any IQ data or the client's magic-peek
    // after `dongle_info` will read random samples instead.
    if let Some(codec) = negotiated_codec {
        let ext = ServerExtension {
            codec,
            // #307 is single-client; role and status are reserved
            // for #392/#394 and always report OK / Control here.
            granted_role: Some(Role::Control),
            status: Status::Ok,
            version: PROTOCOL_VERSION,
        };
        if let Err(e) = writer.write_all(&ext.to_bytes()) {
            tracing::warn!(%e, "failed to send RTLX server extension — client gone");
            return;
        }
    }

    // Per-client shutdown flag. Flipped when any worker exits, so the
    // others stop quickly. Honors the global flag too.
    let client_shutdown = Arc::new(AtomicBool::new(false));
    let merged_shutdown = MergedShutdown::new(global_shutdown, client_shutdown);

    // Buffer data path: USB bulk → bounded channel → TCP writer.
    let (tx, rx) = sync_channel::<Vec<u8>>(buffer_capacity);

    let reader_shutdown = merged_shutdown.clone();
    let reader_device = device.clone();
    let reader_stats = stats.clone();
    let Ok(reader_handle) = thread::Builder::new()
        .name("rtl_tcp-reader".into())
        .spawn(move || {
            data_worker(reader_device, tx, reader_shutdown, reader_stats);
        })
    else {
        tracing::error!("failed to spawn rtl_tcp reader thread — dropping client");
        return;
    };

    // Install the write timeout on the underlying TcpStream
    // BEFORE wrapping in the codec's encoder — the encoder's
    // `write()` delegates to the inner stream's `write()`, which
    // in turn enforces `SO_SNDTIMEO`. Setting after-wrap would
    // lose visibility into the inner stream.
    if let Err(e) = writer.set_write_timeout(Some(WRITER_RECV_TIMEOUT)) {
        tracing::warn!(%e, "set_write_timeout on data channel failed; dropping client");
        merged_shutdown.set_client();
        let _ = reader_handle.join();
        return;
    }
    // Wrap the socket in the stats-tracking adapter BEFORE the
    // encoder so `bytes_sent` reflects post-compression bytes
    // (what actually hit the wire), then wrap in the negotiated
    // codec. Legacy clients get a pass-through (`Codec::None`),
    // so the write path stays byte-identical to the pre-#307
    // behavior — on-wire bytes equal payload bytes in that case.
    let tracked_writer = StatsTrackingWrite {
        inner: writer,
        stats: stats.clone(),
    };
    let encoded_writer = Encoder::new(negotiated_codec.unwrap_or(Codec::None), tracked_writer);
    let writer_shutdown = merged_shutdown.clone();
    let Ok(writer_handle) = thread::Builder::new()
        .name("rtl_tcp-writer".into())
        .spawn(move || {
            tcp_writer(encoded_writer, rx, writer_shutdown);
        })
    else {
        tracing::error!("failed to spawn rtl_tcp writer thread — tearing down client");
        merged_shutdown.set_client();
        let _ = reader_handle.join();
        return;
    };

    let command_shutdown = merged_shutdown.clone();
    let command_device = device;
    let command_stats = stats;
    let command_stream = stream;
    let Ok(command_handle) =
        thread::Builder::new()
            .name("rtl_tcp-command".into())
            .spawn(move || {
                command_worker(
                    command_stream,
                    command_device,
                    command_shutdown,
                    command_stats,
                );
            })
    else {
        tracing::error!("failed to spawn rtl_tcp command thread — tearing down client");
        merged_shutdown.set_client();
        let _ = reader_handle.join();
        let _ = writer_handle.join();
        return;
    };

    // Wait for any worker to exit, then cancel the others.
    let _ = command_handle.join();
    merged_shutdown.set_client();
    let _ = reader_handle.join();
    let _ = writer_handle.join();
}

/// Combines the server-wide shutdown flag with a per-client flag so we can
/// tear down one client without stopping the server, and vice versa.
#[derive(Clone)]
struct MergedShutdown {
    global: Arc<AtomicBool>,
    client: Arc<AtomicBool>,
}

impl MergedShutdown {
    fn new(global: Arc<AtomicBool>, client: Arc<AtomicBool>) -> Self {
        Self { global, client }
    }
    fn is_set(&self) -> bool {
        self.global.load(Ordering::Relaxed) || self.client.load(Ordering::Relaxed)
    }
    fn set_client(&self) {
        self.client.store(true, Ordering::SeqCst);
    }
    /// Escalate to server-wide shutdown: the accept thread exits after
    /// the current session tears down, and `Server::has_stopped()`
    /// eventually observes `true`. Used for unrecoverable errors that
    /// can't be remedied by just dropping the current client, such as
    /// a lost USB dongle (`rusb::Error::NoDevice`).
    fn set_global(&self) {
        self.global.store(true, Ordering::SeqCst);
        self.client.store(true, Ordering::SeqCst);
    }
}

/// Continuously pull USB bulk buffers and push into the bounded queue.
/// Drops on full, matching upstream's `llbuf_num` cap behavior.
fn data_worker(
    device: Arc<Mutex<RtlSdrDevice>>,
    tx: SyncSender<Vec<u8>>,
    shutdown: MergedShutdown,
    stats: Arc<Mutex<ServerStats>>,
) {
    // Pull an Arc<DeviceHandle> once so we don't have to lock the device
    // mutex on every USB read (bulk read is &self-safe via usb_handle).
    let handle = {
        let Ok(dev) = device.lock() else {
            // Poisoned mutex is unrecoverable shared state — close out
            // the whole session so the writer/command workers exit too
            // instead of spinning on a dead channel.
            tracing::error!("device mutex poisoned, data worker aborting and closing session");
            shutdown.set_client();
            return;
        };
        dev.usb_handle()
    };
    let timeout = USB_READ_TIMEOUT;
    // Scratch buffer reused across iterations — only the Vec we actually
    // send to the writer gets a fresh allocation, sized to the data the
    // USB read returned. This avoids allocating 256 KiB on every timeout
    // tick (reviewed on PR #313).
    let mut scratch = vec![0u8; READ_BUFFER_LEN as usize];
    // Edge-trigger flag for the tx-queue-full warning. Set when a
    // drop happens, cleared on the first successful send after — so
    // we log once per stall-and-drain cycle rather than per buffer.
    let mut was_dropping = false;

    while !shutdown.is_set() {
        match handle.read_bulk(sdr_rtlsdr::constants::BULK_ENDPOINT, &mut scratch, timeout) {
            Ok(n) if n > 0 => {
                // Allocate only when we have real data to hand off.
                let buf = scratch[..n].to_vec();
                match tx.try_send(buf) {
                    Ok(()) => {
                        // Rearm the overflow edge so a future stall
                        // logs again.
                        was_dropping = false;
                    }
                    Err(TrySendError::Full(_)) => {
                        // Queue is full — drop this buffer (upstream does
                        // the same when the linked list exceeds llbuf_num;
                        // rtl_tcp.c:137-152). `buffers_dropped` in the
                        // shared stats is the authoritative cumulative
                        // counter; the warn is just an edge signal.
                        if let Ok(mut s) = stats.lock() {
                            s.buffers_dropped = s.buffers_dropped.saturating_add(1);
                        }
                        if !was_dropping {
                            tracing::warn!(
                                "rtl_tcp tx queue full — dropping USB buffers (further drops accumulate silently; see ServerStats::buffers_dropped)"
                            );
                            was_dropping = true;
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        tracing::debug!("writer gone, data worker exiting");
                        return;
                    }
                }
            }
            Ok(_) | Err(rusb::Error::Timeout) => {
                // No data — loop and re-check shutdown.
            }
            Err(rusb::Error::NoDevice) => {
                // Dongle unplug is unrecoverable at the server level —
                // the accept loop has nothing to serve. Escalate to a
                // global shutdown so the accept thread exits, the CLI
                // sees `has_stopped() == true`, and new clients don't
                // connect to a dead-device server.
                tracing::error!("rtl_tcp: USB device lost mid-stream, stopping server");
                shutdown.set_global();
                return;
            }
            Err(e) => {
                tracing::error!(%e, "rtl_tcp bulk read error");
                shutdown.set_client();
                return;
            }
        }
    }
}

/// `Write` adapter that mirrors the underlying [`TcpStream`] but
/// updates [`ServerStats::bytes_sent`] with the post-compression
/// byte count from each successful write. Placed between the
/// [`Encoder`] and the socket so the stat reflects **on-wire**
/// throughput instead of pre-compression payload size — otherwise
/// the server-panel's data-rate row would show raw sample rate
/// even when LZ4 is active and operators couldn't see whether
/// compression was actually saving bandwidth.
///
/// Per CodeRabbit round 1 on PR #399.
struct StatsTrackingWrite {
    inner: TcpStream,
    stats: Arc<Mutex<ServerStats>>,
}

impl Write for StatsTrackingWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        // Poisoned mutex only happens if a stats reader panicked
        // while holding the lock — we'd rather keep streaming and
        // let the stats drift than tear the session down. Matches
        // the existing lock-use pattern in data_worker.
        if let Ok(mut s) = self.stats.lock() {
            s.bytes_sent = s.bytes_sent.saturating_add(n as u64);
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn tcp_writer<W: Write + Send>(
    mut stream: W,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    shutdown: MergedShutdown,
) {
    // Write timeout is installed by the caller on the underlying
    // `TcpStream` before wrapping in the codec — see the comment
    // in `handle_client` where the timeout is set up. Putting it
    // here would lose visibility into the inner stream when
    // `stream` is an `Encoder`.
    //
    // `bytes_sent` bookkeeping is handled by `StatsTrackingWrite`
    // one layer below the encoder, so this function no longer
    // needs a `stats` arg. On-wire counts land there directly.
    //
    // `recv_timeout` lets us notice shutdown even when the USB
    // reader is starving (e.g., dongle unplug).
    loop {
        if shutdown.is_set() {
            return;
        }
        match rx.recv_timeout(WRITER_RECV_TIMEOUT) {
            Ok(buf) => {
                if let Err(e) = stream.write_all(&buf) {
                    tracing::debug!(%e, "rtl_tcp client socket write failed, closing");
                    shutdown.set_client();
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
                    tracing::debug!(%e, "rtl_tcp client socket flush failed, closing");
                    shutdown.set_client();
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Re-check shutdown flag above.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return;
            }
        }
    }
}

fn command_worker(
    mut stream: TcpStream,
    device: Arc<Mutex<RtlSdrDevice>>,
    shutdown: MergedShutdown,
    stats: Arc<Mutex<ServerStats>>,
) {
    // Upstream loops on a 1 s select() so shutdown is noticed promptly.
    // Our equivalent is the socket read timeout. If we can't install it,
    // `read_full` would block indefinitely in `stream.read()` without
    // ever re-checking the shutdown flag — which would deadlock
    // `handle_client`'s join on this worker, then the accept thread's
    // join on handle_client, then `Server::Drop`. Treat the failure as
    // fatal for this client session.
    if let Err(e) = stream.set_read_timeout(Some(COMMAND_READ_TIMEOUT)) {
        tracing::warn!(%e, "set_read_timeout on command channel failed; dropping client");
        shutdown.set_client();
        return;
    }
    let mut buf = [0u8; COMMAND_LEN];
    while !shutdown.is_set() {
        match read_full(&mut stream, &mut buf, &shutdown) {
            ReadResult::Ok => {}
            ReadResult::Eof => {
                tracing::debug!("rtl_tcp command channel EOF");
                shutdown.set_client();
                return;
            }
            ReadResult::Shutdown => return,
            ReadResult::Err(e) => {
                tracing::warn!(%e, "rtl_tcp command recv error");
                shutdown.set_client();
                return;
            }
        }
        let Some(cmd) = Command::from_bytes(&buf) else {
            // Upstream silently drops unknown opcodes (switch has no default).
            tracing::debug!(op = buf[0], "rtl_tcp unknown command opcode, dropping");
            continue;
        };
        let Ok(mut dev) = device.lock() else {
            // Same rationale as data_worker: a poisoned device mutex
            // is unrecoverable, and silently dropping commands here
            // would leave the client driving the UI with no visible
            // effect on the server. Close the session.
            tracing::error!("device mutex poisoned, command worker aborting and closing session");
            shutdown.set_client();
            return;
        };
        dispatch(&mut dev, cmd);
        drop(dev);
        if let Ok(mut s) = stats.lock() {
            let now = Instant::now();
            s.last_command = Some((cmd.op, now));
            // Push onto the bounded ring. Pop the oldest entry when
            // we'd otherwise exceed the cap — keeps memory bounded
            // on long-running sessions without a dedicated ring-
            // buffer crate.
            if s.recent_commands.len() >= RECENT_COMMANDS_CAPACITY {
                s.recent_commands.pop_front();
            }
            s.recent_commands.push_back((cmd.op, now));
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

enum ReadResult {
    Ok,
    Eof,
    Shutdown,
    Err(std::io::Error),
}

/// Read exactly `buf.len()` bytes, splitting across multiple `read`s but
/// re-checking the shutdown flag on each timeout. Mirrors the upstream
/// `while(left > 0)` loop in rtl_tcp.c:297-313.
fn read_full(stream: &mut TcpStream, buf: &mut [u8], shutdown: &MergedShutdown) -> ReadResult {
    let mut filled = 0;
    while filled < buf.len() {
        if shutdown.is_set() {
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
    fn update_stats_on_disconnect_clears_per_session_counters() {
        // Session-scoped counters (bytes_sent, buffers_dropped,
        // last_command, current_* commanded fields) must be cleared
        // when the client disconnects — otherwise a UI polling
        // ServerStats would see stale data from the prior session
        // while connected_client = None, e.g. the status row would
        // still show "100.3 MHz @ 2.4 MHz" for a dead session.
        let mut recent = VecDeque::new();
        recent.push_back((CommandOp::SetCenterFreq, Instant::now()));
        recent.push_back((CommandOp::SetTunerGain, Instant::now()));
        let stats = Arc::new(Mutex::new(ServerStats {
            connected_client: Some(SocketAddr::from(([127, 0, 0, 1], 42_000))),
            connected_since: Some(Instant::now()),
            bytes_sent: 12345,
            buffers_dropped: 7,
            last_command: Some((CommandOp::SetCenterFreq, Instant::now())),
            current_freq_hz: Some(100_300_000),
            current_sample_rate_hz: Some(2_400_000),
            current_gain_tenths_db: Some(200),
            current_gain_auto: Some(false),
            recent_commands: recent,
        }));
        update_stats_on_disconnect(&stats);
        let s = stats.lock().unwrap();
        assert!(s.connected_client.is_none());
        assert!(s.connected_since.is_none());
        assert_eq!(s.bytes_sent, 0);
        assert_eq!(s.buffers_dropped, 0);
        assert!(s.last_command.is_none());
        assert!(s.current_freq_hz.is_none());
        assert!(s.current_sample_rate_hz.is_none());
        assert!(s.current_gain_tenths_db.is_none());
        assert!(s.current_gain_auto.is_none());
        assert!(
            s.recent_commands.is_empty(),
            "activity log ring must clear on disconnect to avoid stale entries in the next session"
        );
    }

    #[test]
    fn server_stats_default_is_not_connected() {
        let stats = ServerStats::default();
        assert!(stats.connected_client.is_none());
        assert!(stats.connected_since.is_none());
        assert_eq!(stats.bytes_sent, 0);
        assert_eq!(stats.buffers_dropped, 0);
        assert!(stats.last_command.is_none());
        assert!(stats.current_freq_hz.is_none());
        assert!(stats.current_sample_rate_hz.is_none());
        assert!(stats.current_gain_tenths_db.is_none());
        assert!(stats.current_gain_auto.is_none());
        assert!(stats.recent_commands.is_empty());
    }

    #[test]
    fn recent_commands_capacity_matches_documented_bound() {
        // Sanity check on the published const. If the UI side starts
        // depending on a specific size for pagination, changing the
        // constant becomes a contract break this test catches.
        assert_eq!(RECENT_COMMANDS_CAPACITY, 50);
    }

    #[test]
    fn merged_shutdown_set_global_escalates_to_both_flags() {
        // set_client() → client=true, global unchanged.
        // set_global() → both flags true, so accept loop also exits.
        // Regression test for the "NoDevice flips client only" bug:
        // unplug used to stop the current session but leave the accept
        // thread polling forever against a dead dongle.
        let ms = MergedShutdown::new(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(!ms.is_set());

        ms.set_client();
        assert!(ms.is_set());
        assert!(!ms.global.load(Ordering::Relaxed));
        assert!(ms.client.load(Ordering::Relaxed));

        // Reset client so we can see set_global set BOTH.
        ms.client.store(false, Ordering::SeqCst);
        assert!(!ms.is_set());
        ms.set_global();
        assert!(ms.global.load(Ordering::Relaxed));
        assert!(ms.client.load(Ordering::Relaxed));
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

    // ============================================================
    // sniff_client_hello regression tests (CodeRabbit round 2 on PR #399)
    //
    // The sniff is the only piece of `handle_client` that can run
    // without a real RTL-SDR dongle, so unit tests live here.
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
        // **Regression test for CodeRabbit round 2 on PR #399.**
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
