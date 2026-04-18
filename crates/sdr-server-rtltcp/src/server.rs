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

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sdr_rtlsdr::device::RtlSdrDevice;

use crate::dispatch::dispatch;
use crate::error::ServerError;
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

/// Default sample rate in Hz. Matches upstream `rtl_tcp.c:DEFAULT_SAMPLE_RATE_HZ`.
///
/// Exposed so the CLI can share the same constant instead of hard-coding
/// the literal — keeps CLI and library defaults in lock-step if we ever
/// change it.
pub const DEFAULT_SAMPLE_RATE_HZ: u32 = 2_048_000;

/// Default center frequency in Hz, matching upstream rtl_tcp's
/// `frequency = 100000000` default at rtl_tcp.c:389.
pub const DEFAULT_CENTER_FREQ_HZ: u32 = 100_000_000;

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
#[derive(Debug, Clone, Default)]
pub struct ServerStats {
    pub connected_client: Option<SocketAddr>,
    pub connected_since: Option<Instant>,
    pub bytes_sent: u64,
    pub buffers_dropped: u64,
    pub last_command: Option<(CommandOp, Instant)>,
}

/// Running server handle.
pub struct Server {
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    stats: Arc<Mutex<ServerStats>>,
    bind: SocketAddr,
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
        // Set a short accept timeout so the accept loop can notice the
        // shutdown flag promptly on Server::drop.
        listener.set_nonblocking(false)?;

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

        tracing::info!(
            bind = %config.bind,
            tuner = ?device.tuner_type(),
            gain_count = device.tuner_gains().len(),
            "rtl_tcp server listening"
        );

        let shutdown = Arc::new(AtomicBool::new(false));
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
            stats.clone(),
            capacity,
        )?;

        Ok(Server {
            shutdown,
            accept_thread: Some(accept_thread),
            stats,
            bind: config.bind,
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

    /// Signal shutdown and wait for the accept thread to exit.
    ///
    /// Equivalent to dropping the `Server`, but lets the caller propagate
    /// join panics if desired.
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

/// Apply the user's initial settings to the freshly-opened device.
///
/// Mirrors the setup block in rtl_tcp.c:490-520.
fn apply_initial_state(
    dev: &mut RtlSdrDevice,
    initial: &InitialDeviceState,
) -> Result<(), ServerError> {
    if initial.direct_sampling != 0 {
        dev.set_direct_sampling(initial.direct_sampling)?;
    }
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
/// Returns `Err` on thread spawn failure (rare — kernel resource
/// exhaustion). Callers propagate up to the user.
fn spawn_accept_thread(
    listener: TcpListener,
    device: Arc<Mutex<RtlSdrDevice>>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Mutex<ServerStats>>,
    buffer_capacity: usize,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("rtl_tcp-accept".into())
        .spawn(move || {
            // Poll-accept with a short timeout so we notice shutdown within
            // a second. std's TcpListener doesn't have set_read_timeout for
            // the listen fd itself, so we use set_nonblocking + sleep.
            if let Err(e) = listener.set_nonblocking(true) {
                tracing::error!(
                    %e,
                    "failed to set accept listener nonblocking, accept thread exiting"
                );
                return;
            }

            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        tracing::info!(%peer, "rtl_tcp client connected");
                        // Switch stream back to blocking for the per-client
                        // workers; only the accept fd is nonblocking.
                        if let Err(e) = stream.set_nonblocking(false) {
                            tracing::error!(%e, "failed to set client socket blocking");
                            continue;
                        }
                        configure_client_socket(&stream);
                        update_stats_on_connect(&stats, peer);
                        handle_client(
                            stream,
                            device.clone(),
                            shutdown.clone(),
                            stats.clone(),
                            buffer_capacity,
                        );
                        update_stats_on_disconnect(&stats);
                        tracing::info!(%peer, "rtl_tcp client disconnected");
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        tracing::error!(%e, "rtl_tcp accept error");
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            }
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
    // Non-unix targets: caller swallows the warning.
    Ok(())
}

fn update_stats_on_connect(stats: &Arc<Mutex<ServerStats>>, peer: SocketAddr) {
    if let Ok(mut s) = stats.lock() {
        s.connected_client = Some(peer);
        s.connected_since = Some(Instant::now());
        s.bytes_sent = 0;
        s.buffers_dropped = 0;
        s.last_command = None;
    }
}

fn update_stats_on_disconnect(stats: &Arc<Mutex<ServerStats>>) {
    if let Ok(mut s) = stats.lock() {
        s.connected_client = None;
        s.connected_since = None;
    }
}

/// Serve exactly one client. Spawns the three worker threads, waits for
/// the first to exit, signals the others, joins all.
fn handle_client(
    stream: TcpStream,
    device: Arc<Mutex<RtlSdrDevice>>,
    global_shutdown: Arc<AtomicBool>,
    stats: Arc<Mutex<ServerStats>>,
    buffer_capacity: usize,
) {
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
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(%e, "failed to clone client stream for writer — dropping client");
            return;
        }
    };
    if let Err(e) = writer.write_all(&header_bytes) {
        tracing::warn!(%e, "failed to send dongle_info_t — client gone");
        return;
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

    let writer_shutdown = merged_shutdown.clone();
    let writer_stats = stats.clone();
    let Ok(writer_handle) = thread::Builder::new()
        .name("rtl_tcp-writer".into())
        .spawn(move || {
            tcp_writer(writer, rx, writer_shutdown, writer_stats);
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
            tracing::error!("device mutex poisoned, data worker aborting");
            return;
        };
        dev.usb_handle()
    };
    let timeout = Duration::from_secs(1);
    // Scratch buffer reused across iterations — only the Vec we actually
    // send to the writer gets a fresh allocation, sized to the data the
    // USB read returned. This avoids allocating 256 KiB on every timeout
    // tick (reviewed on PR #313).
    let mut scratch = vec![0u8; READ_BUFFER_LEN as usize];

    while !shutdown.is_set() {
        match handle.read_bulk(sdr_rtlsdr::constants::BULK_ENDPOINT, &mut scratch, timeout) {
            Ok(n) if n > 0 => {
                // Allocate only when we have real data to hand off.
                let buf = scratch[..n].to_vec();
                match tx.try_send(buf) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // Queue is full — drop this buffer (upstream does
                        // the same when the linked list exceeds llbuf_num;
                        // rtl_tcp.c:137-152).
                        if let Ok(mut s) = stats.lock() {
                            s.buffers_dropped = s.buffers_dropped.saturating_add(1);
                        }
                        tracing::warn!("rtl_tcp tx queue full — dropping USB buffer");
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
                tracing::error!("rtl_tcp: USB device lost mid-stream");
                shutdown.set_client();
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

fn tcp_writer(
    mut stream: TcpStream,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    shutdown: MergedShutdown,
    stats: Arc<Mutex<ServerStats>>,
) {
    // `recv_timeout` lets us notice shutdown even when the USB reader is
    // starving (e.g., dongle unplug).
    loop {
        if shutdown.is_set() {
            return;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(buf) => {
                if let Err(e) = stream.write_all(&buf) {
                    tracing::debug!(%e, "rtl_tcp client socket write failed, closing");
                    shutdown.set_client();
                    return;
                }
                if let Ok(mut s) = stats.lock() {
                    s.bytes_sent = s.bytes_sent.saturating_add(buf.len() as u64);
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
    if let Err(e) = stream.set_read_timeout(Some(COMMAND_READ_TIMEOUT)) {
        tracing::warn!(%e, "set_read_timeout on command channel failed");
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
        if let Ok(mut dev) = device.lock() {
            dispatch(&mut dev, cmd);
        }
        if let Ok(mut s) = stats.lock() {
            s.last_command = Some((cmd.op, Instant::now()));
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
        };
        match Server::start(config) {
            Err(crate::ServerError::PortInUse(ref addr)) => {
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
    fn server_stats_default_is_not_connected() {
        let stats = ServerStats::default();
        assert!(stats.connected_client.is_none());
        assert!(stats.connected_since.is_none());
        assert_eq!(stats.bytes_sent, 0);
        assert_eq!(stats.buffers_dropped, 0);
        assert!(stats.last_command.is_none());
    }

    #[test]
    fn buffer_capacity_zero_uses_default() {
        // ServerConfig exposes `buffer_capacity: 0` as "use default". This
        // is checked during Server::start, but we can sanity-check the
        // DEFAULT_BUFFER_CAPACITY matches upstream's llbuf_num = 500
        // (rtl_tcp.c:61).
        assert_eq!(DEFAULT_BUFFER_CAPACITY, 500);
    }
}
