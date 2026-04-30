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
        .map_or(0.0, |d| d.as_secs_f64());
    obj.insert("timestamp".to_string(), Value::from(ts));

    // station_id — omit when None or empty.
    if let Some(id) = station_id.filter(|s| !s.is_empty()) {
        obj.insert("station_id".to_string(), Value::from(id));
    }

    obj.insert("channel".to_string(), Value::from(msg.channel_idx));
    obj.insert("freq".to_string(), Value::from(msg.freq_hz / 1e6));
    obj.insert("level".to_string(), Value::from(msg.level_db));
    obj.insert("error".to_string(), Value::from(msg.error_count));
    obj.insert("mode".to_string(), Value::from(byte_to_string(msg.mode)));
    obj.insert("label".to_string(), Value::from(label_to_string(msg.label)));
    obj.insert("tail".to_string(), Value::from(msg.aircraft.as_str()));

    // App identity — `acarsdec` emits "acarsdec"; we emit our
    // own crate name + version so downstream consumers can
    // distinguish.
    let mut app = Map::new();
    app.insert("name".to_string(), Value::from("sdr-rs"));
    app.insert("ver".to_string(), Value::from(env!("CARGO_PKG_VERSION")));
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
fn label_to_string(label: [u8; 2]) -> String {
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
    #[allow(clippy::float_cmp)]
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
