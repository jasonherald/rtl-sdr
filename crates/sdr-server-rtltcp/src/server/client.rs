//! Per-client worker halves: the TCP writer chain
//! ([`StatsTrackingWrite`] → encoder → [`tcp_writer`], with the #709
//! stall-riding [`write_chunk_shedding_backlog`]) and the command
//! reader ([`command_worker`] + [`read_full`]). One writer + one
//! command thread per connected client. Split out of `server.rs` per
//! the file-size refactor (#818); pure moves, no behavior change.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use librtlsdr_rs::RtlSdrDevice;

use crate::broadcaster::{ClientRegistry, ClientSlot};
use crate::dispatch::dispatch;
use crate::extension::Role;
use crate::protocol::{COMMAND_LEN, Command, CommandOp};

use super::{COMMAND_READ_TIMEOUT, MAX_CONSECUTIVE_WRITE_STALLS, WRITER_RECV_TIMEOUT};

/// `Write` adapter sitting between the negotiated `Encoder` and the
/// raw `TcpStream`. Updates the slot's per-client `bytes_sent`
/// counter AND the registry's aggregate `total_bytes_sent` with
/// the on-wire (post-compression) byte count from each successful
/// write. Counting at this layer (not inside `ClientRegistry::broadcast`)
/// means the aggregate and per-client counters never diverge and
/// both reflect bytes that actually reached the socket. Per
/// CodeRabbit round 1 on PR #402.
pub(super) struct StatsTrackingWrite {
    pub(super) inner: TcpStream,
    pub(super) slot: Arc<ClientSlot>,
    pub(super) registry: Arc<ClientRegistry>,
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

/// Per-client writer loop: drains the client's bounded channel into
/// the (possibly codec-wrapped) socket until shutdown, disconnection,
/// or a terminal write outcome. One thread per connected client.
pub(super) fn tcp_writer<W: Write + Send>(
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
pub(super) enum ChunkOutcome {
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
pub(super) fn write_chunk_shedding_backlog<W: Write>(
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

/// Per-client command loop: reads 5-byte command frames from the
/// client's socket and dispatches them to the shared device mutex,
/// enforcing the #392 listener role gate. One thread per client.
pub(super) fn command_worker(
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
