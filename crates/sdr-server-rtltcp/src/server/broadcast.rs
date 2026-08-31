//! The server-lifetime USB broadcaster: one thread pulling bulk
//! reads from the dongle and fanning each chunk out to every
//! connected client via [`ClientRegistry::broadcast`], with the #711
//! consecutive-USB-error budget ([`classify_usb_read`]) and the
//! prune/reap housekeeping cadence. Split out of `server.rs` per the
//! file-size refactor (#818); pure moves, no behavior change.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use librtlsdr_rs::RtlSdrDevice;

use crate::broadcaster::ClientRegistry;

use super::{
    BROADCASTER_PRUNE_EVERY_N_TICKS, MAX_CONSECUTIVE_USB_ERRORS, READ_BUFFER_LEN, USB_READ_TIMEOUT,
};

/// Spawn the server-lifetime broadcaster thread. Pulls from USB and
/// calls [`ClientRegistry::broadcast`] once per chunk. Runs even when
/// there are zero connected clients — the dongle streams regardless,
/// matching upstream's always-on async read. When clients connect
/// they join the stream mid-flow (no per-client reset).
pub(super) fn spawn_broadcaster_thread(
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("rtl_tcp-broadcaster".into())
        .spawn(move || {
            broadcaster_worker(device, registry, shutdown);
        })
}

fn broadcaster_worker(
    device: Arc<Mutex<RtlSdrDevice>>,
    registry: Arc<ClientRegistry>,
    shutdown: Arc<AtomicBool>,
) {
    let Some(handle) = usb_handle_or_shutdown(&device, &shutdown) else {
        return;
    };
    // Scratch buffer reused across iterations — only the Vec<u8>
    // that the registry clones per-client gets a fresh allocation,
    // sized to the data the USB read actually returned.
    let mut scratch = vec![0u8; READ_BUFFER_LEN as usize];
    let mut ticks_since_prune: u32 = 0;
    let mut usb_errors: u32 = 0;

    while !shutdown.load(Ordering::Relaxed) {
        let read = handle.read_bulk(RtlSdrDevice::BULK_ENDPOINT, &mut scratch, USB_READ_TIMEOUT);
        match classify_usb_read(read, &mut usb_errors) {
            UsbReadOutcome::Data(n) => {
                registry.broadcast(&scratch[..n]);
                ticks_since_prune = ticks_since_prune.saturating_add(1);
                if ticks_since_prune >= BROADCASTER_PRUNE_EVERY_N_TICKS {
                    prune_and_reap(&registry);
                    ticks_since_prune = 0;
                }
            }
            UsbReadOutcome::Idle => {
                // No data — loop and re-check shutdown.
            }
            UsbReadOutcome::Retry(e) => {
                tracing::warn!(
                    %e,
                    consecutive = usb_errors,
                    "rtl_tcp bulk read error — retrying"
                );
            }
            UsbReadOutcome::Stop(e) => {
                // Dongle unplug (or a sustained run of failed
                // transfers) is unrecoverable at the server
                // level. Escalate to a global shutdown so the
                // accept thread exits, the CLI sees
                // `has_stopped() == true`, and connected
                // clients' command / writer loops observe the
                // flag and tear down.
                tracing::error!(
                    %e,
                    consecutive = usb_errors,
                    "rtl_tcp: USB read failure is terminal, stopping server"
                );
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
        }
    }
    // Final prune on exit so the pruned-slots metric doesn't
    // indefinitely lag behind truth when the server stops with
    // dead slots still registered.
    registry.prune_disconnected();
}

/// Pull the USB handle once so the broadcaster doesn't lock the
/// device mutex on every bulk read. The handle is Arc-cloneable and
/// thread-safe for bulk reads; the mutex-guarded device is still
/// required for command dispatch and configuration changes, which
/// run on per-client command workers. A poisoned mutex is
/// unrecoverable: signal server shutdown and return `None`.
fn usb_handle_or_shutdown(
    device: &Mutex<RtlSdrDevice>,
    shutdown: &AtomicBool,
) -> Option<Arc<rusb::DeviceHandle<rusb::GlobalContext>>> {
    let Ok(dev) = device.lock() else {
        tracing::error!(
            "device mutex poisoned, broadcaster aborting and signalling server shutdown"
        );
        shutdown.store(true, Ordering::SeqCst);
        return None;
    };
    Some(dev.usb_handle())
}

/// Broadcaster housekeeping on its prune cadence: drop disconnected
/// slots and reap finished per-client worker handles — otherwise a
/// long-lived server with connection churn would keep every
/// completed writer/command handle alive until shutdown (OS thread
/// resources + TLS linger). Per `CodeRabbit` round 5 on PR #402.
fn prune_and_reap(registry: &ClientRegistry) {
    let removed = registry.prune_disconnected();
    if removed > 0 {
        tracing::debug!(removed, "rtl_tcp pruned disconnected client slots");
    }
    let reaped = registry.reap_finished_worker_handles();
    if reaped > 0 {
        tracing::debug!(reaped, "rtl_tcp reaped finished per-client worker threads");
    }
}

/// What the broadcaster does with one USB bulk-read result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UsbReadOutcome {
    /// `n` bytes to fan out.
    Data(usize),
    /// Nothing to do: zero-length read or timeout.
    Idle,
    /// Transient failure: counted, read again.
    Retry(rusb::Error),
    /// Device gone, or [`MAX_CONSECUTIVE_USB_ERRORS`] failures in a
    /// row (this error being the last of them): stop the server.
    Stop(rusb::Error),
}

/// librtlsdr's `xfer_errors` rule (#711 / #808): any successful read
/// — including a zero-length one — resets `consecutive_errors`; a
/// `Timeout` is neutral; `NoDevice` is terminal at once; any other
/// error is counted and retried until the budget is spent.
pub(super) fn classify_usb_read(
    result: Result<usize, rusb::Error>,
    consecutive_errors: &mut u32,
) -> UsbReadOutcome {
    match result {
        Ok(n) => {
            *consecutive_errors = 0;
            if n > 0 {
                UsbReadOutcome::Data(n)
            } else {
                UsbReadOutcome::Idle
            }
        }
        Err(rusb::Error::Timeout) => UsbReadOutcome::Idle,
        Err(e @ rusb::Error::NoDevice) => UsbReadOutcome::Stop(e),
        Err(e) => {
            *consecutive_errors = consecutive_errors.saturating_add(1);
            if *consecutive_errors >= MAX_CONSECUTIVE_USB_ERRORS {
                UsbReadOutcome::Stop(e)
            } else {
                UsbReadOutcome::Retry(e)
            }
        }
    }
}
