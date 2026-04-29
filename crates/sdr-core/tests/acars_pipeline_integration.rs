//! Headless ACARS controller integration test. Exercises the
//! full `SetAcarsEnabled` → engage → `AcarsEnabledChanged` round-trip
//! via the public `Engine` API, without spinning up a real
//! source thread or GTK loop.
//!
//! Approach: with no source attached (`state.source = None`),
//! the engage path's `if let Some(source) = ...` guards skip
//! every source-side mutation. `Frontend::set_decimation(1)` and
//! `ChannelBank::new(2.5MSps, 130.3375MHz, US-6)` succeed without
//! hardware, so the bank-instantiation lifecycle is observable
//! purely through `DspToUi` acks. End-to-end frame decoding is
//! covered by the sub-project 1 e2e test (`sdr-acars`); this
//! suite is purely about the controller-side wiring.
//!
//! NOTE: The `Engine::new()` constructor spawns a DSP thread
//! that immediately starts polling the command channel. We
//! send `SetAcarsEnabled(true)` then poll the receiver for
//! the corresponding `AcarsEnabledChanged(Ok(true))` ack
//! within a generous timeout, then send `SetAcarsEnabled(false)`
//! and poll for `AcarsEnabledChanged(Ok(false))`. The DSP
//! thread is dropped when the Engine is dropped at end-of-scope.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use sdr_core::Engine;
use sdr_core::messages::{DspToUi, UiToDsp};

/// Drain the receiver until we see an `AcarsEnabledChanged`
/// ack, or the timeout fires. Returns the inner `Result`
/// payload (Ok or Err) so callers can assert on the variant.
fn wait_for_acars_ack(
    rx: &std::sync::mpsc::Receiver<DspToUi>,
    timeout: Duration,
) -> Option<Result<bool, sdr_core::acars_airband_lock::AcarsEnableError>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // ignore non-ACARS messages and recv_timeout errors; keep polling
        if let Ok(DspToUi::AcarsEnabledChanged(result)) = rx.recv_timeout(Duration::from_millis(50))
        {
            return Some(result);
        }
    }
    None
}

#[test]
fn engage_disengage_round_trip_emits_acks() {
    let engine = Engine::new(PathBuf::new()).expect("engine should spawn");
    let rx = engine
        .subscribe()
        .expect("first subscribe should return Some");
    let cmd = engine.command_sender();

    // Engage.
    cmd.send(UiToDsp::SetAcarsEnabled(true))
        .expect("send SetAcarsEnabled(true)");
    let engage_ack = wait_for_acars_ack(&rx, Duration::from_secs(2))
        .expect("engage ack should arrive within 2 seconds");
    assert!(
        matches!(engage_ack, Ok(true)),
        "expected Ok(true) engage ack, got {engage_ack:?}"
    );

    // Disengage.
    cmd.send(UiToDsp::SetAcarsEnabled(false))
        .expect("send SetAcarsEnabled(false)");
    let disengage_ack = wait_for_acars_ack(&rx, Duration::from_secs(2))
        .expect("disengage ack should arrive within 2 seconds");
    assert!(
        matches!(disengage_ack, Ok(false)),
        "expected Ok(false) disengage ack, got {disengage_ack:?}"
    );
}

#[test]
fn double_engage_is_idempotent() {
    let engine = Engine::new(PathBuf::new()).expect("engine should spawn");
    let rx = engine.subscribe().expect("subscribe");
    let cmd = engine.command_sender();

    cmd.send(UiToDsp::SetAcarsEnabled(true)).expect("send");
    let first = wait_for_acars_ack(&rx, Duration::from_secs(2)).expect("first ack");
    assert!(matches!(first, Ok(true)));

    // Second engage while already on: spec says idempotent —
    // controller emits Ok(true) ack without re-engaging.
    cmd.send(UiToDsp::SetAcarsEnabled(true)).expect("send");
    let second = wait_for_acars_ack(&rx, Duration::from_secs(2)).expect("second ack");
    assert!(matches!(second, Ok(true)));
}

#[test]
fn disengage_when_off_is_idempotent() {
    let engine = Engine::new(PathBuf::new()).expect("engine should spawn");
    let rx = engine.subscribe().expect("subscribe");
    let cmd = engine.command_sender();

    // Disengage without prior engage — should still ack Ok(false).
    cmd.send(UiToDsp::SetAcarsEnabled(false)).expect("send");
    let ack = wait_for_acars_ack(&rx, Duration::from_secs(2)).expect("ack");
    assert!(matches!(ack, Ok(false)));
}
