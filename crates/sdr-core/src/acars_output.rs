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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs::File;
    use std::io::{BufRead, BufReader};
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
}
