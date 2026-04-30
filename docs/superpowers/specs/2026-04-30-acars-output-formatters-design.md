# ACARS Output Formatters (issue #578, v1)

> Pipe decoded ACARS messages out of the in-memory ring into
> two persistent destinations: a local JSONL file and a UDP
> JSON feeder for external aggregators (airframes.io et al.).
> MQTT publisher deferred to a follow-up.

## Goal

Turn the `AcarsMessage` stream — already flowing through
`controller.rs::acars_decode_tap` to the UI viewer — into a
useful long-term data source by:

1. Appending each message as one JSON line to a user-chosen
   file (`~/sdr-recordings/acars.jsonl` by default).
2. Forwarding each message as a UDP datagram (`<json>\n`) to a
   user-chosen `host:port` (defaulting to airframes.io's
   feeder address).

Both destinations off by default. Both are toggled
independently from the Aviation activity panel.

## Non-goals

- **MQTT publisher.** Per the issue body itself, MQTT is "lower
  priority"; deferred to a separate issue.
- **File rotation.** Single rolling JSONL file; users with
  log-rotation needs use logrotate or rename externally.
- **TLS / authentication on the feeder.** airframes.io accepts
  unauthenticated UDP-JSON; v1 matches that.
- **Supervisor / line-format outputs** (acarsdec's `Netoutsv`,
  `Netoutpp`). JSON only.
- **Aircraft-grouped tab.** Owned by issue #579.

## Architecture

### Module split (5 components — 2 new modules + 3 wiring extensions)

```text
crates/sdr-acars/src/
├── json.rs                 ← NEW: pure serializer (no I/O)
crates/sdr-core/src/
├── acars_output.rs         ← NEW: file + UDP writers (owns I/O)
├── controller.rs           ← MODIFY: wire writers into acars_decode_tap
├── messages.rs             ← MODIFY: 5 new UiToDsp commands
crates/sdr-ui/src/
├── acars_config.rs         ← MODIFY: 5 new config keys + helpers
├── sidebar/aviation_panel.rs  ← MODIFY: Output preferences group
├── window.rs               ← MODIFY: connect signals + DspToUi handlers
```

The split honors the workspace rule that `sdr-acars` is a
pure-DSP crate (no I/O, no threading, no sockets). The JSON
serializer is pure data → string and lives in `sdr-acars` so
the `sdr-acars-cli` binary could adopt it later. The writers —
which own a `BufWriter<File>` and a `UdpSocket` — live in
`sdr-core`, alongside `controller.rs` which already owns the
`AcarsMessage` lifecycle.

### Component 1: JSON serializer (`crates/sdr-acars/src/json.rs`)

Single public function:

```rust
/// Serialize one `AcarsMessage` to a single-line JSON string
/// suitable for JSONL writing or UDP feeding. No trailing
/// newline — caller appends `\n` if needed.
///
/// `station_id` is the operator-chosen identifier embedded in
/// the JSON's `station_id` field. Pass `None` to omit it.
#[must_use]
pub fn serialize_message(msg: &AcarsMessage, station_id: Option<&str>) -> String;
```

Uses `serde_json::Value` builder pattern for clarity and to
keep the field-presence rules (e.g. omit `block_id` when 0)
explicit. Library-crate rule applies — no `unwrap`/`panic!`.

#### JSON schema

Mirrors `original/acarsdec/output.c::buildjson` lines 227-323
verbatim where fields overlap, plus one extension field:

| Field | Type | Source | Notes |
|---|---|---|---|
| `timestamp` | f64 | `msg.timestamp.duration_since(UNIX_EPOCH)` (sec.frac) | acarsdec parity |
| `station_id` | string | param, omitted when `None` or `""` | acarsdec parity |
| `channel` | u8 | `msg.channel_idx` | acarsdec parity |
| `freq` | f64 (3 dp) | `msg.freq_hz / 1e6` | acarsdec parity (MHz) |
| `level` | f32 (1 dp) | `msg.level_db` | acarsdec parity |
| `error` | u8 | `msg.error_count` | acarsdec parity |
| `mode` | string (1 char) | `msg.mode` | acarsdec parity |
| `label` | string (2 char) | `msg.label` | acarsdec parity |
| `block_id` | string (1 char) | `msg.block_id`, omitted if `0` | acarsdec parity |
| `ack` | string \| `false` | `msg.ack` (or `false` when `'!'`) | acarsdec parity |
| `tail` | string | `msg.aircraft` | acarsdec parity |
| `flight` | string | `msg.flight_id`, omitted if `None` | acarsdec parity (downlink-only) |
| `msgno` | string | `msg.message_no`, omitted if `None` | acarsdec parity (downlink-only) |
| `text` | string | `msg.text`, omitted if empty | acarsdec parity |
| `end` | `true` | only if `msg.end_of_message` | acarsdec parity |
| `depa` | string | `msg.parsed.sa`, gated on `Some` | acarsdec parity |
| `dsta` | string | `msg.parsed.da`, gated on `Some` | acarsdec parity |
| `eta` | string | `msg.parsed.eta`, gated on `Some` | acarsdec parity |
| `gtout` | string | `msg.parsed.gout`, gated on `Some` | acarsdec parity |
| `gtin` | string | `msg.parsed.gin`, gated on `Some` | acarsdec parity |
| `wloff` | string | `msg.parsed.woff`, gated on `Some` | acarsdec parity |
| `wlin` | string | `msg.parsed.won`, gated on `Some` | acarsdec parity |
| `app` | object | `{ name: "sdr-rs", ver: env!("CARGO_PKG_VERSION") }` | acarsdec uses `"acarsdec"` — we use our own name |
| `reassembled_blocks` | u8 | only if `msg.reassembled_block_count > 1` | our extension; airframes.io ignores unknown fields |

#### Tests

~10 unit tests in `json::tests`:

- `serializes_minimal_uplink_message`
- `serializes_full_downlink_message_with_oooi`
- `omits_empty_text_field`
- `omits_block_id_when_zero`
- `omits_flight_and_msgno_when_uplink`
- `ack_serializes_as_false_when_bang`
- `omits_station_id_when_none_or_empty`
- `oooi_fields_appear_when_parsed_some`
- `end_field_only_when_end_of_message`
- `reassembled_blocks_field_only_when_gt_one`

Each builds a hand-crafted `AcarsMessage`, calls
`serialize_message`, parses the result with `serde_json` and
asserts on field presence + values.

### Component 2: Writers (`crates/sdr-core/src/acars_output.rs`)

Two narrow types, no shared trait (YAGNI — one is a file, one
is a socket; abstraction adds no leverage in v1).

#### `JsonlWriter`

```rust
pub struct JsonlWriter {
    file: BufWriter<File>,
    path: PathBuf,
}

impl JsonlWriter {
    /// Open `path` in append mode (creates parent dirs if
    /// missing). Returns the wrapped writer ready for `write`.
    pub fn open(path: &Path) -> io::Result<Self>;

    /// Serialize `msg` and append one line `<json>\n` to the
    /// file. Caller-friendly: returns `io::Error` on write
    /// failure (caller decides whether to drop the writer or
    /// keep going).
    pub fn write(
        &mut self,
        msg: &AcarsMessage,
        station_id: Option<&str>,
    ) -> io::Result<()>;

    /// Flush the buffered writer. Called on disengage and on
    /// app shutdown.
    pub fn flush(&mut self) -> io::Result<()>;

    /// The path the writer was opened against. Read-only.
    pub fn path(&self) -> &Path;
}
```

`Drop` impl flushes best-effort (logs `tracing::warn!` on
flush failure). Append mode + buffered: ACARS messages are
small (~500 B) and bursty; default `BufWriter` capacity (8 KB)
absorbs a peak burst without each `write` syscalling.

#### `UdpFeeder`

```rust
pub struct UdpFeeder {
    socket: UdpSocket,
    addr: SocketAddr,
    addr_str: String,
}

impl UdpFeeder {
    /// Resolve `host:port` and bind a local ephemeral UDP
    /// socket to send from. The returned feeder caches the
    /// resolved address; if DNS shifts, drop + reopen.
    pub fn open(addr: &str) -> io::Result<Self>;

    /// Serialize `msg`, append `\n`, send one UDP datagram.
    /// Errors are returned but typically logged + ignored at
    /// the call site (UDP packet drops are normal).
    pub fn send(
        &self,
        msg: &AcarsMessage,
        station_id: Option<&str>,
    ) -> io::Result<()>;

    /// The `host:port` string the feeder was opened against.
    pub fn addr_str(&self) -> &str;
}
```

UDP send is sub-millisecond + non-blocking by default. No
retry, no buffering. If the feed endpoint is down, packets
drop on the wire — same behavior as acarsdec's `Netoutjson`.

#### Tests

- `jsonl_writer_round_trip` — open tempfile, write a message,
  reopen as reader, parse JSON, assert fields match.
- `jsonl_writer_appends_across_writes` — write 3 messages,
  read back 3 lines, parse each.
- `jsonl_writer_open_creates_parent_dirs` — pass nested path
  in tempdir, verify dir + file created.
- `udp_feeder_round_trip` — bind a `UdpSocket` on
  `127.0.0.1:0`, open feeder pointing at it, send a message,
  recv on the listener, parse JSON, assert fields match.
- `udp_feeder_open_invalid_addr_errors` — `"not-a-host:port"`
  returns `Err`, doesn't panic.

### Component 3: Controller wiring (`controller.rs`)

#### `DspState` extends with three new fields

```rust
pub(crate) acars_jsonl: Option<crate::acars_output::JsonlWriter>,
pub(crate) acars_udp: Option<crate::acars_output::UdpFeeder>,
pub(crate) acars_station_id: Option<String>,
```

All `None`/`None` by default — populated from config replay at
startup and updated via the new `UiToDsp` commands below.

#### `acars_decode_tap` closure extends

The existing closure (around `controller.rs:875`) currently:

```rust
bank.process(iq_c32, |msg| {
    let _ = dsp_tx.send(DspToUi::AcarsMessage(Box::new(msg)));
});
```

becomes:

```rust
bank.process(iq_c32, |msg| {
    if let Some(w) = jsonl.as_mut() {
        if let Err(e) = w.write(&msg, station_id.as_deref()) {
            tracing::warn!("acars jsonl write failed: {e}");
        }
    }
    if let Some(f) = udp.as_ref() {
        if let Err(e) = f.send(&msg, station_id.as_deref()) {
            tracing::warn!("acars udp send failed: {e}");
        }
    }
    let _ = dsp_tx.send(DspToUi::AcarsMessage(Box::new(msg)));
});
```

The closure captures the writers + station_id by mutable
reference from `acars_decode_tap`'s parameters; the function
signature gains:

```rust
fn acars_decode_tap(
    bank: &mut Option<sdr_acars::ChannelBank>,
    init_failed: &mut bool,
    source_rate_hz: f64,
    center_hz: f64,
    channels: &[f64],
    iq: &[sdr_types::Complex],
    dsp_tx: &mpsc::Sender<DspToUi>,
    jsonl: &mut Option<JsonlWriter>,    // NEW
    udp: &Option<UdpFeeder>,             // NEW
    station_id: &Option<String>,         // NEW
)
```

The single caller (`process_iq_block`) passes the matching
fields from `DspState`.

#### Per-message warn rate-limiting

A misconfigured feeder (e.g. unreachable host) would log per
ACARS message — at ACARS bursty peak that's spammy. Wrap with
a 30 s minimum interval per (warn-source, message-pattern)
key. We already have a rate-limiter pattern from PR #586/#588
(scanner/audio gating logic) — reuse the simple "last warn
SystemTime" mutable in `DspState`:

```rust
acars_jsonl_warn_at: Option<SystemTime>,
acars_udp_warn_at: Option<SystemTime>,
```

Update on warn, suppress further warns within 30 s.

#### Lifecycle

| Event | Effect on writers |
|---|---|
| App startup | Both `None`; commands replay from config |
| `SetAcarsJsonlEnabled(true)` while engaged | Open `JsonlWriter` at configured path; warn + toast on failure |
| `SetAcarsJsonlEnabled(false)` | Flush + drop writer |
| `SetAcarsJsonlPath(p)` while writer open | Flush + reopen at `p`; warn + toast on failure |
| `SetAcarsNetworkEnabled(true)` | Open `UdpFeeder` at configured addr |
| `SetAcarsNetworkEnabled(false)` | Drop feeder |
| `SetAcarsNetworkAddr(s)` while feeder open | Reopen at `s` |
| `SetAcarsStationId(s)` | Update `acars_station_id`; takes effect on next message |
| ACARS disengage / source change / shutdown | Flush + drop both writers |

The flush-drop on disengage is symmetric with the existing
WAV recorder pattern in `controller.rs::handle_set_*`
helpers. No new threads — synchronous calls in the existing
DSP thread.

#### `UiToDsp` commands (5 new)

```rust
SetAcarsJsonlEnabled(bool),
SetAcarsJsonlPath(String),
SetAcarsNetworkEnabled(bool),
SetAcarsNetworkAddr(String),
SetAcarsStationId(String),
```

Each is a single-field update. The path/addr commands trigger
a reopen if the corresponding writer is currently active.

### Component 4: UI panel (`aviation_panel.rs`)

New `AdwPreferencesGroup` "Output" appended after the existing
"Channels" group:

```text
Output
  Station ID:  [_________________________]
  ─────────────────────────────────────────
  [ ] Write JSON log
       Path:  [~/sdr-recordings/acars.jsonl    ]
  ─────────────────────────────────────────
  [ ] Forward to network feeder
       Address:  [feed.airframes.io:5550        ]
```

Widget breakdown:

- `AdwEntryRow` `station_id_row` — always visible, max 8 chars
  (matches acarsdec's `idstation` field convention).
- `AdwSwitchRow` `jsonl_enable_row` — Title "Write JSON log",
  subtitle dynamic ("Off" / "<path>").
- `AdwEntryRow` `jsonl_path_row` — visible only when
  `jsonl_enable_row` is on. Default text:
  `~/sdr-recordings/acars.jsonl`.
- `AdwSwitchRow` `network_enable_row` — Title "Forward to
  network feeder", subtitle dynamic ("Off" / "<addr>").
- `AdwEntryRow` `network_addr_row` — visible only when
  `network_enable_row` is on. Default text:
  `feed.airframes.io:5550`.

Visibility toggles: bind path/addr `set_visible` to their
toggle's `active` property via `gtk4::glib::PropertyExpression`,
matching the existing pattern from PR #587 (ACARS viewer
filter).

#### Signal wiring (in `window.rs::connect_aviation_panel`)

- Toggle `notify::active` → `UiToDsp::SetAcars{Jsonl,Network}Enabled`.
- Entry `apply` (Enter/focus-out) → `UiToDsp::SetAcars{JsonlPath,NetworkAddr,StationId}`.
- Initial seeding from config at panel build time (transient-
  index guard same pattern as the region combo from PR #593).

### Component 5: Config keys (`acars_config.rs`)

Five new keys, default values shown:

```rust
pub const KEY_ACARS_JSONL_ENABLED: &str = "acars_jsonl_enabled";        // false
pub const KEY_ACARS_JSONL_PATH: &str = "acars_jsonl_path";              // ""
pub const KEY_ACARS_NETWORK_ENABLED: &str = "acars_network_enabled";    // false
pub const KEY_ACARS_NETWORK_ADDR: &str = "acars_network_addr";          // "feed.airframes.io:5550"
pub const KEY_ACARS_STATION_ID: &str = "acars_station_id";              // ""
```

Empty `acars_jsonl_path` is interpreted by the writer as the
default path `~/sdr-recordings/acars.jsonl` (computed via
`glib::home_dir().join("sdr-recordings").join("acars.jsonl")`
to match the existing convention from PR #571 etc.).

Five paired `read_*` / `save_*` helper fns following the
existing pattern in `acars_config.rs`.

## Error handling

- **Open failure**: `tracing::warn!` + emit a one-shot `DspToUi`
  toast variant (`AcarsOutputError(kind, message)`) so the
  Aviation panel can surface the failure to the user.
- **Per-message write failure**: `tracing::warn!` rate-limited to
  one warn per 30 s per writer. Don't disable the writer — the
  next message retries (file may have been remounted, network
  may have come back).
- **DNS resolution failure**: Same as open failure — surfaced as
  toast on enable.
- **`Drop` flush failure**: `tracing::warn!` only (Drop can't
  return errors).

## Testing

### Unit tests

Listed per component above. Total ~15 unit tests:
- `json.rs`: 10
- `acars_output.rs`: 5

### Integration tests

Two new tests in `crates/sdr-core/tests/acars_output_integration.rs`:

1. `engage_with_jsonl_writes_messages_to_disk` — full pipeline
   harness (already exists for ACARS engage path), enable
   JSONL, feed synthetic IQ, verify JSONL file has expected
   line count.
2. `engage_with_udp_feeder_sends_packets` — bind a loopback
   UDP listener, enable feeder pointing at it, feed synthetic
   IQ, verify N packets received with parseable JSON.

### Manual smoke (user)

After install:

1. Enable ACARS → "Write JSON log" → confirm
   `~/sdr-recordings/acars.jsonl` populates with one line per
   decode.
2. `nc -ulk 5550` in another terminal → enable "Forward to
   network feeder" pointed at `127.0.0.1:5550` → confirm
   datagrams arrive as well-formed JSON.
3. Set Station ID to `"ABCD"`, re-engage, confirm the JSON
   shows `"station_id": "ABCD"` on subsequent messages.
4. Disable ACARS → confirm JSONL flushes (no pending bytes
   missing from the tail) and feeder closes.

## File layout

```text
crates/sdr-acars/src/
├── json.rs                 ← NEW (~250 LOC incl. tests)
├── lib.rs                  ← +1 mod, +1 re-export

crates/sdr-core/src/
├── acars_output.rs         ← NEW (~250 LOC incl. tests)
├── controller.rs           ← +~80 LOC (DspState fields, tap signature, command handlers, lifecycle)
├── messages.rs             ← +5 enum variants
├── lib.rs                  ← +1 mod (acars_output)

crates/sdr-ui/src/
├── acars_config.rs         ← +~120 LOC (5 keys, 5 read/save pairs, tests)
├── sidebar/aviation_panel.rs  ← +~80 LOC (Output group + 5 widgets)
├── window.rs               ← +~60 LOC (signal wiring + DspToUi handlers + replay)
```

## Estimated diff

~600 LOC added, ~5 modified. Single bundled PR. The 5-component
split keeps each new file under ~250 LOC, and the boundaries
are crisp (pure serializer ↔ I/O writers ↔ controller wiring ↔
UI panel ↔ config helpers).

## Out of scope

- MQTT publisher (separate follow-up issue)
- File rotation / size-cap logic
- Aircraft-grouped tab (#579)
- TLS / authentication on the network feeder
- Supervisor or line-format outputs
- CLI binary integration (the JSON serializer lives where the
  CLI can pick it up later, but no CLI changes in v1)

## Open questions

None at design time. The 5-command shape, single-rolling-file
convention, and synchronous I/O are all decisions; if any
turn out wrong during implementation we'll revisit with a
documented rationale.

## References

- `original/acarsdec/output.c::buildjson` lines 227-323 — JSON
  schema source of truth
- `original/acarsdec/netout.c::Netoutjson` — UDP feeder
  protocol reference
- airframes.io feeder spec: <https://app.airframes.io/about>
- Issue #578 acceptance criteria
- Sibling specs:
  - `docs/superpowers/specs/2026-04-28-acars-design.md` (epic root)
  - `docs/superpowers/specs/2026-04-30-acars-label-parsers-design.md` (PR #594, foundation for OOOI fields in JSON)
- Existing patterns mirrored:
  - `acars_config.rs` config-key + read/save helper convention
  - `controller.rs::handle_set_*` lifecycle pattern
  - WAV recorder flush-on-disengage pattern (epic #420 sub-tasks)
