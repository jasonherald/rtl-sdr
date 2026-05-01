# ACARS Async Output I/O + Custom Channel Sets (issues #596 + #592, bundled)

> Bundle two ACARS follow-up items into a single PR: (1) move the
> JSONL writer + UDP feeder I/O to a worker thread so a slow
> disk or NFS stall can't surface as DSP-thread underruns, and
> (2) add user-defined custom channel sets to the Aviation
> panel alongside the existing US-6 / Europe predefined regions.

## Goal

Two ACARS output-side improvements that touch different bug
classes (threading vs UI/migration). Bundled per the user's
established pattern for cross-cutting CR review:

1. **#596 — async output I/O.** Move `JsonlWriter::write` and
   `UdpFeeder::send` off the DSP thread onto a dedicated writer
   thread. Bounded mpsc channel with drop-on-full + 30 s warn
   rate-limit. Closes the synchronous-I/O fragility flagged in
   CR round 4 of PR #595.

2. **#592 — custom channel sets.** Extend `AcarsRegion` with a
   `Custom(Box<[f64]>)` variant. Migrate the `[ChannelStats; 6]`
   and `[ActionRow; 6]` const-arrays to `Vec`/`Box<[T]>` across
   ~16 sites. Add an Aviation-panel CSV editor with span +
   count validation. Persist via a new `acars_custom_channels`
   config key.

## Non-goals

- **Generalising channel count above 8.** `MAX_CUSTOM_CHANNELS = 8`
  is enough for any realistic ACARS cluster within 2.4 MHz.
- **Per-channel UI customisation** (custom labels, colours, etc.).
  Only the frequency list is user-defined; rendering stays
  uniform.
- **Async I/O for non-ACARS sinks.** Audio writer, scanner CSV,
  satellite recorder all stay synchronous (none have shown the
  same DSP-thread fragility).
- **Persisted ring buffer for un-flushed messages on shutdown.**
  In-flight messages in the channel are dropped on app exit.
  Acceptable given the 256-deep buffer + sub-second drain time.

## Architecture

### Module layout

```text
crates/sdr-core/src/
├── acars_output.rs         ← extend: AcarsOutputs becomes
│                              owner of the writer thread + tx
├── controller.rs           ← MODIFY: AcarsOutputs lives in DspState;
│                              acars_decode_tap try_send's instead
│                              of calling write/send directly
├── acars_airband_lock.rs   ← MODIFY: AcarsRegion gets Custom;
│                              channels() returns &[f64]; new
│                              constants + validate_custom_channels
├── acars_config.rs         ← MODIFY: new acars_custom_channels key
crates/sdr-ui/src/
├── state.rs                ← MODIFY: acars_channel_stats: Vec
├── window.rs               ← MODIFY: array→Vec assignments + iter
├── sidebar/aviation_panel.rs ← MODIFY: channel_rows Vec; new Custom
│                                EntryRow + visibility binding
crates/sdr-ffi/src/
├── event.rs                ← MODIFY: test fixture array→Vec
crates/sdr-acars/src/
└── lib.rs / channel.rs     ← REVIEW only — `ChannelStats` is
                              already `&[ChannelStats]` at API
                              boundary; no changes expected
```

### Part A — Async output I/O (#596)

#### Components

**`AcarsOutputs` struct** (`crates/sdr-core/src/acars_output.rs`):

```rust
pub struct AcarsOutputs {
    /// Sender side held by the DSP thread. `try_send` drops on
    /// full; the writer thread owns the receiver.
    pub tx: mpsc::SyncSender<AcarsOutputMessage>,
    /// Shared, runtime-mutable config readable by the writer
    /// thread, mutable from window.rs handlers (jsonl path/
    /// enable, network addr/enable, station_id). Read-heavy
    /// access pattern fits RwLock; lock contention is
    /// negligible against UDP send latency.
    pub config: Arc<RwLock<AcarsWriterConfig>>,
    /// Join handle for clean shutdown via Drop.
    writer_thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, Default)]
pub struct AcarsWriterConfig {
    pub jsonl_path: Option<PathBuf>,
    pub network_addr: Option<String>,
    pub station_id: Option<String>,
}

pub enum AcarsOutputMessage {
    Decoded(AcarsMessage),
}
```

#### Channel + drop semantics

- `mpsc::sync_channel(256)` (bounded). 256 ≈ 4–5 minutes of
  worst-case ACARS bursts; covers any realistic disk stall short
  of total filesystem hang.
- DSP-thread closure (`acars_decode_tap`) does
  `try_send(Decoded(msg))`:
  - `Ok(())` → message handed off; nothing more.
  - `Err(TrySendError::Full(_))` → drop. Increment a drop counter
    on `AcarsOutputs`; emit a 30 s rate-limited warn:
    *"ACARS output channel full (N drops in 30s); writer thread
    falling behind"*.
  - `Err(TrySendError::Disconnected(_))` → silent (writer thread
    already gone — only happens during shutdown, no warn needed).

#### Writer thread

Spawned in `AcarsOutputs::new()`:

```rust
fn run_writer(
    rx: mpsc::Receiver<AcarsOutputMessage>,
    config: Arc<RwLock<AcarsWriterConfig>>,
) {
    let mut jsonl: Option<(PathBuf, JsonlWriter)> = None;
    let mut udp:   Option<(String, UdpFeeder)>   = None;

    loop {
        match rx.recv() {
            Ok(AcarsOutputMessage::Decoded(msg)) => {
                let cfg = config.read().expect("writer config poisoned");
                // Reopen JsonlWriter if path changed (or is None).
                ensure_jsonl(&mut jsonl, cfg.jsonl_path.as_deref());
                // Reopen UdpFeeder if addr changed (or is None).
                ensure_udp(&mut udp, cfg.network_addr.as_deref());
                let station_id = cfg.station_id.clone();
                drop(cfg);

                if let Some((_, w)) = jsonl.as_mut() {
                    if let Err(e) = w.write(&msg, station_id.as_deref()) {
                        warn_jsonl_rate_limited(e);
                    }
                }
                if let Some((_, f)) = udp.as_mut() {
                    if let Err(e) = f.send(&msg, station_id.as_deref()) {
                        warn_udp_rate_limited(e);
                    }
                }
            }
            Err(_) => break, // tx dropped
        }
    }
}
```

`ensure_jsonl` / `ensure_udp` rebuild the writer when the
configured path/addr changed (or close it when set to `None`).
The 30 s rate-limit on per-write/per-send failures stays —
moves from the DSP closure into the writer thread.

#### Lifecycle

- **Startup:** `DspState::new` constructs `AcarsOutputs::new()`
  once. The writer thread spawns immediately and waits on `rx`.
- **Config updates:** `window.rs` handlers acquire
  `outputs.config.write()` and mutate. The writer thread sees
  the change on the next message via its `read()`.
- **Shutdown:** `DspState` drops → `AcarsOutputs` drops →
  `tx` drops → `recv()` returns `Err(Disconnected)` → loop
  exits → `JoinHandle::join()` reaps the thread.

### Part B — Custom channel sets (#592)

#### Type changes

**`AcarsRegion`** (`crates/sdr-core/src/acars_airband_lock.rs`):

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum AcarsRegion {
    #[default]
    Us6,
    Europe,
    /// User-defined channel set. Frequencies in Hz, validated
    /// at construction by `validate_custom_channels`.
    Custom(Box<[f64]>),
}

impl AcarsRegion {
    /// Channels for this region (Hz). Returns a borrowed slice
    /// so all variants share one accessor.
    #[must_use]
    pub fn channels(&self) -> &[f64] {
        match self {
            Self::Us6    => &US_SIX_CHANNELS_HZ,
            Self::Europe => &EUROPE_SIX_CHANNELS_HZ,
            Self::Custom(c) => c,
        }
    }

    pub fn center_hz(&self) -> f64 { /* unchanged — iterates self.channels() */ }

    pub fn config_id(&self) -> &'static str {
        match self {
            Self::Us6        => "us-6",
            Self::Europe     => "europe",
            Self::Custom(_)  => "custom",
        }
    }

    pub fn from_config_id(id: &str) -> Self {
        match id {
            "europe" => Self::Europe,
            "custom" => Self::Custom(Box::new([])),  // freqs from separate key
            _        => Self::Us6,
        }
    }

    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Us6        => "United States (US-6)",
            Self::Europe     => "Europe",
            Self::Custom(_)  => "Custom",
        }
    }
}
```

**Drops `Copy`** (forced by `Box<[f64]>`). Existing call sites
that took `AcarsRegion` by value need to switch to `&AcarsRegion`
or `region.clone()`. The actual call sites are few — most code
holds a region in `state.rs`'s `acars_region: RefCell<AcarsRegion>`
and reads via borrow.

#### Validator

```rust
pub const MAX_CUSTOM_CHANNELS: usize = 8;
pub const MAX_CHANNEL_SPAN_HZ: f64 = 2_400_000.0;

#[derive(Debug, Clone, PartialEq)]
pub enum CustomChannelError {
    Empty,
    TooMany { count: usize, max: usize },
    InvalidFrequency { value: f64 },
    SpanExceeded { low_hz: f64, high_hz: f64, span_hz: f64 },
}

pub fn validate_custom_channels(chans: &[f64]) -> Result<(), CustomChannelError> {
    if chans.is_empty() { return Err(CustomChannelError::Empty); }
    if chans.len() > MAX_CUSTOM_CHANNELS {
        return Err(CustomChannelError::TooMany { count: chans.len(), max: MAX_CUSTOM_CHANNELS });
    }
    for &c in chans {
        if !c.is_finite() || c <= 0.0 {
            return Err(CustomChannelError::InvalidFrequency { value: c });
        }
    }
    let (mut min, mut max) = (chans[0], chans[0]);
    for &c in &chans[1..] {
        if c < min { min = c; }
        if c > max { max = c; }
    }
    let span = max - min;
    if span > MAX_CHANNEL_SPAN_HZ {
        return Err(CustomChannelError::SpanExceeded {
            low_hz: min, high_hz: max, span_hz: span,
        });
    }
    Ok(())
}
```

`Display` impl on `CustomChannelError` formats user-friendly
toast text. Example for `SpanExceeded`:
*"Span 3.5 MHz exceeds 2.4 MHz limit (129.125 to 132.625 MHz)"*.

#### Const-array → Vec migration

Affected sites (16 total across 6 files; identified via
`grep -rn ACARS_CHANNEL_COUNT crates/`):

| File | Change |
|------|--------|
| `crates/sdr-ui/src/state.rs:267` | `[ChannelStats; ACARS_CHANNEL_COUNT]` → `Vec<ChannelStats>` |
| `crates/sdr-ui/src/state.rs:421` | initializer `[default; 6]` → `Vec::new()` (or `Vec::with_capacity(6)`) |
| `crates/sdr-ui/src/state.rs:524-526` | length assertion: drop the const-equality check; replace with bounds check on engage |
| `crates/sdr-ui/src/sidebar/aviation_panel.rs:52` | field type `Vec<adw::ActionRow>` |
| `crates/sdr-ui/src/sidebar/aviation_panel.rs:174` | `array::from_fn` → `Vec::with_capacity` + push loop, given a `channel_count: usize` parameter |
| `crates/sdr-ui/src/sidebar/aviation_panel.rs:238` | `channel_rows` field init in struct literal — already a Vec, no special handling |
| `crates/sdr-ui/src/window.rs:2092` | direct array assignment → Vec assignment (length-aware) |
| `crates/sdr-ui/src/window.rs:2156-2157` | `[default; 6]` → `Vec::new()` clear |
| `crates/sdr-ui/src/window.rs:11998` | `.iter()` over channel_rows — works as-is |
| `crates/sdr-ui/src/window.rs:12041` | iteration — works as-is |
| `crates/sdr-core/src/controller.rs:3708` | `try_from::<&[T], [T; N]>` → `Vec<T>::from(slice)` or direct push from the source iterator |
| `crates/sdr-ffi/src/event.rs:1296` | test fixture `[default; 6]` → `vec![default; 6]` |

`ACARS_CHANNEL_COUNT` itself stays as a constant — it's still
useful as the *predefined* count and as the `Vec::with_capacity`
hint when the actual length isn't known up-front.

**Aviation panel rebuild on region change:** `connect_aviation_panel`
in `window.rs` listens for the region combo's `notify::selected`.
Currently it dispatches `SetAcarsRegion(region)` and persists.
Extend: when the selected region changes, **rebuild** the
`channel_rows` list (remove all from the `AdwPreferencesGroup`,
build new ones from `region.channels().len()`, add). Always
rebuild — recycling rows for the same-N case (US-6 ↔ Europe,
both 6) adds bookkeeping that isn't worth it.

#### UI: Custom region editor

**Aviation panel (`crates/sdr-ui/src/sidebar/aviation_panel.rs`):**

1. Region combo gets a third entry: `[US-6, Europe, Custom]`.
   Update `REGION_OPTIONS` slice + `from_combo_index` helper
   accordingly (use `from_config_id("custom")` for the third).

2. New `AdwEntryRow` titled *"Custom channels (MHz, comma-separated)"*,
   added to the Aviation group right after the region combo:

```rust
let custom_channels_row = adw::EntryRow::builder()
    .title("Custom channels (MHz, comma-separated)")
    .build();
custom_channels_row.set_visible(false);  // shown only when Custom selected
```

3. **Visibility binding:** `region_row.bind_property("selected", &custom_channels_row, "visible")`
   with a transform fn that returns `true` iff selected index
   is the Custom slot.

4. **Apply handler** (on `connect_apply` — fires on Enter or
   focus-loss of the EntryRow):
   - Parse text: split by `,`, trim, parse each as `f64`.
     Multiply by 1_000_000 to get Hz.
   - Run `validate_custom_channels`.
   - **Pass:** persist via `Config::set_acars_custom_channels`,
     dispatch `UiToDsp::SetAcarsRegion(Custom(parsed))`, remove
     any prior `error` CSS class.
   - **Fail:** `custom_channels_row.add_css_class("error")` for
     inline visual feedback, AND emit a toast via the existing
     toast overlay with the `Display` text from
     `CustomChannelError`.

5. Hydrate on panel open: read `Config::acars_custom_channels()`
   and pre-fill the EntryRow with the stored CSV (formatted
   to 3 decimal MHz).

#### Persistence

New config key in `acars_config.rs`:

```rust
pub const ACARS_CUSTOM_CHANNELS_KEY: &str = "acars_custom_channels";

impl Config {
    pub fn acars_custom_channels(&self) -> Vec<f64> { /* parse JSON array */ }
    pub fn set_acars_custom_channels(&mut self, chans: &[f64]) { /* serialize JSON array */ }
}
```

Stored as a JSON array of f64 Hz values (matches the existing
config style).

The existing `acars_region` key holds `"us-6"` / `"europe"` /
`"custom"`. On load (in the startup-replay path in `window.rs`):

1. Read `acars_region` key. If `"custom"`, also read
   `acars_custom_channels` key.
2. If channels is non-empty AND passes `validate_custom_channels`:
   build `AcarsRegion::Custom(channels.into_boxed_slice())`.
3. Otherwise: fall back to `AcarsRegion::default()` (Us6) and
   leave the panel showing Custom with an empty EntryRow + an
   inline error if the saved channels were invalid (a toast
   would be wrong here — startup is not a user action).

`from_config_id("custom")` itself returns `Custom(Box::new([]))`
unconditionally — the *channels* live in the second key, and
this two-step is handled by the load-side caller, not by
`from_config_id`. The `Custom([])` placeholder is never engaged
(the engage gate refuses it via `CustomChannelError::Empty`).

## Data flow

```text
                    (UI thread)                                (DSP thread)
                         │                                          │
   user toggles JSONL    │                                          │
   → write config lock   │  outputs.config.write() = …             │
                         │                                          │
   user types CSV freqs  │  validate → SetAcarsRegion(Custom(c))   │
   → dispatch to DSP     │  ─────────────────────────────────────►  │
                         │                                          │ engage uses
                         │                                          │ region.channels()
                         │                                          │ verbatim
                         │                                          │
                         │            (DSP)              (Writer thread)
                         │              │                       │
                         │  msg decoded │ try_send ─►          │
                         │              │ ─────────────────────►│ recv()
                         │              │                       │ read config lock
                         │              │                       │ JsonlWriter::write
                         │              │                       │ UdpFeeder::send
                         │              │                       │
                         │              │  ── if channel full:  │
                         │              │  drop + warn (30s RL) │
```

## Edge cases

### #596 (async I/O)

- **Channel saturation under sustained load.** If decode rate
  permanently exceeds writer throughput (e.g. NFS gone away),
  the channel fills and we drop. The 30 s warn names the drop
  count so users know data was lost — no silent data loss.
- **Worker thread panics.** Unlikely (no `unwrap`/`expect` in
  the hot path; only `expect` on the RwLock for poisoning,
  which terminates the thread cleanly). On panic, subsequent
  `try_send`s succeed until the channel fills (256 messages),
  then `Err(Disconnected)` after the channel is dropped via
  GC. We log nothing on disconnect — but in this scenario the
  user already saw the warn from the panic. **Mitigation:**
  keep `expect` calls minimal; rely on the writer thread being
  panic-free in normal operation.
- **Path/addr change while message in flight.** The in-flight
  message in the channel is processed against the *new* config
  on its way out (writer thread reads the lock per-message).
  Acceptable — no message lost, and the lag is at most one
  message worth of latency.
- **Writer thread shutdown timeout.** `JoinHandle::join()` is
  blocking. If the writer thread is stuck inside a slow
  `JsonlWriter::write`, app exit blocks too. **Mitigation:**
  `JsonlWriter` already uses `BufWriter` with a Drop-time
  flush; if that flush itself stalls, app exit blocks — but
  this is the same behavior as today, just on a different
  thread. Not a regression.

### #592 (custom channels)

- **N=1 custom channel.** Legal. Source center = that
  frequency. No span to exceed. Engage gate works normally.
- **Empty Custom on first open.** Combo shows Custom but
  EntryRow is empty. User must type frequencies before
  engaging — engage gate refuses (validator returns
  `CustomChannelError::Empty`).
- **CSV parse failure (non-numeric, missing comma, etc.).**
  Treated as a validation error with a generic toast
  ("Invalid custom-channel list: <parse error>") + inline
  `error` CSS class. No persistence, no DSP dispatch.
- **Saved Custom, switch to US-6, switch back to Custom.** The
  saved CSV reloads from config. The combo's index correctly
  selects the Custom slot.
- **Switching region mid-session with ACARS engaged.** The
  existing engage/disengage gate (PR #584) serializes this:
  region change forces a disengage + re-engage cycle. The
  channel_rows rebuild happens during the disengage path. No
  new edge case introduced here.
- **Channel rows briefly empty during region swap.** The
  rebuild removes old rows then adds new ones. Visually a
  flicker but acceptable; no data race because the swap is on
  the GTK main thread.

## Testing

### Unit tests

- **`acars_output.rs::tests`:**
  - `try_send_drops_when_full` — fill channel to 256, send
    one more, assert `Err(TrySendError::Full)` and drop counter
    incremented.
  - `writer_thread_exits_on_disconnect` — drop tx, call
    `join()` with timeout, assert it returns within 100 ms.
  - `writer_reopens_on_path_change` — write to path A,
    change config to path B, write another, assert both files
    exist with correct content.
- **`acars_airband_lock.rs::tests`:**
  - Replace existing `region_us6_channels_match_const` style
    tests with `&[f64]` accessor versions.
  - `validate_custom_channels` table tests: empty, one valid,
    eight valid, nine invalid (TooMany), NaN invalid, Inf
    invalid, negative invalid, span-just-under-2.4MHz valid,
    span-just-over-2.4MHz invalid (with exact field assertions
    on `SpanExceeded`).
  - `from_config_id_round_trips_custom` — `from_config_id("custom")`
    yields `Custom([])`; `region.config_id() == "custom"`.
- **`acars_config.rs::tests`:**
  - `acars_custom_channels_round_trips` — set, write, read
    back, assert deep-equality.

### Smoke (USER ONLY)

Manual GTK smoke at the end:

1. **Async I/O sanity** — toggle JSONL on, watch the file
   accumulate while ACARS is decoding; confirm zero DSP
   underruns in the logs over a 5-minute window.
2. **Custom region happy path** — open Aviation panel; switch
   to Custom; type `131.55, 131.525, 130.025`; press Enter;
   confirm the Channels group rebuilds to 3 rows; engage
   ACARS; confirm decoding occurs across those 3 channels.
3. **Validation rejection** — type `129.0, 132.0, 133.0` (span
   4.0 MHz); confirm toast names the offending pair and the
   EntryRow shows the error CSS class. No persistence.
4. **Persistence round-trip** — set a valid Custom region,
   close + reopen the app; confirm the panel pre-fills the
   EntryRow with the stored CSV and the combo shows Custom.
5. **Region swap** — Custom (3 chan) → US-6 (6 chan) → Europe
   (6 chan) → Custom (3 chan again from saved config); confirm
   channel_rows resize correctly each time and engage works
   in each.
6. **Drop-on-full visibility** — (best-effort) inject a load
   spike or block the writer thread artificially via a paused
   /tmp directory; confirm the 30 s warn fires once with a
   drop count.

## File budget

| File | LOC ballpark |
|------|--------------|
| `crates/sdr-core/src/acars_output.rs` | +200 (struct, writer thread, drop logic, tests) |
| `crates/sdr-core/src/{controller.rs, dsp_state.rs}` | +50 (try_send + lifecycle) |
| `crates/sdr-core/src/acars_airband_lock.rs` | +80 (Custom variant, validator, tests) |
| `crates/sdr-core/src/acars_config.rs` | +30 (custom_channels key) |
| `crates/sdr-ui/src/state.rs` | +20 (Vec migration) |
| `crates/sdr-ui/src/window.rs` | +30 (assignments + rebuild handler) |
| `crates/sdr-ui/src/sidebar/aviation_panel.rs` | +80 (Vec field, custom EntryRow, apply handler) |
| `crates/sdr-ffi/src/event.rs` | +5 (test fixture) |
| **Total** | **~485 LOC** |

Single bundled PR.

## Out-of-scope items (deferred)

- Generalising channel count above 8 (#592 followup if needed)
- Per-channel UI customisation (custom labels, colors)
- Async I/O for non-ACARS sinks (audio writer, scanner CSV,
  satellite recorder)
- Persisted ring buffer across app restarts for un-flushed
  output messages
- Configurable channel buffer capacity (256 hard-coded)

## References

- Issue #596 — async output I/O
- Issue #592 — custom channel sets
- PR #595 (closed #578) — synchronous v1 of the output
  formatters; this PR's #596 work refactors the I/O layer
  introduced there
- PR #593 (closed #581) — predefined regions; this PR's #592
  work extends the region enum
- CR round 4 on PR #595 — origin of the async-I/O concern
- `crates/sdr-core/src/acars_airband_lock.rs:39` —
  `ACARS_CHANNEL_COUNT` constant + region enum
- `crates/sdr-core/src/acars_output.rs` — current synchronous
  writer/feeder
- `crates/sdr-ui/src/sidebar/aviation_panel.rs:52` — current
  `[ActionRow; ACARS_CHANNEL_COUNT]` layout being migrated
