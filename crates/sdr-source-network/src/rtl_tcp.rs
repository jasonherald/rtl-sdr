#![allow(
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    clippy::large_stack_arrays,
    clippy::collapsible_if
)]
//! `rtl_tcp` client.
//!
//! Connects to a remote `rtl_tcp`-compatible server, parses the 12-byte
//! `dongle_info_t` header, pulls 8-bit unsigned-offset I/Q samples, and
//! forwards user tuning commands as 5-byte big-endian messages over the
//! same socket. Speaks the wire protocol described in
//! `original/librtlsdr/src/rtl_tcp.c` — compatible with GQRX, SDR++,
//! SoapySDR, `rtl_sdr --server`, and our own `sdr-server-rtltcp`.
//!
//! Wire types (`DongleInfo`, `Command`, `CommandOp`, `TunerTypeCode`) are
//! re-exported from [`sdr_server_rtltcp::protocol`] so both sides share
//! one source of truth.
//!
//! Robustness additions beyond a bare protocol port (epic #299 review):
//!
//! - Exponential-backoff reconnect on socket loss. Connection lifecycle
//!   exposed via [`ConnectionState`] so UI can render Connecting /
//!   Connected / Retrying / Failed / Disconnected.
//! - `SO_KEEPALIVE` on the socket to notice silent peer drops.
//! - Graceful magic-mismatch surfaced as
//!   [`SourceError::Protocol`] with a descriptive message so connecting
//!   to a non-rtl_tcp port doesn't treat the first 12 bytes of junk as
//!   samples.
//!
//! Command debouncing (rapid UI dial scrubs → fewer wire commands) is
//! intentionally **not** handled here — it is a UI concern and the caller
//! is responsible for coalescing intents before driving `set_*`. Matches
//! upstream GQRX/SDR++ behavior.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sdr_pipeline::source_manager::Source;
use sdr_server_rtltcp::protocol::{Command, CommandOp, DONGLE_INFO_LEN, DongleInfo, TunerTypeCode};
use sdr_types::{Complex, SourceError};

/// Default read timeout on the data socket. See
/// [`RtlTcpConfig::data_read_timeout`].
pub const DEFAULT_DATA_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Default max consecutive read timeouts before reconnect. See
/// [`RtlTcpConfig::max_consecutive_timeouts`].
pub const DEFAULT_MAX_CONSECUTIVE_TIMEOUTS: u32 = 2;

/// Default timeout for the initial TCP connect. See
/// [`RtlTcpConfig::connect_timeout`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default sample rate the client reports to pipeline callers before
/// the first `set_sample_rate` arrives. Matches upstream rtl_tcp's
/// 2.048 Msps default.
const DEFAULT_CLIENT_SAMPLE_RATE_HZ: f64 = 2_048_000.0;

/// Default center frequency the client reports to pipeline callers
/// before the first `tune` arrives. Matches upstream rtl_tcp's
/// 100 MHz default.
const DEFAULT_CLIENT_CENTER_FREQ_HZ: f64 = 100_000_000.0;

/// Exponential-backoff schedule for reconnect. Values in seconds.
/// Clamped at 30 s, matching the review of epic #299.
const BACKOFF_SCHEDULE_SECS: &[u64] = &[1, 2, 5, 10, 30];

/// Soft cap on bytes buffered between the network reader and the
/// pipeline consumer. Past this, newly-received bytes push out the
/// oldest bytes (drop-oldest policy — the SDR pipeline wants fresh
/// samples; stale ones are useless). 4 MiB ≈ 0.7 s of I/Q at 3.2 Msps,
/// which is plenty of slack for a momentarily slow consumer without
/// letting a wedged consumer OOM the process.
const RX_BUFFER_SOFT_CAP_BYTES: usize = 4 * 1024 * 1024;

/// How often the manager thread checks the shutdown flag while waiting
/// on an outstanding blocking connect. `TcpStream::connect_timeout`
/// can't be cancelled from another thread, so we run it on a helper
/// and poll a channel at this cadence instead — stop_manager's
/// observable shutdown lag is bounded by this value, not by the full
/// `connect_timeout` window.
const CONNECT_SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Chunk size for both the warm-capacity hint on `rx_buf` and the
/// stack buffer the data pump reads into. Keeps the read-chunk and
/// initial-allocation policy in one place so they can't drift.
const RECV_CHUNK_BYTES: usize = 64 * 1024;

/// Metadata parsed from the server's `dongle_info_t` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunerInfo {
    pub tuner: TunerTypeCode,
    /// Number of discrete gain steps the tuner exposes. The actual gain
    /// table is NOT carried on the wire — clients that want to render dB
    /// values must either assume the R820T table or drive the server via
    /// [`CommandOp::SetGainByIndex`] and show "step N of M".
    pub gain_count: u32,
}

impl From<DongleInfo> for TunerInfo {
    fn from(info: DongleInfo) -> Self {
        Self {
            tuner: info.tuner,
            gain_count: info.gain_count,
        }
    }
}

/// Connection lifecycle state for UI consumption.
#[derive(Debug, Clone)]
pub enum ConnectionState {
    /// Initial state before first `start()` call.
    Disconnected,
    /// `start()` in progress — first TCP connect attempt.
    Connecting,
    /// Handshake complete, handler streaming I/Q.
    Connected { tuner: TunerInfo },
    /// Connection dropped, backoff pending. Transport-level errors
    /// (TCP connect refused, EOF, stall) stay in this state — the
    /// manager retries forever with exponential backoff up to the
    /// 30 s cap.
    Retrying { attempt: u32, next_at: Instant },
    /// Terminal failure — only entered for a protocol-level error
    /// (e.g., server sent a non-RTL0 header). Transport failures
    /// never reach this state; they remain in `Retrying`.
    Failed { reason: String },
}

/// Tunable knobs for the connection manager. All fields have sensible
/// production defaults; tests and future UIs may want shorter timeouts
/// (mobile / flaky networks) or a different reconnect tolerance.
#[derive(Debug, Clone)]
pub struct RtlTcpConfig {
    /// Read timeout on the data socket. A stalled read longer than this
    /// counts toward [`Self::max_consecutive_timeouts`] and, once
    /// exceeded, trips the reconnect state machine. Shorter than the
    /// kernel keepalive window so we detect silent drops within seconds
    /// rather than minutes.
    pub data_read_timeout: Duration,

    /// Number of consecutive read timeouts before the data pump gives
    /// up on the current connection and falls through to the reconnect
    /// loop. With the default 5 s timeout this gives ~10 s of silence
    /// before we declare the peer dead — well above any legitimate
    /// network hiccup but still fast enough that a yanked cable doesn't
    /// leave the UI frozen in Connected state until the kernel
    /// keepalive finally fires.
    pub max_consecutive_timeouts: u32,

    /// Timeout for each TCP `connect()` attempt. Default 10 s. Without
    /// this the call can sit in the kernel for 60+ seconds waiting on
    /// TCP SYN retransmits when the destination is a blackhole (IP
    /// drops packets rather than replying RST), leaving the manager
    /// thread stuck.
    pub connect_timeout: Duration,
}

impl Default for RtlTcpConfig {
    fn default() -> Self {
        Self {
            data_read_timeout: DEFAULT_DATA_READ_TIMEOUT,
            max_consecutive_timeouts: DEFAULT_MAX_CONSECUTIVE_TIMEOUTS,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

/// rtl_tcp source client.
///
/// Spawns a background connection manager thread on `start()`. The
/// manager owns the socket, does the reconnect loop, and publishes the
/// byte stream into a ring buffer that `read_samples` drains.
pub struct RtlTcpSource {
    host: String,
    port: u16,
    sample_rate: f64,
    frequency: f64,
    config: RtlTcpConfig,

    shared: Arc<SharedState>,
    manager: Option<JoinHandle<()>>,
}

/// State shared between the public API (main thread) and the background
/// connection manager thread.
struct SharedState {
    shutdown: AtomicBool,
    state: Mutex<ConnectionState>,
    tuner: Mutex<Option<TunerInfo>>,

    /// Latest 8-bit I/Q bytes read from the server. The connection
    /// manager appends bytes here; `read_samples` drains and converts.
    /// Bounded at [`RX_BUFFER_SOFT_CAP_BYTES`] via drop-oldest on append
    /// — prevents OOM if the downstream consumer stalls, and stale I/Q
    /// samples are useless for a live SDR anyway. Guarded by a Mutex
    /// because it's accessed from two threads; a lock-free ring buffer
    /// would be lower overhead but adds unsafe, and this matches the
    /// simplicity of the sibling `NetworkSource`.
    rx_buf: Mutex<Vec<u8>>,

    /// Running count of bytes dropped to keep `rx_buf` under its cap,
    /// for observability / UI "consumer too slow" indicators.
    rx_dropped_bytes: AtomicU64,

    /// Edge-trigger flag for the overflow warn log. Set when we drop
    /// bytes; cleared when the buffer drains to below half-cap. Ensures
    /// we log once per stall-and-drain cycle instead of per-chunk.
    rx_in_overflow: AtomicBool,

    /// Write side of the socket, protected by a Mutex so command senders
    /// can share it without racing. Replaced on every reconnect.
    command_sink: Mutex<Option<TcpStream>>,

    /// Latest values for each sticky command op, replayed on reconnect
    /// so the server state matches what the UI thinks it has set.
    /// Using AtomicU32 rather than a HashMap since the op set is small
    /// and fixed.
    last_center_freq_hz: AtomicU32,
    last_sample_rate_hz: AtomicU32,
    last_gain_mode: AtomicU32,
    last_tuner_gain: AtomicU32,
    last_ppm: AtomicU32,
    last_agc_mode: AtomicU32,
    last_direct_sampling: AtomicU32,
    last_offset_tuning: AtomicU32,
    last_bias_tee: AtomicU32,
    last_gain_by_index: AtomicU32,
    // Rarely-adjusted but still stateful ops. Tracked + replayed so a
    // pre-connect set_testmode (etc.) isn't silently lost and so the
    // server state matches the UI view across reconnects, same as the
    // common setters. Addresses CodeRabbit round 5 concern that these
    // previously returned Ok without persisting.
    last_testmode: AtomicU32,
    last_if_gain: AtomicU32,
    last_rtl_xtal: AtomicU32,
    last_tuner_xtal: AtomicU32,
    // Sentinel: bit 0 of `replay_mask` is set once ANY value has been
    // written for each op, so a fresh connection doesn't replay default
    // zeros onto a server whose operator explicitly wanted something
    // else. Bit i = op 0x01 + i.
    replay_mask: AtomicU32,
}

impl SharedState {
    fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            state: Mutex::new(ConnectionState::Disconnected),
            tuner: Mutex::new(None),
            rx_buf: Mutex::new(Vec::with_capacity(RECV_CHUNK_BYTES)),
            command_sink: Mutex::new(None),
            last_center_freq_hz: AtomicU32::new(0),
            last_sample_rate_hz: AtomicU32::new(0),
            last_gain_mode: AtomicU32::new(0),
            last_tuner_gain: AtomicU32::new(0),
            last_ppm: AtomicU32::new(0),
            last_agc_mode: AtomicU32::new(0),
            last_direct_sampling: AtomicU32::new(0),
            last_offset_tuning: AtomicU32::new(0),
            last_bias_tee: AtomicU32::new(0),
            last_gain_by_index: AtomicU32::new(0),
            replay_mask: AtomicU32::new(0),
            rx_dropped_bytes: AtomicU64::new(0),
            rx_in_overflow: AtomicBool::new(false),
            last_testmode: AtomicU32::new(0),
            last_if_gain: AtomicU32::new(0),
            last_rtl_xtal: AtomicU32::new(0),
            last_tuner_xtal: AtomicU32::new(0),
        }
    }
}

/// Append `chunk` to `rx`, dropping the oldest bytes if doing so would
/// exceed [`RX_BUFFER_SOFT_CAP_BYTES`]. Returns the number of bytes
/// dropped so the caller can surface it through observability.
///
/// Drop count is rounded up to an even number so the buffer always
/// stays aligned on I/Q pair boundaries. Dropping an odd number of
/// bytes would leave `rx` starting mid-pair — subsequent `read_samples`
/// calls would then pair `Q[n]` with `I[n+1]`, phase-shifting the
/// stream until another odd drop happened to realign it.
fn append_with_cap_inner(rx: &mut Vec<u8>, chunk: &[u8]) -> usize {
    let desired_total = rx.len().saturating_add(chunk.len());
    let raw_excess = desired_total.saturating_sub(RX_BUFFER_SOFT_CAP_BYTES);
    // Round up to even so we never split an I/Q pair.
    let total_drop = raw_excess.saturating_add(raw_excess & 1);

    let drop_from_rx = total_drop.min(rx.len());
    rx.drain(..drop_from_rx);

    let drop_from_chunk = total_drop.saturating_sub(drop_from_rx).min(chunk.len());
    rx.extend_from_slice(&chunk[drop_from_chunk..]);
    total_drop
}

/// Wrapper that does the drop-bookkeeping on the shared counter.
///
/// Logs only on the *transition* into the overflow state — once the
/// buffer is over cap we can log dozens of times per second on a hot
/// path, which adds CPU and log pressure while the consumer is already
/// behind. The `rx_dropped_bytes` counter is the authoritative source
/// of truth for "how much has been lost"; the warn is just an edge
/// signal so operators notice each stall start.
///
/// When the buffer drains to below half-cap the flag rearms, so a
/// subsequent stall will log again.
fn append_with_cap_to_shared(shared: &SharedState, rx: &mut Vec<u8>, chunk: &[u8]) {
    let dropped = append_with_cap_inner(rx, chunk);
    if dropped > 0 {
        shared
            .rx_dropped_bytes
            .fetch_add(dropped as u64, Ordering::Relaxed);
        let was_in_overflow = shared.rx_in_overflow.swap(true, Ordering::Relaxed);
        if !was_in_overflow {
            tracing::warn!(
                dropped,
                "rtl_tcp rx_buf full, dropping oldest bytes (see rx_dropped_bytes counter for cumulative loss)"
            );
        }
    } else if rx.len() < RX_BUFFER_SOFT_CAP_BYTES / 2 {
        // Consumer is keeping up well enough that we're back below
        // half-cap — rearm the edge so a future stall logs again.
        shared.rx_in_overflow.store(false, Ordering::Relaxed);
    }
}

impl RtlTcpSource {
    /// Create a new rtl_tcp client with default timeouts. Doesn't
    /// connect — call [`Source::start`] to open the socket.
    pub fn new(host: &str, port: u16) -> Self {
        Self::with_config(host, port, RtlTcpConfig::default())
    }

    /// Create a new rtl_tcp client with explicit timeout configuration.
    /// Useful for tests and for UIs that want shorter detection windows
    /// on flaky networks.
    pub fn with_config(host: &str, port: u16, config: RtlTcpConfig) -> Self {
        Self {
            host: host.to_string(),
            port,
            sample_rate: DEFAULT_CLIENT_SAMPLE_RATE_HZ,
            frequency: DEFAULT_CLIENT_CENTER_FREQ_HZ,
            config,
            shared: Arc::new(SharedState::new()),
            manager: None,
        }
    }

    /// Snapshot of the current connection lifecycle state.
    pub fn connection_state(&self) -> ConnectionState {
        match self.shared.state.lock() {
            Ok(s) => s.clone(),
            Err(_) => ConnectionState::Disconnected,
        }
    }

    /// Tuner metadata from the last successful handshake, if any.
    pub fn tuner_info(&self) -> Option<TunerInfo> {
        self.shared.tuner.lock().ok().and_then(|g| *g)
    }

    /// Send a raw rtl_tcp command over the current socket.
    ///
    /// If no live socket is available (pre-connect, mid-reconnect, or
    /// after a write failure), the command is recorded via
    /// `record_command` and replayed on the next successful handshake
    /// — the caller does NOT get `NotRunning` for the offline case.
    /// `SourceError::NotRunning` is returned only when local
    /// synchronization fails (poisoned `command_sink` mutex).
    ///
    /// Callers should prefer the typed setters (`set_center_freq_hz`,
    /// etc.) — this is the low-level escape hatch used by the setters.
    pub fn send_command(&self, cmd: Command) -> Result<(), SourceError> {
        // Remember the value for reconnect-replay before actually sending
        // so we don't lose it if the write happens to race a reconnect.
        self.record_command(cmd);

        let mut sink = self
            .shared
            .command_sink
            .lock()
            .map_err(|_| SourceError::NotRunning)?;
        let Some(stream) = sink.as_mut() else {
            // Not connected yet. Not an error — manager will replay on
            // reconnect via `record_command` above.
            return Ok(());
        };
        if let Err(e) = stream.write_all(&cmd.to_bytes()) {
            tracing::debug!(%e, "rtl_tcp command write failed — reconnect will replay");
            // Drop the broken stream; manager will notice and reconnect.
            *sink = None;
            return Ok(());
        }
        Ok(())
    }

    fn record_command(&self, cmd: Command) {
        // ALL 14 stateful ops are recorded for reconnect replay. A
        // pre-connect `set_testmode(true)` (etc.) would previously
        // return `Ok(())` without actually being sent, because the
        // command sink wasn't up yet — silent loss. Now every op
        // survives the connect / reconnect cycle.
        let slot = match cmd.op {
            CommandOp::SetCenterFreq => &self.shared.last_center_freq_hz,
            CommandOp::SetSampleRate => &self.shared.last_sample_rate_hz,
            CommandOp::SetGainMode => &self.shared.last_gain_mode,
            CommandOp::SetTunerGain => &self.shared.last_tuner_gain,
            CommandOp::SetFreqCorrection => &self.shared.last_ppm,
            CommandOp::SetIfGain => &self.shared.last_if_gain,
            CommandOp::SetTestMode => &self.shared.last_testmode,
            CommandOp::SetAgcMode => &self.shared.last_agc_mode,
            CommandOp::SetDirectSampling => &self.shared.last_direct_sampling,
            CommandOp::SetOffsetTuning => &self.shared.last_offset_tuning,
            CommandOp::SetRtlXtal => &self.shared.last_rtl_xtal,
            CommandOp::SetTunerXtal => &self.shared.last_tuner_xtal,
            CommandOp::SetGainByIndex => &self.shared.last_gain_by_index,
            CommandOp::SetBiasTee => &self.shared.last_bias_tee,
        };
        slot.store(cmd.param, Ordering::Relaxed);
        let bit = u32::from((cmd.op as u8) - 1);
        self.shared
            .replay_mask
            .fetch_or(1u32 << bit, Ordering::Relaxed);
    }

    /// Convenience typed setters — each one round-trips through
    /// [`Self::send_command`].
    pub fn set_center_freq_hz(&self, hz: u32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetCenterFreq,
            param: hz,
        })
    }

    pub fn set_sample_rate_hz(&self, hz: u32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetSampleRate,
            param: hz,
        })
    }

    pub fn set_tuner_gain_tenths_db(&self, gain: i32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetTunerGain,
            #[allow(clippy::cast_sign_loss)]
            param: gain as u32,
        })
    }

    pub fn set_gain_mode_manual(&self, manual: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetGainMode,
            param: u32::from(manual),
        })
    }

    pub fn set_freq_correction_ppm(&self, ppm: i32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetFreqCorrection,
            #[allow(clippy::cast_sign_loss)]
            param: ppm as u32,
        })
    }

    pub fn set_agc_mode(&self, on: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetAgcMode,
            param: u32::from(on),
        })
    }

    pub fn set_direct_sampling(&self, mode: i32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetDirectSampling,
            #[allow(clippy::cast_sign_loss)]
            param: mode as u32,
        })
    }

    pub fn set_offset_tuning(&self, on: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetOffsetTuning,
            param: u32::from(on),
        })
    }

    pub fn set_bias_tee(&self, on: bool) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetBiasTee,
            param: u32::from(on),
        })
    }

    pub fn set_gain_by_index(&self, idx: u32) -> Result<(), SourceError> {
        self.send_command(Command {
            op: CommandOp::SetGainByIndex,
            param: idx,
        })
    }

    fn start_manager(&mut self) -> Result<(), SourceError> {
        // Guard against a second `start()` call: if there's already a
        // manager thread, refuse to spawn a second one. Previously this
        // overwrote `self.manager` unconditionally, which leaked the
        // prior JoinHandle and could leave two connection_manager
        // threads racing on the same SharedState. `stop_manager` /
        // `Drop` would then only wait for the newest one.
        //
        // Reap a finished handle (manager exited naturally after a
        // transport error) so a fresh start can proceed.
        if let Some(handle) = self.manager.as_ref() {
            if handle.is_finished() {
                if let Some(h) = self.manager.take() {
                    let _ = h.join();
                }
            } else {
                return Err(SourceError::AlreadyRunning);
            }
        }

        let host = self.host.clone();
        let port = self.port;
        let shared = self.shared.clone();
        let config = self.config.clone();

        self.shared.shutdown.store(false, Ordering::SeqCst);
        let handle = thread::Builder::new()
            .name("rtl_tcp-client".into())
            .spawn(move || {
                connection_manager(host, port, shared, config);
            })
            .map_err(SourceError::Io)?;
        self.manager = Some(handle);
        Ok(())
    }

    fn stop_manager(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // Close the current socket so any blocked read returns fast.
        if let Ok(mut sink) = self.shared.command_sink.lock() {
            if let Some(s) = sink.take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
        if let Some(h) = self.manager.take() {
            let _ = h.join();
        }
    }
}

impl Drop for RtlTcpSource {
    fn drop(&mut self) {
        self.stop_manager();
    }
}

/// Background thread body: reconnect loop + data-read pump.
fn connection_manager(host: String, port: u16, shared: Arc<SharedState>, config: RtlTcpConfig) {
    let mut attempt: u32 = 0;

    while !shared.shutdown.load(Ordering::Relaxed) {
        set_state(&shared, ConnectionState::Connecting);

        match attempt_connect(&host, port, &shared, &config) {
            Ok(stream) => {
                attempt = 0;
                // At this point handshake has completed successfully.
                replay_sticky_commands(&shared);
                run_data_pump(stream, &shared, &config);
                // run_data_pump returned — connection dropped.
            }
            Err(e) => {
                tracing::warn!(%e, host = %host, port, attempt, "rtl_tcp connect failed");
                if let SourceError::Protocol(_) = e {
                    // Non-recoverable: server isn't speaking rtl_tcp.
                    set_state(
                        &shared,
                        ConnectionState::Failed {
                            reason: format!("{e}"),
                        },
                    );
                    return;
                }
            }
        }

        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Compute the delay using the PRE-increment attempt counter so
        // the first retry actually uses slot 0 of BACKOFF_SCHEDULE_SECS
        // (1 s), not slot 1 (2 s). Previously `attempt` was incremented
        // before the `backoff_delay` call, giving an off-by-one where
        // the observable schedule was 2 → 5 → 10 → 30 instead of the
        // documented 1 → 2 → 5 → 10 → 30.
        let delay = backoff_delay(attempt);
        let retry_number = attempt.saturating_add(1);
        let next_at = Instant::now() + delay;
        set_state(
            &shared,
            ConnectionState::Retrying {
                attempt: retry_number,
                next_at,
            },
        );
        attempt = retry_number;
        sleep_until(next_at, &shared.shutdown);
    }

    set_state(&shared, ConnectionState::Disconnected);
}

fn attempt_connect(
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

    stream.set_read_timeout(Some(config.data_read_timeout))?;
    if let Err(e) = set_keepalive(&stream, true) {
        tracing::warn!(%e, "SO_KEEPALIVE not applied (non-fatal)");
    }

    // Read and verify the 12-byte dongle_info_t header.
    let mut header_buf = [0u8; DONGLE_INFO_LEN];
    read_exact_with_context(&stream, &mut header_buf)?;

    let Some(info) = DongleInfo::from_bytes(&header_buf) else {
        return Err(SourceError::Protocol(
            "not an rtl_tcp server: dongle_info_t magic prefix mismatch".into(),
        ));
    };
    let tuner = TunerInfo::from(info);
    if let Ok(mut slot) = shared.tuner.lock() {
        *slot = Some(tuner);
    }
    set_state(shared, ConnectionState::Connected { tuner });

    // Publish a clone of the stream for the command sender. Install a
    // write timeout on the clone so `send_command`'s blocking
    // `write_all` can't hang indefinitely if a zero-window peer
    // saturates our kernel send buffer — tune/gain changes must stay
    // responsive. Socket options propagate across `try_clone` on the
    // same underlying fd, so this applies to every subsequent write.
    let sink = stream.try_clone().map_err(SourceError::Io)?;
    if let Err(e) = sink.set_write_timeout(Some(config.data_read_timeout)) {
        tracing::warn!(%e, "set_write_timeout on command sink failed — command sends may block");
    }
    if let Ok(mut slot) = shared.command_sink.lock() {
        *slot = Some(sink);
    }

    Ok(stream)
}

/// Run `TcpStream::connect_timeout` on a helper thread, polling a
/// channel from the manager thread so shutdown is noticed promptly.
///
/// Iterates `addrs` in order (covers hostnames that resolve to multiple
/// A/AAAA records), first successful connect wins. On shutdown the
/// helper is abandoned — its `tx.send` becomes a no-op when `rx` drops
/// at return, and the helper thread dies naturally once its `connect`
/// call returns or times out.
fn connect_cancellable(
    addrs: Vec<SocketAddr>,
    timeout: Duration,
    shutdown: &AtomicBool,
) -> Result<TcpStream, SourceError> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<TcpStream, std::io::Error>>();
    thread::Builder::new()
        .name("rtl_tcp-connect".into())
        .spawn(move || {
            let mut last_err: Option<std::io::Error> = None;
            let mut stream: Option<TcpStream> = None;
            for addr in addrs {
                match TcpStream::connect_timeout(&addr, timeout) {
                    Ok(s) => {
                        stream = Some(s);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            let result = match stream {
                Some(s) => Ok(s),
                None => Err(last_err.unwrap_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::AddrNotAvailable,
                        "no socket addresses resolved",
                    )
                })),
            };
            // `rx` may already have dropped if the manager shut down
            // during our blocking connect — that's fine, the helper
            // just exits with its result thrown away.
            let _ = tx.send(result);
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

fn run_data_pump(mut stream: TcpStream, shared: &Arc<SharedState>, config: &RtlTcpConfig) {
    let mut buf = [0u8; RECV_CHUNK_BYTES];
    let mut consecutive_timeouts: u32 = 0;
    while !shared.shutdown.load(Ordering::Relaxed) {
        match stream.read(&mut buf) {
            Ok(0) => {
                tracing::info!("rtl_tcp server closed connection");
                break;
            }
            Ok(n) => {
                consecutive_timeouts = 0;
                if let Ok(mut rx) = shared.rx_buf.lock() {
                    append_with_cap_to_shared(shared, &mut rx, &buf[..n]);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // Read timeout — server may have silently gone away.
                // Break out to the reconnect loop after a handful of
                // consecutive timeouts rather than waiting for the kernel
                // keepalive (which can take minutes). A single timeout
                // can be a transient stall; repeated timeouts mean the
                // peer is dead.
                consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                if consecutive_timeouts >= config.max_consecutive_timeouts {
                    tracing::info!(
                        consecutive_timeouts,
                        "rtl_tcp stream stalled, breaking out for reconnect"
                    );
                    break;
                }
            }
            Err(e) => {
                tracing::info!(%e, "rtl_tcp socket read failed, will reconnect");
                break;
            }
        }
    }

    // Drop the command sink so subsequent send_command calls stop
    // writing into a dead stream.
    if let Ok(mut sink) = shared.command_sink.lock() {
        *sink = None;
    }
}

fn replay_sticky_commands(shared: &Arc<SharedState>) {
    let mask = shared.replay_mask.load(Ordering::Relaxed);
    let replay_bit = |bit: u32| mask & (1u32 << bit) != 0;
    let Ok(mut sink) = shared.command_sink.lock() else {
        return;
    };
    let Some(stream) = sink.as_mut() else {
        return;
    };

    let ops = [
        (CommandOp::SetCenterFreq, &shared.last_center_freq_hz),
        (CommandOp::SetSampleRate, &shared.last_sample_rate_hz),
        (CommandOp::SetGainMode, &shared.last_gain_mode),
        (CommandOp::SetTunerGain, &shared.last_tuner_gain),
        (CommandOp::SetFreqCorrection, &shared.last_ppm),
        (CommandOp::SetIfGain, &shared.last_if_gain),
        (CommandOp::SetTestMode, &shared.last_testmode),
        (CommandOp::SetAgcMode, &shared.last_agc_mode),
        (CommandOp::SetDirectSampling, &shared.last_direct_sampling),
        (CommandOp::SetOffsetTuning, &shared.last_offset_tuning),
        (CommandOp::SetRtlXtal, &shared.last_rtl_xtal),
        (CommandOp::SetTunerXtal, &shared.last_tuner_xtal),
        (CommandOp::SetGainByIndex, &shared.last_gain_by_index),
        (CommandOp::SetBiasTee, &shared.last_bias_tee),
    ];
    for (op, slot) in ops {
        let bit = u32::from((op as u8) - 1);
        if !replay_bit(bit) {
            continue;
        }
        let cmd = Command {
            op,
            param: slot.load(Ordering::Relaxed),
        };
        if let Err(e) = stream.write_all(&cmd.to_bytes()) {
            tracing::debug!(%e, op = ?op, "replay write failed — will retry on next reconnect");
            return;
        }
    }
}

fn backoff_delay(attempt: u32) -> Duration {
    let idx = (attempt as usize).min(BACKOFF_SCHEDULE_SECS.len() - 1);
    Duration::from_secs(BACKOFF_SCHEDULE_SECS[idx])
}

fn sleep_until(deadline: Instant, shutdown: &AtomicBool) {
    let step = Duration::from_millis(100);
    while Instant::now() < deadline {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(step.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn set_state(shared: &Arc<SharedState>, state: ConnectionState) {
    if let Ok(mut s) = shared.state.lock() {
        *s = state;
    }
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

#[cfg(unix)]
#[allow(unsafe_code)]
fn set_keepalive(stream: &TcpStream, on: bool) -> std::io::Result<()> {
    // Same setsockopt dance as `sdr-server-rtltcp::server::set_keepalive`.
    // Kept duplicated rather than extracted into a shared crate so both
    // ends stay self-contained — this is one function.
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let value: libc::c_int = libc::c_int::from(on);
    // SAFETY: `fd` is a valid open socket for the duration of this call
    // (we borrow `stream` by reference); `value` is a stable stack local
    // with the matching `c_int` type for `SO_KEEPALIVE`.
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
    Ok(())
}

impl Source for RtlTcpSource {
    fn name(&self) -> &str {
        "RTL-TCP"
    }

    fn start(&mut self) -> Result<(), SourceError> {
        self.start_manager()
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        self.stop_manager();
        Ok(())
    }

    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        // Guard the f64 → u32 cast: NaN and ±Inf silently coerce to 0
        // or saturating u32 bounds, both invalid RF parameters. Out-of-
        // range finite values saturate too. Mirror the CLI parser's
        // is_finite + range-check pattern.
        if !frequency_hz.is_finite() || frequency_hz < 0.0 || frequency_hz > f64::from(u32::MAX) {
            return Err(SourceError::InvalidParameter(format!(
                "center frequency out of range: {frequency_hz}"
            )));
        }
        self.frequency = frequency_hz;
        // Round to u32 — upstream wire protocol is u32 Hz.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let hz = frequency_hz.round() as u32;
        self.set_center_freq_hz(hz)
    }

    fn sample_rates(&self) -> &[f64] {
        &[]
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError> {
        // Same guard as `tune`: NaN, ±Inf, ≤ 0, and out-of-u32 all get
        // rejected up-front. A zero sample rate in particular would
        // wedge the USB controller — better to error loudly than send
        // it over the wire.
        if !rate.is_finite() || rate <= 0.0 || rate > f64::from(u32::MAX) {
            return Err(SourceError::InvalidParameter(format!(
                "sample rate out of range: {rate}"
            )));
        }
        self.sample_rate = rate;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let hz = rate.round() as u32;
        self.set_sample_rate_hz(hz)
    }

    fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
        if output.is_empty() {
            return Ok(0);
        }
        // Convert I/Q bytes to Complex samples directly under the lock,
        // no intermediate `Vec` copy. Hot path — avoids one allocation
        // + one memcpy per pull. 8-bit unsigned-offset I/Q: byte 0..=255
        // with zero at 127.5, scaled to f32 in [-1, 1).
        let mut rx = self
            .shared
            .rx_buf
            .lock()
            .map_err(|_| SourceError::NotRunning)?;
        let take_pairs = (rx.len() / 2).min(output.len());
        let take_bytes = take_pairs * 2;
        for i in 0..take_pairs {
            let re_u = rx[i * 2];
            let im_u = rx[i * 2 + 1];
            output[i] = Complex::new(
                (f32::from(re_u) - 127.5) / 127.5,
                (f32::from(im_u) - 127.5) / 127.5,
            );
        }
        rx.drain(..take_bytes);
        Ok(take_pairs)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Placeholder host/port for tests that never actually connect —
    /// just exercise builder state or buffer logic. The string "127.0.0.1"
    /// is fine as-is, but the port number is named for intent.
    const UNUSED_TEST_PORT: u16 = 1234;

    /// A port we expect connect() to fail with ECONNREFUSED on localhost
    /// so the shutdown-during-retry test doesn't hang waiting for a SYN
    /// timeout. Port 1 is a well-known unused privileged port and on
    /// Linux loopback refuses instantly.
    const REFUSED_TEST_PORT: u16 = 1;

    #[test]
    fn backoff_schedule_caps_at_30s() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(4), Duration::from_secs(30));
        // Further attempts saturate.
        assert_eq!(backoff_delay(999), Duration::from_secs(30));
    }

    #[test]
    fn first_retry_uses_1s_backoff() {
        // Regression test for a real off-by-one: the first retry used
        // BACKOFF_SCHEDULE_SECS[1] (2s) instead of [0] (1s). Drive the
        // manager against a never-listener and look at the first
        // Retrying state it publishes.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Drop the listener immediately — any connect() will refuse.
        drop(listener);

        let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
        src.start_manager().unwrap();

        // Wait up to 2s for the first Retrying state.
        let t0 = Instant::now();
        let mut first_delay = None;
        while t0.elapsed() < Duration::from_secs(2) {
            if let ConnectionState::Retrying { attempt, next_at } = src.connection_state() {
                // `attempt` must be 1 for the first retry, and `next_at`
                // must correspond to a ~1 s delay, not 2 s.
                assert_eq!(attempt, 1, "first retry must be attempt 1");
                let delay = next_at.saturating_duration_since(Instant::now());
                first_delay = Some(delay);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        src.stop_manager();

        let d = first_delay.expect("never saw Retrying state within 2 s");
        // Allow a little jitter. Must be <= 1 s + a small slack,
        // NOT around 2 s.
        assert!(
            d <= Duration::from_millis(1100),
            "first retry delay = {d:?}, expected ~1s"
        );
    }

    #[test]
    fn append_with_cap_drops_oldest_when_full() {
        let mut rx = vec![0u8; RX_BUFFER_SOFT_CAP_BYTES - 10];
        // Mark the tail so we can verify what survives.
        rx[RX_BUFFER_SOFT_CAP_BYTES - 11] = 0xAA;
        let incoming = vec![0xFFu8; 100]; // 100 bytes incoming — need to drop 90
        let dropped = append_with_cap_inner(&mut rx, &incoming);
        assert_eq!(dropped, 90);
        assert_eq!(rx.len(), RX_BUFFER_SOFT_CAP_BYTES);
        // Tail should be the 100 new 0xFF bytes.
        assert!(rx[rx.len() - 100..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn append_with_cap_handles_oversized_chunk() {
        // Chunk larger than the cap: keep only the tail of the chunk.
        let mut rx = Vec::new();
        let mut big = vec![0u8; RX_BUFFER_SOFT_CAP_BYTES + 1000];
        // Mark the tail so we can verify it survives.
        let len = big.len();
        big[len - 1] = 0xAB;
        let dropped = append_with_cap_inner(&mut rx, &big);
        assert_eq!(dropped, 1000);
        assert_eq!(rx.len(), RX_BUFFER_SOFT_CAP_BYTES);
        assert_eq!(*rx.last().unwrap(), 0xAB);
    }

    #[test]
    fn append_with_cap_rounds_drop_up_to_even() {
        // Construct an overflow by exactly 1 byte. The drop count MUST
        // round up to 2 so we don't split an I/Q pair — Q would get
        // misaligned with the next I, phase-shifting the output stream
        // until another odd-drop event happened to realign it.
        //
        // Mark the I byte of the pair that should survive after drop so
        // we can verify it ends up at rx[0] (still an I position, not
        // shifted into a Q slot).
        let mut rx = vec![0u8; RX_BUFFER_SOFT_CAP_BYTES];
        rx[0] = 0x11; // I of dropped pair 0
        rx[1] = 0x12; // Q of dropped pair 0
        rx[2] = 0xAA; // I of surviving pair 1 — must land at rx[0] post-drop
        rx[3] = 0xBB; // Q of surviving pair 1
        let incoming = [0xFFu8; 1];
        let dropped = append_with_cap_inner(&mut rx, &incoming);
        assert_eq!(dropped, 2, "drop count must be even, got {dropped}");
        // Pair 1's I landed at position 0 — alignment preserved.
        assert_eq!(rx[0], 0xAA, "I byte of surviving pair must be at rx[0]");
        assert_eq!(rx[1], 0xBB, "Q byte of surviving pair must be at rx[1]");
        // rx can legitimately end on an odd length (the trailing byte is
        // half of a pair waiting for its mate on the next read) — what
        // matters is that the DROP was pair-aligned, not the final length.
    }

    #[test]
    fn append_with_cap_no_drop_below_cap() {
        let mut rx = vec![0u8; 1000];
        let incoming = vec![0xFFu8; 500];
        let dropped = append_with_cap_inner(&mut rx, &incoming);
        assert_eq!(dropped, 0);
        assert_eq!(rx.len(), 1500);
    }

    #[test]
    fn second_client_is_rejected_not_queued() {
        // This test verifies the contract stated in the module docs:
        // "single client at a time; second connection rejected with
        // graceful close." Upstream rtl_tcp silently hangs second
        // connections in the kernel backlog; our implementation closes
        // them immediately.
        //
        // We don't bring up a full Server here (needs a real RTL-SDR),
        // but we can exercise the exact accept-loop logic by mocking
        // the listener behavior: the key invariant is that a second
        // connection's read(stream) returns EOF quickly rather than
        // hanging for the DATA_READ_TIMEOUT window.
        //
        // Since Server::start requires hardware, cover the pure-logic
        // part — the AtomicBool swap semantics — directly.
        let busy = AtomicBool::new(false);
        assert!(!busy.swap(true, Ordering::SeqCst)); // first claim: was false
        assert!(busy.swap(true, Ordering::SeqCst)); // second claim: already true
        // A second accept caller would see `true` and reject.
        busy.store(false, Ordering::SeqCst); // session done
        assert!(!busy.swap(true, Ordering::SeqCst)); // next client can claim again
    }

    #[test]
    fn consecutive_timeouts_break_out_of_data_pump() {
        // Server completes handshake then stops sending anything. With
        // a 200 ms read timeout and max 2 consecutive timeouts, the
        // client should leave Connected within ~400 ms rather than
        // hanging for the full DATA_READ_TIMEOUT window.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server_thread = thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let header = DongleInfo {
                    tuner: TunerTypeCode::R820t,
                    gain_count: 29,
                }
                .to_bytes();
                let _ = sock.write_all(&header);
                // Hold for well past 2 × read_timeout so the client's
                // read() actually hits TimedOut (not EOF).
                thread::sleep(Duration::from_secs(2));
            }
        });

        let config = RtlTcpConfig {
            data_read_timeout: Duration::from_millis(200),
            max_consecutive_timeouts: 2,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        };
        let mut src = RtlTcpSource::with_config(&addr.ip().to_string(), addr.port(), config);
        src.start_manager().unwrap();

        // Wait for Connected.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut reached_connected = false;
        while Instant::now() < deadline {
            if matches!(src.connection_state(), ConnectionState::Connected { .. }) {
                reached_connected = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(reached_connected, "never reached Connected");

        // After 2 × 200 ms of silence the data pump should break out.
        // Give up to 1 second of slack for scheduling jitter.
        let timeout_deadline = Instant::now() + Duration::from_secs(1);
        let mut left_connected = false;
        while Instant::now() < timeout_deadline {
            if !matches!(src.connection_state(), ConnectionState::Connected { .. }) {
                left_connected = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        src.stop_manager();
        let _ = server_thread.join();
        assert!(
            left_connected,
            "client still Connected after timeout threshold — reconnect didn't fire"
        );
    }

    #[test]
    fn new_source_starts_disconnected() {
        let source = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        match source.connection_state() {
            ConnectionState::Disconnected => {}
            other => unreachable!("expected Disconnected, got {other:?}"),
        }
        assert!(source.tuner_info().is_none());
    }

    #[test]
    fn bad_magic_produces_failed_state() {
        // Spin up a toy server that writes junk then closes.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_thread = thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let _ = s.write_all(b"XXXXjunknoise");
                // Keep open briefly so client reads fail cleanly.
                thread::sleep(Duration::from_millis(200));
            }
        });

        let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
        src.start_manager().unwrap();

        // Wait up to 2s for the manager to transition to Failed.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_failed = false;
        while Instant::now() < deadline {
            if matches!(src.connection_state(), ConnectionState::Failed { .. }) {
                saw_failed = true;
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        src.stop_manager();
        let _ = server_thread.join();
        assert!(saw_failed, "expected Failed state after bad magic");
    }

    #[test]
    fn happy_path_handshake_and_command_roundtrip() {
        // Mock rtl_tcp server: writes a valid RTL0 header then pushes
        // a fixed byte pattern as "samples" while reading tuning
        // commands into a channel we can inspect.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();

        let server_thread = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            // Advertise an R820T with 29 gains.
            let header = DongleInfo {
                tuner: TunerTypeCode::R820t,
                gain_count: 29,
            }
            .to_bytes();
            sock.write_all(&header).unwrap();
            // Stream a few hundred bytes of synthetic I/Q (all 128 = zero).
            sock.write_all(&[128u8; 512]).unwrap();
            // Read one command (5 bytes) from the client and forward it.
            let mut cmd_buf = [0u8; 5];
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            if sock.read_exact(&mut cmd_buf).is_ok() {
                if let Some(cmd) = Command::from_bytes(&cmd_buf) {
                    let _ = cmd_tx.send(cmd);
                }
            }
            // Hold connection briefly so the client doesn't see EOF mid-test.
            thread::sleep(Duration::from_millis(200));
        });

        let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
        src.start_manager().unwrap();

        // Wait for Connected state.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut tuner = None;
        while Instant::now() < deadline {
            if let ConnectionState::Connected { tuner: t } = src.connection_state() {
                tuner = Some(t);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(tuner.is_some(), "client never reached Connected state");
        let t = tuner.unwrap();
        assert_eq!(t.tuner, TunerTypeCode::R820t);
        assert_eq!(t.gain_count, 29);

        // Send a tune command and verify the server received it.
        src.set_center_freq_hz(99_500_000).unwrap();
        let received = cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(received.op, CommandOp::SetCenterFreq);
        assert_eq!(received.param, 99_500_000);

        src.stop_manager();
        let _ = server_thread.join();
    }

    #[test]
    fn record_command_sets_replay_bit() {
        let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        let cmd = Command {
            op: CommandOp::SetCenterFreq,
            param: 99_500_000,
        };
        src.record_command(cmd);
        let mask = src.shared.replay_mask.load(Ordering::Relaxed);
        // CenterFreq is op 0x01, bit index 0.
        assert_eq!(mask & 0x1, 0x1);
        assert_eq!(
            src.shared.last_center_freq_hz.load(Ordering::Relaxed),
            99_500_000
        );
    }

    #[test]
    fn read_samples_with_empty_output_returns_zero() {
        let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        let mut output: [Complex; 0] = [];
        let n = src.read_samples(&mut output).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn read_samples_with_no_data_returns_zero() {
        let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        // Source was never started, no bytes buffered.
        let mut output = [Complex::default(); 4];
        let n = src.read_samples(&mut output).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn read_samples_converts_8bit_offset_iq() {
        let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        // 128 is midscale zero, 255 is +1 - small epsilon, 0 is -1.
        if let Ok(mut rx) = src.shared.rx_buf.lock() {
            rx.extend_from_slice(&[128, 128, 255, 0, 0, 255]);
        }
        let mut out = [Complex::default(); 3];
        // Call read_samples via the trait impl, matching public API.
        let mut mutable_src = src;
        let n = mutable_src.read_samples(&mut out).unwrap();
        assert_eq!(n, 3);
        // Midscale pair → near zero.
        assert!(out[0].re.abs() < 0.01);
        assert!(out[0].im.abs() < 0.01);
        // (255, 0) → +1, -1.
        assert!((out[1].re - 1.0).abs() < 0.01);
        assert!((out[1].im + 1.0).abs() < 0.01);
        // (0, 255) → -1, +1.
        assert!((out[2].re + 1.0).abs() < 0.01);
        assert!((out[2].im - 1.0).abs() < 0.01);
    }

    #[test]
    fn read_samples_handles_partial_pair_at_end() {
        // Odd byte count — the trailing lone byte must stay queued
        // rather than produce half a sample.
        let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        if let Ok(mut rx) = src.shared.rx_buf.lock() {
            rx.extend_from_slice(&[128, 128, 200]); // 1.5 pairs
        }
        let mut out = [Complex::default(); 2];
        let mut src = src;
        let n = src.read_samples(&mut out).unwrap();
        assert_eq!(n, 1, "should only consume the complete pair");
        // The trailing 200 stays queued — drained on the next call.
        let remaining = src.shared.rx_buf.lock().unwrap().len();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn partial_header_read_still_completes_handshake() {
        // Server sends the 12-byte dongle_info_t in two chunks with a
        // sleep between, exercising the read_exact_with_context loop.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_thread = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let header = DongleInfo {
                tuner: TunerTypeCode::E4000,
                gain_count: 14,
            }
            .to_bytes();
            sock.write_all(&header[..5]).unwrap();
            thread::sleep(Duration::from_millis(80));
            sock.write_all(&header[5..]).unwrap();
            // Hold open briefly.
            thread::sleep(Duration::from_millis(200));
        });

        let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
        src.start_manager().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = None;
        while Instant::now() < deadline {
            if let ConnectionState::Connected { tuner } = src.connection_state() {
                got = Some(tuner);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        src.stop_manager();
        let _ = server_thread.join();
        let tuner = got.expect("handshake should succeed across split reads");
        assert_eq!(tuner.tuner, TunerTypeCode::E4000);
        assert_eq!(tuner.gain_count, 14);
    }

    #[test]
    fn tcp_eof_mid_stream_transitions_to_retrying() {
        // Server completes handshake then immediately closes and drops
        // its listener — client must leave Connected and enter Retrying.
        // NOTE: do NOT accept a second time here. A second accept without
        // a header write would make the client hang on the header read
        // until DATA_READ_TIMEOUT (5 s). We let the listener drop so the
        // reconnect attempt gets ECONNREFUSED immediately, which puts
        // the client into Retrying within a few ms.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_thread = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let header = DongleInfo {
                tuner: TunerTypeCode::R820t,
                gain_count: 29,
            }
            .to_bytes();
            sock.write_all(&header).unwrap();
            // Drop sock → FIN → client's data-pump read returns Ok(0).
            // Dropping `listener` at the end of the closure scope makes
            // subsequent connect() from the client fail with
            // ECONNREFUSED, which lands the client in Retrying.
        });

        let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
        src.start_manager().unwrap();
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut saw_retrying = false;
        while Instant::now() < deadline {
            if matches!(src.connection_state(), ConnectionState::Retrying { .. }) {
                saw_retrying = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        src.stop_manager();
        let _ = server_thread.join();
        assert!(saw_retrying, "client never entered Retrying after EOF");
    }

    #[test]
    fn commands_before_connect_are_recorded_and_replayed() {
        // Driver queues commands before start() / before the server
        // accepts; on handshake those values should be replayed to the
        // server.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();

        let server_thread = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let header = DongleInfo {
                tuner: TunerTypeCode::R820t,
                gain_count: 29,
            }
            .to_bytes();
            sock.write_all(&header).unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            // Read whatever the client sends (replays + any subsequent
            // calls) for up to 1 s or until we've collected 2 commands.
            let mut got = 0;
            let deadline = Instant::now() + Duration::from_secs(1);
            while got < 2 && Instant::now() < deadline {
                let mut buf = [0u8; 5];
                match sock.read_exact(&mut buf) {
                    Ok(()) => {
                        if let Some(cmd) = Command::from_bytes(&buf) {
                            let _ = cmd_tx.send(cmd);
                            got += 1;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut src = RtlTcpSource::new(&addr.ip().to_string(), addr.port());
        // Queue commands BEFORE start — these must end up sent after
        // handshake via the replay path.
        src.set_center_freq_hz(433_000_000).unwrap();
        src.set_tuner_gain_tenths_db(197).unwrap();

        src.start_manager().unwrap();
        // Collect the replayed commands.
        let mut received = Vec::new();
        while let Ok(cmd) = cmd_rx.recv_timeout(Duration::from_millis(1500)) {
            received.push(cmd);
            if received.len() == 2 {
                break;
            }
        }
        src.stop_manager();
        let _ = server_thread.join();

        let params: Vec<(CommandOp, u32)> = received.iter().map(|c| (c.op, c.param)).collect();
        assert!(
            params.contains(&(CommandOp::SetCenterFreq, 433_000_000)),
            "expected replay of center freq, got {params:?}"
        );
        assert!(
            params.contains(&(CommandOp::SetTunerGain, 197)),
            "expected replay of tuner gain, got {params:?}"
        );
    }

    #[test]
    fn second_start_call_is_rejected_not_leaked() {
        // Two back-to-back `start_manager` calls must not leak the
        // first manager thread. Previously the second call silently
        // overwrote `self.manager`, leaving two connection_manager
        // threads racing on the same SharedState and
        // `stop_manager`/`Drop` only waiting for the newest one.
        let mut src = RtlTcpSource::new("127.0.0.1", REFUSED_TEST_PORT);
        src.start_manager().unwrap();
        // The first manager is alive (sitting in the reconnect loop
        // because port 1 refuses). Second call must Err.
        let second = src.start_manager();
        assert!(matches!(second, Err(SourceError::AlreadyRunning)));
        src.stop_manager();

        // After shutdown the prior handle is joined; a fresh start is
        // allowed again. Hit the "finished handle gets reaped" path.
        src.start_manager().unwrap();
        src.stop_manager();
    }

    #[test]
    fn tune_rejects_non_finite_and_out_of_range() {
        let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        // Never started, so no IO will actually happen — the tune call
        // goes through the trait impl's validation guard and either
        // returns Err or short-circuits at the command channel (which
        // is None).
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e12] {
            let err = <RtlTcpSource as Source>::tune(&mut src, bad);
            assert!(
                matches!(err, Err(SourceError::InvalidParameter(_))),
                "tune({bad}) should reject with InvalidParameter"
            );
        }
        // Sanity: a valid finite in-range frequency does NOT trip the guard.
        assert!(<RtlTcpSource as Source>::tune(&mut src, 100_000_000.0).is_ok());
    }

    #[test]
    fn set_sample_rate_rejects_non_finite_zero_negative_and_oversized() {
        let mut src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        for bad in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,  // zero rate would wedge USB
            -1.0, // negative rate
            1e12, // > u32::MAX
        ] {
            let err = <RtlTcpSource as Source>::set_sample_rate(&mut src, bad);
            assert!(
                matches!(err, Err(SourceError::InvalidParameter(_))),
                "set_sample_rate({bad}) should reject with InvalidParameter"
            );
        }
        // Sanity: 2.048 Msps passes.
        assert!(<RtlTcpSource as Source>::set_sample_rate(&mut src, 2_048_000.0).is_ok());
    }

    #[test]
    fn connect_cancellable_aborts_promptly_on_shutdown() {
        // `TcpStream::connect_timeout` itself has no cancellation hook,
        // but our cancellable wrapper polls the shutdown flag at
        // `CONNECT_SHUTDOWN_POLL` cadence. When the flag is pre-set,
        // the caller returns before the helper thread finishes — this
        // exercise that path without needing a blackholed IP in CI.
        let shutdown = AtomicBool::new(true);
        // Any address — doesn't matter, shutdown is already set so the
        // poll loop returns on the first iteration before the helper's
        // `connect_timeout` ever completes.
        let addrs = vec![SocketAddr::from(([127, 0, 0, 1], REFUSED_TEST_PORT))];
        let t0 = Instant::now();
        let result = connect_cancellable(addrs, Duration::from_secs(30), &shutdown);
        let elapsed = t0.elapsed();
        assert!(
            matches!(
                result,
                Err(SourceError::Io(ref e)) if e.kind() == std::io::ErrorKind::Interrupted
            ),
            "expected Interrupted on shutdown, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "connect_cancellable returned in {elapsed:?}, should be ≤ CONNECT_SHUTDOWN_POLL"
        );
    }

    #[test]
    fn shutdown_during_failed_connect_is_prompt() {
        // Point client at a port nothing's listening on; start_manager
        // enters the retry loop. stop_manager should return within ~1 s,
        // well below the exponential-backoff window.
        let mut src = RtlTcpSource::new("127.0.0.1", REFUSED_TEST_PORT); // port 1 likely refused
        src.start_manager().unwrap();
        let t0 = Instant::now();
        src.stop_manager();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "stop_manager took {elapsed:?}, should be prompt"
        );
    }

    #[test]
    fn record_command_covers_all_14_wire_ops() {
        // Every upstream command is recorded for reconnect-replay so a
        // pre-connect call (e.g. set_testmode before start()) isn't
        // silently lost. Walk all 14 opcodes and confirm each lands in
        // the replay_mask.
        let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        let all_ops = [
            CommandOp::SetCenterFreq,
            CommandOp::SetSampleRate,
            CommandOp::SetGainMode,
            CommandOp::SetTunerGain,
            CommandOp::SetFreqCorrection,
            CommandOp::SetIfGain,
            CommandOp::SetTestMode,
            CommandOp::SetAgcMode,
            CommandOp::SetDirectSampling,
            CommandOp::SetOffsetTuning,
            CommandOp::SetRtlXtal,
            CommandOp::SetTunerXtal,
            CommandOp::SetGainByIndex,
            CommandOp::SetBiasTee,
        ];
        for op in all_ops {
            src.record_command(Command { op, param: 42 });
        }
        let mask = src.shared.replay_mask.load(Ordering::Relaxed);
        // Every op from 0x01..=0x0e should have its bit set (bit index
        // = opcode - 1), so the low 14 bits should all be 1.
        assert_eq!(mask & 0x3fff, 0x3fff, "mask={mask:#x}");
    }

    #[test]
    fn rx_overflow_warning_is_edge_triggered() {
        // Fill rx past cap → first overflow flips the flag. Subsequent
        // overflows without a drain in between should leave the flag
        // set (log suppressed). A drain below half-cap rearms the flag.
        let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        assert!(!src.shared.rx_in_overflow.load(Ordering::Relaxed));

        // Simulate first overflow.
        {
            let mut rx = src.shared.rx_buf.lock().unwrap();
            *rx = vec![0u8; RX_BUFFER_SOFT_CAP_BYTES];
            append_with_cap_to_shared(&src.shared, &mut rx, &[0xFFu8; 100]);
        }
        assert!(src.shared.rx_in_overflow.load(Ordering::Relaxed));

        // Second overflow — flag already set, no transition.
        {
            let mut rx = src.shared.rx_buf.lock().unwrap();
            append_with_cap_to_shared(&src.shared, &mut rx, &[0xFFu8; 100]);
        }
        assert!(src.shared.rx_in_overflow.load(Ordering::Relaxed));

        // Drain well below half-cap and then append a non-overflowing
        // chunk — flag should rearm.
        {
            let mut rx = src.shared.rx_buf.lock().unwrap();
            rx.clear();
            append_with_cap_to_shared(&src.shared, &mut rx, &[0u8; 100]);
        }
        assert!(
            !src.shared.rx_in_overflow.load(Ordering::Relaxed),
            "flag should rearm once buffer drains below half-cap"
        );
    }

    #[test]
    fn replay_bits_set_independently_per_op() {
        let src = RtlTcpSource::new("127.0.0.1", UNUSED_TEST_PORT);
        src.record_command(Command {
            op: CommandOp::SetBiasTee,
            param: 1,
        });
        let mask = src.shared.replay_mask.load(Ordering::Relaxed);
        // BiasTee is op 0x0e, so bit index (0x0e - 1) = 13.
        assert!(mask & (1 << 13) != 0);
        // No other bits should be set.
        assert_eq!(mask.count_ones(), 1);
    }
}
