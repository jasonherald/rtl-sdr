//! StatusNotifierItem tray-icon sidecar for sdr-rs.
//!
//! Pure-Rust StatusNotifierItem implementation via `ksni`, run on a
//! dedicated `std::thread` with a per-thread `smol` runtime so the
//! main `sdr-ui` GTK loop never has to be aware of async. Linux-only.
//!
//! Communication is one-way: this crate sends [`TrayEvent`]s through
//! a `std::sync::mpsc::Sender`; the UI side bridges to its main loop
//! via `glib::idle_add_local` or a periodic timeout. The UI never
//! holds tray-side state. If [`spawn`] returns [`SpawnError`], callers
//! should fall back to "no tray, exit on window close" — the rest of
//! the app must work without us.

#![cfg(target_os = "linux")]
#![cfg_attr(test, allow(unsafe_code))]
#![allow(unused_crate_dependencies)]

use std::sync::mpsc;
use std::thread::JoinHandle;

mod icon;

/// Events the tray sends to the GTK UI thread.
#[derive(Debug, Clone, Copy)]
pub enum TrayEvent {
    Show,
    Hide,
    ToggleVisibility,
    Quit,
}

/// Errors returned by [`spawn`] when the tray cannot start.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("no StatusNotifierWatcher on session bus (likely missing AppIndicator extension)")]
    TrayWatcherUnavailable,
    #[error("tray spawn failed: {0}")]
    Other(String),
}

/// Owned handle to a running tray service. Drop or call [`shutdown`]
/// to stop the thread.
///
/// [`shutdown`]: TrayHandle::shutdown
pub struct TrayHandle {
    stop_tx: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl TrayHandle {
    pub fn shutdown(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            if let Err(e) = join.join() {
                tracing::warn!("tray thread panicked during shutdown: {e:?}");
            }
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn the tray service. Stub — Task 2 lands the real ksni body.
///
/// # Errors
///
/// Always returns `Err(SpawnError::Other)` until Task 2.
pub fn spawn(_events: mpsc::Sender<TrayEvent>) -> Result<TrayHandle, SpawnError> {
    Err(SpawnError::Other("not yet implemented".to_string()))
}
