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

mod accept;
mod broadcast;
mod client;
mod config;

pub use config::{InitialDeviceState, Server, ServerConfig, ServerStats, TunerAdvertiseInfo};

use std::time::Duration;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
