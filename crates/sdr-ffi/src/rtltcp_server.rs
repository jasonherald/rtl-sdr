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
    InitialDeviceState, Server, ServerConfig, ServerError, ServerStats, TunerAdvertiseInfo,
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
}

/// Live server statistics snapshot.
///
/// `connected_client_addr` and `tuner_name` are filled into
/// caller-allocated buffers handed to `sdr_rtltcp_server_stats`;
/// the struct here carries only scalar fields.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SdrRtlTcpServerStats {
    /// `true` while a client is connected (also implies
    /// `uptime_secs` / `bytes_sent` are meaningful).
    pub has_client: bool,
    /// Seconds since the current client connected. 0 when
    /// `has_client == false`.
    pub uptime_secs: f64,
    /// Bytes streamed to the client this session.
    pub bytes_sent: u64,
    /// Buffer-drop count this session.
    pub buffers_dropped: u64,
    /// **Client-issued** center frequency override in Hz —
    /// what the connected client most recently requested via
    /// `SetCenterFreq`. 0 before the client has issued the
    /// command; reset on client disconnect. Does **not**
    /// reflect the server's configured `initial_freq_hz` or
    /// the live device register — hosts that want "what the
    /// dongle is actually tuned to" should combine this with
    /// `SdrRtlTcpServerConfig::initial_freq_hz` when
    /// `has_client && current_freq_hz == 0`.
    pub current_freq_hz: u32,
    /// **Client-issued** sample-rate override, same semantics
    /// as `current_freq_hz` above (0 until the client sets it;
    /// resets on disconnect; doesn't reflect the applied
    /// initial).
    pub current_sample_rate_hz: u32,
    /// Most recent client-issued tuner-gain request in 0.1 dB.
    /// Valid only when `has_current_gain_value == true` — a
    /// zero value here is ambiguous otherwise (could mean
    /// "zero-dB manual gain" or "client never set gain").
    pub current_gain_tenths_db: i32,
    /// `true` when the client's last gain-mode request was
    /// auto; `false` when manual. Valid only when
    /// `has_current_gain_mode == true`.
    pub current_gain_auto: bool,
    /// `true` once the client has issued at least one
    /// `SetTunerGain` command this session. The two gain
    /// validity bits are tracked independently because a
    /// client can send `SetGainMode(auto)` without a preceding
    /// `SetTunerGain` (and vice versa). Per `CodeRabbit`
    /// round 7 on PR #360.
    pub has_current_gain_value: bool,
    /// `true` once the client has issued at least one
    /// `SetGainMode` command this session. Valid companion to
    /// `current_gain_auto` — without this flag a `false`
    /// value would be indistinguishable from "client hasn't
    /// asked for a gain mode yet."
    pub has_current_gain_mode: bool,
    /// Tuner's advertised discrete gain count (from
    /// `dongle_info_t`). Populated by `Server::start` during
    /// the dongle-open phase — non-zero for the entire server
    /// lifetime, including before the first client connects.
    pub gain_count: u32,
    /// Number of entries in the recent-commands ring — lets
    /// hosts size a follow-up `_recent_commands_json` buffer.
    pub recent_commands_count: u32,
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
        let server_cfg = ServerConfig {
            bind,
            device_index: cfg.device_index,
            initial,
            buffer_capacity,
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

/// Snapshot the server's live stats into a caller-provided
/// struct + string buffers.
///
/// `out_stats` must be non-null. `out_client_addr` and
/// `out_tuner_name` are NUL-terminated on success; pass
/// `NULL` buffers (with corresponding `*_len = 0`) to skip.
/// Truncation is not an error — the result is NUL-terminated
/// at `*_len - 1` when the source string is longer.
///
/// # Safety
///
/// Pointers must either be null or point at writable buffers
/// with the indicated capacities.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_stats(
    handle: *mut SdrRtlTcpServer,
    out_stats: *mut SdrRtlTcpServerStats,
    out_client_addr: *mut c_char,
    client_addr_len: usize,
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

        // Copy the string fields into caller buffers (if
        // provided). Truncate cleanly at `len - 1`.
        let client_str = stats
            .connected_client
            .map_or_else(String::new, |addr| addr.to_string());
        // SAFETY: caller contract on out_client_addr / len.
        unsafe { write_cstr(out_client_addr, client_addr_len, &client_str) };
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

/// Write the recent-commands ring to `out_buf` as a JSON array.
/// Each entry is `{"op": "<name>", "seconds_ago": <f64>}`.
/// The wire-level 4-byte command param is not in the upstream
/// `ServerStats::recent_commands` payload (only the opcode +
/// dispatch time are captured), so the param field is not
/// surfaced here — matches the contract in `include/sdr_core.h`.
///
/// On success returns `SDR_CORE_OK` and NUL-terminates the
/// buffer. On too-small buffer returns `SDR_CORE_ERR_INVALID_ARG`
/// and writes the required size (including NUL) to `*out_required`
/// when non-null. On stopped server returns `NOT_RUNNING`.
///
/// # Safety
///
/// `out_buf` must point at a writable buffer of at least
/// `buf_len` bytes. `out_required` is optional (pass null to
/// skip).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdr_rtltcp_server_recent_commands_json(
    handle: *mut SdrRtlTcpServer,
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
        let json = match recent_commands_to_json(&stats) {
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
    let uptime_secs = stats
        .connected_since
        .map_or(0.0, |t| t.elapsed().as_secs_f64());
    SdrRtlTcpServerStats {
        has_client: stats.connected_client.is_some(),
        uptime_secs,
        bytes_sent: stats.bytes_sent,
        buffers_dropped: stats.buffers_dropped,
        current_freq_hz: stats.current_freq_hz.unwrap_or(0),
        current_sample_rate_hz: stats.current_sample_rate_hz.unwrap_or(0),
        current_gain_tenths_db: stats.current_gain_tenths_db.unwrap_or(0),
        current_gain_auto: stats.current_gain_auto.unwrap_or(false),
        has_current_gain_value: stats.current_gain_tenths_db.is_some(),
        has_current_gain_mode: stats.current_gain_auto.is_some(),
        gain_count: tuner.gain_count,
        #[allow(
            clippy::cast_possible_truncation,
            reason = "recent_commands is capped at RECENT_COMMANDS_CAPACITY = 50"
        )]
        recent_commands_count: stats.recent_commands.len() as u32,
    }
}

fn recent_commands_to_json(stats: &ServerStats) -> Result<String, serde_json::Error> {
    use serde_json::json;
    let now = std::time::Instant::now();
    let entries: Vec<_> = stats
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
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::time::Duration;

    // --------------------------------------------------------
    //  Shared test fixtures — per `CodeRabbit` round 5 on
    //  PR #360. Repeated literals across the `initial_from_c`
    //  and `bind_socket_addr` tests funnel through these.
    // --------------------------------------------------------

    /// TCP port used in happy-path configs. Matches the
    /// `sdr_server_rtltcp::DEFAULT_PORT` convention (1234).
    const TEST_PORT: u16 = 1234;

    /// Second test port used to prove `bind_socket_addr` honors
    /// the caller-supplied value on the all-interfaces path.
    const TEST_ALT_PORT: u16 = 9000;

    /// Default center frequency in Hz — 100 MHz WFM band.
    const TEST_FREQ_HZ: u32 = 100_000_000;

    /// Default sample rate — 2.048 Msps (canonical RTL-SDR
    /// value that doesn't starve the USB controller).
    const TEST_SAMPLE_RATE_HZ: u32 = 2_048_000;

    /// Non-zero tuner gain in tenths of dB. 256 = 25.6 dB —
    /// well inside the R820T's table so the
    /// "auto vs manual" gain-round-trip assertion has a value
    /// that's unambiguously "manual."
    const TEST_NONZERO_GAIN_TENTHS: i32 = 256;

    /// R820T discrete gain-step count, used by the stats
    /// fixture when constructing a synthetic
    /// `TunerAdvertiseInfo`.
    const TEST_TUNER_GAIN_COUNT: u32 = 29;

    /// Sentinel that trips the direct-sampling validation.
    const TEST_INVALID_DIRECT_SAMPLING: i32 = 3;

    /// Device index = 0 matches the first attached dongle.
    const TEST_DEVICE_INDEX: u32 = 0;

    /// Build a happy-path `SdrRtlTcpServerConfig`. Tests tweak
    /// a single field to target one validation branch at a
    /// time — that way a future schema addition lands in one
    /// place instead of N tests.
    fn base_test_config() -> SdrRtlTcpServerConfig {
        SdrRtlTcpServerConfig {
            bind_address: SDR_BIND_LOOPBACK,
            port: TEST_PORT,
            device_index: TEST_DEVICE_INDEX,
            buffer_capacity: 0,
            initial_freq_hz: TEST_FREQ_HZ,
            initial_sample_rate_hz: TEST_SAMPLE_RATE_HZ,
            initial_gain_tenths_db: 0,
            initial_ppm: 0,
            initial_bias_tee: false,
            initial_direct_sampling: 0,
        }
    }

    #[test]
    fn bind_socket_addr_loopback() {
        let addr = bind_socket_addr(SDR_BIND_LOOPBACK, TEST_PORT).unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), TEST_PORT);
    }

    #[test]
    fn bind_socket_addr_all_interfaces() {
        let addr = bind_socket_addr(SDR_BIND_ALL_INTERFACES, TEST_ALT_PORT).unwrap();
        assert_eq!(addr.ip().to_string(), "0.0.0.0");
        assert_eq!(addr.port(), TEST_ALT_PORT);
    }

    #[test]
    fn bind_socket_addr_rejects_unknown() {
        assert!(bind_socket_addr(99, TEST_PORT).is_err());
    }

    #[test]
    fn bind_socket_addr_zero_port_uses_default() {
        let addr = bind_socket_addr(SDR_BIND_LOOPBACK, 0).unwrap();
        assert_eq!(addr.port(), DEFAULT_PORT);
    }

    #[test]
    fn initial_from_c_rejects_zero_sample_rate() {
        // Pins the guard added in round 2 per `CodeRabbit` —
        // a zero-init `SdrRtlTcpServerConfig` must not slip
        // through and wedge the RTL-SDR USB controller.
        let mut cfg = base_test_config();
        cfg.initial_sample_rate_hz = 0;
        assert!(initial_from_c(&cfg).is_err());
    }

    #[test]
    fn initial_from_c_rejects_out_of_range_direct_sampling() {
        let mut cfg = base_test_config();
        cfg.initial_direct_sampling = TEST_INVALID_DIRECT_SAMPLING;
        assert!(initial_from_c(&cfg).is_err());
    }

    #[test]
    fn initial_from_c_zero_gain_maps_to_auto() {
        let cfg = base_test_config();
        let initial = initial_from_c(&cfg).unwrap();
        assert_eq!(initial.gain_tenths_db, None);
    }

    #[test]
    fn initial_from_c_nonzero_gain_preserved() {
        let mut cfg = base_test_config();
        cfg.initial_gain_tenths_db = TEST_NONZERO_GAIN_TENTHS;
        let initial = initial_from_c(&cfg).unwrap();
        assert_eq!(initial.gain_tenths_db, Some(TEST_NONZERO_GAIN_TENTHS));
    }

    #[test]
    fn start_rejects_oversized_buffer_capacity() {
        // Pins the MAX_BUFFER_CAPACITY guard added in round 9
        // per `CodeRabbit`. Construct options that would
        // otherwise pass every earlier check (loopback bind,
        // valid port, valid direct-sampling, non-zero sample
        // rate) and drive `buffer_capacity` past the cap —
        // `_start` must return `InvalidArg` before touching
        // the device layer.
        let opts = SdrRtlTcpServerConfig {
            bind_address: SDR_BIND_LOOPBACK,
            port: TEST_PORT,
            device_index: TEST_DEVICE_INDEX,
            buffer_capacity: SDR_RTLTCP_SERVER_MAX_BUFFER_CAPACITY + 1,
            initial_freq_hz: TEST_FREQ_HZ,
            initial_sample_rate_hz: TEST_SAMPLE_RATE_HZ,
            initial_gain_tenths_db: 0,
            initial_ppm: 0,
            initial_bias_tee: false,
            initial_direct_sampling: 0,
        };
        let mut handle: *mut SdrRtlTcpServer = std::ptr::null_mut();
        let rc = unsafe { sdr_rtltcp_server_start(&raw const opts, &raw mut handle) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
        assert!(handle.is_null());
    }

    #[test]
    fn start_with_null_pointers_returns_invalid_arg() {
        let rc = unsafe { sdr_rtltcp_server_start(std::ptr::null(), std::ptr::null_mut()) };
        assert_eq!(rc, SdrCoreError::InvalidArg.as_int());
    }

    #[test]
    fn stats_with_null_handle_returns_invalid_handle() {
        let mut stats = SdrRtlTcpServerStats::default();
        let rc = unsafe {
            sdr_rtltcp_server_stats(
                std::ptr::null_mut(),
                &raw mut stats,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(rc, SdrCoreError::InvalidHandle.as_int());
    }

    #[test]
    fn stop_handles_null_gracefully() {
        // No crash, no panic.
        unsafe { sdr_rtltcp_server_stop(std::ptr::null_mut()) };
    }

    #[test]
    fn stats_to_c_preserves_independent_gain_validity() {
        // Four-state matrix for the two gain Options on
        // `ServerStats`. Pins the "don't collapse into a
        // single `has_current_gain` bit" behavior landed in
        // round 7. Per `CodeRabbit` round 8 on PR #360.
        let tuner = TunerAdvertiseInfo {
            name: "R820T".into(),
            gain_count: TEST_TUNER_GAIN_COUNT,
        };

        // (None, None) → neither set (Default leaves both at None).
        let mut stats = ServerStats::default();
        let c = stats_to_c(&stats, &tuner);
        assert!(!c.has_current_gain_value);
        assert!(!c.has_current_gain_mode);

        // (Some(v), None) → value set, mode unknown
        stats.current_gain_tenths_db = Some(TEST_NONZERO_GAIN_TENTHS);
        stats.current_gain_auto = None;
        let c = stats_to_c(&stats, &tuner);
        assert!(c.has_current_gain_value);
        assert!(!c.has_current_gain_mode);
        assert_eq!(c.current_gain_tenths_db, TEST_NONZERO_GAIN_TENTHS);
        assert!(!c.current_gain_auto);

        // (None, Some(auto)) → mode set, value unknown
        stats.current_gain_tenths_db = None;
        stats.current_gain_auto = Some(true);
        let c = stats_to_c(&stats, &tuner);
        assert!(!c.has_current_gain_value);
        assert!(c.has_current_gain_mode);
        assert!(c.current_gain_auto);

        // (Some(v), Some(manual)) → both set, explicit manual
        stats.current_gain_tenths_db = Some(TEST_NONZERO_GAIN_TENTHS);
        stats.current_gain_auto = Some(false);
        let c = stats_to_c(&stats, &tuner);
        assert!(c.has_current_gain_value);
        assert!(c.has_current_gain_mode);
        assert_eq!(c.current_gain_tenths_db, TEST_NONZERO_GAIN_TENTHS);
        assert!(!c.current_gain_auto);
        assert_eq!(c.gain_count, TEST_TUNER_GAIN_COUNT);
    }

    #[test]
    fn has_stopped_null_handle_returns_true() {
        assert!(unsafe { sdr_rtltcp_server_has_stopped(std::ptr::null_mut()) });
    }

    /// Sentinel byte the test buffers fill with — any non-NUL
    /// value works; using the same constant avoids the
    /// `u8 as c_char` cast-wrap lint.
    const FILL_BYTE: c_char = 0x78; // ASCII 'x'

    #[test]
    fn write_cstr_truncates_without_overflow() {
        let mut buf = [FILL_BYTE; 5];
        // SAFETY: buffer is owned locally.
        unsafe { write_cstr(buf.as_mut_ptr(), buf.len(), "hello world") };
        // Expect "hell\0"
        assert_eq!(buf[4], 0);
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert_eq!(s, "hell");
    }

    #[test]
    fn write_cstr_null_or_zero_len_is_noop() {
        unsafe { write_cstr(std::ptr::null_mut(), 0, "hi") };
        unsafe { write_cstr(std::ptr::null_mut(), 5, "hi") };
        let mut buf = [FILL_BYTE; 1];
        unsafe { write_cstr(buf.as_mut_ptr(), 0, "hi") };
        assert_eq!(buf[0], FILL_BYTE);
    }

    #[test]
    fn recent_commands_json_empty_when_no_commands() {
        let stats = ServerStats::default();
        let json = recent_commands_to_json(&stats).expect("serialize empty ring");
        assert_eq!(json, "[]");
    }

    #[test]
    fn recent_commands_json_entries_shape() {
        let mut stats = ServerStats::default();
        stats
            .recent_commands
            .push_back((CommandOp::SetCenterFreq, std::time::Instant::now()));
        stats.recent_commands.push_back((
            CommandOp::SetBiasTee,
            std::time::Instant::now()
                .checked_sub(Duration::from_secs(3))
                .expect("Instant::now - 3s is representable"),
        ));
        let json = recent_commands_to_json(&stats).expect("serialize populated ring");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["op"], "SetCenterFreq");
        assert_eq!(arr[1]["op"], "SetBiasTee");
        let seconds_ago = arr[1]["seconds_ago"].as_f64().unwrap();
        assert!(seconds_ago >= 3.0, "expected >=3s, got {seconds_ago}");
    }
}
