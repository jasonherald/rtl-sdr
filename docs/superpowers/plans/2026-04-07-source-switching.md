# Source Switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the hardcoded RTL-SDR device in the DSP controller with a trait-based source abstraction supporting RTL-SDR, Network, and File sources, with live switching from the UI sidebar.

**Architecture:** Extend the existing `Source` trait in `sdr-pipeline` with `read_samples()` and gain methods. Move the USB ring buffer into `RtlSdrSource` so each source handles its own I/O. Refactor `DspState` to hold `Box<dyn Source>` instead of `RtlSdrDevice`. Wire the source panel dropdown to a new `SetSourceType` message.

**Tech Stack:** Rust, GTK4/libadwaita, PipeWire, rusb, sdr-pipeline Source trait

---

### Task 1: Extend Source Trait with read_samples and gain methods

**Files:**
- Modify: `crates/sdr-pipeline/src/source_manager.rs`
- Modify: `crates/sdr-types/src/error.rs` (add ReadFailed variant)

- [ ] **Step 1: Add ReadFailed to SourceError**

In `crates/sdr-types/src/error.rs`, add after `AlreadyRunning`:
```rust
#[error("read failed: {0}")]
ReadFailed(String),
```

- [ ] **Step 2: Add read_samples and gain methods to Source trait**

In `crates/sdr-pipeline/src/source_manager.rs`, add to the `Source` trait after `set_sample_rate`:
```rust
/// Read IQ samples into the output buffer. Returns number of Complex samples written.
/// May block briefly waiting for data. Each source handles its own I/O mechanism.
fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError>;

/// Set tuner gain in tenths of dB (RTL-SDR specific, no-op for others).
fn set_gain(&mut self, _gain_tenths: i32) -> Result<(), SourceError> { Ok(()) }

/// Set AGC mode (RTL-SDR specific, no-op for others).
fn set_gain_mode(&mut self, _manual: bool) -> Result<(), SourceError> { Ok(()) }

/// Get available gain values in tenths of dB (empty for non-tuner sources).
fn gains(&self) -> &[i32] { &[] }
```

Add `use sdr_types::Complex;` to imports.

- [ ] **Step 3: Update MockSource and TrackingSource in tests**

Add `read_samples` implementations returning DC silence:
```rust
fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
    for s in output.iter_mut() { *s = Complex::default(); }
    Ok(output.len())
}
```

- [ ] **Step 4: Build and run tests**

Run: `cargo test -p sdr-pipeline -p sdr-types`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-pipeline/src/source_manager.rs crates/sdr-types/src/error.rs
git commit -m "extend Source trait with read_samples and gain methods"
```

---

### Task 2: Implement read_samples for RtlSdrSource (with ring buffer)

**Files:**
- Modify: `crates/sdr-source-rtlsdr/src/lib.rs`

The key architectural decision: move the ring buffer from `dsp_controller.rs` into `RtlSdrSource`. When `start()` is called, spawn the USB reader thread. `read_samples()` reads from the ring buffer and converts u8→Complex.

- [ ] **Step 1: Add ring buffer and reader thread to RtlSdrSource**

Add fields to `RtlSdrSource`:
```rust
pub struct RtlSdrSource {
    device: Option<RtlSdrDevice>,
    device_index: u32,
    sample_rate: f64,
    frequency: f64,
    running: Arc<AtomicBool>,
    // Ring buffer for async USB reads
    ring: Option<Arc<UsbRingBuffer>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
}
```

Copy `RingSlot` and `UsbRingBuffer` structs from `dsp_controller.rs` into this file (they're self-contained). Add ring buffer constants:
```rust
const RAW_BUF_SIZE: usize = 16_384 * 2; // 16K IQ pairs × 2 bytes
const RING_SLOTS: usize = 32;
```

- [ ] **Step 2: Update start() to spawn the reader thread**

In `start()`, after opening the device and configuring it, spawn the USB reader thread:
```rust
fn start(&mut self) -> Result<(), SourceError> {
    // ... existing open/configure code ...
    
    let ring = Arc::new(UsbRingBuffer::new(RING_SLOTS, RAW_BUF_SIZE));
    let ring_writer = Arc::clone(&ring);
    let cancel = Arc::clone(&self.running);
    let handle = self.device.as_ref().unwrap().usb_handle();
    
    let thread = std::thread::Builder::new()
        .name("rtlsdr-reader".to_string())
        .spawn(move || { /* USB read loop */ })
        .map_err(|e| SourceError::OpenFailed(e.to_string()))?;
    
    self.ring = Some(ring);
    self.reader_thread = Some(thread);
    Ok(())
}
```

- [ ] **Step 3: Implement read_samples()**

```rust
fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
    let ring = self.ring.as_ref().ok_or(SourceError::NotRunning)?;
    // Try to read from ring buffer, convert u8→Complex
    let idx = ring.read_idx.load(Ordering::Relaxed) % ring.slot_count;
    let slot = &ring.slots[idx];
    if slot.state.load(Ordering::Acquire) != 1 {
        return Ok(0); // No data available yet
    }
    let len = slot.len.load(Ordering::Relaxed);
    let count = {
        let data = slot.data.lock().expect("ring slot poisoned");
        Self::convert_samples(&data[..len], output)
    };
    slot.state.store(0, Ordering::Release);
    ring.read_idx.fetch_add(1, Ordering::Relaxed);
    Ok(count)
}
```

- [ ] **Step 4: Update stop() to join the reader thread**

```rust
fn stop(&mut self) -> Result<(), SourceError> {
    self.running.store(false, Ordering::Relaxed);
    if let Some(thread) = self.reader_thread.take() {
        let _ = thread.join();
    }
    self.ring = None;
    self.device = None;
    Ok(())
}
```

- [ ] **Step 5: Implement gain methods**

```rust
fn set_gain(&mut self, gain_tenths: i32) -> Result<(), SourceError> { ... }
fn set_gain_mode(&mut self, manual: bool) -> Result<(), SourceError> { ... }
fn gains(&self) -> &[i32] { ... }
```

- [ ] **Step 6: Build and test**

Run: `cargo test -p sdr-source-rtlsdr`
Expected: All pass

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-source-rtlsdr/
git commit -m "implement read_samples for RtlSdrSource with ring buffer"
```

---

### Task 3: Implement read_samples for NetworkSource and FileSource

**Files:**
- Modify: `crates/sdr-source-network/src/lib.rs`
- Modify: `crates/sdr-source-file/src/lib.rs`

Both already have internal `read_samples()` methods — just need to expose them via the trait.

- [ ] **Step 1: NetworkSource — delegate to existing read_samples**

The existing `read_samples()` at line 72 is a standalone method, not a trait impl. Wire it:
```rust
fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
    // Call the existing method (self.read_samples is the internal one)
    NetworkSource::read_samples_internal(self, output)
}
```
Or rename the internal method to avoid conflict.

- [ ] **Step 2: FileSource — delegate to existing read_samples**

Same pattern — wire the existing `read_samples()` method as the trait impl.

- [ ] **Step 3: Build and test**

Run: `cargo test -p sdr-source-network -p sdr-source-file`
Expected: All pass

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-source-network/ crates/sdr-source-file/
git commit -m "implement Source::read_samples for network and file sources"
```

---

### Task 4: Refactor DspState to use Box<dyn Source>

**Files:**
- Modify: `crates/sdr-ui/src/dsp_controller.rs`

This is the largest task. Replace `device: Option<RtlSdrDevice>` and `usb_reader: Option<UsbReader>` with `source: Option<Box<dyn Source>>`.

- [ ] **Step 1: Remove UsbReader, UsbRingBuffer, RingSlot from dsp_controller**

Delete the entire ring buffer infrastructure (lines 130-266) — it's now inside RtlSdrSource.

- [ ] **Step 2: Update DspState struct**

Replace:
```rust
device: Option<RtlSdrDevice>,
usb_reader: Option<UsbReader>,
```
With:
```rust
source: Option<Box<dyn Source>>,
```

Remove `use sdr_rtlsdr::RtlSdrDevice;` import. Add `use sdr_pipeline::source_manager::Source;`.

- [ ] **Step 3: Update DspState::new()**

Initialize with `source: None` instead of `device: None, usb_reader: None`.

- [ ] **Step 4: Refactor open_device() → open_source()**

Replace hardcoded RTL-SDR open with Source trait calls:
```rust
fn open_source(state: &mut DspState) -> Result<(), String> {
    let mut source = Box::new(RtlSdrSource::new(DEVICE_INDEX));
    source.set_sample_rate(state.sample_rate)
        .map_err(|e| e.to_string())?;
    source.tune(state.center_freq)
        .map_err(|e| e.to_string())?;
    source.start()
        .map_err(|e| e.to_string())?;
    
    rebuild_frontend(state)?;
    rebuild_vfo(state)?;
    state.source = Some(source);
    Ok(())
}
```

- [ ] **Step 5: Update cleanup()**

```rust
fn cleanup(state: &mut DspState) {
    if let Some(source) = &mut state.source {
        let _ = source.stop();
    }
    state.source = None;
    // ... audio sink stop ...
}
```

- [ ] **Step 6: Update process_iq_block()**

Replace ring buffer reading with `source.read_samples()`:
```rust
let Some(source) = &mut state.source else { return; };
let iq_count = match source.read_samples(&mut state.iq_buf) {
    Ok(0) => { std::thread::yield_now(); return; }
    Ok(n) => n,
    Err(e) => { tracing::warn!("source read error: {e}"); return; }
};
```

- [ ] **Step 7: Update Tune, SetSampleRate, SetGain, SetAgc handlers**

Route through Source trait methods:
```rust
UiToDsp::Tune(freq) => {
    state.center_freq = freq;
    if let Some(source) = &mut state.source {
        if let Err(e) = source.tune(freq) { ... }
    }
}
UiToDsp::SetGain(gain_db) => {
    if let Some(source) = &mut state.source {
        let gain_tenths = (gain_db * 10.0) as i32;
        if let Err(e) = source.set_gain(gain_tenths) { ... }
    }
}
```

- [ ] **Step 8: Update GainList sending on Start**

```rust
if let Some(source) = &state.source {
    let gains: Vec<f64> = source.gains()
        .iter()
        .map(|&g| f64::from(g) / 10.0)
        .collect();
    if !gains.is_empty() {
        let _ = dsp_tx.send(DspToUi::GainList(gains));
    }
}
```

- [ ] **Step 9: Build and test**

Run: `cargo test --workspace`
Expected: All pass (behavior unchanged, just refactored)

- [ ] **Step 10: Commit**

```bash
git add crates/sdr-ui/src/dsp_controller.rs
git commit -m "refactor DspState to use Box<dyn Source> instead of RtlSdrDevice"
```

---

### Task 5: Add source switching messages and handler

**Files:**
- Modify: `crates/sdr-ui/src/messages.rs`
- Modify: `crates/sdr-ui/src/dsp_controller.rs`

- [ ] **Step 1: Add SourceType enum and message**

In `messages.rs`:
```rust
/// Available source types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    RtlSdr,
    Network,
    File,
}
```

Add to `UiToDsp`:
```rust
SetSourceType(SourceType),
SetNetworkConfig { hostname: String, port: u16 },
SetFilePath(std::path::PathBuf),
```

- [ ] **Step 2: Add SetSourceType handler in dsp_controller**

```rust
UiToDsp::SetSourceType(source_type) => {
    let was_running = state.source.is_some();
    if was_running { cleanup(state); }
    state.source_type = source_type;
    // Don't auto-start — user needs to press play
}
```

Add `source_type: SourceType` to DspState, update `open_source()` to create the right source type based on it.

- [ ] **Step 3: Update open_source to be source-type-aware**

```rust
fn open_source(state: &mut DspState) -> Result<(), String> {
    let mut source: Box<dyn Source> = match state.source_type {
        SourceType::RtlSdr => Box::new(RtlSdrSource::new(DEVICE_INDEX)),
        SourceType::Network => {
            let mut ns = NetworkSource::new(&state.network_host, state.network_port, ...);
            ns
        }
        SourceType::File => {
            let mut fs = FileSource::new(&state.file_path);
            fs
        }
    };
    source.set_sample_rate(state.sample_rate).map_err(|e| e.to_string())?;
    source.start().map_err(|e| e.to_string())?;
    // ... rest of setup ...
}
```

- [ ] **Step 4: Add message test**

- [ ] **Step 5: Build and test**

Run: `cargo test --workspace`

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/
git commit -m "add source switching messages and handler"
```

---

### Task 6: Wire source panel UI controls

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/source_panel.rs`
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 1: Remove TODO and wire device_row to send SetSourceType**

In `source_panel.rs` `connect_device_visibility`, replace the TODO with actual DSP send. Need to pass AppState into this function.

- [ ] **Step 2: Wire network hostname/port controls**

Connect `hostname_row` and `port_row` to send `SetNetworkConfig` when changed.

- [ ] **Step 3: Add connect_device_visibility AppState parameter**

Update the function signature to accept `&Rc<AppState>` and send messages.

- [ ] **Step 4: Update connect_source_panel in window.rs**

Pass `state` through to `connect_device_visibility`.

- [ ] **Step 5: Build, test, install**

Run: `cargo test --workspace && make install`

- [ ] **Step 6: Manual test**

- Switch between RTL-SDR and Network in the dropdown
- Verify RTL-SDR controls show/hide correctly
- Verify source switching restarts properly

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-ui/
git commit -m "wire source panel controls to DSP source switching"
```

---

### Task 7: Final integration and cleanup

**Files:**
- Modify: `crates/sdr-ui/Cargo.toml` (remove direct rusb dependency if no longer needed)
- Modify: Various files for clippy cleanup

- [ ] **Step 1: Run full test suite**

```bash
cargo test --workspace
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 2: Remove unused imports and dead code**

Clean up any remaining `use sdr_rtlsdr::RtlSdrDevice` references, unused `rusb` imports, etc.

- [ ] **Step 3: Make install and manual test**

Test all three source types:
- RTL-SDR: tune to FM station, verify audio
- Network: won't connect without a server, but verify no crash
- File: won't play without a WAV, but verify source switch works

- [ ] **Step 4: Final commit**

```bash
git add -A
git commit -m "source switching: cleanup and integration"
```
