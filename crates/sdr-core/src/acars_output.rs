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

use sdr_acars::AcarsMessage;

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
}
