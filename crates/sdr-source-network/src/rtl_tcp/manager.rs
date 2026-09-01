//! The `rtl_tcp` client's reconnect / backoff state machine (issue
//! #818): the connection-manager thread body, the exponential
//! backoff schedule walk, shutdown-aware sleeping, sticky-command
//! replay on reconnect, and the shared-state mutators the lifecycle
//! paths use. Split out of `rtl_tcp.rs` per the Codacy 500-NLOC
//! file gate — behavior is unchanged.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use sdr_server_rtltcp::protocol::{Command, CommandOp};
use sdr_types::SourceError;

use super::handshake::{HandshakeOutcome, attempt_connect};
use super::pump::run_data_pump;
use super::{
    BACKOFF_SCHEDULE_SECS, ConnectionState, IF_GAIN_STAGES, RETRY_SLEEP_STEP, RtlTcpConfig,
    SharedState,
};

/// Background thread body: reconnect loop + data-read pump.
pub(super) fn connection_manager(
    host: String,
    port: u16,
    shared: Arc<SharedState>,
    config: RtlTcpConfig,
) {
    let mut attempt: u32 = 0;

    while !shared.shutdown.load(Ordering::Relaxed) {
        set_state(&shared, ConnectionState::Connecting);

        match attempt_connect(&host, port, &shared, &config) {
            Ok(HandshakeOutcome { stream, codec }) => {
                attempt = 0;
                // At this point handshake has completed successfully.
                replay_sticky_commands(&shared);
                // `run_data_pump` returns `Ok(())` on a normal
                // transient dropout (EOF, stall, generic socket
                // error — all reconnect-worthy) and
                // `Err(SourceError::Protocol(_))` on unrecoverable
                // stream corruption (LZ4 mid-stream decode
                // failure — the next reconnect would hit the same
                // issue). Terminal errors route to `Failed` the
                // same way a non-recoverable `attempt_connect`
                // error does; transient errors fall through to
                // the reconnect-with-backoff loop below. Per
                // CodeRabbit round 5 on PR #399.
                if let Err(e) = run_data_pump(stream, codec, &shared, &config) {
                    tracing::warn!(%e, "rtl_tcp data pump terminated with non-recoverable error");
                    set_state(
                        &shared,
                        ConnectionState::Failed {
                            reason: format!("{e}"),
                        },
                    );
                    return;
                }
                // run_data_pump returned Ok — connection dropped transiently.
            }
            Err(e) => {
                // A failed handshake drops its stream; remove its clone.
                // Successful sessions keep it as the cancel handle until
                // `run_data_pump` clears it at session end.
                clear_pending_stream(&shared);
                tracing::warn!(%e, host = %host, port, attempt, "rtl_tcp connect failed");
                if route_terminal_connect_error(&shared, &e) {
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

// Route each terminal error-kind to its
// dedicated `ConnectionState` variant so the UI
// can offer specific recovery actions (take-
// control, enter key, re-prompt) instead of a
// generic "Failed" with opaque reason string.
// Pre-#396 AuthRequired/AuthFailed folded into
// `Failed { reason: "protocol error: ..." }` and
// `ControllerBusy` auto-retried via
// `TemporarilyUnavailable`. Per #396, each gets
// its own terminal state.
/// `true` = the error is terminal and the manager thread must exit
/// (the terminal `ConnectionState` has already been published);
/// `false` = transient, fall through to the backoff loop. Split out
/// of [`connection_manager`] per the 50-NLOC gate (PR #880 Codacy
/// precedent).
fn route_terminal_connect_error(shared: &Arc<SharedState>, e: &SourceError) -> bool {
    match e {
        SourceError::ControllerBusy => {
            set_state(shared, ConnectionState::ControllerBusy);
            true
        }
        SourceError::AuthRequired => {
            set_state(shared, ConnectionState::AuthRequired);
            true
        }
        SourceError::AuthFailed => {
            set_state(shared, ConnectionState::AuthFailed);
            true
        }
        SourceError::Protocol(_) => {
            // Non-recoverable: server isn't speaking
            // rtl_tcp, or the extended handshake was
            // rejected for a reason we don't have a
            // dedicated state for (ListenerCapReached,
            // parse errors, future status codes).
            set_state(
                shared,
                ConnectionState::Failed {
                    reason: format!("{e}"),
                },
            );
            true
        }
        // `TemporarilyUnavailable` (transient network
        // conditions the caller wants us to back off
        // and retry on — NOT role denials, which are
        // now their own variants above) and every
        // other `SourceError` variant fall through to
        // the backoff loop below.
        _ => false,
    }
}

/// The sticky-op replay table: one `(op, last-value slot)` pair per
/// single-slot stateful command, in opcode order. (`SetIfGain` is
/// per-stage and spliced in by the caller.) Split out of
/// [`replay_sticky_commands`] per the 50-NLOC gate (PR #880 Codacy
/// precedent).
fn sticky_op_table(shared: &SharedState) -> [(CommandOp, &std::sync::atomic::AtomicU32); 13] {
    [
        (CommandOp::SetCenterFreq, &shared.last_center_freq_hz),
        (CommandOp::SetSampleRate, &shared.last_sample_rate_hz),
        (CommandOp::SetGainMode, &shared.last_gain_mode),
        (CommandOp::SetTunerGain, &shared.last_tuner_gain),
        (CommandOp::SetFreqCorrection, &shared.last_ppm),
        (CommandOp::SetTestMode, &shared.last_testmode),
        (CommandOp::SetAgcMode, &shared.last_agc_mode),
        (CommandOp::SetDirectSampling, &shared.last_direct_sampling),
        (CommandOp::SetOffsetTuning, &shared.last_offset_tuning),
        (CommandOp::SetRtlXtal, &shared.last_rtl_xtal),
        (CommandOp::SetTunerXtal, &shared.last_tuner_xtal),
        (CommandOp::SetGainByIndex, &shared.last_gain_by_index),
        (CommandOp::SetBiasTee, &shared.last_bias_tee),
    ]
}

fn replay_sticky_commands(shared: &Arc<SharedState>) {
    let Ok(mut sink) = shared.command_sink.lock() else {
        return;
    };
    let Some(stream) = sink.as_mut() else {
        return;
    };
    // Snapshot the masks only while holding the sink lock: setters record
    // under the same lock, so a gain written while replay waited for it
    // cannot leave a stale sibling in the mask we act on (CR round 3 on
    // PR #792).
    let mask = shared.replay_mask.load(Ordering::Relaxed);
    let replay_bit = |bit: u32| mask & (1u32 << bit) != 0;

    let if_gain_mask = shared.if_gain_mask.load(Ordering::Relaxed);
    let if_gain_ops = (0..IF_GAIN_STAGES)
        .filter(|stage_idx| if_gain_mask & (1u32 << stage_idx) != 0)
        .map(|stage_idx| (CommandOp::SetIfGain, &shared.last_if_gain[stage_idx]));
    let ops = sticky_op_table(shared);
    // IF gain stages go where `SetIfGain` sits in opcode order — before
    // the first op with a higher opcode — one command per recorded
    // stage. Derived from the table so a reorder cannot silently move
    // the insertion point.
    let if_gain_at = ops
        .iter()
        .position(|(op, _)| (*op as u8) > (CommandOp::SetIfGain as u8))
        .unwrap_or(ops.len());
    let (head, tail) = ops.split_at(if_gain_at);
    for (op, slot) in head
        .iter()
        .copied()
        .chain(if_gain_ops)
        .chain(tail.iter().copied())
    {
        let bit = u32::from((op as u8) - 1);
        if !replay_bit(bit) {
            continue;
        }
        let cmd = Command {
            op,
            param: slot.load(Ordering::Relaxed),
        };
        if let Err(e) = stream.write_all(&cmd.to_bytes()) {
            tracing::debug!(%e, op = ?op, "replay write failed — tearing down session to force reconnect");
            // Same pattern as `send_command`: full shutdown so
            // `run_data_pump` breaks out immediately instead of
            // continuing to pump on a half-dead socket. Otherwise the
            // replay would partially land on the wire, leaving server
            // state desynced from the client's view of it.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            *sink = None;
            return;
        }
    }
}

pub(super) fn backoff_delay(attempt: u32) -> Duration {
    let idx = (attempt as usize).min(BACKOFF_SCHEDULE_SECS.len() - 1);
    Duration::from_secs(BACKOFF_SCHEDULE_SECS[idx])
}

fn sleep_until(deadline: Instant, shutdown: &AtomicBool) {
    let step = RETRY_SLEEP_STEP;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(step.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub(super) fn clear_pending_stream(shared: &SharedState) {
    if let Ok(mut pending) = shared.pending_stream.lock() {
        *pending = None;
    }
}

pub(super) fn set_state(shared: &Arc<SharedState>, state: ConnectionState) {
    if let Ok(mut s) = shared.state.lock() {
        *s = state;
    }
}
