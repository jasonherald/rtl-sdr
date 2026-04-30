# ACARS Output Formatters Implementation Plan (issue #578, v1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pipe each decoded `AcarsMessage` into two persistent destinations: an append-only JSONL file at `~/sdr-recordings/acars.jsonl` and a UDP JSON datagram feeder pointed at airframes.io (or any user-specified host:port). Both off by default, both toggled from a new "Output" preferences group on the Aviation activity panel.

**Architecture:** Pure JSON serializer in `sdr-acars` (no I/O). I/O writers (`JsonlWriter` + `UdpFeeder`) in `sdr-core` next to `controller.rs`. Five new `UiToDsp` commands wire toggle/path/addr changes into the controller's `DspState`. Synchronous I/O in the DSP thread (BufWriter + UDP `send_to`); per-message warn rate-limit (30 s/writer) prevents log spam from a misconfigured feeder.

**Tech Stack:** Rust 2024, `serde_json` (workspace dep), `BufWriter<File>`, `UdpSocket`. No new crates.

**Branch:** `feat/acars-output-formatters` (already checked out, off `main` at `b49d9d4`). Single bundled PR. ~600 LOC target.

**Spec:** `docs/superpowers/specs/2026-04-30-acars-output-formatters-design.md`

**C reference:** `original/acarsdec/output.c::buildjson` (lines 227-323) for JSON schema; `original/acarsdec/netout.c::Netoutjson` for UDP feeder protocol.

---

## File Structure

```text
crates/sdr-acars/src/
├── json.rs                           ← NEW (~250 LOC incl. tests)
├── lib.rs                            ← +1 mod, +1 re-export

crates/sdr-core/src/
├── acars_output.rs                   ← NEW (~250 LOC incl. tests)
├── controller.rs                     ← +~80 LOC
├── messages.rs                       ← +5 enum variants
├── lib.rs                            ← +1 mod (acars_output)

crates/sdr-ui/src/
├── acars_config.rs                   ← +~120 LOC
├── sidebar/aviation_panel.rs         ← +~80 LOC
├── window.rs                         ← +~60 LOC
```

**Workspace gates** (run after every task that touches Rust code):

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-acars              # for json.rs-only tasks
cargo test -p sdr-core               # for acars_output / controller tasks
cargo test --workspace --features sdr-transcription/whisper-cpu  # for cross-crate tasks
cargo fmt --all -- --check
```

---

## Task 1: Scaffold `json.rs` + serializer for basic uplink fields

**Files:**
- Create: `crates/sdr-acars/src/json.rs`
- Modify: `crates/sdr-acars/src/lib.rs`

This task lands the public API + the simplest message path: timestamp, channel, freq, level, error, mode, label, tail, app. Block-ID-gated and downlink fields land in Task 2; text/end/station/reassembled in Task 3; OOOI in Task 4.

- [ ] **Step 1.1: Create `json.rs` skeleton**

```rust
//! JSON serializer for `AcarsMessage`. Pure data → string, no
//! I/O. Schema mirrors `original/acarsdec/output.c::buildjson`
//! (lines 227-323) verbatim where fields overlap, plus one
//! extension field (`reassembled_blocks`) for our multi-block
//! reassembly count.
//!
//! Issue #578. Spec at
//! `docs/superpowers/specs/2026-04-30-acars-output-formatters-design.md`.
//!
//! Used by `crates/sdr-core/src/acars_output.rs`'s `JsonlWriter`
//! and `UdpFeeder`. Stays in `sdr-acars` (the pure-DSP crate)
//! so no I/O sneaks into the data path; the writers in
//! `sdr-core` own the actual file handles + sockets.

use std::time::UNIX_EPOCH;

use serde_json::{Map, Value};

use crate::frame::AcarsMessage;

/// Serialize one `AcarsMessage` to a single-line JSON string.
/// No trailing newline — caller appends `\n` for JSONL writes
/// or UDP framing.
///
/// `station_id` is the operator-chosen identifier embedded in
/// the JSON's `station_id` field. Pass `None` (or `Some("")`)
/// to omit it from the output.
#[must_use]
pub fn serialize_message(msg: &AcarsMessage, station_id: Option<&str>) -> String {
    let mut obj = Map::new();

    // Unix timestamp as fractional seconds.
    let ts = msg
        .timestamp
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    obj.insert("timestamp".to_string(), Value::from(ts));

    // station_id — omit when None or empty.
    if let Some(id) = station_id.filter(|s| !s.is_empty()) {
        obj.insert("station_id".to_string(), Value::from(id));
    }

    obj.insert("channel".to_string(), Value::from(msg.channel_idx));
    obj.insert("freq".to_string(), Value::from(msg.freq_hz / 1e6));
    obj.insert("level".to_string(), Value::from(msg.level_db));
    obj.insert("error".to_string(), Value::from(msg.error_count));
    obj.insert(
        "mode".to_string(),
        Value::from(byte_to_string(msg.mode)),
    );
    obj.insert(
        "label".to_string(),
        Value::from(label_to_string(&msg.label)),
    );
    obj.insert("tail".to_string(), Value::from(msg.aircraft.as_str()));

    // App identity — acarsdec emits "acarsdec"; we emit our
    // own crate name + version so downstream consumers can
    // distinguish.
    let mut app = Map::new();
    app.insert("name".to_string(), Value::from("sdr-rs"));
    app.insert(
        "ver".to_string(),
        Value::from(env!("CARGO_PKG_VERSION")),
    );
    obj.insert("app".to_string(), Value::Object(app));

    Value::Object(obj).to_string()
}

/// Format a single byte as a 1-char string. ACARS payloads
/// are 7-bit ASCII so the cast is faithful; non-ASCII bytes
/// would still produce a valid (if odd) Unicode codepoint.
fn byte_to_string(b: u8) -> String {
    let c = b as char;
    let mut s = String::with_capacity(1);
    s.push(c);
    s
}

/// Format the 2-byte label as a 2-char string.
fn label_to_string(label: &[u8; 2]) -> String {
    let mut s = String::with_capacity(2);
    s.push(label[0] as char);
    s.push(label[1] as char);
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use arrayvec::ArrayString;
    use serde_json::Value;

    use super::*;
    use crate::frame::AcarsMessage;

    /// Build a minimal `AcarsMessage` for tests — uplink, no
    /// downlink fields, empty text, no OOOI, single-block.
    fn make_uplink_msg() -> AcarsMessage {
        AcarsMessage {
            timestamp: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            channel_idx: 2,
            freq_hz: 131_550_000.0,
            level_db: 12.0,
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
    fn serializes_minimal_uplink_message() {
        let msg = make_uplink_msg();
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(v["timestamp"].as_f64().unwrap(), 1_700_000_000.0);
        assert_eq!(v["channel"].as_u64().unwrap(), 2);
        assert!((v["freq"].as_f64().unwrap() - 131.55).abs() < 1e-6);
        assert!((v["level"].as_f64().unwrap() - 12.0).abs() < 1e-6);
        assert_eq!(v["error"].as_u64().unwrap(), 0);
        assert_eq!(v["mode"].as_str().unwrap(), "2");
        assert_eq!(v["label"].as_str().unwrap(), "H1");
        assert_eq!(v["tail"].as_str().unwrap(), ".N12345");
        assert_eq!(v["app"]["name"].as_str().unwrap(), "sdr-rs");
        assert!(v["app"]["ver"].is_string());
        // Fields not yet implemented should not be present.
        assert!(v.get("station_id").is_none());
        assert!(v.get("block_id").is_none());
        assert!(v.get("flight").is_none());
        assert!(v.get("text").is_none());
    }

    #[test]
    fn omits_station_id_when_none_or_empty() {
        let msg = make_uplink_msg();
        let out_none = serialize_message(&msg, None);
        let out_empty = serialize_message(&msg, Some(""));
        let v_none: Value = serde_json::from_str(&out_none).unwrap();
        let v_empty: Value = serde_json::from_str(&out_empty).unwrap();
        assert!(v_none.get("station_id").is_none());
        assert!(v_empty.get("station_id").is_none());
    }

    #[test]
    fn includes_station_id_when_set() {
        let msg = make_uplink_msg();
        let out = serialize_message(&msg, Some("ABCD"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["station_id"].as_str().unwrap(), "ABCD");
    }
}
```

- [ ] **Step 1.2: Wire module + re-export in `lib.rs`**

Find the existing `pub mod` block in `crates/sdr-acars/src/lib.rs` (after `pub mod frame;`). Insert `pub mod json;` in alphabetical order:

```rust
pub mod frame;
pub mod json;
pub mod label;
```

And add the re-export after `pub use frame::{AcarsMessage, FrameParser};`:

```rust
pub use frame::{AcarsMessage, FrameParser};
pub use json::serialize_message as serialize_acars_json;
```

(Re-exporting under a more specific name avoids collision risk if other crates add their own `serialize_message` later.)

- [ ] **Step 1.3: Run gates**

```bash
cargo test -p sdr-acars json
cargo clippy -p sdr-acars --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 3 tests pass (`json::tests::serializes_minimal_uplink_message`, `omits_station_id_when_none_or_empty`, `includes_station_id_when_set`), clippy + fmt clean.

- [ ] **Step 1.4: Commit**

```bash
git add crates/sdr-acars/src/json.rs crates/sdr-acars/src/lib.rs
git commit -m "feat(sdr-acars): scaffold JSON serializer (basic fields)

Issue #578. New crates/sdr-acars/src/json.rs with public
serialize_message(msg, station_id) -> String. Schema mirrors
acarsdec output.c::buildjson lines 227-323. This task lands
the basic field set (timestamp, channel, freq, level, error,
mode, label, tail, app); block-ID-gated, OOOI, and reassembly
extension fields land in subsequent tasks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Block-ID-gated downlink fields (block_id, ack, flight, msgno)

**Files:**
- Modify: `crates/sdr-acars/src/json.rs`

acarsdec's logic (`output.c:257-272`): when `bid != 0`, emit `block_id` + `ack` (or `false` when ack == `'!'`); on downlink (which our parser already gates by populating `flight_id` / `message_no`) emit `flight` + `msgno`.

- [ ] **Step 2.1: Extend serializer**

In `serialize_message`, after the `tail` insertion and before `app`, add:

```rust
    // block_id — omit when 0 (no-bid uplinks). When present,
    // emit ack as `false` if `'!'`, else as 1-char string.
    if msg.block_id != 0 {
        obj.insert(
            "block_id".to_string(),
            Value::from(byte_to_string(msg.block_id)),
        );
        if msg.ack == b'!' {
            obj.insert("ack".to_string(), Value::from(false));
        } else {
            obj.insert(
                "ack".to_string(),
                Value::from(byte_to_string(msg.ack)),
            );
        }
    }

    // Downlink-only fields. Our parser populates these only
    // for downlink blocks, so the Some-check is the natural
    // gate.
    if let Some(f) = &msg.flight_id {
        obj.insert("flight".to_string(), Value::from(f.as_str()));
    }
    if let Some(n) = &msg.message_no {
        obj.insert("msgno".to_string(), Value::from(n.as_str()));
    }
```

- [ ] **Step 2.2: Add 4 tests** (append to `tests` module)

```rust
    fn make_downlink_msg() -> AcarsMessage {
        let mut m = make_uplink_msg();
        m.block_id = b'1';
        m.ack = b'\x15';
        m.flight_id = Some(ArrayString::from("UA1234").unwrap());
        m.message_no = Some(ArrayString::from("M01A").unwrap());
        m.text = "REPORT".to_string();
        m
    }

    #[test]
    fn serializes_full_downlink_message() {
        let msg = make_downlink_msg();
        let out = serialize_message(&msg, Some("STN1"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["block_id"].as_str().unwrap(), "1");
        assert_eq!(v["ack"].as_str().unwrap(), "\x15");
        assert_eq!(v["flight"].as_str().unwrap(), "UA1234");
        assert_eq!(v["msgno"].as_str().unwrap(), "M01A");
        assert_eq!(v["station_id"].as_str().unwrap(), "STN1");
    }

    #[test]
    fn omits_block_id_and_ack_when_block_id_zero() {
        let mut msg = make_downlink_msg();
        msg.block_id = 0;
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("block_id").is_none());
        assert!(v.get("ack").is_none());
    }

    #[test]
    fn ack_serializes_as_false_when_bang() {
        let mut msg = make_downlink_msg();
        msg.ack = b'!';
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ack"], Value::from(false));
    }

    #[test]
    fn omits_flight_and_msgno_for_uplink() {
        let msg = make_uplink_msg(); // flight_id, message_no = None
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("flight").is_none());
        assert!(v.get("msgno").is_none());
    }
```

- [ ] **Step 2.3: Run gates**

```bash
cargo test -p sdr-acars json
cargo clippy -p sdr-acars --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 7 tests pass (3 prior + 4 new).

- [ ] **Step 2.4: Commit**

```bash
git add crates/sdr-acars/src/json.rs
git commit -m "feat(sdr-acars): JSON serializer block_id/ack/flight/msgno fields

Issue #578. Mirrors acarsdec output.c:257-272: block_id +
ack only when bid != 0; ack serialized as false when '!';
flight/msgno only when populated by the parser (which gates
on downlink internally).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Text, end, and reassembled-blocks extension

**Files:**
- Modify: `crates/sdr-acars/src/json.rs`

- [ ] **Step 3.1: Extend serializer**

In `serialize_message`, after the `msgno` block and before `app`, add:

```rust
    // text — omit when empty.
    if !msg.text.is_empty() {
        obj.insert("text".to_string(), Value::from(msg.text.as_str()));
    }

    // end — emit only when the closing byte was ETX (final
    // block).
    if msg.end_of_message {
        obj.insert("end".to_string(), Value::from(true));
    }

    // Our extension: reassembled multi-block count, only when
    // > 1 (single-block messages are the default and don't
    // need to surface the count). airframes.io ignores unknown
    // fields.
    if msg.reassembled_block_count > 1 {
        obj.insert(
            "reassembled_blocks".to_string(),
            Value::from(msg.reassembled_block_count),
        );
    }
```

- [ ] **Step 3.2: Add 3 tests**

```rust
    #[test]
    fn omits_empty_text_field() {
        let msg = make_uplink_msg(); // text is empty
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("text").is_none());
    }

    #[test]
    fn end_field_only_when_end_of_message() {
        let mut msg = make_uplink_msg();
        msg.end_of_message = false;
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("end").is_none());

        msg.end_of_message = true;
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["end"], Value::from(true));
    }

    #[test]
    fn reassembled_blocks_field_only_when_gt_one() {
        let mut msg = make_uplink_msg();
        msg.reassembled_block_count = 1;
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("reassembled_blocks").is_none());

        msg.reassembled_block_count = 3;
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["reassembled_blocks"].as_u64().unwrap(), 3);
    }
```

- [ ] **Step 3.3: Run gates**

```bash
cargo test -p sdr-acars json
cargo clippy -p sdr-acars --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 10 tests pass.

- [ ] **Step 3.4: Commit**

```bash
git add crates/sdr-acars/src/json.rs
git commit -m "feat(sdr-acars): JSON serializer text/end/reassembled fields

Issue #578. text omitted when empty; end emitted only when
end_of_message (matches acarsdec's '0x17 → end: true' rule);
reassembled_blocks is our airframes.io-compatible extension
field, emitted only when count > 1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: OOOI fields (depa, dsta, eta, gtout, gtin, wloff, wlin)

**Files:**
- Modify: `crates/sdr-acars/src/json.rs`

acarsdec's mapping from `oooi_t` → JSON keys:

| `Oooi` field (Rust) | JSON key |
|---|---|
| `sa` | `depa` |
| `da` | `dsta` |
| `eta` | `eta` |
| `gout` | `gtout` |
| `gin` | `gtin` |
| `woff` | `wloff` |
| `won` | `wlin` |

Each emitted only when `Some`.

- [ ] **Step 4.1: Extend serializer**

In `serialize_message`, after the `reassembled_blocks` block and before `app`, add:

```rust
    // OOOI metadata — emit each present Oooi field under its
    // acarsdec JSON key. Mirrors output.c:281-294.
    if let Some(oooi) = &msg.parsed {
        if let Some(v) = &oooi.sa {
            obj.insert("depa".to_string(), Value::from(v.as_str()));
        }
        if let Some(v) = &oooi.da {
            obj.insert("dsta".to_string(), Value::from(v.as_str()));
        }
        if let Some(v) = &oooi.eta {
            obj.insert("eta".to_string(), Value::from(v.as_str()));
        }
        if let Some(v) = &oooi.gout {
            obj.insert("gtout".to_string(), Value::from(v.as_str()));
        }
        if let Some(v) = &oooi.gin {
            obj.insert("gtin".to_string(), Value::from(v.as_str()));
        }
        if let Some(v) = &oooi.woff {
            obj.insert("wloff".to_string(), Value::from(v.as_str()));
        }
        if let Some(v) = &oooi.won {
            obj.insert("wlin".to_string(), Value::from(v.as_str()));
        }
    }
```

- [ ] **Step 4.2: Add 1 test**

```rust
    #[test]
    fn oooi_fields_appear_when_parsed_some() {
        use crate::label_parsers::Oooi;

        let mut msg = make_downlink_msg();
        msg.parsed = Some(Oooi {
            sa: Some(ArrayString::from("KORD").unwrap()),
            da: Some(ArrayString::from("KSFO").unwrap()),
            eta: Some(ArrayString::from("0830").unwrap()),
            gout: Some(ArrayString::from("0700").unwrap()),
            gin: None,
            woff: Some(ArrayString::from("0715").unwrap()),
            won: Some(ArrayString::from("1015").unwrap()),
        });
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["depa"].as_str().unwrap(), "KORD");
        assert_eq!(v["dsta"].as_str().unwrap(), "KSFO");
        assert_eq!(v["eta"].as_str().unwrap(), "0830");
        assert_eq!(v["gtout"].as_str().unwrap(), "0700");
        assert_eq!(v["wloff"].as_str().unwrap(), "0715");
        assert_eq!(v["wlin"].as_str().unwrap(), "1015");
        assert!(v.get("gtin").is_none()); // gin was None
    }

    #[test]
    fn oooi_fields_omitted_when_parsed_none() {
        let msg = make_uplink_msg();
        assert!(msg.parsed.is_none());
        let out = serialize_message(&msg, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        for key in ["depa", "dsta", "eta", "gtout", "gtin", "wloff", "wlin"] {
            assert!(v.get(key).is_none(), "{key} should be absent");
        }
    }
```

- [ ] **Step 4.3: Run gates**

```bash
cargo test -p sdr-acars json
cargo clippy -p sdr-acars --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 12 tests pass (10 prior + 2 new). The serializer is now complete for v1.

- [ ] **Step 4.4: Commit**

```bash
git add crates/sdr-acars/src/json.rs
git commit -m "feat(sdr-acars): JSON serializer OOOI fields (complete v1 schema)

Issue #578. Maps Oooi → acarsdec JSON keys:
sa→depa, da→dsta, eta→eta, gout→gtout, gin→gtin,
woff→wloff, won→wlin. Each emitted only when Some.
Mirrors acarsdec output.c:281-294.

JSON serializer is now feature-complete for v1; writers in
crates/sdr-core/src/acars_output.rs come next.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Scaffold `acars_output.rs` + `JsonlWriter`

**Files:**
- Create: `crates/sdr-core/src/acars_output.rs`
- Modify: `crates/sdr-core/src/lib.rs`

- [ ] **Step 5.1: Create `acars_output.rs` with `JsonlWriter`**

```rust
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
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
        })
    }

    /// Serialize `msg` and append `<json>\n` to the file.
    pub fn write(
        &mut self,
        msg: &AcarsMessage,
        station_id: Option<&str>,
    ) -> io::Result<()> {
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
```

- [ ] **Step 5.2: Wire `pub mod acars_output;` in `crates/sdr-core/src/lib.rs`**

Find the existing `pub mod` block and insert in alphabetical order. Add `pub mod acars_output;` (no re-export — types are referenced via `crate::acars_output::JsonlWriter` from `controller.rs`, kept module-scoped).

- [ ] **Step 5.3: Add `tempfile` to `sdr-core` dev-dependencies if not already present**

Check `crates/sdr-core/Cargo.toml`. If `[dev-dependencies]` doesn't include `tempfile`, add:

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

(`tempfile` should be in `[workspace.dependencies]` already; if not, add `tempfile = "3"` to root `Cargo.toml`.)

- [ ] **Step 5.4: Run gates**

```bash
cargo build -p sdr-core
cargo test -p sdr-core acars_output
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 3 `acars_output::tests` pass, clippy + fmt clean.

- [ ] **Step 5.5: Commit**

```bash
git add crates/sdr-core/src/acars_output.rs crates/sdr-core/src/lib.rs crates/sdr-core/Cargo.toml
git commit -m "feat(sdr-core): scaffold acars_output + JsonlWriter

Issue #578. New crates/sdr-core/src/acars_output.rs with
JsonlWriter (BufWriter<File> append-only, parent-dir
auto-create, drop-flush). UdpFeeder lands in the next task.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `UdpFeeder` (UDP JSON datagram sender)

**Files:**
- Modify: `crates/sdr-core/src/acars_output.rs`

- [ ] **Step 6.1: Add `UdpFeeder` struct + impl**

In `acars_output.rs`, after the `JsonlWriter` `Drop` impl and before the `tests` module, insert:

```rust
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
        let addr = addr_str
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| {
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
    pub fn send(
        &self,
        msg: &AcarsMessage,
        station_id: Option<&str>,
    ) -> io::Result<()> {
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
```

- [ ] **Step 6.2: Add 2 tests** (append to `tests` module)

```rust
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
```

- [ ] **Step 6.3: Run gates**

```bash
cargo test -p sdr-core acars_output
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 5 `acars_output::tests` pass (3 prior + 2 new), clippy + fmt clean.

- [ ] **Step 6.4: Commit**

```bash
git add crates/sdr-core/src/acars_output.rs
git commit -m "feat(sdr-core): UdpFeeder for JSON-over-UDP message forwarding

Issue #578. Mirrors acarsdec netout.c::Netoutjson —
fire-and-forget UDP datagrams of '<json>\\n'. Resolves
host:port at open time; binds an ephemeral local socket
(IPv4 or IPv6 matching the resolved peer family). No
retry, no acks — UDP packet drops are normal.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `UiToDsp` commands + `DspState` `AcarsOutputs` struct

**Files:**
- Modify: `crates/sdr-core/src/messages.rs`
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 7.1: Add 5 `UiToDsp` variants in `messages.rs`**

Find the `UiToDsp` enum (search for `pub enum UiToDsp`). Add the new variants alphabetically grouped with the other ACARS commands (look for `SetAcarsEnabled` / `SetAcarsRegion`):

```rust
    /// Toggle the ACARS JSONL log writer on/off. Issue #578.
    SetAcarsJsonlEnabled(bool),
    /// Update the JSONL log path. Empty string ⇒ default
    /// path (`~/sdr-recordings/acars.jsonl`). Issue #578.
    SetAcarsJsonlPath(String),
    /// Toggle the ACARS UDP JSON feeder on/off. Issue #578.
    SetAcarsNetworkEnabled(bool),
    /// Update the feeder host:port. Issue #578.
    SetAcarsNetworkAddr(String),
    /// Update the operator station ID embedded in the JSON's
    /// `station_id` field. Empty string ⇒ field omitted.
    /// Issue #578.
    SetAcarsStationId(String),
```

- [ ] **Step 7.2: Add `DspToUi::AcarsOutputError` variant in `messages.rs`**

In the same file, find `pub enum DspToUi` and add:

```rust
    /// Surfaces an output writer's open / DNS / I/O error to
    /// the UI for toast display. Sent on `JsonlWriter::open`
    /// or `UdpFeeder::open` failure. Issue #578.
    AcarsOutputError {
        /// `"jsonl"` or `"udp"` — used to scope the toast.
        kind: &'static str,
        /// Human-readable error message (already includes
        /// the file path or host:port).
        message: String,
    },
```

- [ ] **Step 7.3: Add `AcarsOutputs` struct + `DspState` field in `controller.rs`**

In `crates/sdr-core/src/controller.rs`, after the `DspState` struct definition (around line 238, after the existing `acars_*` fields), add a new module-private struct:

```rust
/// Output-writer bundle owned by `DspState`. Keeps the
/// JSONL writer, UDP feeder, station ID, and per-writer
/// warn-rate-limit timestamps together so the
/// `acars_decode_tap` signature stays narrow. Issue #578.
struct AcarsOutputs {
    jsonl: Option<crate::acars_output::JsonlWriter>,
    udp: Option<crate::acars_output::UdpFeeder>,
    station_id: Option<String>,
    /// Last warn timestamp for JSONL write failures. 30 s
    /// rate limit prevents log spam from a misconfigured
    /// path. Issue #578.
    jsonl_warn_at: Option<std::time::Instant>,
    /// Last warn timestamp for UDP send failures.
    udp_warn_at: Option<std::time::Instant>,
}

impl AcarsOutputs {
    const fn new() -> Self {
        Self {
            jsonl: None,
            udp: None,
            station_id: None,
            jsonl_warn_at: None,
            udp_warn_at: None,
        }
    }
}

/// Minimum interval between repeated warn-log emissions for
/// the same writer. Issue #578.
const ACARS_OUTPUT_WARN_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);
```

Add `acars_outputs: AcarsOutputs,` to `DspState` (after the existing `acars_region` field). Add `acars_outputs: AcarsOutputs::new(),` to the `DspState::new` initializer.

- [ ] **Step 7.4: Run gates**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: workspace builds (no behavior changes yet — commands are defined but not handled, struct is empty). Clippy may flag unused fields on `AcarsOutputs`; suppress with `#[allow(dead_code)]` on the struct with a removal note (next task wires the handlers).

- [ ] **Step 7.5: Commit**

```bash
git add crates/sdr-core/src/messages.rs crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): UiToDsp output commands + AcarsOutputs state

Issue #578. Adds 5 SetAcars* output commands + 1
AcarsOutputError DspToUi variant. AcarsOutputs struct
bundles JsonlWriter, UdpFeeder, station ID, and per-writer
warn-rate-limit timestamps. Handlers + tap wiring land in
the next two tasks; #[allow(dead_code)] is removed there.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Command handlers (open / reopen / close on toggle / path / disengage)

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 8.1: Add 5 handler functions**

In `controller.rs`, after the existing `handle_set_acars_enabled` (search for it; lives around line 3200-3400 area), add these five handlers:

```rust
fn handle_set_acars_jsonl_enabled(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, enabled: bool) {
    if !enabled {
        if let Some(mut w) = state.acars_outputs.jsonl.take() {
            if let Err(e) = w.flush() {
                tracing::warn!("acars jsonl flush on disable failed: {e}");
            }
        }
        return;
    }
    // Already open? No-op.
    if state.acars_outputs.jsonl.is_some() {
        return;
    }
    let path = jsonl_path_for(&state.acars_outputs);
    open_jsonl(state, dsp_tx, &path);
}

fn handle_set_acars_jsonl_path(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, path: String) {
    let was_open = state.acars_outputs.jsonl.is_some();
    if let Some(mut w) = state.acars_outputs.jsonl.take() {
        if let Err(e) = w.flush() {
            tracing::warn!("acars jsonl flush on path-change failed: {e}");
        }
    }
    if was_open {
        let resolved = resolve_jsonl_path(&path);
        open_jsonl(state, dsp_tx, &resolved);
    }
    // If JSONL wasn't enabled, defer the open until the user
    // re-enables (the path will be re-read at that moment).
    // Stash the path in AcarsOutputs by keeping the latest
    // string accessible — for now we resolve at open time, so
    // nothing to stash here.
    let _ = path; // path is consulted at next enable via resolve_jsonl_path
}

fn handle_set_acars_network_enabled(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, enabled: bool) {
    if !enabled {
        state.acars_outputs.udp = None;
        return;
    }
    if state.acars_outputs.udp.is_some() {
        return;
    }
    let addr = network_addr_for(&state.acars_outputs);
    open_udp(state, dsp_tx, &addr);
}

fn handle_set_acars_network_addr(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, addr: String) {
    let was_open = state.acars_outputs.udp.is_some();
    state.acars_outputs.udp = None;
    if was_open {
        open_udp(state, dsp_tx, &addr);
    }
    let _ = addr;
}

fn handle_set_acars_station_id(state: &mut DspState, station_id: String) {
    state.acars_outputs.station_id = if station_id.is_empty() {
        None
    } else {
        Some(station_id)
    };
}
```

- [ ] **Step 8.2: Add helper functions**

In the same file, add these helpers before the handlers:

```rust
/// Resolve a JSONL path string. Empty ⇒ default
/// `~/sdr-recordings/acars.jsonl`.
fn resolve_jsonl_path(path: &str) -> std::path::PathBuf {
    if path.is_empty() {
        glib::home_dir()
            .join("sdr-recordings")
            .join("acars.jsonl")
    } else {
        std::path::PathBuf::from(path)
    }
}

/// Default JSONL path for the current `AcarsOutputs`. Today
/// that's always the home-derived default; future work could
/// pull from an `AcarsOutputs::pending_path` field.
fn jsonl_path_for(_outputs: &AcarsOutputs) -> std::path::PathBuf {
    resolve_jsonl_path("")
}

/// Default UDP feeder address for the current `AcarsOutputs`.
/// Today that's always airframes.io's default; future work
/// could pull from an `AcarsOutputs::pending_addr` field.
fn network_addr_for(_outputs: &AcarsOutputs) -> String {
    "feed.airframes.io:5550".to_string()
}

/// Open the JSONL writer; on failure log + emit
/// `DspToUi::AcarsOutputError` toast.
fn open_jsonl(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, path: &std::path::Path) {
    match crate::acars_output::JsonlWriter::open(path) {
        Ok(w) => {
            tracing::info!("acars jsonl writer opened at {}", path.display());
            state.acars_outputs.jsonl = Some(w);
        }
        Err(e) => {
            let message = format!("Could not open {}: {e}", path.display());
            tracing::warn!("acars jsonl open failed: {message}");
            let _ = dsp_tx.send(DspToUi::AcarsOutputError {
                kind: "jsonl",
                message,
            });
        }
    }
}

/// Open the UDP feeder; on failure log + emit
/// `DspToUi::AcarsOutputError` toast.
fn open_udp(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, addr: &str) {
    match crate::acars_output::UdpFeeder::open(addr) {
        Ok(f) => {
            tracing::info!("acars udp feeder opened at {addr}");
            state.acars_outputs.udp = Some(f);
        }
        Err(e) => {
            let message = format!("Could not open feeder at {addr}: {e}");
            tracing::warn!("acars udp open failed: {message}");
            let _ = dsp_tx.send(DspToUi::AcarsOutputError {
                kind: "udp",
                message,
            });
        }
    }
}
```

**Note on path/addr persistence:** The helpers `jsonl_path_for` and `network_addr_for` currently always return defaults. To make `SetAcarsJsonlPath` / `SetAcarsNetworkAddr` actually persist the user's chosen value across enable/disable cycles, add `pending_jsonl_path: Option<String>` and `pending_network_addr: Option<String>` to `AcarsOutputs`, set them in the path/addr handlers, and consult them in the helpers. Apply that refinement now:

```rust
// In AcarsOutputs (Task 7's struct):
struct AcarsOutputs {
    jsonl: Option<crate::acars_output::JsonlWriter>,
    udp: Option<crate::acars_output::UdpFeeder>,
    station_id: Option<String>,
    jsonl_warn_at: Option<std::time::Instant>,
    udp_warn_at: Option<std::time::Instant>,
    /// Latest user-set JSONL path (resolved when opening).
    /// `None` ⇒ use default `~/sdr-recordings/acars.jsonl`.
    pending_jsonl_path: Option<String>,
    /// Latest user-set feeder addr. `None` ⇒ default
    /// `feed.airframes.io:5550`.
    pending_network_addr: Option<String>,
}
```

Update `AcarsOutputs::new` to initialise both `None`. Update the helpers:

```rust
fn jsonl_path_for(outputs: &AcarsOutputs) -> std::path::PathBuf {
    resolve_jsonl_path(outputs.pending_jsonl_path.as_deref().unwrap_or(""))
}

fn network_addr_for(outputs: &AcarsOutputs) -> String {
    outputs
        .pending_network_addr
        .clone()
        .unwrap_or_else(|| "feed.airframes.io:5550".to_string())
}
```

Update path/addr handlers to stash the user's value:

```rust
fn handle_set_acars_jsonl_path(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, path: String) {
    state.acars_outputs.pending_jsonl_path = Some(path.clone());
    let was_open = state.acars_outputs.jsonl.is_some();
    if let Some(mut w) = state.acars_outputs.jsonl.take() {
        if let Err(e) = w.flush() {
            tracing::warn!("acars jsonl flush on path-change failed: {e}");
        }
    }
    if was_open {
        let resolved = resolve_jsonl_path(&path);
        open_jsonl(state, dsp_tx, &resolved);
    }
}

fn handle_set_acars_network_addr(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, addr: String) {
    state.acars_outputs.pending_network_addr = Some(addr.clone());
    let was_open = state.acars_outputs.udp.is_some();
    state.acars_outputs.udp = None;
    if was_open {
        open_udp(state, dsp_tx, &addr);
    }
}
```

- [ ] **Step 8.3: Wire the 5 handlers into the `handle_command` match**

In `controller.rs::handle_command` (around line 884), add the 5 new arms (alphabetically grouped with other `SetAcars*` arms):

```rust
        UiToDsp::SetAcarsJsonlEnabled(enabled) => {
            handle_set_acars_jsonl_enabled(state, dsp_tx, enabled);
        }
        UiToDsp::SetAcarsJsonlPath(path) => {
            handle_set_acars_jsonl_path(state, dsp_tx, path);
        }
        UiToDsp::SetAcarsNetworkEnabled(enabled) => {
            handle_set_acars_network_enabled(state, dsp_tx, enabled);
        }
        UiToDsp::SetAcarsNetworkAddr(addr) => {
            handle_set_acars_network_addr(state, dsp_tx, addr);
        }
        UiToDsp::SetAcarsStationId(id) => {
            handle_set_acars_station_id(state, id);
        }
```

- [ ] **Step 8.4: Add disengage flush hook**

The existing ACARS disengage path lives in `handle_set_acars_enabled` (the `enabled = false` branch). Find that branch and add — right before it returns / restores pre-lock state — a flush + close of the output writers:

```rust
        // Flush + drop output writers. They get reopened on
        // the next `SetAcarsJsonlEnabled(true)` / `SetAcarsNetworkEnabled(true)`
        // command. Issue #578.
        if let Some(mut w) = state.acars_outputs.jsonl.take() {
            if let Err(e) = w.flush() {
                tracing::warn!("acars jsonl flush on disengage failed: {e}");
            }
        }
        state.acars_outputs.udp = None;
```

(Place this block in the disengage branch alongside the `acars_bank = None` / `acars_pre_lock = None` cleanup.)

- [ ] **Step 8.5: Remove `#[allow(dead_code)]` from `AcarsOutputs`** if added in Task 7.

- [ ] **Step 8.6: Run gates**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-core
cargo fmt --all -- --check
```

Expected: workspace builds, all sdr-core tests pass (no new tests in this task — handlers will be exercised in Task 13's smoke + future integration tests).

- [ ] **Step 8.7: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): ACARS output command handlers + lifecycle

Issue #578. Wires the 5 SetAcars* output commands into
handle_command. Open/close/reopen lifecycle:
- Enable ⇒ open with current pending path/addr (default if
  unset)
- Disable ⇒ flush+drop
- Path/addr change while open ⇒ flush+reopen, persisted
  in pending_jsonl_path / pending_network_addr
- ACARS disengage ⇒ flush+drop both writers

Failures emit DspToUi::AcarsOutputError for UI toast
display.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Wire writers into `acars_decode_tap` closure

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

- [ ] **Step 9.1: Extend `acars_decode_tap` signature**

Find `fn acars_decode_tap` (around line 822). Add 3 params at the end (before the body):

```rust
fn acars_decode_tap(
    bank: &mut Option<sdr_acars::ChannelBank>,
    init_failed: &mut bool,
    source_rate_hz: f64,
    center_hz: f64,
    channels: &[f64],
    iq: &[sdr_types::Complex],
    dsp_tx: &mpsc::Sender<DspToUi>,
    outputs: &mut AcarsOutputs,    // NEW
) {
```

(Pass the whole `AcarsOutputs` rather than 5 separate refs — matches the bundle pattern from Task 7.)

- [ ] **Step 9.2: Update the closure**

Replace the existing closure body (around line 875):

```rust
    bank.process(iq_c32, |msg| {
        let _ = dsp_tx.send(crate::messages::DspToUi::AcarsMessage(Box::new(msg)));
    });
```

with:

```rust
    bank.process(iq_c32, |msg| {
        // JSONL write — log warn (rate-limited) on failure.
        if let Some(w) = outputs.jsonl.as_mut() {
            if let Err(e) = w.write(&msg, outputs.station_id.as_deref()) {
                let now = std::time::Instant::now();
                let elapsed = outputs
                    .jsonl_warn_at
                    .map_or(ACARS_OUTPUT_WARN_MIN_INTERVAL, |t| {
                        now.duration_since(t)
                    });
                if elapsed >= ACARS_OUTPUT_WARN_MIN_INTERVAL {
                    tracing::warn!(
                        "acars jsonl write failed (warn-rate-limited 30s): {e}"
                    );
                    outputs.jsonl_warn_at = Some(now);
                }
            }
        }

        // UDP send — same warn pattern.
        if let Some(f) = outputs.udp.as_ref() {
            if let Err(e) = f.send(&msg, outputs.station_id.as_deref()) {
                let now = std::time::Instant::now();
                let elapsed = outputs
                    .udp_warn_at
                    .map_or(ACARS_OUTPUT_WARN_MIN_INTERVAL, |t| {
                        now.duration_since(t)
                    });
                if elapsed >= ACARS_OUTPUT_WARN_MIN_INTERVAL {
                    tracing::warn!(
                        "acars udp send failed (warn-rate-limited 30s): {e}"
                    );
                    outputs.udp_warn_at = Some(now);
                }
            }
        }

        let _ = dsp_tx.send(crate::messages::DspToUi::AcarsMessage(Box::new(msg)));
    });
```

- [ ] **Step 9.3: Update the call site in `process_iq_block`**

Find the existing `acars_decode_tap(...)` call (around line 3531). Add the new arg:

```rust
                    acars_decode_tap(
                        &mut state.acars_bank,
                        &mut state.acars_init_failed,
                        state.sample_rate,
                        state.center_freq,
                        &state.acars_region.channels(),
                        &state.processed_buf[..processed_count],
                        dsp_tx,
                        &mut state.acars_outputs,    // NEW
                    );
```

- [ ] **Step 9.4: Update unit-test call sites in `acars_decode_tap`**

There are two existing test-side calls to `acars_decode_tap` (around lines 4578 and 4600 per the conversation context). Pass a fresh `AcarsOutputs::new()` as the new arg:

```rust
        super::acars_decode_tap(
            &mut bank,
            &mut init_failed,
            // ... existing args ...
            &dsp_tx,
            &mut AcarsOutputs::new(),
        );
```

- [ ] **Step 9.5: Run gates**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-core
cargo fmt --all -- --check
```

Expected: workspace builds, all sdr-core tests pass (the existing acars_decode_tap unit tests get the no-op `AcarsOutputs::new()` and continue to pass).

- [ ] **Step 9.6: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "feat(sdr-core): wire ACARS output writers into decode_tap

Issue #578. acars_decode_tap now takes &mut AcarsOutputs;
the per-message closure writes to the JSONL writer and
UDP feeder when active. Per-writer warn rate-limit at 30s
prevents log spam from a misconfigured destination
(mirrors the PR #586/#588 audio-gating warn-rate-limit
pattern).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Config keys + read/save helpers (`acars_config.rs`)

**Files:**
- Modify: `crates/sdr-ui/src/acars_config.rs`

- [ ] **Step 10.1: Add 5 config keys**

After the existing `pub const KEY_ACARS_*` block in `acars_config.rs`, add:

```rust
/// JSONL writer toggle. Default `false`. Issue #578.
pub const KEY_ACARS_JSONL_ENABLED: &str = "acars_jsonl_enabled";

/// JSONL log file path. Empty ⇒ `~/sdr-recordings/acars.jsonl`
/// at writer-open time. Issue #578.
pub const KEY_ACARS_JSONL_PATH: &str = "acars_jsonl_path";

/// UDP feeder toggle. Default `false`. Issue #578.
pub const KEY_ACARS_NETWORK_ENABLED: &str = "acars_network_enabled";

/// UDP feeder host:port. Default `feed.airframes.io:5550`.
/// Issue #578.
pub const KEY_ACARS_NETWORK_ADDR: &str = "acars_network_addr";

/// Operator station identifier embedded in the JSON's
/// `station_id` field. Empty ⇒ field omitted. Issue #578.
pub const KEY_ACARS_STATION_ID: &str = "acars_station_id";

const DEFAULT_ACARS_JSONL_ENABLED: bool = false;
const DEFAULT_ACARS_NETWORK_ENABLED: bool = false;
const DEFAULT_ACARS_NETWORK_ADDR: &str = "feed.airframes.io:5550";
```

- [ ] **Step 10.2: Add 5 read/save helper pairs**

```rust
#[must_use]
pub fn read_acars_jsonl_enabled(config: &ConfigManager) -> bool {
    config.read(|v| {
        v.get(KEY_ACARS_JSONL_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(DEFAULT_ACARS_JSONL_ENABLED)
    })
}

pub fn save_acars_jsonl_enabled(config: &ConfigManager, value: bool) {
    config.write(|v| {
        v[KEY_ACARS_JSONL_ENABLED] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_jsonl_path(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_ACARS_JSONL_PATH)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    })
}

pub fn save_acars_jsonl_path(config: &ConfigManager, value: &str) {
    config.write(|v| {
        v[KEY_ACARS_JSONL_PATH] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_network_enabled(config: &ConfigManager) -> bool {
    config.read(|v| {
        v.get(KEY_ACARS_NETWORK_ENABLED)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(DEFAULT_ACARS_NETWORK_ENABLED)
    })
}

pub fn save_acars_network_enabled(config: &ConfigManager, value: bool) {
    config.write(|v| {
        v[KEY_ACARS_NETWORK_ENABLED] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_network_addr(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_ACARS_NETWORK_ADDR)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || DEFAULT_ACARS_NETWORK_ADDR.to_string(),
                str::to_string,
            )
    })
}

pub fn save_acars_network_addr(config: &ConfigManager, value: &str) {
    config.write(|v| {
        v[KEY_ACARS_NETWORK_ADDR] = serde_json::json!(value);
    });
}

#[must_use]
pub fn read_acars_station_id(config: &ConfigManager) -> String {
    config.read(|v| {
        v.get(KEY_ACARS_STATION_ID)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    })
}

pub fn save_acars_station_id(config: &ConfigManager, value: &str) {
    config.write(|v| {
        v[KEY_ACARS_STATION_ID] = serde_json::json!(value);
    });
}
```

- [ ] **Step 10.3: Add round-trip tests**

In the existing `tests` module of `acars_config.rs`:

```rust
    #[test]
    fn output_keys_default_when_unset() {
        let cfg = fresh_config();
        assert!(!read_acars_jsonl_enabled(&cfg));
        assert_eq!(read_acars_jsonl_path(&cfg), "");
        assert!(!read_acars_network_enabled(&cfg));
        assert_eq!(read_acars_network_addr(&cfg), "feed.airframes.io:5550");
        assert_eq!(read_acars_station_id(&cfg), "");
    }

    #[test]
    fn output_keys_round_trip() {
        let cfg = fresh_config();
        save_acars_jsonl_enabled(&cfg, true);
        save_acars_jsonl_path(&cfg, "/tmp/foo.jsonl");
        save_acars_network_enabled(&cfg, true);
        save_acars_network_addr(&cfg, "127.0.0.1:5550");
        save_acars_station_id(&cfg, "TEST1");
        assert!(read_acars_jsonl_enabled(&cfg));
        assert_eq!(read_acars_jsonl_path(&cfg), "/tmp/foo.jsonl");
        assert!(read_acars_network_enabled(&cfg));
        assert_eq!(read_acars_network_addr(&cfg), "127.0.0.1:5550");
        assert_eq!(read_acars_station_id(&cfg), "TEST1");
    }
```

- [ ] **Step 10.4: Run gates**

```bash
cargo test -p sdr-ui acars_config
cargo clippy -p sdr-ui --all-targets --features whisper-cpu -- -D warnings
cargo fmt --all -- --check
```

Expected: 5 acars_config tests pass (3 prior + 2 new), clippy + fmt clean.

- [ ] **Step 10.5: Commit**

```bash
git add crates/sdr-ui/src/acars_config.rs
git commit -m "feat(sdr-ui): config keys + helpers for ACARS output settings

Issue #578. 5 new keys (acars_jsonl_enabled / _path,
acars_network_enabled / _addr, acars_station_id) with
read_/save_ helper pairs following the existing pattern.
Defaults: log off, path empty (resolved at writer-open time
to ~/sdr-recordings/acars.jsonl), feeder off,
addr feed.airframes.io:5550, station empty.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Aviation panel "Output" preferences group

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/aviation_panel.rs`

- [ ] **Step 11.1: Add struct fields**

In `aviation_panel.rs`, find the `pub struct AviationPanel` definition. Add 5 new public widget fields after the existing fields:

```rust
    /// Operator station ID — embedded in JSON's
    /// `station_id` field. Issue #578.
    pub station_id_row: adw::EntryRow,
    /// Toggle for the JSONL log writer. Issue #578.
    pub jsonl_enable_row: adw::SwitchRow,
    /// Path entry for the JSONL log. Visible only when
    /// `jsonl_enable_row` is on. Issue #578.
    pub jsonl_path_row: adw::EntryRow,
    /// Toggle for the UDP JSON feeder. Issue #578.
    pub network_enable_row: adw::SwitchRow,
    /// host:port entry for the feeder. Visible only when
    /// `network_enable_row` is on. Issue #578.
    pub network_addr_row: adw::EntryRow,
```

- [ ] **Step 11.2: Build the Output preferences group**

In the `build` (or equivalent) function for `AviationPanel`, after the existing "Channels" preferences group is added to the page, add:

```rust
    // Output preferences group — JSONL log + UDP feeder +
    // station ID. Issue #578.
    let output_group = adw::PreferencesGroup::builder()
        .title("Output")
        .description("Log decoded messages to disk and forward them to external feeders (e.g. airframes.io).")
        .build();

    let station_id_row = adw::EntryRow::builder()
        .title("Station ID")
        .build();
    output_group.add(&station_id_row);

    let jsonl_enable_row = adw::SwitchRow::builder()
        .title("Write JSON log")
        .subtitle("Off")
        .build();
    output_group.add(&jsonl_enable_row);

    let jsonl_path_row = adw::EntryRow::builder()
        .title("Log file path")
        .build();
    jsonl_path_row.set_visible(false);
    output_group.add(&jsonl_path_row);

    let network_enable_row = adw::SwitchRow::builder()
        .title("Forward to network feeder")
        .subtitle("Off")
        .build();
    output_group.add(&network_enable_row);

    let network_addr_row = adw::EntryRow::builder()
        .title("Feeder address")
        .build();
    network_addr_row.set_visible(false);
    output_group.add(&network_addr_row);

    // Visibility binding: path row visible only when JSONL on.
    jsonl_enable_row
        .bind_property("active", &jsonl_path_row, "visible")
        .sync_create()
        .build();
    network_enable_row
        .bind_property("active", &network_addr_row, "visible")
        .sync_create()
        .build();

    page.add(&output_group);
```

- [ ] **Step 11.3: Add the 5 new fields to the `AviationPanel { ... }` constructor return**

Update the constructor's return-struct expression:

```rust
    AviationPanel {
        page,
        // ... existing fields ...
        station_id_row,
        jsonl_enable_row,
        jsonl_path_row,
        network_enable_row,
        network_addr_row,
    }
```

- [ ] **Step 11.4: Run gates**

```bash
cargo build -p sdr-ui --features whisper-cpu
cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: builds cleanly. No new tests in this task — widget construction is verified by the GTK smoke (Task 14).

- [ ] **Step 11.5: Commit**

```bash
git add crates/sdr-ui/src/sidebar/aviation_panel.rs
git commit -m "feat(sdr-ui): Aviation panel Output preferences group

Issue #578. New AdwPreferencesGroup with 5 widgets:
station_id_row (always visible), jsonl_enable + path
(path conditionally visible), network_enable + addr
(addr conditionally visible). Visibility bound to the
toggle's active property via gtk4 property binding.

Signal wiring (notify::active, EntryRow::apply) and
config replay land in Task 12.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Wire signals + DspToUi handlers + config replay

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 12.1: Find the existing `connect_aviation_panel` (or equivalent)**

Search for `panels.aviation` references in `window.rs`. The signal-wiring block is where region selection / enable toggle are connected. Add the 5 new wires.

- [ ] **Step 12.2: Wire `station_id_row::apply` → `SetAcarsStationId`**

```rust
    // Station ID — apply on Enter / focus-out.
    {
        let ui_tx = ui_tx.clone();
        let config = config.clone();
        panels.aviation.station_id_row.connect_apply(move |row| {
            let value = row.text().to_string();
            crate::acars_config::save_acars_station_id(&config, &value);
            let _ = ui_tx.send(UiToDsp::SetAcarsStationId(value));
        });
    }
```

- [ ] **Step 12.3: Wire `jsonl_enable_row` toggle**

```rust
    {
        let ui_tx = ui_tx.clone();
        let config = config.clone();
        let path_row = panels.aviation.jsonl_path_row.clone();
        let switch_row = panels.aviation.jsonl_enable_row.clone();
        switch_row.connect_active_notify(move |row| {
            let active = row.is_active();
            crate::acars_config::save_acars_jsonl_enabled(&config, active);
            let _ = ui_tx.send(UiToDsp::SetAcarsJsonlEnabled(active));
            // Subtitle reflects current path or "Off".
            row.set_subtitle(if active {
                let p = path_row.text();
                if p.is_empty() {
                    "~/sdr-recordings/acars.jsonl"
                } else {
                    p.as_str()
                }
            } else {
                "Off"
            });
        });
    }
```

- [ ] **Step 12.4: Wire `jsonl_path_row::apply`**

```rust
    {
        let ui_tx = ui_tx.clone();
        let config = config.clone();
        panels.aviation.jsonl_path_row.connect_apply(move |row| {
            let value = row.text().to_string();
            crate::acars_config::save_acars_jsonl_path(&config, &value);
            let _ = ui_tx.send(UiToDsp::SetAcarsJsonlPath(value));
        });
    }
```

- [ ] **Step 12.5: Wire `network_enable_row` + `network_addr_row::apply`** (mirror the JSONL pattern)

```rust
    {
        let ui_tx = ui_tx.clone();
        let config = config.clone();
        let addr_row = panels.aviation.network_addr_row.clone();
        let switch_row = panels.aviation.network_enable_row.clone();
        switch_row.connect_active_notify(move |row| {
            let active = row.is_active();
            crate::acars_config::save_acars_network_enabled(&config, active);
            let _ = ui_tx.send(UiToDsp::SetAcarsNetworkEnabled(active));
            row.set_subtitle(if active {
                let a = addr_row.text();
                if a.is_empty() {
                    "feed.airframes.io:5550"
                } else {
                    a.as_str()
                }
            } else {
                "Off"
            });
        });
    }
    {
        let ui_tx = ui_tx.clone();
        let config = config.clone();
        panels.aviation.network_addr_row.connect_apply(move |row| {
            let value = row.text().to_string();
            crate::acars_config::save_acars_network_addr(&config, &value);
            let _ = ui_tx.send(UiToDsp::SetAcarsNetworkAddr(value));
        });
    }
```

- [ ] **Step 12.6: Add config replay at panel build time**

In the panel-build / window-init path where existing acars-config replay happens (e.g. seeding the region combo), add:

```rust
    // Seed widgets from config + dispatch initial state.
    panels
        .aviation
        .station_id_row
        .set_text(&crate::acars_config::read_acars_station_id(&config));
    panels
        .aviation
        .jsonl_path_row
        .set_text(&crate::acars_config::read_acars_jsonl_path(&config));
    panels
        .aviation
        .network_addr_row
        .set_text(&crate::acars_config::read_acars_network_addr(&config));
    panels
        .aviation
        .jsonl_enable_row
        .set_active(crate::acars_config::read_acars_jsonl_enabled(&config));
    panels
        .aviation
        .network_enable_row
        .set_active(crate::acars_config::read_acars_network_enabled(&config));

    // Initial dispatch — give the controller the current
    // values so writers open if persisted as enabled.
    let _ = ui_tx.send(UiToDsp::SetAcarsStationId(
        crate::acars_config::read_acars_station_id(&config),
    ));
    let _ = ui_tx.send(UiToDsp::SetAcarsJsonlPath(
        crate::acars_config::read_acars_jsonl_path(&config),
    ));
    let _ = ui_tx.send(UiToDsp::SetAcarsNetworkAddr(
        crate::acars_config::read_acars_network_addr(&config),
    ));
    let _ = ui_tx.send(UiToDsp::SetAcarsJsonlEnabled(
        crate::acars_config::read_acars_jsonl_enabled(&config),
    ));
    let _ = ui_tx.send(UiToDsp::SetAcarsNetworkEnabled(
        crate::acars_config::read_acars_network_enabled(&config),
    ));
```

- [ ] **Step 12.7: Handle `DspToUi::AcarsOutputError` toast**

In the existing `DspToUi` dispatch match (search for other `DspToUi::Acars*` handlers), add:

```rust
            DspToUi::AcarsOutputError { kind, message } => {
                let toast = adw::Toast::builder()
                    .title(format!("ACARS {kind} output: {message}"))
                    .timeout(5)
                    .build();
                state.toast_overlay.add_toast(toast);
            }
```

- [ ] **Step 12.8: Run gates**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo fmt --all -- --check
```

Expected: workspace builds, all tests pass.

- [ ] **Step 12.9: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(sdr-ui): wire ACARS output panel signals + config replay

Issue #578. Connects station_id / jsonl-enable / jsonl-path /
network-enable / network-addr widgets to UiToDsp commands.
Toggles update subtitles to reflect current path/addr or 'Off'.
Config replay at startup seeds widgets + dispatches initial
state so persisted writers reopen on launch. AcarsOutputError
DspToUi messages surface as 5-second toasts.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Workspace gates

**Files:** none (verification only)

- [ ] **Step 13.1: Full workspace gates**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo fmt --all -- --check
```

Expected: all green.

- [ ] **Step 13.2: Per-crate test counts sanity-check**

```bash
cargo test -p sdr-acars 2>&1 | grep -E "test result"
cargo test -p sdr-core acars_output 2>&1 | grep -E "test result"
cargo test -p sdr-ui acars_config 2>&1 | grep -E "test result"
```

Expected:
- `sdr-acars json::tests` should have 12 tests
- `sdr-core acars_output::tests` should have 5 tests
- `sdr-ui acars_config::tests` should have 5 tests (3 prior + 2 new)
- All pass

- [ ] **Step 13.3: No commit needed for this task** — it's pure verification.

---

## Task 14: Manual GTK smoke (USER ONLY — Claude installs, user tests)

**Files:** none (smoke verification)

- [ ] **Step 14.1: Install the binary**

```bash
make install CARGO_FLAGS="--release --features whisper-cuda"
```

Verify the install succeeded (look for `acars_jsonl_enabled` in the binary's strings):

```bash
strings $HOME/.local/bin/sdr-rs 2>/dev/null | grep -m1 acars_jsonl_enabled || echo "not found"
```

(Expected: prints `acars_jsonl_enabled` once. If not found, the build is stale — re-run install.)

- [ ] **Step 14.2: User smoke checklist** (paste verbatim into the chat)

User runs through the following manually:

1. **Aviation panel renders the Output group** — open Aviation activity (Ctrl+8 or click the airplane icon). Confirm the new "Output" preferences group appears below "Channels" with 5 widgets:
   - Station ID (always visible, empty by default)
   - Write JSON log toggle (off, subtitle "Off")
   - Forward to network feeder toggle (off, subtitle "Off")
2. **JSONL writer happy path** — type "TEST1" into Station ID + press Enter. Toggle Write JSON log on; confirm the path entry appears with default `~/sdr-recordings/acars.jsonl`. Engage ACARS. Confirm `~/sdr-recordings/acars.jsonl` is created and grows by one line per decoded message. Each line parses as JSON (try `tail -1 ~/sdr-recordings/acars.jsonl | python3 -m json.tool`).
3. **Disengage flushes** — disengage ACARS, confirm `tail` of the file shows the most recent message (no truncation from the buffer).
4. **JSONL reopen on path change** — enable the writer, change the path to `/tmp/acars-smoke.jsonl`, press Enter. Re-engage; confirm new path is populated, old path remains untouched.
5. **UDP feeder happy path** — in another terminal: `nc -ulk 5550`. In the app, set Feeder address to `127.0.0.1:5550`, toggle Forward to network feeder on. Engage ACARS; confirm `nc` prints one JSON line per decoded message.
6. **UDP feeder bad address** — set Feeder address to `nonexistent.invalid:5550` and toggle on; confirm a toast appears with `ACARS udp output: Could not open feeder at nonexistent.invalid:5550: …`. Toggle off.
7. **Persistence** — close the app, reopen. Confirm:
   - Station ID still "TEST1"
   - JSONL path still whatever you set it to
   - Both toggles in their last state
   - If JSONL was on at close, it reopens automatically and resumes writing after engage.
8. **Disable ACARS while output enabled** — confirm output writers flush + close; re-engage reopens them.

- [ ] **Step 14.3: User reports back**

User responds with "smoke pass" or specific failures. **STOP HERE — do not proceed to Task 15 until the user reports the smoke pass.**

---

## Task 15: Final pre-push sweep + push branch

**Files:** none (verification + push)

- [ ] **Step 15.1: Final fmt + clippy sweep**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
```

Expected: both clean.

- [ ] **Step 15.2: Verify branch state**

```bash
git status
git log --oneline main..HEAD
```

Expected: clean working tree; ~12-14 commits on the branch (one per task plus any code-review fixes).

- [ ] **Step 15.3: Push branch**

```bash
git push -u origin feat/acars-output-formatters
```

- [ ] **Step 15.4: DO NOT open a PR**

User opens the PR. Stop after push.

---

## Self-review checklist

After writing this plan I cross-checked it against the spec:

- **Spec coverage:**
  - JSON serializer + 12 schema fields → Tasks 1-4 ✓
  - JsonlWriter (open/write/flush/path/Drop) → Task 5 ✓
  - UdpFeeder (open/send/addr_str) → Task 6 ✓
  - 5 UiToDsp commands → Task 7 ✓
  - 1 DspToUi::AcarsOutputError → Task 7 ✓
  - DspState AcarsOutputs struct → Task 7 ✓
  - Command handlers + lifecycle (open/reopen/close on disengage) → Task 8 ✓
  - Tap closure wiring + warn rate-limit → Task 9 ✓
  - 5 config keys + helpers + tests → Task 10 ✓
  - Aviation panel Output group → Task 11 ✓
  - Signal wiring + DspToUi handler + config replay → Task 12 ✓
  - Workspace gates → Task 13 ✓
  - Manual smoke → Task 14 ✓
  - Push → Task 15 ✓
  - Out-of-scope items (MQTT, rotation, TLS, supervisor format, CLI integration, aircraft tab) → not addressed (correct) ✓

- **Placeholder scan:** None of the No-Placeholder patterns appear in the plan body.

- **Type consistency:**
  - `JsonlWriter::open(&Path) -> io::Result<Self>` — used identically in Tasks 5, 8.
  - `JsonlWriter::write(&mut self, &AcarsMessage, Option<&str>) -> io::Result<()>` — used identically in Tasks 5, 9.
  - `UdpFeeder::open(&str) -> io::Result<Self>` — used identically in Tasks 6, 8.
  - `UdpFeeder::send(&self, &AcarsMessage, Option<&str>) -> io::Result<()>` — used identically in Tasks 6, 9.
  - `serialize_message(&AcarsMessage, Option<&str>) -> String` (re-exported as `serialize_acars_json`) — used identically in Tasks 1-4, 5, 6.
  - `AcarsOutputs` struct fields (`jsonl`, `udp`, `station_id`, `jsonl_warn_at`, `udp_warn_at`, `pending_jsonl_path`, `pending_network_addr`) defined in Task 7 step 7.3 + 8.2; consumed in Tasks 8, 9.
  - 5 config-key constants (`KEY_ACARS_JSONL_ENABLED`, etc.) defined in Task 10; consumed in Task 12.

All matches.

- **Test count totals (final state):**
  - sdr-acars: 12 new (json) + 98 existing = 110
  - sdr-core: 5 new (acars_output) + existing
  - sdr-ui: 2 new (acars_config) + existing 3 = 5 total in that test module
