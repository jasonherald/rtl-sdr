//! The `rtl_tcp` client's data pump (issue #818): the blocking read
//! loop with the consecutive-timeout stall detector, the bounded
//! drop-oldest receive-buffer append, and end-of-session teardown.
//! Split out of `rtl_tcp.rs` per the Codacy 500-NLOC file gate —
//! behavior is unchanged.

use std::collections::VecDeque;
use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sdr_server_rtltcp::codec::{Codec, Decoder};
use sdr_types::SourceError;

use super::manager::clear_pending_stream;
use super::{RECV_CHUNK_BYTES, RX_BUFFER_SOFT_CAP_BYTES, RtlTcpConfig, SharedState};

/// Append `chunk` to `rx`, dropping the oldest bytes if doing so would
/// exceed [`RX_BUFFER_SOFT_CAP_BYTES`]. Returns the number of bytes
/// dropped so the caller can surface it through observability.
///
/// Drop count is rounded up to an even number so the buffer always
/// stays aligned on I/Q pair boundaries. Dropping an odd number of
/// bytes would leave `rx` starting mid-pair — subsequent `read_samples`
/// calls would then pair `Q[n]` with `I[n+1]`, phase-shifting the
/// stream until another odd drop happened to realign it.
pub(super) fn append_with_cap_inner(rx: &mut VecDeque<u8>, chunk: &[u8]) -> usize {
    let desired_total = rx.len().saturating_add(chunk.len());
    let raw_excess = desired_total.saturating_sub(RX_BUFFER_SOFT_CAP_BYTES);
    // Round up to even so we never split an I/Q pair.
    let total_drop = raw_excess.saturating_add(raw_excess & 1);

    let drop_from_rx = total_drop.min(rx.len());
    rx.drain(..drop_from_rx);

    let drop_from_chunk = total_drop.saturating_sub(drop_from_rx).min(chunk.len());
    rx.extend(chunk[drop_from_chunk..].iter().copied());
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
pub(super) fn append_with_cap_to_shared(shared: &SharedState, rx: &mut VecDeque<u8>, chunk: &[u8]) {
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

pub(super) fn run_data_pump(
    stream: TcpStream,
    codec: Codec,
    shared: &Arc<SharedState>,
    config: &RtlTcpConfig,
) -> Result<(), SourceError> {
    // Wrap the TCP stream in the negotiated decoder. Legacy /
    // vanilla-server paths hit `Codec::None` which is a
    // zero-overhead pass-through; only LZ4 connections pay the
    // framing cost.
    //
    // Read timeout was installed in `attempt_connect` on the
    // underlying TcpStream; `Decoder::Lz4` delegates its `read()`
    // to the inner stream so `SO_RCVTIMEO` still fires. BUT a framed
    // decoder cannot resume after a timeout: its `read_exact` has
    // already consumed part of a header/body when `SO_RCVTIMEO`
    // returns, so retrying on the same decoder restarts mid-frame and
    // surfaces as terminal `InvalidData`. For framed codecs the first
    // timeout is therefore a teardown-and-reconnect (#743); only the
    // raw pass-through tolerates `max_consecutive_timeouts`.
    let tolerated_timeouts = if codec == Codec::None {
        config.max_consecutive_timeouts
    } else {
        1
    };
    let mut reader = Decoder::new(codec, stream);
    let mut buf = [0u8; RECV_CHUNK_BYTES];
    let mut consecutive_timeouts: u32 = 0;
    // Default Ok path: any break out of the loop (EOF, stall,
    // generic socket error) is a reconnect-worthy dropout, not
    // a terminal failure. Only explicit `return Err(...)` below
    // for LZ4 decode corruption escapes as terminal.
    let mut outcome: Result<(), SourceError> = Ok(());
    while !shared.shutdown.load(Ordering::Relaxed) {
        match reader.read(&mut buf) {
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
                if consecutive_timeouts >= tolerated_timeouts {
                    tracing::info!(
                        consecutive_timeouts,
                        "rtl_tcp stream stalled, breaking out for reconnect"
                    );
                    break;
                }
            }
            Err(e) if codec != Codec::None && e.kind() == std::io::ErrorKind::InvalidData => {
                // LZ4 frame corruption mid-stream — either a codec
                // mismatch we negotiated wrong (server lied about
                // capability) or an on-the-wire bit flip under a
                // transport that doesn't guarantee integrity. The
                // stream state is unrecoverable: the next read
                // would start mid-block, so every subsequent
                // reconnect would hit the same corruption.
                // Surface as `SourceError::Protocol` so the
                // connection manager routes to terminal
                // `ConnectionState::Failed` instead of spinning
                // on the backoff schedule forever. Per CodeRabbit
                // round 5 on PR #399.
                outcome = Err(SourceError::Protocol(format!(
                    "rtl_tcp {codec} decode failed mid-stream (unrecoverable): {e}"
                )));
                break;
            }
            Err(e) => {
                tracing::info!(%e, "rtl_tcp socket read failed, will reconnect");
                break;
            }
        }
    }

    end_session(shared);
    outcome
}

/// Tear down the per-session state when the data pump exits: drop the
/// command sink so later `send_command` calls stop writing into a dead
/// stream (and the session cancel handle with it), clear any buffered
/// I/Q so the next session doesn't rewind the consumer with pre-drop
/// samples, and rearm the edge-triggered overflow warning so a stall in
/// the next session logs again.
pub(super) fn end_session(shared: &SharedState) {
    if let Ok(mut sink) = shared.command_sink.lock() {
        *sink = None;
    }
    clear_pending_stream(shared);
    if let Ok(mut rx) = shared.rx_buf.lock() {
        rx.clear();
    }
    shared.rx_in_overflow.store(false, Ordering::Relaxed);
}
