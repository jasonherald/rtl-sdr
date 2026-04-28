//! `StatusNotifierItem` tray-icon sidecar for sdr-rs.
//!
//! Pure-Rust `StatusNotifierItem` implementation via `ksni`, run on a
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

use std::sync::mpsc;
use std::thread::JoinHandle;

use ksni::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};

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
#[derive(Debug)]
pub struct TrayHandle {
    stop_tx: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl TrayHandle {
    pub fn shutdown(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take()
            && let Err(e) = join.join()
        {
            tracing::warn!("tray thread panicked during shutdown: {e:?}");
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `ksni::Tray` implementation. Forwards user input to a `mpsc::Sender`
/// owned by the UI side, so this struct stays tiny and Send.
struct SdrTray {
    events: mpsc::Sender<TrayEvent>,
}

impl SdrTray {
    fn send(&self, event: TrayEvent) {
        if let Err(e) = self.events.send(event) {
            tracing::debug!("tray event receiver gone: {e}");
        }
    }
}

impl ksni::Tray for SdrTray {
    fn id(&self) -> String {
        "com.sdr.rs".to_string()
    }

    fn title(&self) -> String {
        "SDR-RS".to_string()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: "SDR-RS".to_string(),
            description: "Software-defined radio".to_string(),
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let (width, height, data) = icon::current_icon();
        vec![ksni::Icon {
            width,
            height,
            data,
        }]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayEvent::ToggleVisibility);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Show / Hide".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    tray.send(TrayEvent::ToggleVisibility);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    tray.send(TrayEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Map a ksni registration error into our public `SpawnError`.
///
/// Error strings containing "watcher" (case-insensitive) — i.e.
/// `ksni::Error::Watcher` — indicate the desktop environment has no
/// `StatusNotifierWatcher`. Everything else (D-Bus reachability
/// problems, `WontShow`) falls into `Other` carrying the message.
///
/// Takes the stringly error so callers in tests can exercise the
/// mapping without needing a real `ksni::Error` value (the enum's
/// variants are crate-private). Per CR round 1 on PR #572.
fn map_spawn_error_msg(msg: &str) -> SpawnError {
    if msg.to_lowercase().contains("watcher") {
        SpawnError::TrayWatcherUnavailable
    } else {
        SpawnError::Other(msg.to_string())
    }
}

/// Spawn the tray service.
///
/// Blocks the calling thread until tray registration succeeds or
/// fails. On success the returned `TrayHandle` owns a dedicated
/// `std::thread` that drives the tray's lifecycle; drop it (or call
/// [`TrayHandle::shutdown`]) to stop the tray.
///
/// # Errors
///
/// - `SpawnError::TrayWatcherUnavailable` if the session has no
///   `StatusNotifierWatcher` (typically missing `AppIndicator`
///   extension on GNOME, or no SNI host on minimal WMs).
/// - `SpawnError::Other` for everything else (D-Bus unreachable,
///   `WontShow`, etc.).
pub fn spawn(events: mpsc::Sender<TrayEvent>) -> Result<TrayHandle, SpawnError> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), SpawnError>>();

    let join = std::thread::Builder::new()
        .name("sdr-tray".to_string())
        .spawn(move || {
            smol::block_on(async move {
                let tray = SdrTray { events };
                match tray.spawn().await {
                    Ok(_handle) => {
                        // Registration succeeded. Hold the handle in
                        // scope until shutdown is signalled, then
                        // drop it to tear down the SNI service.
                        if ready_tx.send(Ok(())).is_err() {
                            // Caller already gave up (e.g. dropped
                            // the receiver after a panic). Bail out.
                            return;
                        }
                        // Block on the std mpsc; we don't need to
                        // poll any futures here because ksni's
                        // background tasks live on its own internal
                        // executor thread (async-io feature).
                        let _ = stop_rx.recv();
                        // _handle drops here, ksni tears down.
                    }
                    Err(e) => {
                        let mapped = map_spawn_error_msg(&e.to_string());
                        let _ = ready_tx.send(Err(mapped));
                    }
                }
            });
        })
        .map_err(|e| SpawnError::Other(format!("failed to spawn tray thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(TrayHandle {
            stop_tx,
            join: Some(join),
        }),
        Ok(Err(spawn_err)) => {
            // Thread already exited after sending the error; reap
            // it so we don't leave a zombie JoinHandle.
            if let Err(e) = join.join() {
                tracing::warn!("tray thread panicked while reporting error: {e:?}");
            }
            Err(spawn_err)
        }
        Err(_) => {
            // Sender closed without sending — the thread panicked
            // before reaching the await. Reap and report.
            let panic_msg = match join.join() {
                Ok(()) => "tray thread exited without reporting status".to_string(),
                Err(e) => format!("tray thread panicked during init: {e:?}"),
            };
            Err(SpawnError::Other(panic_msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the watcher-error mapping. Exercises the same string match
    /// `spawn` performs on a real `ksni::Error::Watcher`, but without
    /// touching env vars or D-Bus — the previous test mutated
    /// `DBUS_SESSION_BUS_ADDRESS` via `unsafe { set_var }`, which is
    /// fragile under parallel `cargo test`. Per CR round 1 on PR #572.
    #[test]
    fn map_spawn_error_msg_recognizes_watcher_strings() {
        // The exact string ksni produces for `Error::Watcher`.
        let msg = "failed to register to the StatusNotifierWatcher: …";
        assert!(matches!(
            map_spawn_error_msg(msg),
            SpawnError::TrayWatcherUnavailable
        ));
    }

    #[test]
    fn map_spawn_error_msg_is_case_insensitive() {
        // Defensive: future ksni / zbus version drift could change
        // capitalization.
        for variant in [
            "WATCHER unavailable",
            "no statusnotifierWATCHER",
            "watcher missing on bus",
        ] {
            assert!(
                matches!(
                    map_spawn_error_msg(variant),
                    SpawnError::TrayWatcherUnavailable
                ),
                "case-insensitive watcher match failed for: {variant}",
            );
        }
    }

    #[test]
    fn map_spawn_error_msg_falls_back_to_other_with_message() {
        let msg = "D-Bus connection error: address is invalid";
        let result = map_spawn_error_msg(msg);
        assert!(
            matches!(&result, SpawnError::Other(s) if s == msg),
            "expected SpawnError::Other({msg:?}), got {result:?}",
        );
    }
}
