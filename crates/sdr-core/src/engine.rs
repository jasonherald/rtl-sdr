//! Public engine facade.
//!
//! [`Engine`] owns the DSP controller thread and exposes a single Rust API
//! consumed by both the GTK UI (today) and the planned macOS `SwiftUI` app
//! (via `sdr-ffi`, M2). Neither consumer touches the DSP thread or the
//! channel internals directly — they go through `Engine`.
//!
//! ## Design notes
//!
//! - **Send + Sync**: consumers can hold the engine in an [`std::sync::Arc`]
//!   and dispatch commands from any thread. The event receiver is wrapped
//!   in `Mutex<Option<_>>` so that [`std::sync::mpsc::Receiver`] (which is
//!   `Send` but **not** `Sync`) can sit on a `Sync` type. The wrapping
//!   `Option` lets [`Engine::subscribe`] take the receiver out exactly once.
//!
//! - **One-shot subscription**: events are channel-based for backwards
//!   compatibility with the GTK pump. The first call to `subscribe` returns
//!   `Some(_)`; every subsequent call returns `None`. Multi-consumer fan-out
//!   is out of scope — both real consumers (GTK and `sdr-ffi`) live in
//!   different processes / build configurations and never coexist.
//!
//! - **FFT delivery is pull-based**: the DSP thread writes new frames into
//!   a [`SharedFftBuffer`] and the consumer drains via [`Engine::pull_fft`]
//!   from its render tick. This is identical to how `sdr-ui::dsp_controller`
//!   already worked — it just lives here now.
//!
//! - **GTK migration helpers**: in addition to the spec'd `send_command` /
//!   `subscribe` / `pull_fft` API, this module exposes [`Engine::command_sender`]
//!   and [`Engine::fft_buffer`] getters that hand the underlying
//!   `Sender<UiToDsp>` and `Arc<SharedFftBuffer>` to callers that prefer
//!   to wire them directly into existing channel/buffer plumbing. These
//!   are convenience methods, not the canonical API — `sdr-ffi` will not
//!   use them. They keep the GTK side's existing `AppState` /
//!   `glib::timeout_add_local` patterns intact so the M1 PR doesn't have
//!   to refactor every panel binding to use a different command path.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::controller::spawn_dsp_thread;
use crate::fft_buffer::SharedFftBuffer;
use crate::messages::{DspToUi, UiToDsp};

/// Default initial size for the shared FFT buffer. Matches the historical
/// `dsp_controller::SharedFftBuffer::new(2048)` site in `sdr-ui::window`.
const DEFAULT_FFT_BUFFER_SIZE: usize = 2048;

/// Errors returned by [`Engine`] lifecycle methods.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Failed to spawn the DSP controller OS thread.
    #[error("failed to spawn DSP controller thread: {0}")]
    SpawnFailed(#[from] std::io::Error),

    /// Failed to deliver a command to the DSP thread (channel disconnected).
    #[error("DSP command channel disconnected")]
    CommandChannelClosed,
}

/// The headless SDR engine.
///
/// Owns the command/event channels and the shared FFT buffer. The DSP
/// controller thread is spawned at construction time and **detached**:
/// when the [`Engine`] is dropped, `cmd_tx` disconnects, the controller's
/// `recv_timeout` loop sees the disconnect on its next tick, and the
/// thread exits and is reaped by the OS. This matches the historical
/// GTK behavior (`spawn_dsp_thread` was fire-and-forget) and avoids the
/// drop-order hazard of holding a `JoinHandle` alongside the only sender
/// — joining without first closing the channel would deadlock.
///
/// `shutdown` is provided as an explicit form for callers (notably the
/// future `sdr-ffi` consumer) that want a deterministic teardown signal,
/// but in v1 it is functionally equivalent to dropping the engine.
pub struct Engine {
    cmd_tx: mpsc::Sender<UiToDsp>,
    /// One-shot subscription slot. `subscribe()` takes the receiver out.
    /// Wrapped in `Mutex<Option<_>>` because `mpsc::Receiver` is `!Sync`.
    evt_rx: Mutex<Option<mpsc::Receiver<DspToUi>>>,
    fft: Arc<SharedFftBuffer>,
}

impl Engine {
    /// Build the engine: create channels, allocate the shared FFT buffer,
    /// and spawn the DSP controller thread. Returns an error if the OS
    /// rejects the thread spawn (rare but possible under resource pressure).
    ///
    /// The DSP thread is **not yet running the source**: send
    /// [`UiToDsp::Start`] via [`Engine::send_command`] to begin sample flow.
    /// This mirrors the historical `spawn_dsp_thread` behavior on the GTK
    /// side, where the thread idled until the user clicked play.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::SpawnFailed`] if `std::thread::Builder::spawn`
    /// returns an error.
    pub fn new() -> Result<Self, EngineError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<UiToDsp>();
        let (evt_tx, evt_rx) = mpsc::channel::<DspToUi>();
        let fft = Arc::new(SharedFftBuffer::new(DEFAULT_FFT_BUFFER_SIZE));

        // The returned JoinHandle is intentionally dropped: the DSP thread
        // is detached. The thread exits when `cmd_tx` is dropped (which
        // happens when this Engine is dropped), at which point the OS
        // reaps the detached thread. Storing the handle here would force
        // us to join during Drop, which would deadlock against our own
        // still-alive `cmd_tx` field.
        let _detached = spawn_dsp_thread(evt_tx, cmd_rx, Arc::clone(&fft))?;

        Ok(Self {
            cmd_tx,
            evt_rx: Mutex::new(Some(evt_rx)),
            fft,
        })
    }

    /// Send a command to the DSP thread. Non-blocking; safe from any
    /// thread.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::CommandChannelClosed`] if the DSP thread has
    /// already exited and the channel receiver has been dropped.
    pub fn send_command(&self, cmd: UiToDsp) -> Result<(), EngineError> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| EngineError::CommandChannelClosed)
    }

    /// Take the event receiver. Returns `Some(_)` exactly once per
    /// `Engine`; subsequent calls return `None`. The caller chooses how
    /// to drain it (a GTK timeout, an `sdr-ffi` dispatcher thread, an
    /// async task, …).
    ///
    /// Locks an internal mutex briefly; never blocks beyond that.
    pub fn subscribe(&self) -> Option<mpsc::Receiver<DspToUi>> {
        self.evt_rx.lock().ok()?.take()
    }

    /// Pull the latest FFT frame, if a new one has been published since
    /// the previous call. Calls `f` synchronously with a borrow of the
    /// frame data when one is available; returns `false` (and does not
    /// invoke `f`) when no new frame is ready.
    ///
    /// Lock-free fast path; locks the underlying buffer only for a short
    /// `memcpy` when a frame is being read.
    pub fn pull_fft<F: FnOnce(&[f32])>(&self, f: F) -> bool {
        self.fft.take_if_ready(f)
    }

    /// Return a clone of the underlying command-channel sender.
    ///
    /// This is a **migration helper** for callers that already hold a
    /// `Sender<UiToDsp>` and want to keep their existing wiring intact.
    /// New code should prefer [`Engine::send_command`].
    ///
    /// The clone is independent — dropping it does not close the channel
    /// while the engine is still alive.
    #[must_use]
    pub fn command_sender(&self) -> mpsc::Sender<UiToDsp> {
        self.cmd_tx.clone()
    }

    /// Return an [`Arc`] to the shared FFT buffer.
    ///
    /// This is a **migration helper** for callers that already pull FFT
    /// frames through `Arc<SharedFftBuffer>::take_if_ready` and don't
    /// want to switch to the [`Engine::pull_fft`] callback wrapper. New
    /// code should prefer `pull_fft`.
    #[must_use]
    pub fn fft_buffer(&self) -> Arc<SharedFftBuffer> {
        Arc::clone(&self.fft)
    }

    /// Explicit teardown signal. Sends [`UiToDsp::Stop`] so the DSP thread
    /// stops the active source cleanly, then returns. The DSP thread will
    /// exit fully when this `Engine` is dropped (which closes `cmd_tx` and
    /// causes the thread's `recv_timeout` loop to see Disconnected).
    ///
    /// In v1 this is informational — the same teardown happens implicitly
    /// on drop. The method exists so future consumers (notably the
    /// `sdr-ffi` Swift wrapper) can declare an explicit "engine done"
    /// hand-off without changing the API later.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::CommandChannelClosed`] if the DSP thread
    /// has already exited and the channel receiver has been dropped.
    pub fn shutdown(&self) -> Result<(), EngineError> {
        // Best-effort Stop. If the channel is already closed (e.g., the
        // DSP thread panicked earlier), report it but don't escalate —
        // the engine is being torn down anyway.
        self.send_command(UiToDsp::Stop)
    }
}

// No explicit Drop impl. The default Drop runs the field destructors in
// declaration order: evt_rx (the held Receiver, if any) drops first;
// cmd_tx drops next, which closes the command channel; the DSP thread's
// recv_timeout sees Disconnected on its next tick and exits. The
// detached thread is reaped by the OS — we never join it from inside
// Drop because doing so would race against our own still-alive cmd_tx
// (deadlock).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_new_spawns_thread_and_subscribe_returns_once() {
        let engine = Engine::new().expect("engine should spawn");

        // First subscribe returns the receiver.
        let rx = engine.subscribe();
        assert!(rx.is_some(), "first subscribe must return Some(receiver)");

        // Second subscribe is None.
        let rx2 = engine.subscribe();
        assert!(rx2.is_none(), "second subscribe must return None");

        // Drop the receiver and shutdown — the DSP thread exits when its
        // command channel disconnects (which happens at engine drop).
    }

    #[test]
    fn pull_fft_returns_false_with_no_frame() {
        let engine = Engine::new().expect("engine should spawn");

        let mut called = false;
        let got = engine.pull_fft(|_| called = true);

        assert!(!got);
        assert!(!called);
    }

    #[test]
    fn command_sender_clone_is_independent() {
        let engine = Engine::new().expect("engine should spawn");
        let sender = engine.command_sender();
        // Sending should not error on a freshly spawned engine.
        sender
            .send(UiToDsp::Stop)
            .expect("sending Stop on a fresh engine should succeed");
    }

    #[test]
    fn fft_buffer_arc_shares_state_with_engine() {
        let engine = Engine::new().expect("engine should spawn");
        let buf = engine.fft_buffer();
        // Arc count: engine + clone we just took = 2.
        // (DSP thread also holds one, so >= 3.)
        assert!(Arc::strong_count(&buf) >= 2);
    }
}
