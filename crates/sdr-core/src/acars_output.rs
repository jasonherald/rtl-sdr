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
use std::sync::{Arc, RwLock, mpsc};

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

/// Output-writer bundle owned by `DspState`. Keeps the JSONL
/// writer, UDP feeder, station ID, and per-writer warn-rate-
/// limit timestamps together so the `acars_decode_tap`
/// signature stays narrow. Issue #578. Async refactor in
/// progress per #596 — fields will migrate to a worker
/// thread + shared config lock in subsequent tasks.
pub struct AcarsOutputs {
    pub jsonl: Option<JsonlWriter>,
    pub udp: Option<UdpFeeder>,
    pub jsonl_enabled: bool,
    pub network_enabled: bool,
    pub station_id: Option<String>,
    pub jsonl_warn_at: Option<std::time::Instant>,
    pub udp_warn_at: Option<std::time::Instant>,
    pub pending_jsonl_path: Option<String>,
    pub pending_network_addr: Option<String>,
}

impl AcarsOutputs {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            jsonl: None,
            udp: None,
            jsonl_enabled: false,
            network_enabled: false,
            station_id: None,
            jsonl_warn_at: None,
            udp_warn_at: None,
            pending_jsonl_path: None,
            pending_network_addr: None,
        }
    }
}

impl Default for AcarsOutputs {
    fn default() -> Self {
        Self::new()
    }
}

/// Writer-thread main loop. Owns the per-thread `JsonlWriter`
/// and `UdpFeeder` instances, reads `config` on each message
/// to detect path/addr changes, and exits cleanly when the
/// sender side disconnects (app shutdown). Issue #596.
#[allow(dead_code)] // used from tests; caller site lands in Task 5
#[allow(clippy::needless_pass_by_value)] // rx must be owned to observe disconnect
fn run_writer_loop(rx: mpsc::Receiver<AcarsOutputMessage>, config: Arc<RwLock<AcarsWriterConfig>>) {
    let mut jsonl: Option<(PathBuf, JsonlWriter)> = None;
    let mut udp: Option<(String, UdpFeeder)> = None;
    let mut jsonl_warn_at: Option<std::time::Instant> = None;
    let mut udp_warn_at: Option<std::time::Instant> = None;

    while let Ok(msg) = rx.recv() {
        let AcarsOutputMessage::Decoded(msg) = msg;

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

        ensure_jsonl(&mut jsonl, want_jsonl_path.as_deref());
        ensure_udp(&mut udp, want_udp_addr.as_deref());

        if let Some((_, w)) = jsonl.as_mut()
            && let Err(e) = w.write(&msg, station_id.as_deref())
        {
            rate_limited_warn("jsonl", &mut jsonl_warn_at, &e);
        }
        if let Some((_, f)) = udp.as_mut()
            && let Err(e) = f.send(&msg, station_id.as_deref())
        {
            rate_limited_warn("udp", &mut udp_warn_at, &e);
        }
    }
}

/// Ensure `slot` holds an open `JsonlWriter` matching `want`.
/// Reopens on path change; closes (drops) when `want` is `None`.
fn ensure_jsonl(slot: &mut Option<(PathBuf, JsonlWriter)>, want: Option<&Path>) {
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
            Err(e) => tracing::warn!("acars jsonl open failed: {e}"),
        }
    }
}

/// Same shape as `ensure_jsonl` but for `UdpFeeder`. The `String`
/// key compares the user-set addr verbatim; resolved peer
/// addresses are not the source of truth.
fn ensure_udp(slot: &mut Option<(String, UdpFeeder)>, want: Option<&str>) {
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
            Err(e) => tracing::warn!("acars udp open failed: {e}"),
        }
    }
}

const ACARS_OUTPUT_WARN_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Emit a `tracing::warn!` at most once per
/// `ACARS_OUTPUT_WARN_MIN_INTERVAL` for `kind`. Mirrors the
/// per-writer 30 s rate-limit that previously lived in
/// `controller.rs::acars_decode_tap`.
fn rate_limited_warn(kind: &str, last: &mut Option<std::time::Instant>, err: &std::io::Error) {
    let now = std::time::Instant::now();
    let elapsed = last.map_or(ACARS_OUTPUT_WARN_MIN_INTERVAL, |t| now.duration_since(t));
    if elapsed >= ACARS_OUTPUT_WARN_MIN_INTERVAL {
        tracing::warn!("acars {kind} write/send failed: {err} (rate-limited 30s)");
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
        let handle = std::thread::spawn(move || {
            run_writer_loop(rx, Arc::clone(&config));
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
        let handle = {
            let config = Arc::clone(&config);
            std::thread::spawn(move || run_writer_loop(rx, config))
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
}
