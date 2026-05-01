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
    pub station_id: Option<String>,
}

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
    #[must_use]
    pub fn new(dsp_tx: mpsc::Sender<crate::messages::DspToUi>) -> Self {
        Self::with_capacity(ACARS_OUTPUT_CHANNEL_CAPACITY, dsp_tx)
    }

    /// Same as `new` but with a caller-chosen channel
    /// capacity. Production calls go through `new`; tests use
    /// this directly via `with_capacity_for_test` to exercise
    /// the drop-on-full path with a cap they can saturate.
    fn with_capacity(capacity: usize, dsp_tx: mpsc::Sender<crate::messages::DspToUi>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(capacity);
        let config = Arc::new(RwLock::new(AcarsWriterConfig::default()));

        let writer_config = Arc::clone(&config);
        let writer_thread = std::thread::Builder::new()
            .name("sdr-acars-writer".into())
            .spawn(move || run_writer_loop(rx, writer_config, dsp_tx))
            .expect("failed to spawn ACARS writer thread");

        Self {
            tx,
            config,
            drop_count: Arc::new(AtomicU64::new(0)),
            last_drop_warn_at: Arc::new(Mutex::new(None)),
            writer_thread: Some(writer_thread),
        }
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
        let mut last = self.last_drop_warn_at.lock().expect("warn lock poisoned");
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
                let (want_jsonl_path, want_udp_addr, _station_id) = {
                    let cfg = config.read().expect("acars writer config poisoned");
                    (
                        cfg.jsonl_path.clone(),
                        cfg.network_addr.clone(),
                        cfg.station_id.clone(),
                    )
                };
                ensure_jsonl(&mut jsonl, want_jsonl_path.as_deref(), &dsp_tx);
                ensure_udp(&mut udp, want_udp_addr.as_deref(), &dsp_tx);
            }
            AcarsOutputMessage::Decoded(msg) => {
                // Snapshot the config under a brief read lock so we
                // don't hold it across blocking I/O.
                let (want_jsonl_path, want_udp_addr, station_id) = {
                    let cfg = config.read().expect("acars writer config poisoned");
                    (
                        cfg.jsonl_path.clone(),
                        cfg.network_addr.clone(),
                        cfg.station_id.clone(),
                    )
                };

                ensure_jsonl(&mut jsonl, want_jsonl_path.as_deref(), &dsp_tx);
                ensure_udp(&mut udp, want_udp_addr.as_deref(), &dsp_tx);

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
        match JsonlWriter::open(want) {
            Ok(w) => *slot = Some((want.to_path_buf(), w)),
            Err(e) => {
                let message = format!("acars jsonl open failed: {e}");
                tracing::warn!("{message}");
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
        match UdpFeeder::open(want) {
            Ok(f) => *slot = Some((want.to_string(), f)),
            Err(e) => {
                let message = format!("acars udp open failed: {e}");
                tracing::warn!("{message}");
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
mod tests {
    use std::fs::File;
    use std::io::{BufRead, BufReader};
    use std::net::UdpSocket;
    use std::time::{Duration, UNIX_EPOCH};

    use arrayvec::ArrayString;
    use sdr_acars::AcarsMessage;
    use serde_json::Value;
    use tempfile::tempdir;

    use std::sync::{Arc, RwLock, mpsc};

    use super::*;

    fn make_msg(channel: u8) -> AcarsMessage {
        AcarsMessage {
            timestamp: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            channel_idx: channel,
            freq_hz: 131_550_000.0,
            level_db: 10.0,
            error_count: 0,
            mode: b'2',
            label: *b"H1",
            block_id: 0,
            ack: 0x15,
            aircraft: ArrayString::from(".N12345").unwrap(),
            flight_id: None,
            message_no: None,
            text: String::new(),
            end_of_message: true,
            reassembled_block_count: 1,
            parsed: None,
        }
    }

    #[test]
    fn jsonl_writer_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("acars.jsonl");
        let mut writer = JsonlWriter::open(&path).unwrap();
        writer.write(&make_msg(2), Some("STN1")).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let f = File::open(&path).unwrap();
        let mut lines = BufReader::new(f).lines();
        let line = lines.next().unwrap().unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["channel"].as_u64().unwrap(), 2);
        assert_eq!(v["station_id"].as_str().unwrap(), "STN1");
        assert!(lines.next().is_none());
    }

    #[test]
    fn jsonl_writer_appends_across_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("acars.jsonl");
        let mut writer = JsonlWriter::open(&path).unwrap();
        writer.write(&make_msg(0), None).unwrap();
        writer.write(&make_msg(1), None).unwrap();
        writer.write(&make_msg(2), None).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let f = File::open(&path).unwrap();
        let lines: Vec<_> = BufReader::new(f).lines().collect::<Result<_, _>>().unwrap();
        assert_eq!(lines.len(), 3);
        for (i, line) in lines.iter().enumerate() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["channel"].as_u64().unwrap(), i as u64);
        }
    }

    #[test]
    fn jsonl_writer_open_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("subdir").join("acars.jsonl");
        let writer = JsonlWriter::open(&path).unwrap();
        assert!(writer.path() == path);
        assert!(path.exists());
    }

    #[test]
    fn udp_feeder_round_trip() {
        // Bind a listener on loopback ephemeral port, open a
        // feeder pointed at it, send one message, recv it,
        // parse the JSON.
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        let listener_addr = listener.local_addr().unwrap();
        let addr_str = format!("127.0.0.1:{}", listener_addr.port());

        let feeder = UdpFeeder::open(&addr_str).unwrap();
        feeder.send(&make_msg(2), Some("STN1")).unwrap();

        let mut buf = vec![0u8; 4096];
        let (n, _from) = listener.recv_from(&mut buf).unwrap();
        let payload = std::str::from_utf8(&buf[..n]).unwrap();
        // Strip trailing newline.
        let json_str = payload.trim_end_matches('\n');
        let v: Value = serde_json::from_str(json_str).unwrap();
        assert_eq!(v["channel"].as_u64().unwrap(), 2);
        assert_eq!(v["station_id"].as_str().unwrap(), "STN1");
        assert_eq!(feeder.addr_str(), &addr_str);
    }

    #[test]
    fn udp_feeder_open_invalid_addr_errors() {
        // Missing port.
        assert!(UdpFeeder::open("not-a-host").is_err());
        // Invalid port.
        assert!(UdpFeeder::open("127.0.0.1:notaport").is_err());
        // Unresolvable host.
        // Use .invalid TLD per RFC 6761 — guaranteed to never resolve.
        assert!(UdpFeeder::open("nonexistent.invalid:5550").is_err());
    }

    #[test]
    fn writer_thread_exits_on_disconnect() {
        // Spawn a writer thread, drop the sender, assert the
        // thread joins within a short timeout. Exercises the
        // recv() returning Err(Disconnected) → loop break path.
        let config = Arc::new(RwLock::new(AcarsWriterConfig::default()));
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(8);
        let (dummy_dsp_tx, _dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
        let handle = std::thread::spawn(move || {
            run_writer_loop(rx, Arc::clone(&config), dummy_dsp_tx);
        });
        drop(tx);
        // Loop should exit promptly. Allow up to 500 ms for
        // schedulability under loaded test workers.
        let start = std::time::Instant::now();
        while !handle.is_finished() && start.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "writer thread did not exit within 500ms of tx drop"
        );
        handle.join().expect("writer thread panicked");
    }

    #[test]
    fn try_send_drops_when_channel_full() {
        // Build an AcarsOutputs against a tiny channel cap (8)
        // by spawning *no* worker — leave the receiver dangling
        // so the channel fills from the first send. The 9th
        // try_send should drop.
        //
        // `AcarsOutputs::with_capacity` is a test-visible
        // constructor that lets tests use a smaller cap than
        // the production 256.
        let outputs = AcarsOutputs::with_capacity_for_test(8);

        for _ in 0..8 {
            assert!(outputs.try_send(make_msg(0)));
        }
        // 9th send: channel full, drop returns false, counter
        // increments.
        assert!(!outputs.try_send(make_msg(0)));
        assert_eq!(outputs.drop_count(), 1);
    }

    #[test]
    fn writer_reopens_on_path_change() {
        // Pump message → path A; switch config to path B; pump
        // message → path B. Assert both files exist with the
        // expected line count.
        let dir = tempdir().unwrap();
        let path_a = dir.path().join("a.jsonl");
        let path_b = dir.path().join("b.jsonl");

        let config = Arc::new(RwLock::new(AcarsWriterConfig {
            jsonl_path: Some(path_a.clone()),
            network_addr: None,
            station_id: None,
        }));
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(8);
        let (dummy_dsp_tx, _dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
        let handle = {
            let config = Arc::clone(&config);
            std::thread::spawn(move || run_writer_loop(rx, config, dummy_dsp_tx))
        };

        tx.send(AcarsOutputMessage::Decoded(make_msg(0))).unwrap();

        // Spin briefly to let the writer process the first
        // message before we mutate the path.
        std::thread::sleep(Duration::from_millis(50));

        config.write().unwrap().jsonl_path = Some(path_b.clone());
        tx.send(AcarsOutputMessage::Decoded(make_msg(1))).unwrap();

        // Drop tx → thread exits; flush on Drop ensures the
        // BufWriter contents land on disk before we read.
        drop(tx);
        handle.join().expect("writer thread panicked");

        let read_lines = |p: &Path| -> Vec<String> {
            let f = File::open(p).unwrap();
            BufReader::new(f).lines().collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(read_lines(&path_a).len(), 1, "path A got the first message");
        assert_eq!(
            read_lines(&path_b).len(),
            1,
            "path B got the second message"
        );
    }

    #[test]
    fn config_changed_signal_wakes_idle_writer() {
        // Verifies the CR round 1 fix on PR #598: send
        // ConfigChanged with no preceding Decoded; the worker
        // re-snapshots config and calls ensure_jsonl, which
        // opens the file in append mode and creates it. Without
        // the fix the worker would only wake on Decoded and the
        // file would never appear.
        let dir = tempdir().unwrap();
        let path_a = dir.path().join("idle_open.jsonl");

        let config = Arc::new(RwLock::new(AcarsWriterConfig {
            jsonl_path: Some(path_a.clone()),
            network_addr: None,
            station_id: None,
        }));
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(8);
        let (dummy_dsp_tx, _dsp_rx) = mpsc::channel::<crate::messages::DspToUi>();
        let handle = {
            let config = Arc::clone(&config);
            std::thread::spawn(move || run_writer_loop(rx, config, dummy_dsp_tx))
        };

        // No Decoded — only ConfigChanged. The worker must wake,
        // resnap config, and open path_a. JsonlWriter::open in
        // append-mode creates the file even with no writes.
        tx.send(AcarsOutputMessage::ConfigChanged).unwrap();

        // Spin briefly to let the worker process ConfigChanged.
        let start = std::time::Instant::now();
        while !path_a.exists() && start.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            path_a.exists(),
            "ConfigChanged should have caused the writer to open path A even with no Decoded messages"
        );

        // Clean shutdown via Shutdown sentinel — exercises the
        // explicit-shutdown arm.
        tx.send(AcarsOutputMessage::Shutdown).unwrap();
        drop(tx);
        handle.join().expect("writer thread panicked");
    }
}
