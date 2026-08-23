//! C ABI for the `rtl_tcp` server (issue #325, ABI 0.11).
//!
//! Wraps the `sdr-server-rtltcp` crate behind a standalone
//! opaque handle (`SdrRtlTcpServer`) so hosts can let a Mac
//! share a locally-attached RTL-SDR dongle over the network.
//! Deliberately does **not** go through `SdrCore`: the server
//! has its own lifecycle, claims exclusive access to the
//! dongle, and has no coupling to the engine's DSP pipeline.
//! Running the engine and the server on the same process
//! would fight over the USB device — the UI is expected to
//! enforce mutual exclusivity by hiding the server panel
//! whenever the local dongle is the engine's active source.
//!
//! Stats (`recent_commands`) are exposed as JSON through a
//! caller-provided buffer, same shape as the RadioReference
//! search result. The remaining counters ride in a fixed-
//! layout struct for zero-copy polling.

use std::ffi::{CString, c_char};
use std::net::SocketAddr;
use std::sync::{Mutex, MutexGuard};

use sdr_server_rtltcp::{
    ClientInfo, InitialDeviceState, Server, ServerConfig, ServerError, ServerStats,
    TunerAdvertiseInfo,
    codec::CodecMask,
    protocol::{CommandOp, DEFAULT_PORT},
};

use crate::error::{SdrCoreError, clear_last_error, set_last_error};
use crate::lifecycle::panic_message;

// ============================================================
//  Bind-address discriminants — must match `SdrBindAddress` in
//  `include/sdr_core.h`. Never reorder or renumber.
// ============================================================

pub const SDR_BIND_LOOPBACK: i32 = 0;
pub const SDR_BIND_ALL_INTERFACES: i32 = 1;

/// Upper bound for `SdrRtlTcpServerConfig::buffer_capacity`.
/// The value becomes the slot count of the bounded
/// `sync_channel::<Vec<u8>>` in `sdr-server-rtltcp`. Each
/// slot holds one USB transfer (~256 KiB), so a bad FFI
/// input could otherwise turn `_start` into a multi-GiB
/// allocation / OOM path. 4096 slots ≈ 1 GiB — well past
/// any reasonable use case (the crate default is 500 =
/// 128 MiB) while keeping `u32::MAX` trivially rejected.
/// Per `CodeRabbit` round 9 on PR #360.
pub const SDR_RTLTCP_SERVER_MAX_BUFFER_CAPACITY: u32 = 4096;

// ============================================================
//  Public C struct layout — `SdrRtlTcpServerConfig`
// ============================================================

/// Server-start configuration. Mirrors the layout in
/// `include/sdr_core.h`.
///
/// The trailing `initial_*` fields correspond one-for-one with
/// `InitialDeviceState` on the Rust side. Hosts that don't want
/// to set a particular initial value can pass the upstream
/// default: `initial_gain_tenths_db = 0` is interpreted as
/// "auto" (the `None` on the Rust side).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrRtlTcpServerConfig {
    pub bind_address: i32,
    pub port: u16,
    pub device_index: u32,
    /// 0 = use `sdr_server_rtltcp::DEFAULT_BUFFER_CAPACITY`.
    pub buffer_capacity: u32,
    pub initial_freq_hz: u32,
    pub initial_sample_rate_hz: u32,
    /// Tuner gain in 0.1 dB. 0 means "auto" (the Rust `None`).
    pub initial_gain_tenths_db: i32,
    pub initial_ppm: i32,
    pub initial_bias_tee: bool,
    /// Direct-sampling mode: 0 = off, 1 = I, 2 = Q. Rejected
    /// outside this range by `sdr_rtltcp_server_start`.
    pub initial_direct_sampling: i32,
    /// Maximum concurrent `Role::Listen` clients. 0 = use
    /// [`sdr_server_rtltcp::DEFAULT_LISTENER_CAP`] (10). Vanilla
    /// `rtl_tcp` clients and the single Control client are NOT
    /// counted — they occupy the controller slot, which is
    /// separate from the listener pool. #392.
    pub listener_cap: u32,
    /// Pre-shared auth key bytes. `NULL` with `auth_key_len == 0`
    /// means "auth disabled" — default, matches today's LAN-trust
    /// model. Non-null pointer + non-zero length enables the
    /// auth gate: every connecting client must present an
    /// `AuthKeyMessage` whose bytes match these bytes
    /// (constant-time compare). The server copies the bytes
    /// into its owned `Vec<u8>` during
    /// `sdr_rtltcp_server_start` — the caller's buffer can be
    /// freed / reused immediately after the call returns.
    ///
    /// Length must be in `1..=256`
    /// ([`sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN`]);
    /// values outside that range cause `sdr_rtltcp_server_start`
    /// to return [`SdrCoreError::InvalidArg`] without starting
    /// the server.
    ///
    /// Keys travel as cleartext over TCP — wrap the connection in
    /// SSH / WireGuard / Tailscale if the threat model demands
    /// WAN-grade confidentiality. This flag enables LAN isolation
    /// (keeping IoT devices / shared-network cohabitants from
    /// seizing the dongle), nothing stronger. #394.
    pub auth_key: *const u8,
    /// Length in bytes of [`Self::auth_key`]. See that field's
    /// doc for valid range and semantics. Must be 0 iff
    /// `auth_key` is NULL.
    pub auth_key_len: u32,
    /// Whether [`Self::compression`] below is meaningful. `false`
    /// (the zero-init default) runs the server with
    /// [`CodecMask::NONE_ONLY`] — legacy clients negotiate with
    /// no compression, identical to pre-#400 behaviour. Added in
    /// ABI 0.19 per issue #400.
    pub has_compression: bool,
    /// Compression-mask wire byte. Only used when
    /// [`Self::has_compression`] is `true`. Value is a
    /// [`CodecMask::to_wire`] byte. Typical values: `0x01` =
    /// `None`-only, `0x03` = `None + LZ4` (accept LZ4
    /// compression hellos + fall back to `None` for vanilla
    /// clients). Must match the codecs the host is willing to
    /// advertise via [`SdrRtlTcpAdvertiseOptions::codecs`] on
    /// mDNS — those two have to stay in sync or clients see
    /// "server advertises LZ4" but hit a `None`-only negotiation
    /// at handshake time.
    pub compression: u8,
}

/// Aggregate server-lifetime statistics snapshot.
///
/// Post-#391 (multi-client): carries only cumulative counters and
/// the tuner's gain count. Per-client session state moved to
/// [`SdrRtlTcpClientInfo`] — callers use
/// [`sdr_rtltcp_server_client_list`] to read it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SdrRtlTcpServerStats {
    /// Number of clients connected at the moment this snapshot
    /// was taken. `sdr_rtltcp_server_client_list` is a separate
    /// live read, so membership may change between the two calls.
    /// Use this as an initial sizing hint for the client array,
    /// then honor `*out_count` returned by the list call and
    /// retry with a larger buffer if the returned count exceeds
    /// the capacity you passed.
    pub connected_count: u32,
    /// Cumulative bytes fanned out across all clients over the
    /// server's lifetime. Monotonic — never reset. UI consumers
    /// derive data-rate from the delta between consecutive
    /// snapshots divided by the poll interval.
    pub total_bytes_sent: u64,
    /// Cumulative buffer drops across all clients over the
    /// server's lifetime. Monotonic. A drop is counted when the
    /// broadcaster's `try_send` into a client's per-client
    /// bounded channel returns `Full` (that client's listener
    /// stalled) — other clients drain independently.
    pub total_buffers_dropped: u64,
    /// Cumulative count of clients accepted over the server's
    /// lifetime. Persists across disconnects; use to answer
    /// "how many sessions has this server served?" without
    /// walking a vec.
    pub lifetime_accepted: u64,
    /// Tuner's advertised discrete gain count (from
    /// `dongle_info_t`). Populated by `Server::start` during
    /// the dongle-open phase — non-zero for the entire server
    /// lifetime, including before the first client connects.
    pub gain_count: u32,
}

/// Fixed-size buffer (bytes) for a client's "ip:port" peer
/// address in [`SdrRtlTcpClientInfo`]. Sized to fit IPv6 literal
/// forms (`[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535` is
/// 47 bytes plus NUL); 64 is the next power-of-two round-up.
pub const SDR_RTLTCP_CLIENT_PEER_LEN: usize = 64;

/// Per-client snapshot, one entry per connected client. Returned
/// as an array from [`sdr_rtltcp_server_client_list`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrRtlTcpClientInfo {
    /// Stable, monotonic identifier assigned at accept time.
    /// Never reused across the server's lifetime — useful for
    /// correlating across stats polls ("client 7 went quiet"
    /// reads more clearly than peer-address equality when the
    /// same peer reconnects on a fresh port).
    pub id: u64,
    /// Peer socket address as "ip:port", NUL-terminated. Space
    /// padded with zeros after the NUL. Always fits within
    /// [`SDR_RTLTCP_CLIENT_PEER_LEN`] including the NUL.
    pub peer_addr: [c_char; SDR_RTLTCP_CLIENT_PEER_LEN],
    /// Seconds since this client handshake completed.
    pub uptime_secs: f64,
    /// Negotiated stream codec wire byte: 0 = None (legacy /
    /// vanilla rtl_tcp), 1 = LZ4. See
    /// `sdr_server_rtltcp::codec::Codec::to_wire`.
    pub codec: u8,
    /// Bytes written to this client's socket since its handshake.
    /// Post-compression (`StatsTrackingWrite` counts bytes at
    /// the TCP level, below the LZ4 encoder). Covers only this
    /// currently-connected session —
    /// [`SdrRtlTcpServerStats::total_bytes_sent`] is a separate
    /// server-lifetime aggregate that also carries contributions
    /// from previously-disconnected clients, so the sum of
    /// per-client `bytes_sent` across the live list does NOT
    /// equal `total_bytes_sent` once at least one disconnect has
    /// occurred. Per `CodeRabbit` round 3 on PR #402.
    pub bytes_sent: u64,
    /// Buffer drops this client has accrued (broadcaster saw
    /// `TrySendError::Full` against this client's channel).
    pub buffers_dropped: u64,
    /// Client-issued center-frequency override in Hz. 0 before
    /// the client has issued `SetCenterFreq` — does not reflect
    /// the server's `initial_freq_hz` or the live device
    /// register. Hosts that want "what the dongle is actually
    /// tuned to" should fall back to
    /// `SdrRtlTcpServerConfig::initial_freq_hz` when
    /// `current_freq_hz == 0`.
    pub current_freq_hz: u32,
    /// Client-issued sample-rate override, same semantics as
    /// `current_freq_hz`.
    pub current_sample_rate_hz: u32,
    /// Most recent client-issued tuner-gain request in 0.1 dB.
    /// Valid only when `has_current_gain_value == true`.
    pub current_gain_tenths_db: i32,
    /// `true` when the client's last gain-mode request was auto;
    /// `false` when manual. Valid only when
    /// `has_current_gain_mode == true`.
    pub current_gain_auto: bool,
    /// `true` once the client has issued at least one
    /// `SetTunerGain` command. Tracked separately from the
    /// mode flag because a client can send `SetGainMode(auto)`
    /// without a preceding `SetTunerGain` (and vice versa).
    pub has_current_gain_value: bool,
    /// `true` once the client has issued at least one
    /// `SetGainMode` command. Valid companion to
    /// `current_gain_auto`.
    pub has_current_gain_mode: bool,
    /// Number of entries in this client's recent-commands ring.
    /// An entry count, not a byte count — callers that want to
    /// serialize the ring as JSON use
    /// [`sdr_rtltcp_server_recent_commands_json`]'s `out_required`
    /// size-probe path for buffer sizing (length depends on opcode
    /// names and float formatting, not a fixed per-entry byte
    /// count).
    pub recent_commands_count: u32,
    /// `true` once the client has issued at least one command
    /// this session. Complementary to `recent_commands_count`:
    /// `recent_commands_count > 0` implies `has_last_command`,
    /// but `has_last_command` alone means "at least one command
    /// dispatched" without committing to whether the ring
    /// already evicted the earliest entries. `last_command_op`
    /// and `last_command_age_secs` are only valid when this is
    /// `true`. Per `CodeRabbit` round 6 on PR #402 — surfaces
    /// the same `last_command` field the Rust UI uses to drive
    /// `pick_most_recent_commander`, so FFI hosts can replicate
    /// the "most recent commander" selection without polling /
    /// parsing the JSON ring for every connected client.
    pub has_last_command: bool,
    /// Wire byte of the client's most recently dispatched
    /// command — matches the opcode values documented in
    /// `rtl_tcp.c:315-372` (`SetCenterFreq = 0x01`,
    /// `SetBiasTee = 0x0e`, etc.). Valid only when
    /// `has_last_command == true`; 0 when the client hasn't
    /// sent a command yet. Hosts can map this back to a human
    /// label via [`sdr_rtltcp_server_recent_commands_json`] or
    /// their own opcode table.
    pub last_command_op: u8,
    /// Seconds elapsed between the client's most recent command
    /// and the moment this snapshot was assembled. A pure
    /// snapshot-time age — NOT a monotonic counter: a fresh
    /// command from the client resets it back near zero on the
    /// next poll, so don't rely on it increasing between
    /// consecutive samples. Intended for comparing *recency
    /// across clients within a single snapshot* (smallest age
    /// wins — replicates the Rust UI's
    /// `pick_most_recent_commander`). Valid only when
    /// `has_last_command == true`.
    pub last_command_age_secs: f64,
    /// Role the server granted to this client: `0 = Control` (can
    /// tune / change gain / etc.), `1 = Listen` (receives the IQ
    /// stream; server drops any commands they send). Matches
    /// `sdr_server_rtltcp::extension::Role::to_wire`. Hosts that
    /// want to render a "Controller" / "Listener" badge in the
    /// client list read this byte directly; the `Role::Control`
    /// value is the default for vanilla `rtl_tcp` clients that
    /// don't speak the RTLX extension (they always land in the
    /// Control slot when it's free, never as listeners). #392.
    pub role: u8,
}

impl Default for SdrRtlTcpClientInfo {
    fn default() -> Self {
        Self {
            id: 0,
            peer_addr: [0; SDR_RTLTCP_CLIENT_PEER_LEN],
            uptime_secs: 0.0,
            codec: 0,
            bytes_sent: 0,
            buffers_dropped: 0,
            current_freq_hz: 0,
            current_sample_rate_hz: 0,
            current_gain_tenths_db: 0,
            current_gain_auto: false,
            has_current_gain_value: false,
            has_current_gain_mode: false,
            recent_commands_count: 0,
            has_last_command: false,
            last_command_op: 0,
            last_command_age_secs: 0.0,
            role: sdr_server_rtltcp::extension::Role::Control.to_wire(),
        }
    }
}

// ============================================================
//  Opaque handle
// ============================================================

/// Opaque server handle. The `Mutex<Option<Server>>` lets us
/// take the `Server` out of the handle on drop/stop even
/// though `Server::stop(self)` consumes by value. String
/// fields returned by `_stats` are written into caller-
/// allocated buffers, so we don't need any handle-scoped
/// string storage.
pub struct SdrRtlTcpServer {
    inner: Mutex<Option<Server>>,
}

impl SdrRtlTcpServer {
    fn new(server: Server) -> Self {
        Self {
            inner: Mutex::new(Some(server)),
        }
    }

    /// Lock the inner server, recovering from mutex poisoning.
    /// A caught panic inside a prior `with_server` body poisons
    /// the mutex; `.ok()` would then collapse every subsequent
    /// call into `None` and the caller would see "server
    /// already stopped" even though the `Server` is still
    /// alive. Use `PoisonError::into_inner` to keep going.
    /// Per `CodeRabbit` round 3 on PR #360.
    fn lock_inner(&self) -> MutexGuard<'_, Option<Server>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("SdrRtlTcpServer: recovering poisoned handle mutex");
                poisoned.into_inner()
            }
        }
    }

    /// Lock the inner server. Returns `None` when the server
    /// has been stopped already.
    fn with_server<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&Server) -> R,
    {
        let guard = self.lock_inner();
        guard.as_ref().map(f)
    }

    /// SAFETY: caller must ensure `handle` is a pointer
    /// previously returned by `sdr_rtltcp_server_start` and
    /// not yet passed to `sdr_rtltcp_server_stop`.
    unsafe fn from_raw<'a>(handle: *mut Self) -> Option<&'a Self> {
        // SAFETY: caller contract.
        if handle.is_null() {
            None
        } else {
            Some(unsafe { &*handle })
        }
    }
}

// ============================================================
//  Config translation
// ============================================================

fn initial_from_c(cfg: &SdrRtlTcpServerConfig) -> Result<InitialDeviceState, SdrCoreError> {
    // Zero sample-rate wedges the RTL-SDR USB controller — the
    // server CLI already rejects it at parse time for the same
    // reason. Catch it at the FFI boundary before we open the
    // device. Per `CodeRabbit` round 1 on PR #360.
    if cfg.initial_sample_rate_hz == 0 {
        set_last_error("sdr_rtltcp_server_start: initial_sample_rate_hz must be > 0");
        return Err(SdrCoreError::InvalidArg);
    }
    // Reuse the bounds the `sdr_core_set_direct_sampling`
    // command FFI exposes so both entry points validate
    // identically. Per `CodeRabbit` round 8 on PR #360.
    if !(crate::command::SDR_DIRECT_SAMPLING_MIN..=crate::command::SDR_DIRECT_SAMPLING_MAX)
        .contains(&cfg.initial_direct_sampling)
    {
        set_last_error(format!(
            "sdr_rtltcp_server_start: initial_direct_sampling must be \
             {}..={}, got {}",
            crate::command::SDR_DIRECT_SAMPLING_MIN,
            crate::command::SDR_DIRECT_SAMPLING_MAX,
            cfg.initial_direct_sampling
        ));
        return Err(SdrCoreError::InvalidArg);
    }
    // `initial_gain_tenths_db == 0` maps to `None` (auto) on
    // the Rust side, matching upstream rtl_tcp's `-g 0` semantics.
    let gain_tenths_db = if cfg.initial_gain_tenths_db == 0 {
        None
    } else {
        Some(cfg.initial_gain_tenths_db)
    };
    Ok(InitialDeviceState {
        center_freq_hz: cfg.initial_freq_hz,
        sample_rate_hz: cfg.initial_sample_rate_hz,
        gain_tenths_db,
        ppm: cfg.initial_ppm,
        bias_tee: cfg.initial_bias_tee,
        direct_sampling: cfg.initial_direct_sampling,
    })
}

fn bind_socket_addr(bind: i32, port: u16) -> Result<SocketAddr, SdrCoreError> {
    let port = if port == 0 { DEFAULT_PORT } else { port };
    match bind {
        SDR_BIND_LOOPBACK => Ok(SocketAddr::from(([127, 0, 0, 1], port))),
        SDR_BIND_ALL_INTERFACES => Ok(SocketAddr::from(([0, 0, 0, 0], port))),
        other => {
            set_last_error(format!(
                "sdr_rtltcp_server_start: unknown bind_address {other}"
            ));
            Err(SdrCoreError::InvalidArg)
        }
    }
}

/// Translate the C-ABI `listener_cap` field into the Rust
/// `ServerConfig::listener_cap`. Zero means "use the crate
/// default" per the header docs — the `0 → DEFAULT_LISTENER_CAP`
/// rule is the public contract, so pulling it out of
/// `sdr_rtltcp_server_start` makes it unit-testable without the
/// hardware-backed start path. Non-zero values passthrough as
/// `u32 → usize` (widening cast, always lossless on 32- and
/// 64-bit targets we build for). Per `CodeRabbit` round 2 on
/// PR #403.
fn listener_cap_from_c(listener_cap: u32) -> usize {
    if listener_cap == 0 {
        sdr_server_rtltcp::DEFAULT_LISTENER_CAP
    } else {
        listener_cap as usize
    }
}

/// Translate the C-ABI `has_compression` / `compression` pair
/// into the Rust `ServerConfig::compression` `CodecMask`. The
/// `has_*` gate is the zero-init-safe opt-in pattern (same as
/// `has_txbuf` / `has_codecs` on [`SdrRtlTcpAdvertiseOptions`]):
///
/// - `has_compression = false` → `CodecMask::NONE_ONLY`, matching
///   the pre-ABI-0.19 hardcoded behaviour.
/// - `has_compression = true` → `CodecMask::from_wire(compression)`.
///
/// Extracted into its own helper (rather than inlined in
/// `sdr_rtltcp_server_start`) so the ABI translation path is
/// unit-testable without the hardware-backed start call.
/// Matches the [`listener_cap_from_c`] / [`auth_key_from_c`]
/// shape. Per `CodeRabbit` round 1 on PR #418, issue #400.
fn compression_from_c(has_compression: bool, compression: u8) -> CodecMask {
    if has_compression {
        CodecMask::from_wire(compression)
    } else {
        CodecMask::NONE_ONLY
    }
}

/// Outcome of [`auth_key_from_c`] — either the server should run
/// without auth (`None` input, or `Some` with valid length) or
/// the caller gave a bad pointer/length pair and
/// `sdr_rtltcp_server_start` should reject with
/// `SdrCoreError::InvalidArg`. Split out so the translation is
/// unit-testable without the hardware-backed start path.
#[derive(Debug, PartialEq, Eq)]
enum AuthKeyFromC {
    /// Auth disabled (caller passed NULL pointer + zero length).
    Disabled,
    /// Auth enabled with the given bytes. Caller stores this in
    /// `ServerConfig::auth_key`.
    Enabled(Vec<u8>),
    /// Malformed input — non-null pointer with zero length, null
    /// pointer with non-zero length, or length exceeding
    /// `MAX_AUTH_KEY_LEN`. `sdr_rtltcp_server_start` surfaces as
    /// `InvalidArg`.
    Invalid,
}

/// Translate the C-ABI `auth_key` + `auth_key_len` pair into a
/// `ServerConfig::auth_key` value. Rules:
///
/// - Both `NULL` and `0`: auth disabled.
/// - Non-null + `len` in `1..=MAX_AUTH_KEY_LEN`: copy `len`
///   bytes, enabled.
/// - Any other combination (null + len > 0, non-null + len = 0,
///   len > MAX): invalid.
///
/// # Safety
///
/// `auth_key` must either be NULL or point at `auth_key_len`
/// readable bytes. Caller's buffer can be freed/reused after
/// this call returns — bytes are copied into the returned Vec.
unsafe fn auth_key_from_c(auth_key: *const u8, auth_key_len: u32) -> AuthKeyFromC {
    use sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN;
    let len_usize = auth_key_len as usize;
    if auth_key.is_null() && auth_key_len == 0 {
        return AuthKeyFromC::Disabled;
    }
    if auth_key.is_null() || auth_key_len == 0 {
        // Exactly one of the pair is zero/null → malformed.
        return AuthKeyFromC::Invalid;
    }
    if len_usize > MAX_AUTH_KEY_LEN {
        // Over-max length would fail at `AuthKeyMessage::to_bytes`
        // downstream; catch here so the diagnostic surfaces
        // cleanly as an InvalidArg rather than a handshake fail.
        return AuthKeyFromC::Invalid;
    }
    // SAFETY: caller contract — pointer is non-null and points
    // at `len_usize` readable bytes. Copy into a Vec so the
    // caller's buffer can be freed / reused after this call.
    let bytes = unsafe { std::slice::from_raw_parts(auth_key, len_usize) }.to_vec();
    AuthKeyFromC::Enabled(bytes)
}

// ============================================================
//  FFI entry points
// ============================================================

/// Start an rtl_tcp server with the given configuration. On
/// success writes the handle to `*out_handle` and returns
/// `SDR_CORE_OK`. On failure returns a negative error code and
/// leaves `*out_handle` untouched.
///
/// # Safety
///
/// `cfg` and `out_handle` must be non-null. The caller is
/// responsible for eventually releasing the handle via
/// `sdr_rtltcp_server_stop`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_start(
    cfg: *const SdrRtlTcpServerConfig,
    out_handle: *mut *mut SdrRtlTcpServer,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if cfg.is_null() || out_handle.is_null() {
            set_last_error("sdr_rtltcp_server_start: null cfg or out_handle");
            return SdrCoreError::InvalidArg.as_int();
        }
        // SAFETY: caller contract.
        let cfg = unsafe { &*cfg };
        let initial = match initial_from_c(cfg) {
            Ok(v) => v,
            Err(e) => return e.as_int(),
        };
        let bind = match bind_socket_addr(cfg.bind_address, cfg.port) {
            Ok(v) => v,
            Err(e) => return e.as_int(),
        };
        let buffer_capacity = if cfg.buffer_capacity == 0 {
            sdr_server_rtltcp::DEFAULT_BUFFER_CAPACITY
        } else if cfg.buffer_capacity > SDR_RTLTCP_SERVER_MAX_BUFFER_CAPACITY {
            // Reject at the boundary. Above this the
            // allocation pressure dominates anything a
            // real server would need. Per `CodeRabbit`
            // round 9 on PR #360.
            set_last_error(format!(
                "sdr_rtltcp_server_start: buffer_capacity {} exceeds max {} \
                 (each slot is ~256 KiB of USB transfer data)",
                cfg.buffer_capacity, SDR_RTLTCP_SERVER_MAX_BUFFER_CAPACITY
            ));
            return SdrCoreError::InvalidArg.as_int();
        } else {
            cfg.buffer_capacity as usize
        };
        let listener_cap = listener_cap_from_c(cfg.listener_cap);
        // SAFETY: `auth_key` is NULL-or-readable-for-`auth_key_len`
        // per the struct doc; caller contract extends to us here.
        let auth_key = match unsafe { auth_key_from_c(cfg.auth_key, cfg.auth_key_len) } {
            AuthKeyFromC::Disabled => None,
            AuthKeyFromC::Enabled(bytes) => Some(bytes),
            AuthKeyFromC::Invalid => {
                set_last_error(format!(
                    "sdr_rtltcp_server_start: invalid auth_key / auth_key_len pair \
                     (auth_key null? = {}, auth_key_len = {}, max = {})",
                    cfg.auth_key.is_null(),
                    cfg.auth_key_len,
                    sdr_server_rtltcp::extension::MAX_AUTH_KEY_LEN,
                ));
                return SdrCoreError::InvalidArg.as_int();
            }
        };
        // `compression` projects from the `has_compression` gate
        // per #400 / ABI 0.19. See [`compression_from_c`] for the
        // full contract (zero-init default = `NONE_ONLY`,
        // opt-in = `from_wire`). Extracted into the helper so
        // the ABI promise has unit-test coverage.
        let compression = compression_from_c(cfg.has_compression, cfg.compression);
        let server_cfg = ServerConfig {
            bind,
            device_index: cfg.device_index,
            initial,
            buffer_capacity,
            compression,
            listener_cap,
            auth_key,
        };
        match Server::start(server_cfg) {
            Ok(server) => {
                let handle = Box::new(SdrRtlTcpServer::new(server));
                // SAFETY: `out_handle` was null-checked above.
                unsafe { *out_handle = Box::into_raw(handle) };
                clear_last_error();
                SdrCoreError::Ok.as_int()
            }
            Err(e) => {
                set_last_error(format!("sdr_rtltcp_server_start: {e}"));
                // Map `ServerError` variants to stable FFI codes
                // by shape, not by parsing the `Display` string.
                // Per `CodeRabbit` round 1 on PR #360.
                match e {
                    ServerError::PortInUse(_) | ServerError::Io(_) => SdrCoreError::Io.as_int(),
                    ServerError::Device(_)
                    | ServerError::NoDevice
                    | ServerError::BadDeviceIndex { .. } => SdrCoreError::Device.as_int(),
                    // Config-time rejection (auth_key length out of
                    // range). Caller supplied an out-of-bounds value
                    // in `SdrRtlTcpServerConfig`; surface as
                    // InvalidArg so the FFI code matches the already-
                    // established "malformed input" error class
                    // (same code `auth_key_from_c` returns for
                    // NULL-pointer/length mismatches).
                    ServerError::InvalidAuthKeyLength { .. } => SdrCoreError::InvalidArg.as_int(),
                }
            }
        }
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_rtltcp_server_start: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Stop and release the server. Blocks until the accept
/// thread has joined and the RTL-SDR dongle is released — on
/// return the device is free for the engine (or any other
/// local consumer) to open immediately. After this call the
/// handle pointer is invalid; do not use it again. Passing
/// null is a no-op.
///
/// The earlier revision of this function off-loaded
/// `Server::stop` onto a detached thread to keep Swift's
/// `@MainActor` callers from wedging on the join. That turned
/// out to be a correctness bug — callers had no way to know
/// when the dongle actually released, so immediate handoff
/// (flip the engine's source to the same dongle) was racy.
/// The `Server`'s poll cadence is 100 ms, so the synchronous
/// join completes in well under a frame on typical hardware.
/// Per `CodeRabbit` round 2 on PR #360.
///
/// # Safety
///
/// `handle` must be either null or a pointer previously returned
/// by `sdr_rtltcp_server_start` and not already passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_stop(handle: *mut SdrRtlTcpServer) {
    if handle.is_null() {
        return;
    }
    // Reclaim ownership so the Box drops when this function
    // returns. `Server::stop(self)` consumes the inner value;
    // if the caller already stopped it the `Option` is `None`
    // and drop is a no-op.
    let boxed = unsafe { Box::from_raw(handle) };
    // Use the same poison-recovery path as `lock_inner` — a
    // prior caught panic inside `with_server` would otherwise
    // leave the mutex poisoned and cause `_stop` to skip
    // `Server::stop(self)` and fall back to `Drop`, breaking
    // the synchronous-stop contract on exactly the
    // panic-recovery path. Per `CodeRabbit` round 3 on PR #360.
    let taken = boxed.lock_inner().take();
    if let Some(server) = taken {
        // Wrap the join in `catch_unwind` so a panic inside
        // `Server::stop` — e.g., a poisoned mutex on the
        // stats ring — can't cross the FFI boundary into
        // Swift. Mirrors the pattern used by every other
        // entry point in this module.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            server.stop();
        }));
        if let Err(payload) = result {
            // Best-effort diagnostic. We can't return an
            // error code from `_stop` (its signature is
            // void), so the payload goes to tracing where a
            // host with log routing can see it.
            tracing::warn!(
                "sdr_rtltcp_server_stop: Server::stop panicked: {}",
                panic_message(&payload)
            );
        }
    }
}

/// Return `true` once the server's accept thread has exited.
/// Useful for CLI tools polling for a clean shutdown, or hosts
/// that want to detect a crashed/auto-stopped server.
///
/// # Safety
///
/// `handle` must be a pointer previously returned by
/// `sdr_rtltcp_server_start` and not yet stopped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_has_stopped(handle: *mut SdrRtlTcpServer) -> bool {
    // Wrap in `catch_unwind` so a panic inside
    // `Server::has_stopped` can't cross the FFI boundary into
    // Swift. Matches the pattern every other entry point in
    // this module uses. Per `CodeRabbit` round 4 on PR #360.
    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract.
        let Some(h) = (unsafe { SdrRtlTcpServer::from_raw(handle) }) else {
            return true;
        };
        h.with_server(Server::has_stopped).unwrap_or(true)
    });
    match result {
        Ok(stopped) => stopped,
        Err(payload) => {
            tracing::warn!(
                "sdr_rtltcp_server_has_stopped: panic: {}",
                panic_message(&payload)
            );
            // Treat a panic as "stopped" — the alternative
            // would be to report a likely-broken server as
            // healthy, which is worse for callers that poll
            // this to detect shutdown.
            true
        }
    }
}

/// Snapshot the server's aggregate stats into a caller-provided
/// struct + tuner-name buffer.
///
/// `out_stats` must be non-null. `out_tuner_name` is an optional
/// NUL-terminated string buffer (pass `NULL` with
/// `tuner_name_len = 0` to skip). Truncation at `tuner_name_len - 1`
/// is not an error. 64 bytes handles any realistic tuner name.
///
/// For per-client state (peer addresses, per-client counters,
/// commanded frequencies, etc.) use `out_stats.connected_count`
/// as an initial sizing hint for
/// [`sdr_rtltcp_server_client_list`]. That call is a separate
/// live read, so membership may change between the two — always
/// honor the list call's returned `*out_count` and retry with a
/// larger buffer if it exceeds the capacity you passed.
///
/// # Safety
///
/// Pointers must either be null or point at writable buffers
/// with the indicated capacities.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_stats(
    handle: *mut SdrRtlTcpServer,
    out_stats: *mut SdrRtlTcpServerStats,
    out_tuner_name: *mut c_char,
    tuner_name_len: usize,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if out_stats.is_null() {
            set_last_error("sdr_rtltcp_server_stats: null out_stats");
            return SdrCoreError::InvalidArg.as_int();
        }
        // SAFETY: caller contract.
        let Some(h) = (unsafe { SdrRtlTcpServer::from_raw(handle) }) else {
            set_last_error("sdr_rtltcp_server_stats: invalid handle");
            return SdrCoreError::InvalidHandle.as_int();
        };

        let Some((stats, tuner)) = h.with_server(|s| (s.stats(), s.tuner_info().clone())) else {
            set_last_error("sdr_rtltcp_server_stats: server already stopped");
            return SdrCoreError::NotRunning.as_int();
        };

        let packed = stats_to_c(&stats, &tuner);
        // SAFETY: out_stats null-checked above.
        unsafe { *out_stats = packed };

        // Copy the tuner name into the caller buffer (if provided).
        // Truncate cleanly at `len - 1`.
        // SAFETY: caller contract on out_tuner_name / len.
        unsafe { write_cstr(out_tuner_name, tuner_name_len, &tuner.name) };

        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_rtltcp_server_stats: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Snapshot every connected client's state into a caller-provided
/// array.
///
/// `out_clients` may be null when `capacity == 0` — in that case
/// the function populates `*out_count` with the current
/// connected-client count so the caller can size its buffer and
/// re-call. When `capacity > 0` and there are more connected
/// clients than `capacity`, the function fills the first
/// `capacity` entries and still writes the full count to
/// `*out_count` (the caller retries with a bigger buffer).
///
/// Returns `SDR_CORE_OK` on success (including the query-count
/// path), `SDR_CORE_ERR_INVALID_ARG` on null `out_count`,
/// `SDR_CORE_ERR_INVALID_HANDLE` on null handle,
/// `SDR_CORE_ERR_NOT_RUNNING` if the server was already stopped.
///
/// Client ordering is stable within a snapshot (oldest-first) but
/// may shift across snapshots as clients disconnect. Use
/// [`SdrRtlTcpClientInfo::id`] for cross-snapshot correlation.
///
/// # Safety
///
/// `out_clients` must either be null (with `capacity == 0`) or
/// point at a writable array of `capacity`
/// `SdrRtlTcpClientInfo` entries. `out_count` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_client_list(
    handle: *mut SdrRtlTcpServer,
    out_clients: *mut SdrRtlTcpClientInfo,
    capacity: usize,
    out_count: *mut usize,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        if out_count.is_null() {
            set_last_error("sdr_rtltcp_server_client_list: null out_count");
            return SdrCoreError::InvalidArg.as_int();
        }
        if out_clients.is_null() && capacity > 0 {
            set_last_error(
                "sdr_rtltcp_server_client_list: null out_clients with non-zero capacity",
            );
            return SdrCoreError::InvalidArg.as_int();
        }
        // SAFETY: caller contract.
        let Some(h) = (unsafe { SdrRtlTcpServer::from_raw(handle) }) else {
            set_last_error("sdr_rtltcp_server_client_list: invalid handle");
            return SdrCoreError::InvalidHandle.as_int();
        };
        let Some(stats) = h.with_server(Server::stats) else {
            set_last_error("sdr_rtltcp_server_client_list: server already stopped");
            return SdrCoreError::NotRunning.as_int();
        };

        let total = stats.connected_clients.len();
        // SAFETY: out_count null-checked above.
        unsafe { *out_count = total };

        // Capture the snapshot clock once for the entire array.
        // Every projected `last_command_age_secs` / `uptime_secs`
        // references this same `Instant`, so FFI hosts comparing
        // ages across clients in one snapshot see a consistent
        // ordering — per-entry `Instant::now()` calls would drift
        // by a few microseconds across the loop and could flip
        // the "smallest age wins" selection. Per `CodeRabbit`
        // round 7 on PR #402.
        let snapshot_at = std::time::Instant::now();
        let to_write = total.min(capacity);
        for (i, info) in stats.connected_clients.iter().take(to_write).enumerate() {
            // SAFETY: `i < to_write <= capacity` and the caller
            // guaranteed `out_clients` points at `capacity`
            // entries.
            unsafe {
                *out_clients.add(i) = client_info_to_c(info, snapshot_at);
            }
        }

        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_rtltcp_server_client_list: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

/// Write one client's recent-commands ring to `out_buf` as a
/// JSON array. Each entry is
/// `{"op": "<name>", "seconds_ago": <f64>}`.
///
/// `client_id` identifies the target client — read it from an
/// earlier `SdrRtlTcpClientInfo::id` snapshot.
///
/// On success returns `SDR_CORE_OK` and NUL-terminates the
/// buffer. On too-small buffer returns
/// `SDR_CORE_ERR_INVALID_ARG` and writes the required size
/// (including NUL) to `*out_required` when non-null. If the
/// specified client isn't currently connected returns
/// `SDR_CORE_ERR_INVALID_ARG` with `*out_required = 0` (client
/// may have disconnected between snapshots). On stopped server
/// returns `NOT_RUNNING`.
///
/// # Safety
///
/// `out_buf` must either be null (with `buf_len == 0`) or point
/// at a writable buffer of at least `buf_len` bytes.
/// `out_required` is optional (pass null to skip).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_recent_commands_json(
    handle: *mut SdrRtlTcpServer,
    client_id: u64,
    out_buf: *mut c_char,
    buf_len: usize,
    out_required: *mut usize,
) -> i32 {
    let result = std::panic::catch_unwind(|| {
        // SAFETY: caller contract.
        let Some(h) = (unsafe { SdrRtlTcpServer::from_raw(handle) }) else {
            set_last_error("sdr_rtltcp_server_recent_commands_json: invalid handle");
            return SdrCoreError::InvalidHandle.as_int();
        };
        let Some(stats) = h.with_server(Server::stats) else {
            set_last_error("sdr_rtltcp_server_recent_commands_json: server already stopped");
            return SdrCoreError::NotRunning.as_int();
        };
        let Some(client) = stats.connected_clients.iter().find(|c| c.id == client_id) else {
            set_last_error(format!(
                "sdr_rtltcp_server_recent_commands_json: client {client_id} not connected"
            ));
            if !out_required.is_null() {
                // SAFETY: caller contract.
                unsafe { *out_required = 0 };
            }
            return SdrCoreError::InvalidArg.as_int();
        };
        let json = match recent_commands_to_json(client) {
            Ok(s) => s,
            Err(e) => {
                set_last_error(format!(
                    "sdr_rtltcp_server_recent_commands_json: JSON encoding failed: {e}"
                ));
                return SdrCoreError::Internal.as_int();
            }
        };
        let Ok(cstr) = CString::new(json) else {
            set_last_error("sdr_rtltcp_server_recent_commands_json: interior NUL (unreachable)");
            return SdrCoreError::Internal.as_int();
        };
        let bytes = cstr.as_bytes_with_nul();
        if !out_required.is_null() {
            // SAFETY: caller contract.
            unsafe { *out_required = bytes.len() };
        }
        if out_buf.is_null() || buf_len < bytes.len() {
            set_last_error(format!(
                "sdr_rtltcp_server_recent_commands_json: buffer too small (need {} bytes)",
                bytes.len()
            ));
            return SdrCoreError::InvalidArg.as_int();
        }
        // SAFETY: out_buf has buf_len bytes per caller contract.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out_buf, bytes.len());
        }
        clear_last_error();
        SdrCoreError::Ok.as_int()
    });
    match result {
        Ok(code) => code,
        Err(payload) => {
            set_last_error(format!(
                "sdr_rtltcp_server_recent_commands_json: panic: {}",
                panic_message(&payload)
            ));
            SdrCoreError::Internal.as_int()
        }
    }
}

// ============================================================
//  Helpers
// ============================================================

fn stats_to_c(stats: &ServerStats, tuner: &TunerAdvertiseInfo) -> SdrRtlTcpServerStats {
    SdrRtlTcpServerStats {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "connected_count is bounded by OS FD limits — far below u32::MAX"
        )]
        connected_count: stats.connected_clients.len() as u32,
        total_bytes_sent: stats.total_bytes_sent,
        total_buffers_dropped: stats.total_buffers_dropped,
        lifetime_accepted: stats.lifetime_accepted,
        gain_count: tuner.gain_count,
    }
}

fn client_info_to_c(info: &ClientInfo, snapshot_at: std::time::Instant) -> SdrRtlTcpClientInfo {
    // Project `last_command` onto the flat FFI trio:
    // `(has_last_command, last_command_op, last_command_age_secs)`,
    // and compute `uptime_secs` the same way. Both ages reference
    // `snapshot_at` — a single `Instant::now()` captured once by
    // `sdr_rtltcp_server_client_list` — so every entry in the
    // emitted array is measured against the same clock. Per-client
    // `Instant::now()` calls would drift by a few microseconds
    // across the projection loop, which is enough to flip the
    // "smallest `last_command_age_secs` wins" ordering FFI hosts
    // use to replicate `pick_most_recent_commander`. Per
    // `CodeRabbit` round 7 on PR #402.
    let (has_last_command, last_command_op, last_command_age_secs) =
        if let Some((op, at)) = info.last_command {
            let age = snapshot_at.saturating_duration_since(at).as_secs_f64();
            (true, op as u8, age)
        } else {
            (false, 0u8, 0.0)
        };
    let mut out = SdrRtlTcpClientInfo {
        id: info.id,
        peer_addr: [0; SDR_RTLTCP_CLIENT_PEER_LEN],
        uptime_secs: snapshot_at
            .saturating_duration_since(info.connected_since)
            .as_secs_f64(),
        codec: info.codec.to_wire(),
        bytes_sent: info.bytes_sent,
        buffers_dropped: info.buffers_dropped,
        current_freq_hz: info.current_freq_hz.unwrap_or(0),
        current_sample_rate_hz: info.current_sample_rate_hz.unwrap_or(0),
        current_gain_tenths_db: info.current_gain_tenths_db.unwrap_or(0),
        current_gain_auto: info.current_gain_auto.unwrap_or(false),
        has_current_gain_value: info.current_gain_tenths_db.is_some(),
        has_current_gain_mode: info.current_gain_auto.is_some(),
        #[allow(
            clippy::cast_possible_truncation,
            reason = "recent_commands is capped at RECENT_COMMANDS_CAPACITY = 50"
        )]
        recent_commands_count: info.recent_commands.len() as u32,
        has_last_command,
        last_command_op,
        last_command_age_secs,
        role: info.role.to_wire(),
    };
    // Write peer_addr into the inline byte array, truncating at
    // `len - 1` to leave room for a NUL. Cast via &mut raw ptr
    // so `write_cstr`'s contract (null-or-writable buffer) is
    // honored uniformly with the other callers.
    let peer = info.peer.to_string();
    // SAFETY: `out.peer_addr` is a stack-local array of size
    // SDR_RTLTCP_CLIENT_PEER_LEN; taking a raw mut ptr to its
    // first element gives write access for the full length.
    unsafe {
        write_cstr(
            out.peer_addr.as_mut_ptr(),
            SDR_RTLTCP_CLIENT_PEER_LEN,
            &peer,
        );
    }
    out
}

fn recent_commands_to_json(info: &ClientInfo) -> Result<String, serde_json::Error> {
    use serde_json::json;
    let now = std::time::Instant::now();
    let entries: Vec<_> = info
        .recent_commands
        .iter()
        .map(|(op, t)| {
            json!({
                "op": command_op_label(*op),
                "seconds_ago": now.saturating_duration_since(*t).as_secs_f64(),
            })
        })
        .collect();
    // Propagate serialization failure to the caller. The FFI
    // layer surfaces it as `SDR_CORE_ERR_INTERNAL` — the
    // header advertises that error code for this path, so
    // collapsing the failure into `"[]"` would turn a real
    // ABI-contract violation into a plausible empty result.
    // Per `CodeRabbit` round 1 on PR #360.
    serde_json::to_string(&entries)
}

fn command_op_label(op: CommandOp) -> &'static str {
    // Surface the wire-command name as a display string. Stays
    // stable across sdr_server_rtltcp revisions because these
    // names are documented in rtl_tcp.c:315-372.
    match op {
        CommandOp::SetCenterFreq => "SetCenterFreq",
        CommandOp::SetSampleRate => "SetSampleRate",
        CommandOp::SetGainMode => "SetGainMode",
        CommandOp::SetTunerGain => "SetTunerGain",
        CommandOp::SetFreqCorrection => "SetFreqCorrection",
        CommandOp::SetIfGain => "SetIfGain",
        CommandOp::SetTestMode => "SetTestMode",
        CommandOp::SetAgcMode => "SetAgcMode",
        CommandOp::SetDirectSampling => "SetDirectSampling",
        CommandOp::SetOffsetTuning => "SetOffsetTuning",
        CommandOp::SetRtlXtal => "SetRtlXtal",
        CommandOp::SetTunerXtal => "SetTunerXtal",
        CommandOp::SetGainByIndex => "SetGainByIndex",
        CommandOp::SetBiasTee => "SetBiasTee",
    }
}

/// Copy `src` into the caller-provided buffer, truncating at
/// `len - 1` to leave room for a terminating NUL. No-op when
/// `buf` is null or `len == 0`.
///
/// # Safety
///
/// `buf` must either be null or point at a writable buffer of
/// at least `len` bytes.
unsafe fn write_cstr(buf: *mut c_char, len: usize, src: &str) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = src.as_bytes();
    // Leave one byte for the NUL terminator.
    let copy_len = bytes.len().min(len.saturating_sub(1));
    // SAFETY: caller contract + `copy_len < len`.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), buf, copy_len);
        *buf.add(copy_len) = 0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
