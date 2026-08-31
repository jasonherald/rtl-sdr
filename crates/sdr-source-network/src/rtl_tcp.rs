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

use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sdr_server_rtltcp::codec::Codec;
#[cfg(test)]
use sdr_server_rtltcp::extension::Role;
use sdr_server_rtltcp::protocol::{DongleInfo, TunerTypeCode};
use sdr_types::SourceError;

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

/// Upper-16-bit field of a `SetIfGain` param is the 1-based IF stage;
/// the lower 16 bits carry the gain (tenths of dB). Mirrors the server's
/// `dispatch.rs` and upstream rtl_tcp.c.
const IF_GAIN_STAGE_SHIFT_BITS: u32 = 16;

/// Sticky replay keeps one `SetIfGain` value per stage.
const IF_GAIN_STAGES: usize = sdr_pipeline::source_manager::RTL_TCP_IF_GAIN_STAGES;

/// Granularity at which the backoff sleep re-checks the shutdown flag.
const RETRY_SLEEP_STEP: Duration = Duration::from_millis(100);

mod commands;
mod handshake;
mod manager;
mod pump;

use manager::{connection_manager, set_state};

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

/// Connection lifecycle state — internal representation with an
/// `Instant`-based retry deadline suited to the scheduling loop.
///
/// UI consumers receive a projected form without `Instant`s (which
/// don't cross crate boundaries cleanly) via `connection_state()`
/// — see the `From<&ConnectionState> for sdr_types::RtlTcpConnectionState`
/// impl below.
#[derive(Debug, Clone)]
pub enum ConnectionState {
    /// Initial state before first `start()` call.
    Disconnected,
    /// `start()` in progress — first TCP connect attempt.
    Connecting,
    /// Handshake complete, handler streaming I/Q. `codec` reflects
    /// the negotiated stream codec from the extended `"RTLX"`
    /// handshake (#307); legacy / uncompressed paths land on
    /// `Codec::None`. `granted_role` carries the server's
    /// `ServerExtension.granted_role` decision (#392): `Some(Role)`
    /// when the server is RTLX-capable and admitted us into a
    /// specific slot, `None` when we didn't send a hello (legacy
    /// path) or the server is a pre-#392 RTLX server that doesn't
    /// yet advertise the field. The UI shows the role badge only
    /// when this is `Some` — a legacy server's role is unknown
    /// territory, and guessing "Controller" there would mis-label
    /// the connection on pre-#392 RTLX servers that still hand
    /// every accepted client a Control-equivalent slot without
    /// saying so on the wire. Per #396 / `CodeRabbit` round 1 on
    /// PR #408.
    Connected {
        tuner: TunerInfo,
        codec: Codec,
        granted_role: Option<sdr_server_rtltcp::extension::Role>,
    },
    /// Connection dropped, backoff pending. Transport-level errors
    /// (TCP connect refused, EOF, stall) stay in this state — the
    /// manager retries forever with exponential backoff up to the
    /// 30 s cap.
    Retrying {
        attempt: u32,
        next_at: Instant,
    },
    /// Terminal failure — only entered for a protocol-level error
    /// (e.g., server sent a non-RTL0 header). Transport failures
    /// never reach this state; they remain in `Retrying`.
    Failed {
        reason: String,
    },
    /// Terminal role-denial states surfaced by the #392/#394
    /// extended handshake. Per #396, the connection manager
    /// stops retrying and waits for the UI to offer the user an
    /// explicit recovery action (take-control, re-enter key, or
    /// give up).
    ControllerBusy,
    AuthRequired,
    AuthFailed,
}

impl From<&ConnectionState> for sdr_types::RtlTcpConnectionState {
    fn from(value: &ConnectionState) -> Self {
        match value {
            ConnectionState::Disconnected => Self::Disconnected,
            ConnectionState::Connecting => Self::Connecting,
            ConnectionState::Connected {
                tuner,
                codec,
                granted_role,
            } => Self::Connected {
                // `TunerTypeCode`'s `Debug` renders the upstream
                // tuner name ("R820T", "E4000", etc.) directly —
                // what the UI wants for the status row subtitle.
                tuner_name: format!("{:?}", tuner.tuner),
                gain_count: tuner.gain_count,
                codec: codec.label().to_string(),
                // Project the server-granted role to `Option<bool>`
                // at the crate boundary so `sdr_types` doesn't have
                // to depend on `sdr-server-rtltcp`'s wire `Role`
                // enum: `true` = Controller, `false` = Listener,
                // `None` = unknown (legacy handshake or pre-#392
                // RTLX server). The UI uses this to decide whether
                // to show the role badge at all — per CodeRabbit
                // round 1 on PR #408, the previous `Option<bool>`
                // derived from the user's requested role could
                // mis-label a session the server actually admitted
                // differently.
                granted_role: granted_role
                    .map(|r| r == sdr_server_rtltcp::extension::Role::Control),
            },
            ConnectionState::Retrying { attempt, next_at } => Self::Retrying {
                attempt: *attempt,
                // Saturating: if the scheduling thread has drifted
                // past the deadline (which just means the next
                // attempt is imminent), we render "0 s" rather
                // than underflow.
                retry_in: next_at
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO),
            },
            ConnectionState::Failed { reason } => Self::Failed {
                reason: reason.clone(),
            },
            ConnectionState::ControllerBusy => Self::ControllerBusy,
            ConnectionState::AuthRequired => Self::AuthRequired,
            ConnectionState::AuthFailed => Self::AuthFailed,
        }
    }
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
    ///
    /// **Applies to the raw pass-through (`Codec::None`) only.** A framed
    /// codec (LZ4) cannot resume after a partial read, so those sessions
    /// tear down for reconnect on the first timeout regardless of this
    /// value (#743).
    pub max_consecutive_timeouts: u32,

    /// Timeout for each TCP `connect()` attempt. Default 10 s. Without
    /// this the call can sit in the kernel for 60+ seconds waiting on
    /// TCP SYN retransmits when the destination is a blackhole (IP
    /// drops packets rather than replying RST), leaving the manager
    /// thread stuck.
    pub connect_timeout: Duration,

    /// Codecs this client is willing to negotiate in the extended
    /// `"RTLX"` handshake (#307). Defaults to [`CodecMask::NONE_ONLY`]
    /// — no hello sent, behaves identically to the pre-#307 client
    /// against any server (vanilla rtl_tcp / GQRX / SDR++ / our own).
    ///
    /// Opting in with [`CodecMask::NONE_AND_LZ4`] causes the client
    /// to prepend an 8-byte `ClientHello` to the connection. sdr-rs
    /// servers parse it and respond with a `ServerExtension` block;
    /// **vanilla rtl_tcp servers misinterpret those 8 bytes** as
    /// two 5-byte commands (with hello bytes straddling the
    /// command-framing boundary), which can cause garbage command
    /// dispatches. The UI / client-side discovery layer should only
    /// flip this bit when it has out-of-band evidence that the
    /// target server speaks the extension — the shipped signal is
    /// the `codecs=` value in the server's mDNS TXT record (see
    /// `sdr-rtltcp-discovery::TxtRecord::codecs`): `codecs=3`
    /// means the server advertises both `None` and `Lz4` so it's
    /// safe to send a hello; an absent or `codecs=1` value means
    /// legacy-only and we should leave this at `NONE_ONLY`.
    pub compression: sdr_server_rtltcp::codec::CodecMask,

    /// Request the `FLAG_REQUEST_TAKEOVER` bit in the extended
    /// handshake — asks the server to kick its existing Control
    /// client if the slot is occupied, and admit us instead.
    /// Defaults to `false`.
    ///
    /// **RTLX-only — NOT safe against vanilla servers.** Setting
    /// this to `true` triggers a `ClientHello` on the wire (the
    /// `extension_enabled` gate below sends a hello if EITHER
    /// `compression != NONE_ONLY` OR `request_takeover == true`).
    /// Vanilla `rtl_tcp` servers misinterpret the 8-byte hello as
    /// two 5-byte commands straddling the framing boundary, which
    /// can cause garbage dispatches — exactly the hazard the
    /// [`Self::compression`] doc already describes.
    ///
    /// Callers must gate this opt-in on the same out-of-band
    /// evidence that gates `compression`: the server's mDNS TXT
    /// record (`codecs=3` = RTLX-capable; absent / `codecs=1` =
    /// legacy-only, keep this `false`). The UI / client-side
    /// discovery layer is responsible for refusing to expose the
    /// takeover action against legacy-only servers, just as it
    /// already refuses to advertise compression for them.
    ///
    /// Normal UI flow when it IS safe: user tries to connect as
    /// Controller, server denies with `status=ControllerBusy`,
    /// client shows a "Take control?" prompt (only if the server
    /// advertised RTLX via mDNS), user confirms, client
    /// reconnects with `request_takeover = true`. sdr-rs servers
    /// with role support (#392+) honor the flag and displace the
    /// prior controller. Per #393 + `CodeRabbit` round 1 on
    /// PR #404 (doc clarification).
    pub request_takeover: bool,

    /// Pre-shared auth key to send in the extended handshake
    /// (#394). `None` means "no key" (default); `Some(bytes)`
    /// activates the eager-auth path: hello's `FLAG_HAS_AUTH`
    /// bit is set AND the client immediately follows with an
    /// `AuthKeyMessage` carrying these bytes. Server-side
    /// behavior:
    /// - If the server doesn't require auth, the key is
    ///   discarded server-side (stream-sync only) and the
    ///   handshake proceeds normally.
    /// - If the server requires auth and the key matches
    ///   (constant-time compare), the handshake proceeds.
    /// - If the server requires auth and the key doesn't match,
    ///   the server responds with `status=AuthFailed` and
    ///   closes; the client's manager transitions to `Failed`
    ///   with a Protocol-kind error so the UI can surface
    ///   "wrong key" guidance.
    ///
    /// **RTLX-only — NOT safe against vanilla servers.** Same
    /// hazard as [`Self::request_takeover`] — setting this
    /// triggers a `ClientHello` emission via the
    /// `extension_enabled` gate below, and vanilla rtl_tcp
    /// servers misinterpret the 8-byte hello as two 5-byte
    /// commands. Gate this on the same out-of-band evidence
    /// that gates `compression` / `request_takeover` (mDNS
    /// TXT `codecs=3`, or cached server-profile knowledge).
    ///
    /// Keys are cleartext over TCP — the threat model is
    /// casual LAN isolation, not WAN-grade security. See #394
    /// for the full threat model discussion. #394.
    pub auth_key: Option<Vec<u8>>,

    /// Role the client requests in its `ClientHello`. Default
    /// [`Role::Control`] matches the pre-#392 single-client
    /// behavior every legacy `rtl_tcp` client assumes. The UI
    /// lets users opt into [`Role::Listen`] for concurrent
    /// read-only access to a server that already has a
    /// controller. Per issue #396.
    ///
    /// **RTLX-only when non-default.** `Role::Listen` implies
    /// the server is #392-aware (has the role gate); vanilla
    /// `rtl_tcp` servers ignore the role byte entirely and
    /// every client is implicitly Control. The
    /// `extension_enabled` gate below already trips when the
    /// hello needs to carry any non-default flag, and the same
    /// mDNS TXT `codecs=3` out-of-band evidence that gates
    /// compression / takeover / auth also gates this field.
    /// `Role::Control` is the back-compat default for vanilla
    /// servers; setting `Role::Listen` against a vanilla
    /// server corrupts the 5-byte command framing (same hazard
    /// as `compression` / `request_takeover` / `auth_key`).
    pub requested_role: sdr_server_rtltcp::extension::Role,
}

impl Default for RtlTcpConfig {
    fn default() -> Self {
        Self {
            data_read_timeout: DEFAULT_DATA_READ_TIMEOUT,
            max_consecutive_timeouts: DEFAULT_MAX_CONSECUTIVE_TIMEOUTS,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            compression: sdr_server_rtltcp::codec::CodecMask::NONE_ONLY,
            request_takeover: false,
            auth_key: None,
            // `Role::Control` matches the pre-#392 single-client
            // behavior every legacy `rtl_tcp` client assumes — no
            // wire change from the default path.
            requested_role: sdr_server_rtltcp::extension::Role::Control,
        }
    }
}

/// rtl_tcp source client.
///
/// Spawns a background connection manager thread on `start()`. The
/// manager owns the socket, does the reconnect loop, and publishes the
/// byte stream into a ring buffer that `read_samples` drains.
///
/// The "current frequency / sample rate" values exposed through
/// [`Source::sample_rate`] / `sample_rate()` live on `SharedState` as
/// atomic f64 bit-patterns rather than on `self`, so every setter path
/// — the `&mut self` trait methods AND the `&self` typed `set_*_hz`
/// helpers — updates the same source of truth. Keeps callers that
/// bypass the trait from seeing a stale cache.
pub struct RtlTcpSource {
    host: String,
    port: u16,
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
    rx_buf: Mutex<VecDeque<u8>>,

    /// Running count of bytes dropped to keep `rx_buf` under its cap,
    /// for observability / UI "consumer too slow" indicators.
    rx_dropped_bytes: AtomicU64,

    /// Edge-trigger flag for the overflow warn log. Set when we drop
    /// bytes; cleared when the buffer drains to below half-cap. Ensures
    /// we log once per stall-and-drain cycle instead of per-chunk.
    rx_in_overflow: AtomicBool,

    /// Cached "current sample rate" reported via the `Source::sample_rate`
    /// getter. f64 bits so we can update from `&self` setters without
    /// interior-mutability boilerplate. Initialized to
    /// [`DEFAULT_CLIENT_SAMPLE_RATE_HZ`].
    cached_sample_rate_bits: AtomicU64,

    /// Cached "current center frequency" reported via `Source::tune`'s
    /// `frequency` accessor pattern. Same rationale as
    /// `cached_sample_rate_bits`.
    cached_frequency_bits: AtomicU64,

    /// Write side of the socket, protected by a Mutex so command senders
    /// can share it without racing. Replaced on every reconnect.
    command_sink: Mutex<Option<TcpStream>>,

    /// Clone of the raw socket held for the whole session, so
    /// `stop_manager` has a cancellation handle that does NOT go through
    /// the `command_sink` mutex: it unblocks a manager parked in the
    /// handshake reads (a peer that accepts and sends nothing) and a
    /// replay `write_all` stalled on a non-reading peer while it holds
    /// the sink lock (#745). Cleared at session end by `run_data_pump`.
    pending_stream: Mutex<Option<TcpStream>>,

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
    /// One `SetIfGain` param per stage (index `stage - 1`); the stage
    /// number lives in the param's upper 16 bits so the raw value can be
    /// replayed verbatim. `if_gain_mask` says which stages are recorded.
    last_if_gain: [AtomicU32; IF_GAIN_STAGES],
    if_gain_mask: AtomicU32,
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
            rx_buf: Mutex::new(VecDeque::with_capacity(RECV_CHUNK_BYTES)),
            command_sink: Mutex::new(None),
            pending_stream: Mutex::new(None),
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
            cached_sample_rate_bits: AtomicU64::new(DEFAULT_CLIENT_SAMPLE_RATE_HZ.to_bits()),
            cached_frequency_bits: AtomicU64::new(DEFAULT_CLIENT_CENTER_FREQ_HZ.to_bits()),
            last_testmode: AtomicU32::new(0),
            last_if_gain: std::array::from_fn(|_| AtomicU32::new(0)),
            if_gain_mask: AtomicU32::new(0),
            last_rtl_xtal: AtomicU32::new(0),
            last_tuner_xtal: AtomicU32::new(0),
        }
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
        // Close the socket through the session cancel handle FIRST: it is
        // guarded by its own mutex, so this works even while
        // `replay_sticky_commands` / `send_command` hold `command_sink`
        // in a blocked `write_all` against a non-reading peer — the
        // shutdown makes that write fail, which releases the sink lock
        // for the teardown below (#745).
        if let Ok(mut pending) = self.shared.pending_stream.lock()
            && let Some(s) = pending.take()
        {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        if let Ok(mut sink) = self.shared.command_sink.lock()
            && let Some(s) = sink.take()
        {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        if let Some(h) = self.manager.take() {
            let _ = h.join();
        }
        // The manager leaves terminal states (Failed / AuthRequired /
        // ...) in place when it returns early; after an explicit stop
        // the public view must read Disconnected with no tuner (#745).
        set_state(&self.shared, ConnectionState::Disconnected);
        if let Ok(mut tuner) = self.shared.tuner.lock() {
            *tuner = None;
        }
    }

    /// Bytes dropped from the receive buffer so far to keep it under
    /// [`RX_BUFFER_SOFT_CAP_BYTES`] — a "consumer too slow" indicator.
    #[must_use]
    pub fn rx_dropped_bytes(&self) -> u64 {
        self.shared.rx_dropped_bytes.load(Ordering::Relaxed)
    }
}

impl Drop for RtlTcpSource {
    fn drop(&mut self) {
        self.stop_manager();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
