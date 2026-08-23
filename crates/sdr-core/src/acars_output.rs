//! ACARS output writers — JSONL file logger and UDP JSON
//! feeder. Owns the I/O surface (file handles + sockets) so
//! the pure-DSP `sdr-acars` crate can stay I/O-free.
//!
//! Both writers consume `&AcarsMessage` and serialize via
//! `sdr_acars::serialize_acars_json`. Synchronous calls in
//! the DSP thread; per-message warn rate-limiting is
//! orchestrated by the caller (controller.rs).
//!
//! Issue #578.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;

use sdr_acars::AcarsMessage;

/// Runtime-mutable writer config. Read-heavy access pattern:
/// the writer thread reads on every message, the UI side writes
/// only on user toggle / address edit / station-id change.
/// Issue #596.
#[derive(Clone, Debug, Default)]
pub struct AcarsWriterConfig {
    /// Where to write the JSONL log. `None` means JSONL output
    /// is disabled. Path changes trigger a reopen on the next
    /// message; the worker closes the previous file.
    pub jsonl_path: Option<PathBuf>,
    /// UDP feeder destination (`"host:port"`). `None` means
    /// network output is disabled.
    pub network_addr: Option<String>,
    /// Station ID injected into each emitted JSON record.
    /// Bounded to [`STATION_ID_MAX_CHARS`] at the
    /// `SetAcarsStationId` handler boundary.
    pub station_id: Option<String>,
}

/// Station-ID length cap, matching acarsdec's `idstation` field
/// width so emitted JSON stays interchangeable with acarsdec
/// consumers. Enforced by the `SetAcarsStationId` handler.
pub(crate) const STATION_ID_MAX_CHARS: usize = 8;

/// Messages handed from the DSP thread to the writer thread.
/// Bounded `mpsc::sync_channel` decouples the DSP-thread
/// `acars_decode_tap` closure from disk / network I/O latency.
pub enum AcarsOutputMessage {
    /// One decoded ACARS message, ready to write + feed.
    Decoded(sdr_acars::AcarsMessage),
    /// The shared `AcarsWriterConfig` was mutated by the UI side.
    /// Wakes the writer to re-snapshot config and apply
    /// `ensure_jsonl` / `ensure_udp` so config-only changes
    /// (disable, path swap, addr swap) take effect immediately
    /// instead of being buffered until the next decoded message.
    /// CR round 1 on PR #598.
    ConfigChanged,
    /// Explicit clean-shutdown signal. `Drop for AcarsOutputs`
    /// emits this before dropping `tx`; the worker also exits
    /// cleanly on `Disconnected` as a fallback. Having an
    /// explicit variant makes shutdown deterministic for tests.
    Shutdown,
}

/// Append-only JSONL writer. One JSON object per line (`\n`-
/// terminated). Wraps the file in a `BufWriter` so bursty
/// per-message writes don't syscall on each one; flushed on
/// drop and on explicit `flush()` calls (controller calls
/// flush on disengage / app shutdown).
pub struct JsonlWriter {
    file: BufWriter<File>,
    path: PathBuf,
}

impl JsonlWriter {
    /// Open `path` in append mode. Creates the parent
    /// directory if missing (mirrors the WAV-recorder pattern
    /// in the satellite recorder). Returns `io::Error` on
    /// open failure — the caller logs + toasts.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
        })
    }

    /// Serialize `msg` and append `<json>\n` to the file.
    pub fn write(&mut self, msg: &AcarsMessage, station_id: Option<&str>) -> io::Result<()> {
        let json = sdr_acars::serialize_acars_json(msg, station_id);
        writeln!(self.file, "{json}")
    }

    /// Flush the buffered writer. Called on disengage and on
    /// app shutdown so the on-disk tail is consistent.
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    /// The path the writer was opened against.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for JsonlWriter {
    fn drop(&mut self) {
        if let Err(e) = self.file.flush() {
            tracing::warn!("acars jsonl flush on drop failed: {e}");
        }
    }
}

/// UDP JSON datagram feeder. Sends each `AcarsMessage` as a
/// single newline-terminated JSON datagram. Fire-and-forget —
/// no retry, no acks. Mirrors `original/acarsdec/netout.c::Netoutjson`
/// (default port 5550 for airframes.io feeders, 5555 in
/// acarsdec's general-purpose example).
pub struct UdpFeeder {
    socket: UdpSocket,
    addr: SocketAddr,
    addr_str: String,
}

impl UdpFeeder {
    /// Resolve `addr_str` (e.g. `"feed.airframes.io:5550"` or
    /// `"127.0.0.1:5550"`), bind a local ephemeral UDP socket,
    /// and cache the resolved peer address. Returns `io::Error`
    /// on parse / DNS / bind failure — the caller logs + toasts.
    pub fn open(addr_str: &str) -> io::Result<Self> {
        let addr = addr_str.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("no address resolved for {addr_str}"),
            )
        })?;
        let bind_addr: SocketAddr = if addr.is_ipv6() {
            "[::]:0".parse().map_err(io::Error::other)?
        } else {
            "0.0.0.0:0".parse().map_err(io::Error::other)?
        };
        let socket = UdpSocket::bind(bind_addr)?;
        Ok(Self {
            socket,
            addr,
            addr_str: addr_str.to_string(),
        })
    }

    /// Serialize `msg`, append `\n`, send one UDP datagram to
    /// the resolved peer.
    pub fn send(&self, msg: &AcarsMessage, station_id: Option<&str>) -> io::Result<()> {
        let mut payload = sdr_acars::serialize_acars_json(msg, station_id);
        payload.push('\n');
        self.socket.send_to(payload.as_bytes(), self.addr)?;
        Ok(())
    }

    /// The original `host:port` string the feeder was opened
    /// against (for diagnostic logging / status display).
    #[must_use]
    pub fn addr_str(&self) -> &str {
        &self.addr_str
    }
}

/// Capacity of the bounded `mpsc::sync_channel` between the
/// DSP thread and the writer thread. 256 is ~4-5 minutes of
/// worst-case ACARS bursts (~1 msg/sec sustained, 10 msg/sec
/// burst peak); covers any realistic disk stall short of total
/// filesystem hang. Issue #596.
pub const ACARS_OUTPUT_CHANNEL_CAPACITY: usize = 256;

/// Output-writer bundle owned by `DspState`. Holds the sender
/// half of the bounded channel + the shared writer config +
/// the worker thread's join handle. The DSP thread calls
/// `try_send` per decoded message; the writer thread (spawned
/// from `new`) does the actual JSONL/UDP I/O. Issue #596.
pub struct AcarsOutputs {
    /// Sender half of the writer channel. `try_send` drops on
    /// full; the worker owns the receiver.
    tx: mpsc::SyncSender<AcarsOutputMessage>,
    /// Shared, runtime-mutable writer config. Written by the
    /// UI side on toggle/edit; read by the writer thread on
    /// each message.
    pub config: Arc<RwLock<AcarsWriterConfig>>,
    /// Cumulative count of messages dropped because the
    /// channel was full. Surfaced via `drop_count` for
    /// rate-limited warn at the call site (and the smoke
    /// checklist).
    drop_count: Arc<AtomicU64>,
    /// Last warn timestamp for channel-full drops. Wrapped in
    /// `Arc<Mutex>` because the warn fires from the DSP thread
    /// (caller of `try_send`); the writer thread doesn't touch
    /// it.
    last_drop_warn_at: Arc<Mutex<Option<std::time::Instant>>>,
    /// Join handle for the writer thread. `Drop` for
    /// `AcarsOutputs` drops `tx`, which signals shutdown via
    /// `recv()` returning `Err(Disconnected)`; we then `join()`.
    writer_thread: Option<JoinHandle<()>>,
}

impl AcarsOutputs {
    /// Construct an async-output bundle and spawn the writer
    /// thread. `dsp_tx` is cloned into the worker so it can
    /// surface open / write / send failures back to the UI as
    /// `DspToUi::AcarsOutputError` toasts (CR round 1 on PR
    /// #598; preserves the UI error contract that the original
    /// synchronous code had in PR #595).
    ///
    /// The thread runs until `Drop for AcarsOutputs` sends an
    /// explicit `Shutdown` message (or — as a fallback — drops
    /// `tx`, at which point the writer's `recv()` returns
    /// `Err(Disconnected)`). Either way the loop exits cleanly.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the writer thread cannot be spawned
    /// (thread / FD exhaustion). The caller — `DspState::new` — turns
    /// that into a `DspToUi::Error` instead of panicking the DSP
    /// thread before its command loop (#701).
    pub fn new(dsp_tx: mpsc::Sender<crate::messages::DspToUi>) -> std::io::Result<Self> {
        Self::with_capacity(ACARS_OUTPUT_CHANNEL_CAPACITY, dsp_tx)
    }

    /// Same as `new` but with a caller-chosen channel
    /// capacity. Production calls go through `new`; tests use
    /// this directly via `with_capacity_for_test` to exercise
    /// the drop-on-full path with a cap they can saturate.
    fn with_capacity(
        capacity: usize,
        dsp_tx: mpsc::Sender<crate::messages::DspToUi>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(capacity);
        let config = Arc::new(RwLock::new(AcarsWriterConfig::default()));

        let writer_config = Arc::clone(&config);
        let writer_thread = std::thread::Builder::new()
            .name("sdr-acars-writer".into())
            .spawn(move || run_writer_loop(rx, writer_config, dsp_tx))?;

        Ok(Self {
            tx,
            config,
            drop_count: Arc::new(AtomicU64::new(0)),
            last_drop_warn_at: Arc::new(Mutex::new(None)),
            writer_thread: Some(writer_thread),
        })
    }

    /// Test-only constructor that builds the channel + config
    /// but skips spawning the worker, leaving the receiver
    /// dangling so tests can fill the channel without races.
    #[cfg(test)]
    fn with_capacity_for_test(capacity: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(capacity);
        // Leak the receiver so the channel doesn't disconnect
        // (which would route try_send into the Disconnected arm
        // instead of Full). std::mem::forget is the cheapest way
        // to do this in test context.
        #[allow(clippy::mem_forget)]
        std::mem::forget(rx);
        let config = Arc::new(RwLock::new(AcarsWriterConfig::default()));
        Self {
            tx,
            config,
            drop_count: Arc::new(AtomicU64::new(0)),
            last_drop_warn_at: Arc::new(Mutex::new(None)),
            writer_thread: None,
        }
    }

    /// Try to hand off `msg` to the writer thread. Returns
    /// `true` on success, `false` if the channel was full
    /// (drop counter incremented; warn fires at most once per
    /// 30 s).
    pub fn try_send(&self, msg: sdr_acars::AcarsMessage) -> bool {
        match self.tx.try_send(AcarsOutputMessage::Decoded(msg)) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                self.drop_count.fetch_add(1, Ordering::Relaxed);
                self.maybe_warn_full();
                false
            }
            // Disconnected only happens on shutdown (writer
            // thread is gone). Silent — caller shouldn't
            // surface noise during teardown.
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    /// Cumulative drop count since startup.
    #[must_use]
    pub fn drop_count(&self) -> u64 {
        self.drop_count.load(Ordering::Relaxed)
    }

    /// Wake the writer thread so it re-snapshots the shared
    /// `config` and applies `ensure_jsonl` / `ensure_udp`. The
    /// controller's `handle_set_acars_*` handlers call this
    /// after every config write so config-only changes
    /// (disable, path swap, addr swap) take effect immediately
    /// — without it, the worker only wakes on `Decoded` and
    /// stale handles linger until the next decoded frame
    /// (CR round 1 on PR #598).
    ///
    /// `try_send`, not `send`: if the channel is full the
    /// worker is already saturated processing `Decoded` and
    /// will re-snapshot config on the next iteration anyway
    /// — a dropped `ConfigChanged` is harmless under that
    /// pressure.
    pub fn notify_config_changed(&self) {
        let _ = self.tx.try_send(AcarsOutputMessage::ConfigChanged);
    }

    /// 30 s-rate-limited warn for channel-full drops. Reads
    /// the current drop count so the message names how many
    /// were lost in this window.
    fn maybe_warn_full(&self) {
        // Recover from poison: a panic while holding this lock must
        // not take the DSP thread down with it (#701).
        let mut last = self
            .last_drop_warn_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let elapsed = last.map_or(ACARS_OUTPUT_WARN_MIN_INTERVAL, |t| now.duration_since(t));
        if elapsed >= ACARS_OUTPUT_WARN_MIN_INTERVAL {
            let n = self.drop_count.load(Ordering::Relaxed);
            tracing::warn!(
                "ACARS output channel full ({n} drops since startup); \
                 writer thread falling behind (rate-limited 30s)"
            );
            *last = Some(now);
        }
    }
}

impl Drop for AcarsOutputs {
    fn drop(&mut self) {
        // Send the explicit Shutdown sentinel first so the
        // worker exits via the deterministic `Shutdown` arm
        // rather than the `Err(Disconnected)` fallback. Both
        // paths drain cleanly, but Shutdown means tests can
        // assert promptness without racing the OS scheduler.
        // `try_send` is fine — if the channel is full the
        // Disconnected fallback below still terminates.
        let _ = self.tx.try_send(AcarsOutputMessage::Shutdown);

        // Closing tx triggers Disconnected → the writer loop
        // exits as a fallback. We still need to join the thread
        // to make sure its Drop impls (BufWriter flush) finish
        // before the process exits.
        if let Some(handle) = self.writer_thread.take() {
            // Drop the tx clone held by `self.tx` first by
            // overwriting it with a drained channel.
            // (mpsc::SyncSender doesn't have an explicit
            // close — Drop is the signal.)
            let (dummy_tx, _) = mpsc::sync_channel::<AcarsOutputMessage>(0);
            self.tx = dummy_tx;
            // Now the original tx is gone (replaced + dropped).
            // Wait for the worker to exit.
            if let Err(e) = handle.join() {
                tracing::warn!("ACARS writer thread join failed: {e:?}");
            }
        }
    }
}

/// Writer-thread main loop. Owns the per-thread `JsonlWriter`
/// and `UdpFeeder` instances, reads `config` on each message
/// (or on `ConfigChanged`) to detect path/addr changes, and
/// exits cleanly on `Shutdown` or when the sender side
/// disconnects (app shutdown). Issue #596 / CR round 1 on PR
/// #598.
#[allow(clippy::needless_pass_by_value)] // rx must be owned to observe disconnect
fn run_writer_loop(
    rx: mpsc::Receiver<AcarsOutputMessage>,
    config: Arc<RwLock<AcarsWriterConfig>>,
    dsp_tx: mpsc::Sender<crate::messages::DspToUi>,
) {
    let mut jsonl: Option<(PathBuf, JsonlWriter)> = None;
    let mut udp: Option<(String, UdpFeeder)> = None;
    let mut jsonl_warn_at: Option<std::time::Instant> = None;
    let mut udp_warn_at: Option<std::time::Instant> = None;
    let mut jsonl_backoff = OpenBackoff::default();
    let mut udp_backoff = OpenBackoff::default();

    // `while let Ok(_)` is the disconnect-fallback path; the
    // inner `match` handles the explicit Shutdown sentinel
    // (which `break`s out of the outer loop). Either path
    // exits cleanly. CR round 1 on PR #598.
    'recv: while let Ok(msg) = rx.recv() {
        match msg {
            AcarsOutputMessage::Shutdown => break 'recv,
            AcarsOutputMessage::ConfigChanged => {
                // No payload to write — just resnap config and
                // close/open. ensure_* close on None and reopen
                // on path/addr change, so disabling JSONL or
                // swapping the destination applies immediately
                // even with no decoded traffic.
                let (want_jsonl_path, want_udp_addr, _station_id) = snapshot_config(&config);
                // An explicit config change is a user action: retry a
                // previously failed open right away (#702).
                jsonl_backoff.reset();
                udp_backoff.reset();
                ensure_jsonl(
                    &mut jsonl,
                    want_jsonl_path.as_deref(),
                    &mut jsonl_backoff,
                    &dsp_tx,
                );
                ensure_udp(
                    &mut udp,
                    want_udp_addr.as_deref(),
                    &mut udp_backoff,
                    &dsp_tx,
                );
            }
            AcarsOutputMessage::Decoded(msg) => {
                // Snapshot the config under a brief read lock so we
                // don't hold it across blocking I/O.
                let (want_jsonl_path, want_udp_addr, station_id) = snapshot_config(&config);

                ensure_jsonl(
                    &mut jsonl,
                    want_jsonl_path.as_deref(),
                    &mut jsonl_backoff,
                    &dsp_tx,
                );
                ensure_udp(
                    &mut udp,
                    want_udp_addr.as_deref(),
                    &mut udp_backoff,
                    &dsp_tx,
                );

                if let Some((_, w)) = jsonl.as_mut()
                    && let Err(e) = w.write(&msg, station_id.as_deref())
                {
                    rate_limited_warn_and_emit("jsonl", &mut jsonl_warn_at, &e, &dsp_tx);
                }
                if let Some((_, f)) = udp.as_mut()
                    && let Err(e) = f.send(&msg, station_id.as_deref())
                {
                    rate_limited_warn_and_emit("udp", &mut udp_warn_at, &e, &dsp_tx);
                }
            }
        }
    }
}

/// Snapshot the writer config under a brief read lock so it is never
/// held across blocking I/O. Recovers from poison: a panic elsewhere
/// while holding the lock must not kill the writer thread (#701).
fn snapshot_config(
    config: &RwLock<AcarsWriterConfig>,
) -> (Option<PathBuf>, Option<String>, Option<String>) {
    let cfg = config
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (
        cfg.jsonl_path.clone(),
        cfg.network_addr.clone(),
        cfg.station_id.clone(),
    )
}

/// Retry gate for a failed output open (#702). With an unwritable
/// path or an unresolvable feeder host, every decoded message used
/// to redo `create_dir_all` + `open` (or DNS) plus a warn and a toast
/// — several per second on a busy airband. After a failure the same
/// target is not retried until [`ACARS_OUTPUT_WARN_MIN_INTERVAL`]
/// elapses; a different target or [`OpenBackoff::reset`] (explicit
/// config change) retries immediately.
#[derive(Debug, Default)]
struct OpenBackoff {
    /// Target whose last open failed, with the failure time.
    failed: Option<(String, std::time::Instant)>,
}

impl OpenBackoff {
    /// `true` while `target` is still inside the backoff window.
    fn should_skip(&self, target: &str) -> bool {
        self.failed
            .as_ref()
            .is_some_and(|(key, at)| key == target && at.elapsed() < ACARS_OUTPUT_WARN_MIN_INTERVAL)
    }

    fn record_failure(&mut self, target: &str) {
        self.failed = Some((target.to_string(), std::time::Instant::now()));
    }

    /// Forget the last failure so the next `ensure_*` retries.
    fn reset(&mut self) {
        self.failed = None;
    }
}

/// Emit `DspToUi::AcarsOutputError` for an open / write / send
/// failure. The matching `tracing::warn!` is the caller's job
/// (separated so the rate-limiter can decide whether to also
/// warn-spam logs); this is the UI-toast surface.
fn emit_output_error(
    dsp_tx: &mpsc::Sender<crate::messages::DspToUi>,
    kind: &'static str,
    message: String,
) {
    let _ = dsp_tx.send(crate::messages::DspToUi::AcarsOutputError { kind, message });
}

/// Ensure `slot` holds an open `JsonlWriter` matching `want`.
/// Reopens on path change; closes (drops) when `want` is `None`.
/// Open failures are logged via `tracing::warn!` AND surfaced
/// to the UI as `DspToUi::AcarsOutputError` for toast display
/// (CR round 1 on PR #598).
fn ensure_jsonl(
    slot: &mut Option<(PathBuf, JsonlWriter)>,
    want: Option<&Path>,
    backoff: &mut OpenBackoff,
    dsp_tx: &mpsc::Sender<crate::messages::DspToUi>,
) {
    let needs_reopen = match (slot.as_ref(), want) {
        (None, None) => false,
        (Some((cur, _)), Some(want)) if cur == want => false,
        _ => true,
    };
    if !needs_reopen {
        return;
    }
    *slot = None;
    if let Some(want) = want {
        let key = want.to_string_lossy();
        if backoff.should_skip(&key) {
            return;
        }
        match JsonlWriter::open(want) {
            Ok(w) => {
                backoff.reset();
                *slot = Some((want.to_path_buf(), w));
            }
            Err(e) => {
                backoff.record_failure(&key);
                let message = format!("acars jsonl open failed: {e}");
                tracing::warn!("{message} (retry in {ACARS_OUTPUT_WARN_MIN_INTERVAL:?})");
                emit_output_error(dsp_tx, "jsonl", message);
            }
        }
    }
}

/// Same shape as `ensure_jsonl` but for `UdpFeeder`. The `String`
/// key compares the user-set addr verbatim; resolved peer
/// addresses are not the source of truth.
fn ensure_udp(
    slot: &mut Option<(String, UdpFeeder)>,
    want: Option<&str>,
    backoff: &mut OpenBackoff,
    dsp_tx: &mpsc::Sender<crate::messages::DspToUi>,
) {
    let needs_reopen = match (slot.as_ref(), want) {
        (None, None) => false,
        (Some((cur, _)), Some(want)) if cur == want => false,
        _ => true,
    };
    if !needs_reopen {
        return;
    }
    *slot = None;
    if let Some(want) = want {
        if backoff.should_skip(want) {
            return;
        }
        match UdpFeeder::open(want) {
            Ok(f) => {
                backoff.reset();
                *slot = Some((want.to_string(), f));
            }
            Err(e) => {
                backoff.record_failure(want);
                let message = format!("acars udp open failed: {e}");
                tracing::warn!("{message} (retry in {ACARS_OUTPUT_WARN_MIN_INTERVAL:?})");
                emit_output_error(dsp_tx, "udp", message);
            }
        }
    }
}

const ACARS_OUTPUT_WARN_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Emit a `tracing::warn!` AND a `DspToUi::AcarsOutputError`
/// at most once per `ACARS_OUTPUT_WARN_MIN_INTERVAL` for
/// `kind`. Mirrors the per-writer 30 s rate-limit that
/// previously lived in `controller.rs::acars_decode_tap`,
/// extended in CR round 1 on PR #598 to also surface the
/// failure to the UI as a toast.
fn rate_limited_warn_and_emit(
    kind: &'static str,
    last: &mut Option<std::time::Instant>,
    err: &std::io::Error,
    dsp_tx: &mpsc::Sender<crate::messages::DspToUi>,
) {
    let now = std::time::Instant::now();
    let elapsed = last.map_or(ACARS_OUTPUT_WARN_MIN_INTERVAL, |t| now.duration_since(t));
    if elapsed >= ACARS_OUTPUT_WARN_MIN_INTERVAL {
        let message = format!("acars {kind} write/send failed: {err}");
        tracing::warn!("{message} (rate-limited 30s)");
        emit_output_error(dsp_tx, kind, message);
        *last = Some(now);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
