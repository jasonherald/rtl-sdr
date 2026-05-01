# ACARS Async Output I/O + Custom Channel Sets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move ACARS output I/O onto a worker thread (#596) AND add user-defined custom channel sets to the Aviation panel (#592), bundled into one PR.

**Architecture:** Part A relocates `AcarsOutputs` from `controller.rs` into `acars_output.rs`, replaces synchronous `JsonlWriter::write` / `UdpFeeder::send` calls in the DSP-thread closure with a `try_send` to a bounded `mpsc::sync_channel(256)`, and runs a dedicated writer thread that owns the writer instances and reads runtime config via `Arc<RwLock<AcarsWriterConfig>>`. Part B extends `AcarsRegion` with a `Custom(Box<[f64]>)` variant (forces dropping `Copy`), migrates `[ChannelStats; 6]` and `[ActionRow; 6]` const-arrays to `Vec` across ~16 sites, and adds a CSV `AdwEntryRow` editor with span/count validation in the Aviation panel.

**Tech Stack:** Rust 1.x, `std::sync::{mpsc, Arc, RwLock, atomic}`, `std::thread`, GTK4 v4.10 + libadwaita via `gtk4-rs`, `serde_json` for config persistence, existing `sdr_acars::ChannelBank` consumer.

**Branch:** `feat/acars-async-io-and-custom-channels` (already off `main`, spec already committed at `75c9e74`).

**Out of scope:** Audio/scanner/satellite output async refactors, persisted ring buffer across app restarts, channel-count generalisation above 8.

---

## File Structure

| File | Role |
|------|------|
| `crates/sdr-core/src/acars_output.rs` | MODIFY — host the new `AcarsOutputs` (was private in `controller.rs`); add `AcarsWriterConfig`, `AcarsOutputMessage`, worker thread spawn + lifecycle. |
| `crates/sdr-core/src/controller.rs` | MODIFY — drop the local `AcarsOutputs` struct; import from `acars_output`. Switch `acars_decode_tap` to `try_send`; switch handlers (`set_jsonl_path`/`set_network_addr`/`set_station_id`) to mutate the shared config lock. |
| `crates/sdr-core/src/acars_airband_lock.rs` | MODIFY — add `Custom(Box<[f64]>)` variant; drop `Copy`; `channels()` returns `&[f64]`; new `MAX_CUSTOM_CHANNELS`, `MAX_CHANNEL_SPAN_HZ`, `CustomChannelError`, `validate_custom_channels`. |
| `crates/sdr-core/src/acars_config.rs` | MODIFY — new `acars_custom_channels` config key + helpers. |
| `crates/sdr-ui/src/state.rs` | MODIFY — `acars_channel_stats: RefCell<Vec<ChannelStats>>` (was const array). |
| `crates/sdr-ui/src/window.rs` | MODIFY — array→Vec assignments; rebuild `channel_rows` on region change in `connect_aviation_panel`; startup-replay handles `Custom` two-key load. |
| `crates/sdr-ui/src/sidebar/aviation_panel.rs` | MODIFY — `channel_rows: Vec<ActionRow>`; add `Custom` to `REGION_OPTIONS`; new `AdwEntryRow` for custom CSV; visibility binding to combo; `connect_apply` validate-and-dispatch. |
| `crates/sdr-ffi/src/event.rs` | MODIFY — test fixture array→Vec. |

---

## Workspace Gates

Run after each task that touches Rust source. Per-crate gates are sufficient when isolated to one crate.

```bash
cargo build -p <crate> --features whisper-cpu  # for sdr-ui
cargo test -p <crate>                          # for sdr-acars / sdr-core
cargo clippy -p <crate> --features whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Final pre-push gates (Task 14):

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo check --workspace --locked --no-default-features --features sherpa-cpu  # CI's --locked gate
cargo fmt --all -- --check  # last
```

The `--locked` gate is mandatory before push per `feedback_cargo_lock_locked_check.md` (CI uses it; local `cargo build` doesn't and silently updates `Cargo.lock`).

---

## Part A — Async output I/O (#596)

### Task 1: Move `AcarsOutputs` from `controller.rs` to `acars_output.rs` (pure refactor)

**Files:**
- Modify: `crates/sdr-core/src/acars_output.rs` (add struct + impl)
- Modify: `crates/sdr-core/src/controller.rs` (remove local struct, import)

This is a pure relocation — the struct keeps the same fields and behaviour. Done first so subsequent tasks have a `pub`-visible struct in `acars_output.rs` to grow.

- [ ] **Step 1: Move the struct**

In `crates/sdr-core/src/acars_output.rs`, after the `UdpFeeder` impl block (after line 125), append:

```rust
/// Output-writer bundle owned by `DspState`. Keeps the JSONL
/// writer, UDP feeder, station ID, and per-writer warn-rate-
/// limit timestamps together so the `acars_decode_tap`
/// signature stays narrow. Issue #578. Async refactor in
/// progress per #596 — fields will migrate to a worker
/// thread + shared config in subsequent tasks.
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
```

- [ ] **Step 2: Remove the local struct from `controller.rs`**

In `crates/sdr-core/src/controller.rs`, delete lines 229-273 (the `struct AcarsOutputs` block plus its `impl`). Keep the `ACARS_OUTPUT_WARN_MIN_INTERVAL` constant at line 277 — it still belongs in `controller.rs` since it's used by `acars_decode_tap`.

- [ ] **Step 3: Update references in `controller.rs`**

Find every `AcarsOutputs` reference in `controller.rs` (use grep) and confirm it picks up the new path. Specifically, the field declaration at line 620:

```rust
acars_outputs: crate::acars_output::AcarsOutputs,
```

And the constructor at line 716:

```rust
acars_outputs: crate::acars_output::AcarsOutputs::new(),
```

The other references (`outputs: &mut AcarsOutputs` in `acars_decode_tap`, `&mut state.acars_outputs` in handlers) will compile after the import is updated.

Add the import at the top of `controller.rs` if not already present:

```rust
use crate::acars_output::AcarsOutputs;
```

(Can use `crate::acars_output::AcarsOutputs` inline instead — pick whichever the file's existing style prefers.)

- [ ] **Step 4: Verify build + test + clippy + fmt**

```bash
cargo build -p sdr-core
cargo test -p sdr-core
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: all clean. The relocation is type-equivalent; existing tests pass unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-core/src/acars_output.rs crates/sdr-core/src/controller.rs
git commit -m "$(cat <<'EOF'
refactor(sdr-core): #596 move AcarsOutputs to acars_output.rs

Pure relocation — the struct and impl move from controller.rs's
private namespace to a pub item in acars_output.rs, where the
upcoming async-I/O refactor (#596) will grow them with a worker
thread, shared config lock, and bounded mpsc channel. No
behaviour change in this commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Add `AcarsWriterConfig` + `AcarsOutputMessage` types

**Files:**
- Modify: `crates/sdr-core/src/acars_output.rs`

Introduce the writer-thread message + shared-config types. Pure-data additions; no consumer wiring yet.

- [ ] **Step 1: Add the types at the top of `acars_output.rs` (after the `use` block, before `JsonlWriter`)**

Add these declarations at the top of `crates/sdr-core/src/acars_output.rs`, after the existing `use` block (after line 17):

```rust
use std::path::PathBuf;
use std::sync::{mpsc, Arc, RwLock};
use std::thread::JoinHandle;

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
    /// The shared `AcarsWriterConfig` was mutated by the UI
    /// side. Wakes the writer to re-snapshot config and apply
    /// `ensure_jsonl` / `ensure_udp` so config-only changes
    /// (disable, path swap, addr swap) take effect immediately
    /// instead of being buffered until the next decoded
    /// message. Wired up in Task 4 (loop arm) and Task 5
    /// (`notify_config_changed` sender). CR round 1 on PR #598.
    ConfigChanged,
    /// Explicit clean-shutdown signal. `Drop for AcarsOutputs`
    /// emits this before dropping `tx`; the worker also exits
    /// cleanly on `Err(Disconnected)` as a fallback. Having an
    /// explicit variant makes shutdown deterministic for tests.
    /// Wired up in Task 4 (loop arm) and Task 5 (`Drop` impl
    /// sender). CR round 6 on PR #598.
    Shutdown,
}
```

- [ ] **Step 2: Verify build + clippy + fmt**

```bash
cargo build -p sdr-core
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean. New types are unused — clippy may warn `dead_code`. If so, this is fine for now (used in Task 3); add `#[allow(dead_code)]` *only* on `AcarsOutputMessage` and `AcarsWriterConfig` if clippy fails. (Use `#[allow(dead_code)]` per-item; do not blanket-allow.) Remove these allows in Task 3 when the types are wired up.

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-core/src/acars_output.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #596 add AcarsWriterConfig + AcarsOutputMessage types

Writer-thread shared config (jsonl_path, network_addr,
station_id) wrapped in Arc<RwLock<...>>. Read-heavy access:
writer reads per-message, UI side writes only on user
toggle/edit. Plus the AcarsOutputMessage enum (single Decoded
variant for now) handed from the DSP thread to the writer
thread via mpsc::sync_channel.

Pure type additions — wired up in subsequent tasks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Build the writer thread (TDD: shutdown-on-disconnect)

**Files:**
- Modify: `crates/sdr-core/src/acars_output.rs`

Spawn the writer thread with shutdown-on-tx-drop semantics. Test asserts `JoinHandle::join` returns within a short timeout when the sender is dropped.

- [ ] **Step 1: Write the failing test**

In `crates/sdr-core/src/acars_output.rs`, inside the existing `mod tests` (after the `udp_feeder_open_invalid_addr_errors` test at the end, around line 242), append:

```rust
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
```

The `Duration` import is already brought in by the existing test fixtures (line 133); the `Arc`/`RwLock`/`mpsc` imports were added in Task 2.

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p sdr-core acars_output::tests::writer_thread_exits_on_disconnect
```

Expected: FAIL with `cannot find function 'run_writer_loop' in this scope`.

- [ ] **Step 3: Add the writer loop**

In `crates/sdr-core/src/acars_output.rs`, insert before the `mod tests` block (around line 127):

```rust
/// Writer-thread main loop. Owns the per-thread `JsonlWriter`
/// and `UdpFeeder` instances, reads `config` on each message
/// to detect path/addr changes, and exits cleanly when the
/// sender side disconnects (app shutdown). Issue #596.
fn run_writer_loop(
    rx: mpsc::Receiver<AcarsOutputMessage>,
    _config: Arc<RwLock<AcarsWriterConfig>>,
) {
    // Real per-message handling lands in Task 4 (path/addr
    // hot-reload) and Task 5 (writes + send). For now, just
    // drain the channel so the shutdown-on-disconnect contract
    // holds.
    while let Ok(_msg) = rx.recv() {
        // Drain only — Task 4 + 5 wire writes here.
    }
}
```

The leading underscore on `_config` suppresses the unused-parameter lint until Task 4 fills it in.

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p sdr-core acars_output::tests::writer_thread_exits_on_disconnect
```

Expected: PASS.

- [ ] **Step 5: Verify clippy + fmt**

```bash
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean. If `dead_code` was suppressed in Task 2, the `AcarsOutputMessage` and `AcarsWriterConfig` allows can stay until Task 5 (when they're consumed externally) — or remove them now if they no longer trigger.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/src/acars_output.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #596 writer thread loop with shutdown-on-disconnect

Drains an mpsc receiver until tx is dropped, then exits cleanly.
Per-message JsonlWriter::write / UdpFeeder::send dispatching
lands in Tasks 4-5; this commit pins the lifecycle contract
(test asserts the thread joins within 500ms of tx drop).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Wire writer hot-reload (TDD: path-change reopen)

**Files:**
- Modify: `crates/sdr-core/src/acars_output.rs`

The writer thread reads `config` on each message and reopens its `JsonlWriter` / `UdpFeeder` when the path/addr changes (or closes them when they go to `None`).

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/sdr-core/src/acars_output.rs`:

```rust
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
        assert_eq!(read_lines(&path_b).len(), 1, "path B got the second message");
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p sdr-core acars_output::tests::writer_reopens_on_path_change
```

Expected: FAIL — currently the writer loop drains without writing.

- [ ] **Step 3: Implement the reopen logic**

Replace the `run_writer_loop` body (currently a drain loop) with:

```rust
fn run_writer_loop(
    rx: mpsc::Receiver<AcarsOutputMessage>,
    config: Arc<RwLock<AcarsWriterConfig>>,
) {
    let mut jsonl: Option<(PathBuf, JsonlWriter)> = None;
    let mut udp: Option<(String, UdpFeeder)> = None;
    let mut jsonl_warn_at: Option<std::time::Instant> = None;
    let mut udp_warn_at: Option<std::time::Instant> = None;

    // `while let Ok(_)` is the disconnect-fallback path; the
    // inner `match` handles the explicit `Shutdown` sentinel
    // (which `break`s out of the outer loop) and the
    // `ConfigChanged` wake-up. Either path exits cleanly.
    'recv: while let Ok(msg) = rx.recv() {
        match msg {
            AcarsOutputMessage::Shutdown => break 'recv,
            AcarsOutputMessage::ConfigChanged => {
                // No payload to write — just resnap config and
                // close/open. `ensure_*` close on `None` and
                // reopen on path/addr change, so disabling
                // JSONL or swapping the destination applies
                // immediately even with no decoded traffic.
                // CR round 1 on PR #598.
                let (want_jsonl_path, want_udp_addr, _station_id) = {
                    let cfg = config.read().unwrap_or_else(|p| p.into_inner());
                    (cfg.jsonl_path.clone(), cfg.network_addr.clone(), cfg.station_id.clone())
                };
                ensure_jsonl(&mut jsonl, want_jsonl_path.as_deref());
                ensure_udp(&mut udp, want_udp_addr.as_deref());
            }
            AcarsOutputMessage::Decoded(msg) => {
                // Snapshot the config under a brief read lock so we
                // don't hold it across blocking I/O. Recover from
                // poisoning rather than panicking — a panic in the
                // writer path would otherwise propagate to all later
                // settings edits. CR round 1 on PR #598.
                let (want_jsonl_path, want_udp_addr, station_id) = {
                    let cfg = config.read().unwrap_or_else(|p| p.into_inner());
                    (cfg.jsonl_path.clone(), cfg.network_addr.clone(), cfg.station_id.clone())
                };

                ensure_jsonl(&mut jsonl, want_jsonl_path.as_deref());
                ensure_udp(&mut udp, want_udp_addr.as_deref());

                if let Some((_, w)) = jsonl.as_mut() {
                    if let Err(e) = w.write(&msg, station_id.as_deref()) {
                        rate_limited_warn("jsonl", &mut jsonl_warn_at, e);
                    }
                }
                if let Some((_, f)) = udp.as_mut() {
                    if let Err(e) = f.send(&msg, station_id.as_deref()) {
                        rate_limited_warn("udp", &mut udp_warn_at, e);
                    }
                }
            }
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

const ACARS_OUTPUT_WARN_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Emit a `tracing::warn!` at most once per
/// `ACARS_OUTPUT_WARN_MIN_INTERVAL` for `kind`. Mirrors the
/// per-writer 30 s rate-limit that previously lived in
/// `controller.rs::acars_decode_tap`.
fn rate_limited_warn(kind: &str, last: &mut Option<std::time::Instant>, err: std::io::Error) {
    let now = std::time::Instant::now();
    let elapsed = last.map_or(ACARS_OUTPUT_WARN_MIN_INTERVAL, |t| now.duration_since(t));
    if elapsed >= ACARS_OUTPUT_WARN_MIN_INTERVAL {
        tracing::warn!("acars {kind} write/send failed: {err} (rate-limited 30s)");
        *last = Some(now);
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p sdr-core acars_output --features whisper-cpu 2>&1 | tail -20
```

Expected: PASS — `writer_thread_exits_on_disconnect` and `writer_reopens_on_path_change` both green, existing JsonlWriter/UdpFeeder tests still pass.

- [ ] **Step 5: Verify clippy + fmt**

```bash
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean. If clippy flags `doc_markdown` on identifiers like `JsonlWriter`, `UdpFeeder`, `BufWriter` in doc comments, wrap in backticks.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/src/acars_output.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #596 writer thread per-message dispatch + hot-reload

Writer thread now reads AcarsWriterConfig on each message and
reopens JsonlWriter / UdpFeeder when path/addr changes. The
30 s rate-limited warn previously in controller.rs::acars_decode_tap
moves into the worker (rate_limited_warn helper).

Test writer_reopens_on_path_change: pump msg → path A, mutate
config to path B, pump msg → path B; assert both files have
the expected line count.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Convert `AcarsOutputs` to async shape (TDD: drop-on-full)

**Files:**
- Modify: `crates/sdr-core/src/acars_output.rs`

Replace the synchronous `AcarsOutputs` fields with `tx`, `config`, drop counter, last-warn timestamp, and `JoinHandle`. Add `try_send` helper that drops on full + 30 s warn rate-limit. Test asserts the 257th try_send into a 256-cap channel is dropped (and counter increments).

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
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
        // Stash the receiver so we don't drop it (Disconnected
        // would be a different error path).
        let rx_keepalive = outputs._test_rx.clone();
        let _ = rx_keepalive;

        for _ in 0..8 {
            assert!(outputs.try_send(make_msg(0)));
        }
        // 9th send: channel full, drop returns false, counter
        // increments.
        assert!(!outputs.try_send(make_msg(0)));
        assert_eq!(outputs.drop_count(), 1);
    }
```

This test references `with_capacity_for_test`, `try_send`, `drop_count`, and `_test_rx` — all of which we add in Step 3.

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p sdr-core acars_output::tests::try_send_drops_when_channel_full
```

Expected: FAIL — those methods don't exist yet.

- [ ] **Step 3: Replace the synchronous `AcarsOutputs` with the async shape**

Replace the entire `AcarsOutputs` struct and its `impl` (added in Task 1, currently around line 130-170) with:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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
    /// recv() returning Err(Disconnected); we then `join()`.
    writer_thread: Option<JoinHandle<()>>,

    // Test-only: an extra receiver clone so unit tests can
    // override the worker (or skip it entirely). Hidden behind
    // a method that's `#[cfg(test)]`-gated; production code
    // never sees this.
    #[cfg(test)]
    pub _test_rx: Arc<Mutex<Option<mpsc::Receiver<AcarsOutputMessage>>>>,
}

impl AcarsOutputs {
    /// Construct an async-output bundle and spawn the writer
    /// thread. The thread runs until `Drop` for `AcarsOutputs`
    /// drops the `tx`, at which point the writer's `recv()`
    /// returns `Err(Disconnected)` and the loop exits.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(ACARS_OUTPUT_CHANNEL_CAPACITY)
    }

    /// Same as `new` but with a caller-chosen channel
    /// capacity. Production calls go through `new`; tests use
    /// this directly via `with_capacity_for_test` to exercise
    /// the drop-on-full path with a cap they can saturate.
    fn with_capacity(capacity: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(capacity);
        let config = Arc::new(RwLock::new(AcarsWriterConfig::default()));

        let writer_config = Arc::clone(&config);
        // Thread spawn can fail (rlimit, OOM). Log + return a
        // None handle rather than panic — the controller still
        // boots and the user sees an error toast on the next
        // ACARS message attempt. CR round 2 on PR #598.
        let writer_thread = match std::thread::Builder::new()
            .name("sdr-acars-writer".into())
            .spawn(move || run_writer_loop(rx, writer_config))
        {
            Ok(handle) => Some(handle),
            Err(e) => {
                tracing::error!("ACARS writer thread spawn failed: {e}");
                None
            }
        };

        Self {
            tx,
            config,
            drop_count: Arc::new(AtomicU64::new(0)),
            last_drop_warn_at: Arc::new(Mutex::new(None)),
            writer_thread,
            #[cfg(test)]
            _test_rx: Arc::new(Mutex::new(None)),
        }
    }

    /// Test-only constructor that builds the channel + config
    /// but skips spawning the worker, leaving the receiver
    /// reachable via `_test_rx`. Used by `try_send_drops_when_channel_full`
    /// to fill a small-cap channel without race conditions.
    #[cfg(test)]
    fn with_capacity_for_test(capacity: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel::<AcarsOutputMessage>(capacity);
        let config = Arc::new(RwLock::new(AcarsWriterConfig::default()));
        Self {
            tx,
            config,
            drop_count: Arc::new(AtomicU64::new(0)),
            last_drop_warn_at: Arc::new(Mutex::new(None)),
            writer_thread: None,
            _test_rx: Arc::new(Mutex::new(Some(rx))),
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

    /// Wake the writer thread so it re-snapshots
    /// `AcarsWriterConfig` and applies `ensure_jsonl` /
    /// `ensure_udp` immediately. Called by the controller's
    /// path/addr/enable/station handlers AFTER they mutate
    /// `config` — without it, the worker only wakes on
    /// `Decoded` and stale handles linger until the next
    /// decoded frame (CR round 1 on PR #598).
    ///
    /// `try_send`, not `send`: if the channel is full the
    /// worker is already saturated processing `Decoded` and
    /// will re-snapshot config on the next iteration anyway —
    /// a dropped `ConfigChanged` is harmless under that
    /// pressure. Disconnected (worker gone during teardown)
    /// is also fine to silently drop.
    pub fn notify_config_changed(&self) {
        let _ = self.tx.try_send(AcarsOutputMessage::ConfigChanged);
    }

    /// 30 s-rate-limited warn for channel-full drops. Reads
    /// the current drop count so the message names how many
    /// were lost in this window.
    fn maybe_warn_full(&self) {
        // Recover from poisoning rather than panicking — a
        // panic in maybe_warn_full would otherwise propagate
        // to every later DSP-thread try_send. CR round 2 on
        // PR #598.
        let mut last = self
            .last_drop_warn_at
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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

impl Default for AcarsOutputs {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AcarsOutputs {
    fn drop(&mut self) {
        // Send the explicit `Shutdown` sentinel first so the
        // worker exits via the deterministic `Shutdown` arm
        // rather than the `Err(Disconnected)` fallback. Both
        // paths drain cleanly, but `Shutdown` means tests can
        // assert promptness without racing the OS scheduler.
        // `try_send` is fine — if the channel is full the
        // `Disconnected` fallback below still terminates.
        // CR round 6 on PR #598.
        let _ = self.tx.try_send(AcarsOutputMessage::Shutdown);

        // Closing tx triggers Disconnected → the writer loop
        // exits as a fallback. We still need to join the
        // thread to make sure its Drop impls (BufWriter flush)
        // finish before the process exits.
        if let Some(handle) = self.writer_thread.take() {
            // Drop the tx clone held by `self.tx` first by
            // overwriting it with a drained channel. (mpsc::SyncSender
            // doesn't have an explicit close — Drop is the
            // signal.)
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
```

Note `ACARS_OUTPUT_WARN_MIN_INTERVAL` is now defined in this file (Task 4 added it as a private const). Reuse it; don't redefine.

- [ ] **Step 4: Run the failing test to confirm it now passes**

```bash
cargo test -p sdr-core acars_output --features whisper-cpu
```

Expected: PASS — `try_send_drops_when_channel_full` green, plus `writer_thread_exits_on_disconnect` and `writer_reopens_on_path_change` (both still green).

- [ ] **Step 5: Verify clippy + fmt**

```bash
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean. If clippy flags `doc_markdown` on identifiers (`JsonlWriter`, `UdpFeeder`, `mpsc::sync_channel`, `Arc<RwLock>`, `JoinHandle`, etc.) wrap in backticks.

The whole-crate build WILL FAIL at this commit because `controller.rs` still references the old `AcarsOutputs` interface (jsonl, udp, jsonl_enabled, station_id, etc. fields). That's the intentional pivot point — Task 6 finishes the migration in the next commit.

Cargo doesn't have a way to compile one submodule in isolation when its sibling is broken (private modules all build together for any test target inside the crate). So the standalone "did `acars_output.rs` come out right" verification is by inspection, not by `cargo test`:

```bash
# Confirm errors are localized to controller.rs (the consumer
# that Task 6 fixes) — there should be no error[E0...] lines
# pointing at acars_output.rs.
cargo check -p sdr-core --features sdr-transcription/whisper-cpu 2>&1 \
    | grep -oE "src/[^.]*\.rs" | sort -u
# Expected output: only `src/controller.rs` (and possibly its
# test file). If anything else shows up, fix it before
# committing — it likely means the new types or signatures
# in acars_output.rs are wrong.
```

Once that check is clean, commit and proceed to Task 6. The full test suite (`cargo test -p sdr-core`) runs at the end of Task 6 once `controller.rs` is back in shape.

Why not merge Tasks 5 + 6 into one commit? Task 5 is a type pivot (~270 LOC of new struct shape + tests) and Task 6 is the consumer-side migration (~75 LOC across many handlers). Splitting keeps each diff focused for code review and `git bisect`. The intentional intermediate red is the trade-off; this verification step makes it explicit so the implementer knows what to expect.

- [ ] **Step 6: Commit (anticipating Task 6 to fix the build)**

```bash
git add crates/sdr-core/src/acars_output.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #596 AcarsOutputs async shape + try_send drop-on-full

Replaces the synchronous AcarsOutputs (jsonl + udp + warn
timestamps + pending paths) with the async-I/O shape:
- mpsc::sync_channel(256) for hand-off to the writer thread
- Arc<RwLock<AcarsWriterConfig>> for runtime config
- AtomicU64 drop counter + Arc<Mutex<Option<Instant>>> for the
  channel-full warn rate-limit

try_send drops on Full and increments the counter; warn fires
at most once per 30 s with the cumulative drop count.

Drop for AcarsOutputs joins the writer thread cleanly.

NOTE: this commit breaks controller.rs at the per-crate build
gate — `controller.rs` still references the old field shape.
Task 6 finishes the migration; this commit is the type pivot.

Test try_send_drops_when_channel_full: 9th send into 8-cap
channel returns false; drop_count == 1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Switch `acars_decode_tap` to `try_send` + delete legacy fields from `AcarsOutputs` consumers

**Files:**
- Modify: `crates/sdr-core/src/controller.rs`

`acars_decode_tap` no longer takes `&mut AcarsOutputs` — it takes `&AcarsOutputs` and calls `outputs.try_send(msg)`. The handlers (`set_acars_jsonl_path`, `set_acars_network_addr`, `set_acars_station_id`, plus the engage path) all switch to mutating `outputs.config.write()` instead of the old `pending_*` fields. Removed-from-state-struct: `jsonl`, `udp`, `jsonl_enabled`, `network_enabled`, `station_id`, `jsonl_warn_at`, `udp_warn_at`, `pending_jsonl_path`, `pending_network_addr`. The corresponding open/close logic (`open_jsonl_writer`, `close_acars_outputs`, etc.) gets deleted — the writer thread owns those concerns now.

This is a large-ish controller.rs edit. Below is the concrete replacement for `acars_decode_tap`; the handler edits follow the same pattern (write the config lock, no return value).

- [ ] **Step 1: Replace `acars_decode_tap` body**

In `crates/sdr-core/src/controller.rs`, find the function (around line 879). Replace the body (lines 879-981) with:

```rust
#[allow(clippy::too_many_arguments)]
fn acars_decode_tap(
    bank: &mut Option<sdr_acars::ChannelBank>,
    init_failed: &mut bool,
    source_rate_hz: f64,
    center_hz: f64,
    channels: &[f64],
    iq: &[sdr_types::Complex],
    dsp_tx: &std::sync::mpsc::Sender<crate::messages::DspToUi>,
    outputs: &crate::acars_output::AcarsOutputs,
) {
    const _: () = assert!(
        std::mem::size_of::<sdr_types::Complex>() == std::mem::size_of::<num_complex::Complex32>(),
        "sdr_types::Complex and num_complex::Complex32 must have identical size \
         for the bytemuck zero-copy cast in acars_decode_tap"
    );
    const _: () = assert!(
        std::mem::align_of::<sdr_types::Complex>()
            == std::mem::align_of::<num_complex::Complex32>(),
        "sdr_types::Complex and num_complex::Complex32 must have identical \
         alignment for the bytemuck zero-copy cast in acars_decode_tap"
    );

    if *init_failed {
        return;
    }
    if bank.is_none() {
        match sdr_acars::ChannelBank::new(source_rate_hz, center_hz, channels) {
            Ok(b) => {
                tracing::info!(
                    "ACARS bank initialised: source_rate={source_rate_hz} \
                     center={center_hz} n_channels={}",
                    channels.len()
                );
                *bank = Some(b);
            }
            Err(e) => {
                tracing::warn!("ACARS bank init failed: {e}");
                *init_failed = true;
                return;
            }
        }
    }
    let Some(bank) = bank.as_mut() else { return };
    let iq_c32: &[num_complex::Complex32] = bytemuck::cast_slice(iq);
    bank.process(iq_c32, |msg| {
        // Hand off to the writer thread via the bounded
        // channel. Drop-on-full is handled by `try_send`
        // (rate-limited warn lives there). The writer thread
        // owns JsonlWriter::write + UdpFeeder::send and reads
        // station_id / paths from the shared config lock.
        // Issue #596.
        outputs.try_send(msg.clone());
        // Forward to the UI viewer regardless of writer state.
        let _ = dsp_tx.send(crate::messages::DspToUi::AcarsMessage(Box::new(msg)));
    });
}
```

Note: `outputs: &mut AcarsOutputs` becomes `outputs: &AcarsOutputs`. `try_send` takes `&self` (interior mutability via the AtomicU64 + Mutex) so a shared borrow is enough. This simplifies all callers — they no longer need a mutable borrow.

- [ ] **Step 2: Update `acars_decode_tap` callers**

Find every call site (use grep `acars_decode_tap(`). They all currently pass `&mut state.acars_outputs`. Change each to `&state.acars_outputs`. There are ~3 callers (in `process_iq_block` and the lazy-init dispatch around line 3679, plus the test fixtures at the bottom of the file).

For the test fixtures at lines 4934+ (the `#[cfg(test)] mod tests` section): they currently do `let mut outputs = super::AcarsOutputs::new();`. Change to:

```rust
let outputs = super::AcarsOutputs::new();
super::acars_decode_tap(
    /* ... */
    &outputs,
);
```

i.e., drop the `mut` and the `&mut`. The tests should still pass — they were exercising the bank-init / decode path, not the I/O fields.

- [ ] **Step 3: Update the station-id handler**

`handle_set_acars_station_id` doesn't need the intent-preservation pattern that the path/addr handlers use (`station_id` is a single user-typed string with no separate "enable" toggle). Replace its body with the trim+cap+empty-as-None pattern:

```rust
fn handle_set_acars_station_id(state: &mut DspState, station_id: &str) {
    // Trim, bound to 8 chars (matches acarsdec's `idstation`
    // field width), and treat empty-after-trim as None so
    // non-UI callers (config replay, future FFI) can't leak
    // whitespace-only or oversized IDs into emitted JSON.
    // CR round 3 on PR #595.
    let trimmed = station_id.trim();
    acars_config_write(&state.acars_outputs.config).station_id = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(8).collect())
    };
    state.acars_outputs.notify_config_changed();
}
```

The corresponding `handle_set_acars_jsonl_path` and `handle_set_acars_network_addr` handlers use the canonical intent-preservation pattern — see **Step 5** below for the full bodies. CR round 2 on PR #598 caught that an earlier draft of this section showed a simpler direct-config-write variant for those two handlers, which would regress the disable/enable restore behaviour by collapsing user intent into runtime writer state. Don't apply that older pattern; route through `acars_last_user_jsonl_path` / `acars_last_user_network_addr` per Step 5.

The `acars_config_write` helper recovers from `RwLock` poisoning rather than panicking; without it, a panic in the writer thread would propagate to every later settings edit.

```rust
/// Acquire the ACARS writer config write lock, recovering from
/// poisoning rather than panicking. CR round 1 on PR #598.
fn acars_config_write(
    cfg: &std::sync::RwLock<crate::acars_output::AcarsWriterConfig>,
) -> std::sync::RwLockWriteGuard<'_, crate::acars_output::AcarsWriterConfig> {
    cfg.write().unwrap_or_else(|poisoned| {
        tracing::warn!("acars writer config lock was poisoned; recovering");
        poisoned.into_inner()
    })
}
```

- [ ] **Step 4: Delete the obsolete helpers**

Delete from `controller.rs`:
- `fn jsonl_path_for(...)` (lines ~4378-4383)
- `fn network_addr_for(...)` (lines ~4385-4395)
- `fn close_acars_outputs(...)` (around line 4449) — its callers (lines 1198, 3401, 4337) should also be removed; the writer thread owns close-on-drop now.
- The lazy-open blocks in `process_iq_block` around lines 1093-1107 — those depended on the `pending_*` fields. The writer thread does ensure_jsonl/ensure_udp on every message; no per-block lazy open is needed.

For each `close_acars_outputs(&mut state.acars_outputs)` call, just delete the line. App-shutdown cleanup is handled by `Drop for AcarsOutputs`.

Also delete the local `ACARS_OUTPUT_WARN_MIN_INTERVAL` if it's still in `controller.rs` (it moved to `acars_output.rs` in Task 4).

- [ ] **Step 5: Update the engage / disengage paths**

In the engage path (`SetAcarsEnabled(true)` handler), the existing code reads `pending_jsonl_path` etc. to open writers. With the new shape, the writer thread is always running — engage just needs to make sure the config has the right values. The `pending_*` setter handlers (Step 3) already update the config, so the engage path can be simplified: just set `jsonl_enabled` / `network_enabled` flags ... but wait — the spec replaces those flags with `jsonl_path: Option` (Some = enabled). Re-read the spec.

The writer config models the runtime state: `jsonl_path: Option<PathBuf>` is the writer's current behaviour (`Some` = write to this path, `None` = don't write). Per CR round 2 on PR #598, the user's *intent* (their last-chosen path) lives separately on `DspState` so a disable→enable cycle can restore it without conflating the writer's runtime state with persistence.

Add two fields to `DspState`:

```rust
struct DspState {
    /* … existing fields … */
    /// Most-recent user-set JSONL destination, preserved
    /// across disable/enable toggles so re-enabling restores
    /// the user's previously-chosen path rather than the
    /// default.
    acars_last_user_jsonl_path: Option<std::path::PathBuf>,
    /// Same pattern as `acars_last_user_jsonl_path` for the
    /// UDP feeder.
    acars_last_user_network_addr: Option<String>,
}
```

Initialize both to `None` in `DspState::new`.

Then the enable-toggle handlers look like this — restore the user's last-chosen value, fall back to the protocol default when the user hasn't picked one yet:

```rust
fn handle_set_acars_jsonl_enabled(
    state: &mut DspState,
    _dsp_tx: &mpsc::Sender<DspToUi>,
    enabled: bool,
) {
    {
        let mut cfg = acars_config_write(&state.acars_outputs.config);
        if enabled {
            // Restore the user's last-chosen path (preserved
            // across disable/enable cycles via
            // `acars_last_user_jsonl_path`). Falls back to the
            // default if the user hasn't picked a path yet.
            // CR round 2 on PR #598.
            cfg.jsonl_path = Some(
                state
                    .acars_last_user_jsonl_path
                    .clone()
                    .unwrap_or_else(|| resolve_jsonl_path("")),
            );
        } else {
            // Disable: clear `cfg.jsonl_path` so the writer
            // stops, but keep `acars_last_user_jsonl_path` so
            // re-enable restores. CR round 2 on PR #598.
            cfg.jsonl_path = None;
        }
    }
    state.acars_outputs.notify_config_changed();
}

fn handle_set_acars_network_enabled(
    state: &mut DspState,
    _dsp_tx: &mpsc::Sender<DspToUi>,
    enabled: bool,
) {
    {
        let mut cfg = acars_config_write(&state.acars_outputs.config);
        if enabled {
            cfg.network_addr = Some(
                state
                    .acars_last_user_network_addr
                    .clone()
                    .unwrap_or_else(|| ACARS_NETWORK_DEFAULT_ADDR.to_string()),
            );
        } else {
            cfg.network_addr = None;
        }
    }
    state.acars_outputs.notify_config_changed();
}
```

The corresponding `handle_set_acars_jsonl_path` / `handle_set_acars_network_addr` handlers update `acars_last_user_jsonl_path` / `acars_last_user_network_addr` always (so the user's intent survives a future disable). They only push to the writer's config if the sink is currently enabled — otherwise the path edit shouldn't accidentally turn the sink on. They also normalize an empty apply correctly: when the sink is currently enabled, an empty apply means "use the default"; when it's currently disabled, an empty apply means "clear my saved path":

```rust
fn handle_set_acars_jsonl_path(
    state: &mut DspState,
    _dsp_tx: &mpsc::Sender<DspToUi>,
    path: &str,
) {
    let trimmed = path.trim();
    let new_value = if trimmed.is_empty() {
        let currently_enabled = acars_config_write(&state.acars_outputs.config)
            .jsonl_path
            .is_some();
        if currently_enabled {
            Some(resolve_jsonl_path(""))
        } else {
            None
        }
    } else {
        Some(resolve_jsonl_path(trimmed))
    };
    state.acars_last_user_jsonl_path.clone_from(&new_value);
    {
        let mut cfg = acars_config_write(&state.acars_outputs.config);
        if cfg.jsonl_path.is_some() {
            cfg.jsonl_path = new_value;
        }
    }
    state.acars_outputs.notify_config_changed();
}
```

(The corresponding `handle_set_acars_network_addr` follows the same shape with `network_addr` and `ACARS_NETWORK_DEFAULT_ADDR`.)

If the existing `handle_set_acars_jsonl_enabled` already exists, replace its body with the above. If it doesn't (i.e., the legacy code packed enable + path into one call), reconcile by checking the `UiToDsp` message variants.

NOTE: this part of Task 6 is genuinely tricky because there are several wired-together fields. The implementer should:
1. Run `cargo build -p sdr-core` and walk each compile error
2. For each missing field reference, decide: replace with config-lock write, or delete (if the helper no longer makes sense)

This is a search-and-fix-compile-errors task; the volume of changes is mechanical but there are ~30 sites across the file. Don't try to enumerate every site here — the build errors are the source of truth.

- [ ] **Step 6: Verify build + test + clippy + fmt**

```bash
cargo build -p sdr-core
cargo test -p sdr-core --features whisper-cpu
cargo clippy -p sdr-core --features whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: all clean. The existing controller-level integration tests should still pass — the visible behaviour from the UI-facing API perspective is unchanged. If a test relies on `state.acars_outputs.jsonl.is_some()` (or similar), update it to read `state.acars_outputs.config.read().unwrap().jsonl_path.is_some()`.

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-core/src/controller.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #596 wire DSP-thread try_send through controller

Switches acars_decode_tap to use AcarsOutputs::try_send (which
hands off to the writer thread) instead of synchronous
JsonlWriter::write / UdpFeeder::send. Handlers (set_acars_jsonl_path,
set_acars_network_addr, set_acars_station_id) now mutate the
shared Arc<RwLock<AcarsWriterConfig>> instead of the old
pending_* fields.

Removed: pending_jsonl_path, pending_network_addr, jsonl_enabled,
network_enabled, jsonl_warn_at, udp_warn_at, jsonl/udp writer
slots from AcarsOutputs (writer thread owns them now);
jsonl_path_for, network_addr_for, close_acars_outputs helpers;
the lazy-open blocks in process_iq_block.

Engage/disengage flips jsonl_path / network_addr Some↔None
in the config; the writer thread's ensure_jsonl / ensure_udp
detect the change on the next message and reopen/close.

Test suite still passes — DSP-thread ACARS path is now
non-blocking on disk/network I/O.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Part B — Custom channel sets (#592)

### Task 7: Add validators + constants to `acars_airband_lock.rs` (TDD)

**Files:**
- Modify: `crates/sdr-core/src/acars_airband_lock.rs`

Add `MAX_CUSTOM_CHANNELS`, `MAX_CHANNEL_SPAN_HZ`, `CustomChannelError`, `validate_custom_channels`. No `AcarsRegion` changes yet — the validator is a free function.

- [ ] **Step 1: Write the failing tests**

Find the existing `mod tests` (likely at the bottom of `acars_airband_lock.rs`; if absent, append a new one). Add these tests inside it:

```rust
    #[test]
    fn validate_rejects_empty() {
        assert_eq!(validate_custom_channels(&[]), Err(CustomChannelError::Empty));
    }

    #[test]
    fn validate_accepts_single_channel() {
        assert_eq!(validate_custom_channels(&[131_550_000.0]), Ok(()));
    }

    #[test]
    fn validate_accepts_max_count() {
        let chans: Vec<f64> = (0..MAX_CUSTOM_CHANNELS)
            .map(|i| 131_000_000.0 + (i as f64) * 100_000.0)
            .collect();
        assert_eq!(validate_custom_channels(&chans), Ok(()));
    }

    #[test]
    fn validate_rejects_too_many() {
        let chans: Vec<f64> = (0..=MAX_CUSTOM_CHANNELS)
            .map(|i| 131_000_000.0 + (i as f64) * 100_000.0)
            .collect();
        assert_eq!(
            validate_custom_channels(&chans),
            Err(CustomChannelError::TooMany {
                count: MAX_CUSTOM_CHANNELS + 1,
                max: MAX_CUSTOM_CHANNELS,
            })
        );
    }

    #[test]
    fn validate_rejects_nan() {
        // Note: f64::NAN != f64::NAN, so `assert_eq!(..., Err(
        // InvalidFrequency { value: f64::NAN }))` would fail
        // even when the variant is correct. Pattern-match
        // the variant and check `value.is_nan()` instead.
        // CR round 4 on PR #598.
        match validate_custom_channels(&[131_550_000.0, f64::NAN]) {
            Err(CustomChannelError::InvalidFrequency { value }) => {
                assert!(value.is_nan());
            }
            other => panic!("expected InvalidFrequency, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_inf() {
        match validate_custom_channels(&[131_550_000.0, f64::INFINITY]) {
            Err(CustomChannelError::InvalidFrequency { .. }) => {}
            other => panic!("expected InvalidFrequency, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_negative_or_zero() {
        match validate_custom_channels(&[131_550_000.0, 0.0]) {
            Err(CustomChannelError::InvalidFrequency { value }) if value == 0.0 => {}
            other => panic!("expected InvalidFrequency(0.0), got {other:?}"),
        }
        match validate_custom_channels(&[131_550_000.0, -1.0]) {
            Err(CustomChannelError::InvalidFrequency { value }) if value == -1.0 => {}
            other => panic!("expected InvalidFrequency(-1.0), got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_span_just_under() {
        // 2.4 MHz exact span — accepted (the constraint is ≤).
        assert_eq!(
            validate_custom_channels(&[129_125_000.0, 131_525_000.0]),
            Ok(())
        );
    }

    #[test]
    fn validate_rejects_span_just_over() {
        // 2.5 MHz — rejected.
        match validate_custom_channels(&[129_000_000.0, 131_500_000.0]) {
            Err(CustomChannelError::SpanExceeded { low_hz, high_hz, span_hz }) => {
                assert!((low_hz - 129_000_000.0).abs() < 1.0);
                assert!((high_hz - 131_500_000.0).abs() < 1.0);
                assert!((span_hz - 2_500_000.0).abs() < 1.0);
            }
            other => panic!("expected SpanExceeded, got {other:?}"),
        }
    }

    #[test]
    fn custom_channel_error_display_span_exceeded() {
        let err = CustomChannelError::SpanExceeded {
            low_hz: 129_000_000.0,
            high_hz: 131_500_000.0,
            span_hz: 2_500_000.0,
        };
        let s = format!("{err}");
        // User-facing toast text — must mention span value and
        // the offending pair in MHz.
        assert!(s.contains("2.5"), "span value present: {s}");
        assert!(s.contains("129"), "low freq present: {s}");
        assert!(s.contains("131"), "high freq present: {s}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p sdr-core acars_airband_lock::tests 2>&1 | tail -20
```

Expected: FAIL with `cannot find function 'validate_custom_channels' in this scope` and `cannot find type 'CustomChannelError' in this scope`.

- [ ] **Step 3: Add the validator + constants + error**

In `crates/sdr-core/src/acars_airband_lock.rs`, after the existing constants (after line 62 — `EUROPE_SIX_CHANNELS_HZ` block), insert:

```rust
/// Maximum number of channels in a user-defined custom region.
/// Sized for any realistic ACARS cluster within
/// `MAX_CHANNEL_SPAN_HZ`. Issue #592.
pub const MAX_CUSTOM_CHANNELS: usize = 8;

/// Maximum allowed span (max - min) of a custom channel set
/// in Hz. Set to 2.4 MHz to leave a 100 kHz margin against
/// the 2.5 MSps source rate (Nyquist bandwidth ≈ 2.5 MHz).
/// Issue #592.
pub const MAX_CHANNEL_SPAN_HZ: f64 = 2_400_000.0;

/// Error variants returned by [`validate_custom_channels`].
/// `Display` derive produces user-facing toast text via
/// `thiserror::Error`. Issue #592 / CR round 2 on PR #598
/// (matches the project convention of using thiserror for
/// library error types rather than hand-rolling Display +
/// Error impls).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CustomChannelError {
    #[error("Custom channel list is empty")]
    Empty,
    #[error("Too many custom channels ({count}); maximum is {max}")]
    TooMany { count: usize, max: usize },
    #[error("Invalid custom-channel frequency: {value}")]
    InvalidFrequency { value: f64 },
    #[error(
        "Span {:.3} MHz exceeds {:.3} MHz limit ({:.3} to {:.3} MHz)",
        *span_hz / 1_000_000.0,
        MAX_CHANNEL_SPAN_HZ / 1_000_000.0,
        *low_hz / 1_000_000.0,
        *high_hz / 1_000_000.0
    )]
    SpanExceeded { low_hz: f64, high_hz: f64, span_hz: f64 },
}

/// Validate a slice of custom-channel frequencies (Hz). Returns
/// `Ok(())` if the list is non-empty, ≤ `MAX_CUSTOM_CHANNELS`,
/// all values are finite + positive, and `max - min ≤
/// MAX_CHANNEL_SPAN_HZ`. Issue #592.
pub fn validate_custom_channels(chans: &[f64]) -> Result<(), CustomChannelError> {
    if chans.is_empty() {
        return Err(CustomChannelError::Empty);
    }
    if chans.len() > MAX_CUSTOM_CHANNELS {
        return Err(CustomChannelError::TooMany {
            count: chans.len(),
            max: MAX_CUSTOM_CHANNELS,
        });
    }
    for &c in chans {
        if !c.is_finite() || c <= 0.0 {
            return Err(CustomChannelError::InvalidFrequency { value: c });
        }
    }
    let (mut min, mut max) = (chans[0], chans[0]);
    for &c in &chans[1..] {
        if c < min {
            min = c;
        }
        if c > max {
            max = c;
        }
    }
    let span = max - min;
    if span > MAX_CHANNEL_SPAN_HZ {
        return Err(CustomChannelError::SpanExceeded {
            low_hz: min,
            high_hz: max,
            span_hz: span,
        });
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p sdr-core acars_airband_lock::tests
```

Expected: PASS — 10 tests green (the 9 new ones + 1 `custom_channel_error_display_span_exceeded`).

- [ ] **Step 5: Verify clippy + fmt**

```bash
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean. If `clippy::float_cmp` fires on `value == 0.0` (we use `c <= 0.0` which is fine; `if value == 0.0` in tests is acceptable for f64::NAN comparison context — wrap in `#[allow(clippy::float_cmp)]` per-test if needed).

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/src/acars_airband_lock.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #592 add custom-channel constants + validator

Adds MAX_CUSTOM_CHANNELS=8, MAX_CHANNEL_SPAN_HZ=2_400_000,
CustomChannelError (Empty/TooMany/InvalidFrequency/SpanExceeded
variants with Display + Error impls), and validate_custom_channels.

Validator rules:
- 1 ≤ N ≤ MAX_CUSTOM_CHANNELS
- All values finite and > 0
- max - min ≤ MAX_CHANNEL_SPAN_HZ

Display formats SpanExceeded as user-facing toast text:
"Span X.XX MHz exceeds 2.4 MHz limit (Y.YYY to Z.ZZZ MHz)".

Pure data — wired into AcarsRegion::Custom in Task 8 and the
Aviation panel apply handler in Task 12.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Add `AcarsRegion::Custom` variant + drop `Copy` (TDD round-trip)

**Files:**
- Modify: `crates/sdr-core/src/acars_airband_lock.rs`

Add the `Custom(Box<[f64]>)` variant. This forces `AcarsRegion` to drop `Copy` (Box isn't Copy). Update `channels()` to return `&[f64]`. Extend `config_id`, `from_config_id`, `display_label` with the Custom arm. Remove `Copy` from `#[derive]`.

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
    #[test]
    fn from_config_id_round_trips_custom() {
        // Custom is a placeholder — channels live in a separate
        // config key, loaded by window.rs::startup-replay.
        let r = AcarsRegion::from_config_id("custom");
        assert_eq!(r.config_id(), "custom");
        // Empty placeholder: from_config_id returns Custom([])
        // unconditionally; the actual frequencies are populated
        // by the load-side caller.
        assert_eq!(r.channels(), &[] as &[f64]);
    }

    #[test]
    fn channels_accessor_returns_borrowed_slice() {
        let us = AcarsRegion::Us6;
        let eu = AcarsRegion::Europe;
        assert_eq!(us.channels().len(), ACARS_CHANNEL_COUNT);
        assert_eq!(eu.channels().len(), ACARS_CHANNEL_COUNT);

        let custom = AcarsRegion::Custom(Box::from([131_550_000.0, 131_525_000.0].as_slice()));
        assert_eq!(custom.channels().len(), 2);
        assert_eq!(custom.channels()[0], 131_550_000.0);
    }

    #[test]
    fn display_label_custom() {
        let custom = AcarsRegion::Custom(Box::from([131_550_000.0].as_slice()));
        assert_eq!(custom.display_label(), "Custom");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p sdr-core acars_airband_lock --features whisper-cpu 2>&1 | tail -10
```

Expected: FAIL — `Custom` is not a variant of `AcarsRegion`.

- [ ] **Step 3: Update the enum + impls**

Replace the `AcarsRegion` enum + impl block in `crates/sdr-core/src/acars_airband_lock.rs` (lines 76-153) with:

```rust
// Note: no `Eq` derive — `f64` only implements `PartialEq` (NaN
// breaks the reflexivity contract Eq requires), so `Custom(Box<[f64]>)`
// blocks `Eq`. The pre-#592 enum had `Eq` because all variants
// were unit; dropping it is forced by the new payload. Existing
// consumers should be using `==` / `!=` / `assert_eq!` which all
// route through `PartialEq` and continue to work. CR round 5 on
// PR #598.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub enum AcarsRegion {
    /// North America (default). Six channels in 129.125–
    /// 131.550 MHz.
    #[default]
    Us6,
    /// Europe. Six channels clustered in 131.450–131.875 MHz.
    Europe,
    /// User-defined channel set. Frequencies in Hz; validated
    /// via `validate_custom_channels` at construction time.
    /// Issue #592.
    Custom(Box<[f64]>),
}

impl AcarsRegion {
    /// Channels for this region (Hz). Returns a borrowed slice
    /// so all variants — including `Custom` — share one
    /// accessor. Issue #592.
    #[must_use]
    pub fn channels(&self) -> &[f64] {
        match self {
            Self::Us6 => &US_SIX_CHANNELS_HZ,
            Self::Europe => &EUROPE_SIX_CHANNELS_HZ,
            Self::Custom(c) => c,
        }
    }

    /// Source center frequency for this region (Hz). Computed
    /// as the midpoint of `min(channels)` and `max(channels)`
    /// so the cluster fits symmetrically inside the 2.5 MHz
    /// Nyquist window.
    #[must_use]
    pub fn center_hz(&self) -> f64 {
        let chans = self.channels();
        if chans.is_empty() {
            // `Custom([])` placeholder shouldn't reach engage,
            // but keep this defensive — return 0.0.
            return 0.0;
        }
        let mut min = chans[0];
        let mut max = chans[0];
        for &c in &chans[1..] {
            if c < min {
                min = c;
            }
            if c > max {
                max = c;
            }
        }
        f64::midpoint(min, max)
    }

    /// Stable string id used as the `acars_region` config key
    /// value. Round-trips with `from_config_id`. The `Custom`
    /// arm always returns `"custom"`; the actual channel list
    /// is persisted under a separate `acars_custom_channels`
    /// config key.
    #[must_use]
    pub fn config_id(&self) -> &'static str {
        match self {
            Self::Us6 => "us-6",
            Self::Europe => "europe",
            Self::Custom(_) => "custom",
        }
    }

    /// Inverse of `config_id`. Falls back to default on
    /// unrecognised strings. Returns `Custom(Box::new([]))`
    /// for `"custom"` — the load-side caller in
    /// `window.rs::startup_replay` populates the actual
    /// frequencies from `acars_custom_channels`.
    #[must_use]
    pub fn from_config_id(id: &str) -> Self {
        match id {
            "europe" => Self::Europe,
            "custom" => Self::Custom(Box::new([])),
            _ => Self::Us6,
        }
    }

    /// Display label for the Aviation panel combo row.
    #[must_use]
    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Us6 => "United States (US-6)",
            Self::Europe => "Europe",
            Self::Custom(_) => "Custom",
        }
    }
}
```

Key changes:
1. `#[derive(Copy)]` removed.
2. `channels(self)` → `channels(&self)`, returns `&[f64]`.
3. `center_hz(self)` → `center_hz(&self)`, handles empty Custom defensively.
4. `config_id(self)` → `config_id(&self)`.
5. `from_config_id` adds `"custom"` arm.
6. `display_label(self)` → `display_label(&self)`.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p sdr-core acars_airband_lock 2>&1 | tail -20
```

Expected: tests for the new behaviour pass. **Existing call sites (and existing tests) of `region.channels()` etc. that use the old `Copy` calling convention may now break.** Use the next steps to surface and fix.

- [ ] **Step 5: Fix call sites that broke from dropping `Copy`**

```bash
cargo build -p sdr-core 2>&1 | grep error | head -20
```

For each error, the fix is one of:
- Was `let r: AcarsRegion = …; let chans = r.channels();` → still works (now borrows).
- Was `region.channels()` returning `[f64; 6]` and indexed as array → adjust to slice indexing (no source changes usually; `&[f64]` and `[f64; 6]` both index with `[i]`).
- Was passing `region` by value to a function: change to `&region` or `region.clone()` depending on caller's use.
- Was `match region { ... }` exhaustive: now requires `Custom(_)` arm or `_ => ...`; add what's appropriate.

This is a search-and-fix-compile-errors task. The likely affected files are `controller.rs`, `acars_config.rs`, and `window.rs`. The diff per file is small (typically `region.channels()` is borrowable as-is).

- [ ] **Step 6: Update existing region tests**

The existing tests for `Us6` / `Europe` likely call `.channels()` and compare to const arrays. They should still work because slice equality with array equality is automatic. If any tests do `let r = AcarsRegion::Us6; let chans = r.channels();` then later `region.center_hz()` where `region` was moved (Copy), they need to switch to `region.center_hz()` after binding `r` (no longer Copy → no move issue, just borrow).

- [ ] **Step 7: Verify build + test + clippy + fmt across sdr-core**

```bash
cargo build -p sdr-core
cargo test -p sdr-core --features whisper-cpu
cargo clippy -p sdr-core --features whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/sdr-core/src/acars_airband_lock.rs crates/sdr-core/src/controller.rs crates/sdr-core/src/acars_config.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #592 add AcarsRegion::Custom variant; drop Copy

Adds Custom(Box<[f64]>) variant with associated impl arms.
channels() now returns &[f64] (borrowed slice — works for both
predefined static arrays and the Custom variant's Box payload).
config_id/from_config_id round-trip "custom" as a placeholder
(empty Box); the actual frequencies live in a separate
acars_custom_channels config key, loaded by startup-replay
in window.rs.

Drops `Copy` from the enum (forced — Box<[f64]> isn't Copy).
Existing call sites that took region by value were borrowing
the value already; the few that moved get a `.clone()` or `&`
prefix.

Test: from_config_id("custom") round-trips; channels accessor
returns borrowed slice for all variants; display_label("Custom").

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: const-array → `Vec` migration sweep

**Files:**
- Modify: `crates/sdr-ui/src/state.rs`
- Modify: `crates/sdr-ui/src/window.rs`
- Modify: `crates/sdr-ui/src/sidebar/aviation_panel.rs`
- Modify: `crates/sdr-core/src/controller.rs`
- Modify: `crates/sdr-ffi/src/event.rs`

These changes must land in one commit because partial migration breaks the build (state.rs uses `Vec<ChannelStats>` but window.rs still does `*… = [Default; 6]`). Mechanical type-substitution sweep.

- [ ] **Step 1: `crates/sdr-ui/src/state.rs:267`**

Find:
```rust
pub acars_channel_stats: RefCell<[ChannelStats; ACARS_CHANNEL_COUNT]>,
```

Replace with:
```rust
pub acars_channel_stats: RefCell<Vec<ChannelStats>>,
```

- [ ] **Step 2: `crates/sdr-ui/src/state.rs:421`**

Find:
```rust
acars_channel_stats: RefCell::new([ChannelStats::default(); ACARS_CHANNEL_COUNT]),
```

Replace with:
```rust
acars_channel_stats: RefCell::new(Vec::new()),
```

(Default empty; it's filled on engage from `region.channels().len()`.)

- [ ] **Step 3: `crates/sdr-ui/src/state.rs:524-526`**

Find:
```rust
state.acars_channel_stats.borrow().len(),
sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT,
"stats array width sourced from ACARS_CHANNEL_COUNT"
```

(This is inside an assert/debug_assert.) Replace the surrounding assertion with one that checks the stats vec width matches the active region's channel count instead:

```rust
state.acars_channel_stats.borrow().len(),
state.acars_region.borrow().channels().len(),
"stats vec width matches active region's channel count"
```

(If `acars_region` field doesn't exist on `AppState` — check it; if absent, leave a comment and let a separate task add it. For now, drop the assertion if there's no stable source for the expected width.)

- [ ] **Step 4: `crates/sdr-ui/src/sidebar/aviation_panel.rs:52`**

Find:
```rust
pub channel_rows: [adw::ActionRow; ACARS_CHANNEL_COUNT],
```

Replace with:
```rust
pub channel_rows: Vec<adw::ActionRow>,
```

- [ ] **Step 5: `crates/sdr-ui/src/sidebar/aviation_panel.rs:174`**

Find the existing `array::from_fn` block:
```rust
let channel_rows: [adw::ActionRow; ACARS_CHANNEL_COUNT] = std::array::from_fn(|_| {
    let row = adw::ActionRow::builder().title("—").subtitle("—").build();
    channels_group.add(&row);
    row
});
```

Replace with a Vec-builder that takes a `channel_count: usize` parameter (which is set from the active region in the caller):

```rust
let channel_rows: Vec<adw::ActionRow> = (0..channel_count)
    .map(|_| {
        let row = adw::ActionRow::builder().title("—").subtitle("—").build();
        channels_group.add(&row);
        row
    })
    .collect();
```

The enclosing function signature also needs the new parameter. Find `pub fn build_aviation_panel(...)` and add `channel_count: usize` to it. Update doc comments accordingly.

- [ ] **Step 6: `crates/sdr-ui/src/sidebar/aviation_panel.rs:238`**

The `channel_rows` field-init in the struct literal `AviationPanel { … channel_rows, … }` already takes a Vec — no change needed beyond the type-substitution above.

- [ ] **Step 7: `crates/sdr-ui/src/window.rs:2092`**

Find:
```rust
*state.acars_channel_stats.borrow_mut() = *ch_stats;
```

(Where `ch_stats` was `&[ChannelStats; ACARS_CHANNEL_COUNT]`.) Replace with:

```rust
*state.acars_channel_stats.borrow_mut() = ch_stats.to_vec();
```

If `ch_stats` is now `&[ChannelStats]` (from a slice-returning DSP message), `.to_vec()` works directly.

- [ ] **Step 8: `crates/sdr-ui/src/window.rs:2156-2157`**

Find:
```rust
*state.acars_channel_stats.borrow_mut() = [sdr_acars::ChannelStats::default();
    sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT];
```

Replace with:
```rust
state.acars_channel_stats.borrow_mut().clear();
```

(Clearing is the right semantic — disengage zeroes out the stats display.)

- [ ] **Step 9: `crates/sdr-ui/src/window.rs:11998` and `:12041`**

These iterate `channel_rows` / `acars_channel_stats`. Iteration via `.iter()` works the same on `Vec<T>` and `[T; N]` — no change needed unless the iteration relied on a fixed length.

If a `.zip()` was previously matching aircraft rows to fixed-length stats, ensure both sides are now Vecs that may have different lengths during transitions. Add `.take(min(rows.len(), stats.len()))` if a length-mismatch could occur.

- [ ] **Step 10: `crates/sdr-core/src/controller.rs:3708`**

Find:
```rust
crate::acars_airband_lock::ACARS_CHANNEL_COUNT]>::try_from(
```

This is a `<[T; ACARS_CHANNEL_COUNT]>::try_from(slice)` call. Replace with a `Vec::from(slice)` or just pass the slice directly through to whichever consumer needed it. The exact replacement depends on the surrounding context; the rule is "drop the const-sized array; pass slice or Vec directly".

- [ ] **Step 11: `crates/sdr-ffi/src/event.rs:1296`**

Find the test fixture:
```rust
[sdr_acars::ChannelStats::default(); sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT],
```

Replace with:
```rust
vec![sdr_acars::ChannelStats::default(); sdr_core::acars_airband_lock::ACARS_CHANNEL_COUNT],
```

Note: `ACARS_CHANNEL_COUNT` stays as a constant — it's still useful as the *predefined* count. The migration is just from `[T; N]` to `Vec<T>` at storage sites.

- [ ] **Step 12: Update `build_aviation_panel` callers**

Find every caller of `build_aviation_panel` (use grep). Each needs to pass `channel_count`. Source it from the active region:

```rust
let region = state.config.borrow().acars_region();
let channel_count = region.channels().len();
let panel = build_aviation_panel(channel_count);
```

- [ ] **Step 13: Verify build + test + clippy + fmt across the workspace**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean. The migration is type-equivalent for predefined regions (length=6), so existing behaviour is preserved.

- [ ] **Step 14: Commit**

```bash
git add crates/sdr-ui/src/state.rs crates/sdr-ui/src/window.rs crates/sdr-ui/src/sidebar/aviation_panel.rs crates/sdr-core/src/controller.rs crates/sdr-ffi/src/event.rs
git commit -m "$(cat <<'EOF'
refactor(sdr-ui,sdr-core,sdr-ffi): #592 const-array → Vec for ACARS channels

Migrates [ChannelStats; ACARS_CHANNEL_COUNT] and
[ActionRow; ACARS_CHANNEL_COUNT] storage sites to Vec
across ~16 sites in 5 files. ACARS_CHANNEL_COUNT itself
stays as a constant (still meaningful as the predefined-
region count).

build_aviation_panel now takes channel_count: usize so it
can build a row list sized to the active region (US-6 / Europe
both 6, Custom variable up to MAX_CUSTOM_CHANNELS).

Atomic single-commit migration — partial migration would
break the build (state.rs Vec assignment vs window.rs array
literal incompatibility).

Behaviour-preserving for predefined regions; the Custom variant
support flows in via Tasks 10-12.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: Add `acars_custom_channels` config persistence (TDD)

**Files:**
- Modify: `crates/sdr-core/src/acars_config.rs`

New config key + getters + setters. Stored as a JSON array of f64 Hz values.

- [ ] **Step 1: Write the failing test**

In `crates/sdr-core/src/acars_config.rs`, find or create `mod tests` at the bottom. Append:

```rust
    #[test]
    fn acars_custom_channels_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = Config::load_or_default(&path);

        config.set_acars_custom_channels(&[131_550_000.0, 131_525_000.0, 130_025_000.0]);
        config.save(&path).unwrap();

        let loaded = Config::load_or_default(&path);
        let chans = loaded.acars_custom_channels();
        assert_eq!(chans, vec![131_550_000.0, 131_525_000.0, 130_025_000.0]);
    }

    #[test]
    fn acars_custom_channels_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = Config::load_or_default(&path);
        assert_eq!(config.acars_custom_channels(), Vec::<f64>::new());
    }
```

Adjust the `Config::load_or_default` / `Config::save` calls to match the actual API of `acars_config.rs` (use grep to find existing pattern; the file likely has a similar set of round-trip tests for other keys).

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p sdr-core acars_config::tests::acars_custom_channels
```

Expected: FAIL with `cannot find function 'set_acars_custom_channels'`.

- [ ] **Step 3: Add the key + accessors**

In `crates/sdr-core/src/acars_config.rs`, find the existing key constants (e.g., `pub const ACARS_REGION_KEY: &str = "acars_region";`). Add:

```rust
pub const ACARS_CUSTOM_CHANNELS_KEY: &str = "acars_custom_channels";
```

Add to `Config`'s impl (next to `acars_region` / `set_acars_region`):

```rust
impl Config {
    /// Custom-channel frequencies (Hz) when the active region
    /// is `Custom`. Empty Vec means no custom channels saved
    /// yet (or the active region isn't Custom). Issue #592.
    #[must_use]
    pub fn acars_custom_channels(&self) -> Vec<f64> {
        self.get_array(ACARS_CUSTOM_CHANNELS_KEY)
            .unwrap_or_default()
    }

    /// Persist a custom-channel frequency list (Hz). Caller is
    /// responsible for validating via
    /// `acars_airband_lock::validate_custom_channels` before
    /// calling.
    pub fn set_acars_custom_channels(&mut self, chans: &[f64]) {
        self.set_array(ACARS_CUSTOM_CHANNELS_KEY, chans);
    }
}
```

If `get_array` / `set_array` don't exist on `Config`, find the equivalent (likely `get_value` / `set_value` with `serde_json::Value`). Use whatever pattern is established in the file.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p sdr-core acars_config 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5: Verify clippy + fmt**

```bash
cargo clippy -p sdr-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-core/src/acars_config.rs
git commit -m "$(cat <<'EOF'
feat(sdr-core): #592 persist acars_custom_channels config key

JSON array of f64 Hz values. Empty default. Caller validates
via acars_airband_lock::validate_custom_channels before set.

Round-trip tested: write 3 freqs, reload, assert equality.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Aviation panel — Custom region UI (combo entry, EntryRow, visibility binding, apply handler)

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/aviation_panel.rs`
- Modify: `crates/sdr-ui/src/window.rs`

UI-side: add `Custom` to the region combo's options; create the Custom-channels `AdwEntryRow`; bind its visibility to combo selection; wire the apply handler. Pure GTK widget code — leans on smoke verification.

- [ ] **Step 1: Extend `REGION_OPTIONS`**

In `crates/sdr-ui/src/sidebar/aviation_panel.rs`, find the existing `REGION_OPTIONS` slice (likely near the top of the file with `LEFT_ACTIVITIES`-style declarations). Add the Custom entry:

```rust
pub const REGION_OPTIONS: &[(&str, &str)] = &[
    ("us-6", "United States (US-6)"),
    ("europe", "Europe"),
    ("custom", "Custom"),
];

/// Resolve a region id (e.g. `"custom"`) to its slot index in
/// `REGION_OPTIONS`. Returns `None` for unknown ids — callers
/// fall through to the predefined default.
///
/// Index-keyed UI logic (the visibility-binding for the Custom
/// channels entry-row, the rebuild handler) routes through
/// this rather than hardcoding the slot number, so reordering
/// `REGION_OPTIONS` only requires editing one slice. CR round
/// 2 on PR #598 (round 1 introduced a `CUSTOM_REGION_COMBO_INDEX`
/// constant; round 2 generalised to the lookup helper).
#[must_use]
pub fn region_index_for_id(id: &str) -> Option<u32> {
    REGION_OPTIONS
        .iter()
        .position(|(opt_id, _)| *opt_id == id)
        .and_then(|p| u32::try_from(p).ok())
}
```

Adjust the field shape to match the existing tuple layout (string id + display label).

- [ ] **Step 2: Add the Custom EntryRow + visibility binding**

In the panel-builder function (around the existing region combo creation), after the `region_row.set_model(...)` line:

```rust
let custom_channels_row = adw::EntryRow::builder()
    .title("Custom channels (MHz, comma-separated)")
    .build();
custom_channels_row.set_visible(false);
acars_group.add(&custom_channels_row);

// Bind visibility to "selected slot resolves to Custom" via
// the lookup helper rather than a raw index. CR round 2 on
// PR #598.
{
    let custom_row = custom_channels_row.clone();
    region_row.connect_selected_notify(move |row| {
        custom_row.set_visible(Some(row.selected()) == region_index_for_id("custom"));
    });
}
```

Add `custom_channels_row` to the `AviationPanel` struct (new field), so `connect_aviation_panel` in `window.rs` can attach the `connect_apply` handler.

- [ ] **Step 3: Wire apply handler in `window.rs::connect_aviation_panel`**

In `crates/sdr-ui/src/window.rs`, find `connect_aviation_panel`. Add (after the existing region-combo handler):

```rust
// Apply handler for custom-channels entry. Fires on Enter or
// focus-loss. Parses CSV → validates → on success persists
// + dispatches SetAcarsRegion(Custom). On failure: toast +
// inline error CSS.
let custom_row = panels.aviation.custom_channels_row.clone();
{
    let state = Rc::clone(state);
    let toast_overlay = toast_overlay.clone();
    let row_for_handler = custom_row.clone();
    custom_row.connect_apply(move |row| {
        let text = row.text();
        let parsed: Result<Vec<f64>, String> = text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<f64>()
                    .map(|mhz| mhz * 1_000_000.0)
                    .map_err(|e| format!("'{s}': {e}"))
            })
            .collect();

        match parsed {
            Err(e) => {
                row_for_handler.add_css_class("error");
                let toast = adw::Toast::builder()
                    .title(format!("Invalid custom channels: {e}"))
                    .timeout(5)
                    .build();
                toast_overlay.add_toast(toast);
                return;
            }
            Ok(chans) => match validate_custom_channels(&chans) {
                Err(e) => {
                    row_for_handler.add_css_class("error");
                    let toast = adw::Toast::builder()
                        .title(e.to_string())
                        .timeout(5)
                        .build();
                    toast_overlay.add_toast(toast);
                }
                Ok(()) => {
                    row_for_handler.remove_css_class("error");
                    state
                        .config
                        .borrow_mut()
                        .set_acars_custom_channels(&chans);
                    let _ = state.config.borrow().save();
                    let region = AcarsRegion::Custom(chans.into_boxed_slice());
                    state.send_dsp(UiToDsp::SetAcarsRegion(region));
                }
            },
        }
    });
}
```

Imports needed: `sdr_core::acars_airband_lock::{validate_custom_channels, AcarsRegion}`.

- [ ] **Step 4: Wire region-combo apply for the channels-rebuild side**

In the same `connect_aviation_panel` function, the region combo's existing `connect_selected_notify` handler dispatches `SetAcarsRegion(...)` and persists. Extend it to also rebuild the channel rows by removing them from the channels_group and re-adding sized to `region.channels().len()`. (Or alternatively, dispatch a refresh signal that the panel listens to.)

The cleanest approach: have `window.rs` rebuild on every region change since it already owns the panel reference. Inside the existing combo handler:

```rust
// Rebuild channel_rows to match the new region's channel count.
// Resolve the AcarsRegion from the selected slot (so we don't
// depend on raw indices) and use its channels().len(). CR
// round 1 on PR #598.
// Resolve the AcarsRegion from the selected slot via a domain
// lookup so we don't depend on raw indices. CR round 2 on
// PR #598 (round 1 used a CUSTOM_REGION_COMBO_INDEX constant;
// round 2 generalised to slice lookup).
let id = REGION_OPTIONS
    .get(selected_idx as usize)
    .map(|(id, _)| *id)
    .unwrap_or("us-6");
let region = if id == "custom" {
    let saved = read_acars_custom_channels(&state.config);
    AcarsRegion::Custom(saved.into_boxed_slice())
} else {
    AcarsRegion::from_config_id(id)
};
let new_count = region.channels().len();
rebuild_aviation_channel_rows(&panels.aviation, new_count);
```

Where `rebuild_aviation_channel_rows` is a helper that:
1. Removes existing rows from `channels_group` (use `panels.aviation.channels_group.remove(&row)` for each row).
2. Builds N new `ActionRow`s; appends to `channels_group` and the `channel_rows` Vec.

This requires exposing `channels_group: adw::PreferencesGroup` on `AviationPanel`. Add it as a public field.

- [ ] **Step 5: Pre-fill the EntryRow on panel open from saved config**

After the panel is built, before connecting handlers:

```rust
// Hydrate Custom EntryRow from saved config.
let saved_chans = state.config.borrow().acars_custom_channels();
if !saved_chans.is_empty() {
    let csv = saved_chans
        .iter()
        .map(|hz| format!("{:.3}", hz / 1_000_000.0))
        .collect::<Vec<_>>()
        .join(", ");
    panels.aviation.custom_channels_row.set_text(&csv);
}
```

- [ ] **Step 6: Verify build + clippy + tests + fmt**

```bash
cargo build -p sdr-ui --features whisper-cpu
cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings
cargo test -p sdr-ui --features whisper-cpu
cargo fmt --all -- --check
```

Expected: clean. The aviation panel changes are GTK widget code; no unit tests added in this commit (smoke covers them).

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-ui/src/sidebar/aviation_panel.rs crates/sdr-ui/src/window.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #592 Aviation panel custom-channels editor

Adds:
- "Custom" option to the region combo (REGION_OPTIONS slot 2)
- AdwEntryRow "Custom channels (MHz, comma-separated)" with
  visibility bound to combo selection (visible only when Custom)
- connect_apply handler: parse CSV → multiply by 1e6 → validate
  via validate_custom_channels → on success persist + dispatch
  SetAcarsRegion(Custom); on failure add `error` CSS class +
  toast naming the offending pair (or parse error)
- pre-fill on panel open from saved acars_custom_channels config
- channels_group rebuild on region change so row count matches
  the new region's channel count

GTK widget code — leans on smoke verification (Task 13).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 12: Startup-replay handles Custom region two-key load

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

When the app starts up, the `startup_replay` (or equivalent) reads `acars_region`. For `"custom"` it must also read `acars_custom_channels` and validate before dispatching `SetAcarsRegion(Custom(...))`.

- [ ] **Step 1: Find the existing startup-replay block**

```bash
grep -n "from_config_id\|acars_region\|SetAcarsRegion" crates/sdr-ui/src/window.rs | head -10
```

Find the block where `Config::acars_region()` is read on startup. Likely in a function called `startup_replay`, `apply_startup_config`, or similar.

- [ ] **Step 2: Replace the startup-replay region resolution with the two-key version**

Find the existing pattern (likely):

```rust
let region_id = state.config.borrow().acars_region();
let region = AcarsRegion::from_config_id(&region_id);
state.send_dsp(UiToDsp::SetAcarsRegion(region));
```

Replace with:

```rust
let cfg = state.config.borrow();
let region_id = cfg.acars_region();
let region = match region_id.as_str() {
    "custom" => {
        let chans = cfg.acars_custom_channels();
        match validate_custom_channels(&chans) {
            Ok(()) => AcarsRegion::Custom(chans.into_boxed_slice()),
            Err(e) => {
                tracing::warn!(
                    "saved custom channels invalid ({e}); falling back to default region"
                );
                AcarsRegion::default()
            }
        }
    }
    _ => AcarsRegion::from_config_id(&region_id),
};
drop(cfg);
state.send_dsp(UiToDsp::SetAcarsRegion(region));
```

- [ ] **Step 3: Verify build + test + clippy + fmt**

```bash
cargo build -p sdr-ui --features whisper-cpu
cargo test -p sdr-ui --features whisper-cpu
cargo clippy -p sdr-ui --features whisper-cpu --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "$(cat <<'EOF'
feat(sdr-ui): #592 startup-replay loads Custom region from two keys

Resolves AcarsRegion at startup as a two-key read:
- acars_region holds "us-6" / "europe" / "custom"
- For "custom", also read acars_custom_channels, validate, and
  build AcarsRegion::Custom(chans.into_boxed_slice())
- On validation failure (e.g., stale config from a previous
  version), warn and fall back to AcarsRegion::default()

The Custom([]) placeholder from from_config_id never reaches
the engage path — either the stored channels load successfully,
or we fall back to Us6.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final tasks

### Task 13: Workspace gates verification (no commit)

**Files:** none — verification only.

- [ ] **Step 1: Run the full workspace gate set including --locked**

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo check --workspace --locked --no-default-features --features sherpa-cpu
cargo fmt --all -- --check
```

Expected: all clean. The `--locked` gate is the one that's saved us from CI failures; it must pass before we push (per `feedback_cargo_lock_locked_check.md`).

- [ ] **Step 2: Skim the diff for obvious issues**

```bash
git log --oneline main..HEAD
git diff main...HEAD --stat
```

Expected: ~12 commits, ~485 LOC ballpark across the files listed in the spec's File budget.

- [ ] **Step 3: No commit (verification only)**

If any gate fails, fix in-place on the appropriate task's commit.

---

### Task 14: Manual GTK smoke (USER ONLY)

**Files:** none modified.

GTK widget code is not unit-testable without a display server. Per project convention (`feedback_smoke_test_workflow.md`): Claude installs the binary; the user runs the smoke checklist manually. Claude does NOT launch the binary.

- [ ] **Step 1 (Claude): Install the binary**

```bash
make install CARGO_FLAGS="--release --features whisper-cuda"
```

Expected: `make install` builds with `--release` and `whisper-cuda` features and copies the binary to `$(BINDIR)/sdr-rs`. Per `feedback_make_install_release_flag.md`, the `--release` flag is required — without it, an old release binary stays in place. Verify with:

```bash
strings $BINDIR/sdr-rs | grep -i "Custom channels (MHz" | head -3
strings $BINDIR/sdr-rs | grep -i "ACARS output channel full" | head -3
```

Expected: at least one match for each — confirms the new strings are in the installed binary.

- [ ] **Step 2 (USER ONLY): Run the smoke checklist**

Hand off to user with:

> Build installed at `$(BINDIR)/sdr-rs`. Please run through the smoke checklist below and report which steps pass / fail before I push the branch.

**Smoke checklist (USER ONLY, copy verbatim into your pre-push report):**

1. **Async I/O sanity**
   - [ ] Start the app, engage ACARS. Toggle JSONL on, set the path to `~/sdr-recordings/acars-async-smoke.jsonl`. Watch the file accumulate over a 5-minute window.
   - [ ] Confirm zero DSP underruns / zero "channel full" warnings in the logs (check tracing output for `ACARS output channel full`).
   - [ ] Tail the log file (`tail -f ~/sdr-recordings/acars-async-smoke.jsonl`) — confirm new lines arrive in real time as ACARS messages decode.
   - [ ] Disable JSONL via the toggle; confirm the file stops growing immediately.
   - [ ] Re-enable JSONL with the SAME path; confirm the file keeps appending (writer reopens).
   - [ ] Change the JSONL path to `~/sdr-recordings/acars-async-smoke-2.jsonl` while ACARS is engaged; confirm the new path starts accumulating and the old one stops.
2. **UDP feeder sanity**
   - [ ] Start a `nc -ul 5550` listener in another terminal. In the app, enable Network feeder pointed at `127.0.0.1:5550`.
   - [ ] Confirm `nc` shows incoming JSON datagrams as ACARS messages decode.
   - [ ] Disable network feeder; confirm `nc` stops receiving.
3. **Custom region happy path**
   - [ ] Open Aviation panel; switch the Region combo to **Custom**. Confirm the "Custom channels (MHz, comma-separated)" EntryRow appears.
   - [ ] Type `131.55, 131.525, 130.025` into the EntryRow; press Enter (apply).
   - [ ] Confirm the Channels group rebuilds to 3 rows.
   - [ ] Engage ACARS; confirm decoding occurs across those 3 channels.
   - [ ] Status row shows correct channel count (3, not 6).
4. **Custom region validation**
   - [ ] In Custom mode, type `129.0, 132.0, 133.0` (span 4.0 MHz). Press Enter.
   - [ ] Confirm a toast appears with text containing "Span 4 MHz exceeds 2.4 MHz limit (129.000 to 133.000 MHz)".
   - [ ] Confirm the EntryRow shows the error CSS class (red-tinted/highlighted).
   - [ ] Confirm no DSP dispatch happened (region didn't change to Custom).
   - [ ] Type valid frequencies; confirm the error CSS class clears.
5. **Custom region max channels**
   - [ ] Type 9 frequencies (e.g. `131.000, 131.025, 131.050, 131.075, 131.100, 131.125, 131.150, 131.175, 131.200`).
   - [ ] Confirm a toast appears with "Too many custom channels (9); maximum is 8".
6. **Persistence round-trip**
   - [ ] Set a valid Custom region (e.g. `131.55, 131.525, 130.025`). Engage ACARS to confirm it works.
   - [ ] Quit the app. Reopen.
   - [ ] Confirm the panel pre-fills the EntryRow with the saved CSV.
   - [ ] Confirm the combo shows "Custom".
   - [ ] Engage ACARS; confirm decoding works on those channels.
7. **Region swap with channel count change**
   - [ ] From Custom (3 chan) → switch to US-6 → confirm Channels group rebuilds to 6 rows.
   - [ ] US-6 → Europe → confirm 6 rows again.
   - [ ] Europe → Custom → confirm 3 rows (from saved config).
   - [ ] Engage ACARS in each; confirm decoding works in each.
8. **Pause/Clear/Sort still work**
   - [ ] Quick smoke of the existing ACARS viewer features — Stream tab, By Aircraft tab, Pause, Clear — to confirm no regression from the migration.
9. **No DSP underruns under normal load**
   - [ ] Run the app for 10 minutes with ACARS engaged + JSONL on + Network on + transcription on. Watch logs for any `ACARS output channel full` warns or audio underruns.

- [ ] **Step 3: Wait for user smoke pass**

Do NOT proceed to Task 15 until the user reports the smoke checklist passing. If any step fails, fix and re-smoke before pushing.

---

### Task 15: Final pre-push sweep + push branch

**Files:** none modified.

- [ ] **Step 1: Re-run gates immediately before push**

Per `feedback_fmt_check_immediately_before_push.md`, fmt is the LAST gate before push. Per `feedback_cargo_lock_locked_check.md`, the `--locked` check is mandatory:

```bash
cargo build --workspace --features sdr-transcription/whisper-cpu
cargo test --workspace --features sdr-transcription/whisper-cpu
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings
cargo check --workspace --locked --no-default-features --features sherpa-cpu
cargo fmt --all -- --check
```

Expected: all clean.

- [ ] **Step 2: Confirm branch state**

```bash
git status
git log --oneline main..HEAD
git diff main...HEAD --stat
```

Expected: clean working tree (or only the unrelated `Cargo.lock` change if `--locked` updated it; if so, commit with `chore: cargo update for --locked` first), ~12 commits ahead of main, ~485 LOC across the files in the File budget.

- [ ] **Step 3: Push branch**

```bash
git push -u origin feat/acars-async-io-and-custom-channels
```

Expected: success. **DO NOT open the PR — the user will do that.** The plan ends here; user opens the PR via GitHub UI or `gh pr create` themselves.

---

## Spec coverage matrix

| Spec section | Task |
|---|---|
| Part A architecture (AcarsOutputs / config / writer thread) | Tasks 1-5 |
| Part A — DSP-thread try_send wiring | Task 6 |
| Part A — runtime config writes (jsonl path / network addr / station_id handlers) | Task 6 |
| Part A — drop-on-full + 30 s warn rate-limit | Task 5 |
| Part A — writer thread shutdown on disconnect | Tasks 3 + 5 |
| Part A — writer thread reopens on path change | Task 4 |
| Part A — `Drop` joins writer thread | Task 5 |
| Part B — `MAX_CUSTOM_CHANNELS` + `MAX_CHANNEL_SPAN_HZ` constants | Task 7 |
| Part B — `CustomChannelError` + `Display` impl | Task 7 |
| Part B — `validate_custom_channels` | Task 7 |
| Part B — `AcarsRegion::Custom(Box<[f64]>)` + drop `Copy` | Task 8 |
| Part B — `channels()` returns `&[f64]` | Task 8 |
| Part B — `from_config_id("custom")` round-trip | Task 8 |
| Part B — const-array → `Vec` migration (16 sites in 5 files) | Task 9 |
| Part B — `acars_custom_channels` config persistence | Task 10 |
| Part B — Aviation panel Custom EntryRow + visibility binding | Task 11 |
| Part B — apply handler with validate-and-dispatch | Task 11 |
| Part B — region-change rebuild of channel rows | Task 11 |
| Part B — startup-replay two-key load | Task 12 |
| Edge case: empty Custom on first open (engage gate refuses) | Task 7 (Empty error) + Task 12 (validates on load) |
| Edge case: N=1 custom channel | Task 7 (validate_accepts_single_channel test) |
| Edge case: span just under / over 2.4 MHz | Task 7 (validate_accepts_span_just_under / validate_rejects_span_just_over) |
| Edge case: writer thread shutdown on app exit | Task 5 (`Drop` impl) |
| Edge case: hot-reload of jsonl path mid-session | Task 4 (writer_reopens_on_path_change test) |
| Workspace gates including --locked | Task 13 + 15 |
| GTK smoke (USER ONLY) | Task 14 |
| Final push (no PR creation) | Task 15 |
