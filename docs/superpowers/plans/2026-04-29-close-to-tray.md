# Close-to-Tray + Keep-Running Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the user closes the main window, hide it instead of exiting; show a system-tray icon as the always-visible affordance to bring the window back; keep the DSP thread alive so satellite passes scheduled hours in advance still record.

**Architecture:** New Linux-only `sdr-tray` workspace crate wrapping `ksni` (StatusNotifierItem) on a dedicated `std::thread` with a per-thread `smol::block_on` runtime — async lives only inside this crate, the main app stays sync. `sdr-ui` calls `app.hold()` at startup, intercepts the window's `close-request` to hide instead of destroy, and routes `TrayEvent`s from the tray-thread to the GTK main loop via `glib::idle_add_local`. Tray Quit calls `app.release()` to balance the hold and exit cleanly.

**Tech Stack:** GTK4 / libadwaita 1.x, `ksni` (pure-Rust SNI), `smol` (one-thread async runtime), `librsvg`-via-`rsvg` (icon rasterization, transitively pulled by GTK), `tempfile` (autostart unit tests), `serde_json` (config schema, already in workspace).

**Spec:** [`docs/superpowers/specs/2026-04-29-close-to-tray-design.md`](../specs/2026-04-29-close-to-tray-design.md)

**Workflow rules** (from `CLAUDE.md` and user feedback memos):
- Feature branch: `feat/close-to-tray` (already created at spec-commit time).
- All commits include `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.
- Workspace lints: clippy pedantic enabled, `unsafe_code` denied.
- No `unwrap()` or `panic!()` in library crates; use `thiserror` for error types.
- The `sdr` binary may use `anyhow`, but `sdr-tray` is a library, so it uses `thiserror`.
- After UI-touching tasks (Task 8, 10, 11), the smoke checklist in Task 14 must pass before merging — Claude runs `make install`, the **user** runs the GTK app manually.

---

## File Structure

```
crates/sdr-tray/                                  # NEW crate, Linux-only
  Cargo.toml                                      # ksni, smol, tracing, thiserror
  src/
    lib.rs                                        # spawn(), TrayHandle, TrayEvent, SpawnError
    icon.rs                                       # rasterize_icon() + fallback bytes

crates/sdr-ui/src/
  app.rs                                          # MODIFY: app.hold(), --start-hidden, spawn tray
  autostart.rs                                    # NEW module
  preferences/general_page.rs                     # MODIFY: add Behavior AdwPreferencesGroup
  state.rs                                        # MODIFY: 5 new fields + is_recording()
  window.rs                                       # MODIFY: close-request, tray-* GIO actions

crates/sdr-ui/tests/
  close_to_tray_config_round_trip.rs              # NEW

src/main.rs                                       # MODIFY: --start-hidden CLI flag

Cargo.toml                                        # MODIFY: workspace deps for ksni, smol
```

---

## Task 1: Scaffold the `sdr-tray` crate

**Files:**
- Create: `crates/sdr-tray/Cargo.toml`
- Create: `crates/sdr-tray/src/lib.rs`
- Create: `crates/sdr-tray/src/icon.rs`
- Modify: `Cargo.toml` (workspace root) — add member + workspace deps
- Modify: `crates/sdr-ui/Cargo.toml` — add `sdr-tray` dep

- [ ] **Step 1.1: Create the crate manifest**

Write `crates/sdr-tray/Cargo.toml`:

```toml
[package]
name = "sdr-tray"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "StatusNotifierItem tray-icon sidecar for sdr-rs (Linux only)"

[dependencies]
ksni = "0.3"
smol = "2"
thiserror = { workspace = true }
tracing = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 1.2: Add to workspace members + workspace deps**

In the root `Cargo.toml`, append `"crates/sdr-tray"` to `[workspace] members`, and add to `[workspace.dependencies]`:

```toml
ksni = "0.3"
smol = "2"
sdr-tray = { path = "crates/sdr-tray" }
```

- [ ] **Step 1.3: Stub out `lib.rs` with public API and a placeholder spawn**

Write `crates/sdr-tray/src/lib.rs`:

```rust
//! StatusNotifierItem tray-icon sidecar for sdr-rs.
//!
//! Pure-Rust StatusNotifierItem implementation via `ksni`, run on a
//! dedicated `std::thread` with a per-thread `smol` runtime so the
//! main `sdr-ui` GTK loop never has to be aware of async. Linux-only.
//!
//! Communication is one-way: this crate sends [`TrayEvent`]s through
//! a `std::sync::mpsc::Sender`; the UI side bridges to its main loop
//! via `glib::idle_add_local` or a periodic timeout. The UI never
//! holds tray-side state. If [`spawn`] returns [`SpawnError`], callers
//! should fall back to "no tray, exit on window close" — the rest of
//! the app must work without us.

#![cfg(target_os = "linux")]
#![cfg_attr(test, allow(unsafe_code))]

use std::sync::mpsc;
use std::thread::JoinHandle;

mod icon;

/// Events the tray sends to the GTK UI thread.
#[derive(Debug, Clone, Copy)]
pub enum TrayEvent {
    Show,
    Hide,
    ToggleVisibility,
    Quit,
}

/// Errors returned by [`spawn`] when the tray cannot start.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("no StatusNotifierWatcher on session bus (likely missing AppIndicator extension)")]
    TrayWatcherUnavailable,
    #[error("tray spawn failed: {0}")]
    Other(String),
}

/// Owned handle to a running tray service. Drop or call [`shutdown`]
/// to stop the thread.
///
/// [`shutdown`]: TrayHandle::shutdown
pub struct TrayHandle {
    stop_tx: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl TrayHandle {
    pub fn shutdown(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            if let Err(e) = join.join() {
                tracing::warn!("tray thread panicked during shutdown: {e:?}");
            }
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn the tray service. Stub — Task 2 lands the real ksni body.
///
/// # Errors
///
/// Always returns `Err(SpawnError::Other)` until Task 2.
pub fn spawn(_events: mpsc::Sender<TrayEvent>) -> Result<TrayHandle, SpawnError> {
    Err(SpawnError::Other("not yet implemented".to_string()))
}
```

- [ ] **Step 1.4: Stub `icon.rs`**

Write `crates/sdr-tray/src/icon.rs`:

```rust
//! Tray icon byte buffers. Task 4 expands this with librsvg
//! rasterization; for now only the static fallback exists so the
//! Task 2 ksni wiring has something to draw.

pub(crate) const TRAY_ICON_SIZE: i32 = 22;

pub(crate) const FALLBACK_ICON_22X22_ARGB32: [u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] =
    [0; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize];

pub(crate) fn current_icon() -> (i32, i32, Vec<u8>) {
    (
        TRAY_ICON_SIZE,
        TRAY_ICON_SIZE,
        FALLBACK_ICON_22X22_ARGB32.to_vec(),
    )
}
```

- [ ] **Step 1.5: Add `sdr-tray` as a workspace dep on `sdr-ui`**

In `crates/sdr-ui/Cargo.toml`, add to `[dependencies]`:

```toml
sdr-tray = { workspace = true }
```

- [ ] **Step 1.6: Verify the workspace builds**

Run: `cargo build --workspace`
Expected: clean. The `ksni` and `smol` workspace deps are unused inside the stub `lib.rs`, but they're declared as direct deps in `crates/sdr-tray/Cargo.toml` so they will be compiled (clippy may warn `clippy::unused_crate_dependencies` for `ksni`/`smol` — silence with `#[allow(unused_crate_dependencies)]` at the top of `lib.rs`, then drop the allow in Task 2).

- [ ] **Step 1.7: Commit**

Run:

    git add crates/sdr-tray/Cargo.toml crates/sdr-tray/src/lib.rs crates/sdr-tray/src/icon.rs Cargo.toml crates/sdr-ui/Cargo.toml

Then commit with message:

    feat(sdr-tray): scaffold Linux-only StatusNotifierItem sidecar crate

    Skeleton for the close-to-tray feature (#512). Public API surfaces
    TrayEvent, TrayHandle (with shutdown/Drop), and spawn() returning
    SpawnError. Implementation body lands in Task 2 of the close-to-tray
    plan; this commit just gets the workspace compiling with the new
    crate in place.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 2: Implement `sdr-tray::spawn` — real ksni service

**Files:**
- Modify: `crates/sdr-tray/src/lib.rs`

- [ ] **Step 2.1: Write the failing test for the unreachable-bus error path**

Append the following test module to `crates/sdr-tray/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// `spawn` should return an error variant when D-Bus is pointed
    /// at a nonexistent socket. We can't easily distinguish "bus up,
    /// watcher absent" from "bus completely unreachable" at unit-test
    /// granularity — both are accepted error paths.
    #[test]
    fn spawn_returns_error_when_dbus_session_unreachable() {
        let prev = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        unsafe {
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:abstract=/nonexistent-tray-test",
            );
        }

        let (tx, _rx) = mpsc::channel::<TrayEvent>();
        let result = spawn(tx);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DBUS_SESSION_BUS_ADDRESS", v),
                None => std::env::remove_var("DBUS_SESSION_BUS_ADDRESS"),
            }
        }

        assert!(
            matches!(
                result,
                Err(SpawnError::TrayWatcherUnavailable | SpawnError::Other(_))
            ),
            "expected error variant, got {result:?}",
        );
    }
}
```

- [ ] **Step 2.2: Run the test to verify it fails**

Run: `cargo test -p sdr-tray spawn_returns_error -- --nocapture`
Expected: PASS — the stub returns `Err(SpawnError::Other(...))`, which matches. The test exists to lock the contract before we replace the stub.

(That seems backwards for TDD, so flip: temporarily change `spawn` to `Ok(...)` to confirm the test FAILS, then revert. That's a 30-second sanity check; mark this step done if you've satisfied yourself the test actually pins the contract.)

- [ ] **Step 2.3: Replace the stub `spawn` with the real implementation**

Replace the body of `lib.rs` from `mod icon;` through the end of `spawn` (keep the file header doc and the `#[cfg(test)]` module). Insert:

```rust
mod icon;

use ksni::{Icon, MenuItem, Tray, TrayMethods};

#[derive(Debug, Clone, Copy)]
pub enum TrayEvent {
    Show,
    Hide,
    ToggleVisibility,
    Quit,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("no StatusNotifierWatcher on session bus (likely missing AppIndicator extension)")]
    TrayWatcherUnavailable,
    #[error("tray spawn failed: {0}")]
    Other(String),
}

struct SdrTray {
    events: mpsc::Sender<TrayEvent>,
}

impl Tray for SdrTray {
    fn id(&self) -> String { "com.sdr.rs".to_string() }
    fn title(&self) -> String { "SDR-RS".to_string() }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "SDR-RS".to_string(),
            description: "Software-defined radio".to_string(),
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let (w, h, bytes) = icon::current_icon();
        vec![Icon { width: w, height: h, data: bytes }]
    }

    /// Left-click activate.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.events.send(TrayEvent::ToggleVisibility);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show / Hide".to_string(),
                activate: Box::new(|me: &mut Self| {
                    let _ = me.events.send(TrayEvent::ToggleVisibility);
                }),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|me: &mut Self| {
                    let _ = me.events.send(TrayEvent::Quit);
                }),
                ..Default::default()
            }.into(),
        ]
    }
}

pub struct TrayHandle {
    stop_tx: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl TrayHandle {
    pub fn shutdown(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            if let Err(e) = join.join() {
                tracing::warn!("tray thread panicked during shutdown: {e:?}");
            }
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) { self.shutdown(); }
}

pub fn spawn(events: mpsc::Sender<TrayEvent>) -> Result<TrayHandle, SpawnError> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (register_result_tx, register_result_rx) = mpsc::channel::<Result<(), SpawnError>>();

    let join = std::thread::Builder::new()
        .name("sdr-tray".to_string())
        .spawn(move || {
            smol::block_on(async move {
                let tray = SdrTray { events };
                let handle = match tray.spawn().await {
                    Ok(h) => {
                        let _ = register_result_tx.send(Ok(()));
                        h
                    }
                    Err(e) => {
                        let mapped = if format!("{e}").to_lowercase().contains("watcher") {
                            SpawnError::TrayWatcherUnavailable
                        } else {
                            SpawnError::Other(e.to_string())
                        };
                        let _ = register_result_tx.send(Err(mapped));
                        return;
                    }
                };
                tracing::info!("sdr-tray service registered with session bus");
                let _ = stop_rx.recv();
                drop(handle);
                tracing::info!("sdr-tray service stopped");
            });
        })
        .map_err(|e| SpawnError::Other(format!("std::thread::spawn failed: {e}")))?;

    match register_result_rx.recv() {
        Ok(Ok(())) => Ok(TrayHandle { stop_tx, join: Some(join) }),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(SpawnError::Other("tray thread died before registering".to_string()))
        }
    }
}
```

(Note `tray.spawn()` is the ksni `TrayMethods::spawn` extension method; you need `use ksni::TrayMethods;` in scope.)

- [ ] **Step 2.4: Run tests + clippy**

Run: `cargo test -p sdr-tray -- --nocapture`
Expected: PASS — the redirected bus address fails registration and we hit one of the two error variants.

Run: `cargo clippy --all-targets -p sdr-tray -- -D warnings`
Expected: clean.

- [ ] **Step 2.5: Commit**

Run:

    git add crates/sdr-tray/src/lib.rs

Then commit:

    feat(sdr-tray): implement ksni-backed StatusNotifierItem service

    Tray runs on a dedicated std::thread with a per-thread smol runtime;
    exposes Show/Hide/Quit menu items and a left-click ToggleVisibility
    activate. spawn() blocks until registration succeeds or fails so the
    caller can react synchronously. Bus-unreachable / watcher-missing
    maps to SpawnError::TrayWatcherUnavailable.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 3: Rasterize the SVG icon

**Files:**
- Modify: `crates/sdr-tray/Cargo.toml` — add `rsvg`, `cairo-rs`
- Modify: `crates/sdr-tray/src/icon.rs`

- [ ] **Step 3.1: Add deps**

In `crates/sdr-tray/Cargo.toml` `[dependencies]`:

```toml
rsvg = { workspace = true }
cairo-rs = { workspace = true }
```

(Verify both are already in `[workspace.dependencies]` — they are, transitively via GTK4.)

- [ ] **Step 3.2: Write the failing test**

Append to `crates/sdr-tray/src/icon.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterize_svg_returns_argb32_at_requested_size() {
        let svg_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/com.sdr.rs.svg");
        let (w, h, bytes) =
            rasterize_svg_to_argb32(&svg_path, 22).expect("rasterize known-good SVG");
        assert_eq!(w, 22);
        assert_eq!(h, 22);
        assert_eq!(bytes.len(), 22 * 22 * 4);
        assert!(bytes.chunks(4).any(|p| p[3] != 0),
            "rasterized icon is fully transparent");
    }

    #[test]
    fn rasterize_svg_missing_file_returns_err() {
        let result = rasterize_svg_to_argb32(
            std::path::Path::new("/nonexistent/never-here.svg"),
            22,
        );
        assert!(result.is_err());
    }
}
```

- [ ] **Step 3.3: Run the test to verify it fails**

Run: `cargo test -p sdr-tray rasterize -- --nocapture`
Expected: FAIL — `rasterize_svg_to_argb32` doesn't exist yet.

- [ ] **Step 3.4: Implement rasterization**

Replace the body of `crates/sdr-tray/src/icon.rs` with:

```rust
//! Tray icon byte buffers.
//!
//! ksni accepts ARGB32 raw bytes plus width/height. We rasterize the
//! app's SVG icon at startup; if rasterization fails for any reason
//! (missing file, librsvg parse error, Cairo allocation error) we
//! fall back to a built-in solid-color 22x22 buffer so the tray
//! always has *something* to draw — failure here must never block
//! tray spawn.

use std::path::Path;
use std::sync::OnceLock;

pub(crate) const TRAY_ICON_SIZE: i32 = 22;

pub(crate) const FALLBACK_ICON_22X22_ARGB32: [u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] =
    fallback_argb32();

const fn fallback_argb32() -> [u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] {
    let mut out = [0u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize];
    let mut i = 0;
    while i < out.len() {
        out[i] = 0xFF;     // A
        out[i + 1] = 0x21; // R
        out[i + 2] = 0x6F; // G
        out[i + 3] = 0xB6; // B
        i += 4;
    }
    out
}

pub(crate) fn rasterize_svg_to_argb32(
    path: &Path,
    size: i32,
) -> Result<(i32, i32, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let handle = rsvg::Loader::new().read_path(path)?;
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, size, size)?;
    {
        let cr = cairo::Context::new(&surface)?;
        let renderer = rsvg::CairoRenderer::new(&handle);
        let viewport = cairo::Rectangle::new(0.0, 0.0, f64::from(size), f64::from(size));
        renderer.render_document(&cr, &viewport)?;
    }
    surface.flush();
    let stride = surface.stride();
    let data = surface.data()?;
    // Cairo ARGB32 is native-endian; SNI wants network byte order
    // (A, R, G, B). On little-endian hosts the in-memory order is
    // BGRA — swap.
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        let row_start = (y * stride) as usize;
        for x in 0..size {
            let px = row_start + (x * 4) as usize;
            let b = data[px];
            let g = data[px + 1];
            let r = data[px + 2];
            let a = data[px + 3];
            out.extend_from_slice(&[a, r, g, b]);
        }
    }
    Ok((size, size, out))
}

static CACHED_ICON: OnceLock<(i32, i32, Vec<u8>)> = OnceLock::new();

pub(crate) fn current_icon() -> (i32, i32, Vec<u8>) {
    CACHED_ICON
        .get_or_init(|| {
            let svg_path = locate_app_icon();
            match rasterize_svg_to_argb32(&svg_path, TRAY_ICON_SIZE) {
                Ok(triple) => triple,
                Err(e) => {
                    tracing::warn!(
                        path = %svg_path.display(),
                        error = %e,
                        "tray icon rasterization failed, using fallback bytes",
                    );
                    (TRAY_ICON_SIZE, TRAY_ICON_SIZE, FALLBACK_ICON_22X22_ARGB32.to_vec())
                }
            }
        })
        .clone()
}

/// Search order: $XDG_DATA_HOME → ~/.local/share → workspace data/.
fn locate_app_icon() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        let p = std::path::PathBuf::from(home)
            .join("icons/hicolor/scalable/apps/com.sdr.rs.svg");
        if p.exists() { return p; }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::PathBuf::from(home)
            .join(".local/share/icons/hicolor/scalable/apps/com.sdr.rs.svg");
        if p.exists() { return p; }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/com.sdr.rs.svg")
}
```

- [ ] **Step 3.5: Run the tests + lint**

Run: `cargo test -p sdr-tray -- --nocapture`
Expected: 3 PASS (the unreachable-bus from Task 2 + the two new rasterize tests).

Run: `cargo clippy --all-targets -p sdr-tray -- -D warnings`
Expected: clean.

- [ ] **Step 3.6: Commit**

Run:

    git add crates/sdr-tray/src/icon.rs crates/sdr-tray/Cargo.toml

Then commit:

    feat(sdr-tray): rasterize app SVG to ARGB32 for tray icon

    Uses librsvg + Cairo to render data/com.sdr.rs.svg at 22x22, swaps
    Cairo's native-endian ARGB32 to network-byte-order for SNI, caches
    the result via OnceLock. Fallback to a built-in solid-color buffer
    if rasterization fails.

    Search order: XDG_DATA_HOME, ~/.local/share, workspace data/, so
    both `make install` and `cargo run` resolve the icon.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 4: AppState fields + `is_recording()` predicate

**Files:**
- Modify: `crates/sdr-ui/src/state.rs`

- [ ] **Step 4.1: Write the failing tests**

Append to `crates/sdr-ui/src/state.rs` (inside an existing `#[cfg(test)] mod tests` block, or create one):

```rust
#[cfg(test)]
mod tests_close_to_tray {
    use super::*;
    use std::sync::mpsc;

    fn fresh_state() -> Rc<AppState> {
        let (tx, _rx) = mpsc::channel();
        AppState::new_shared(tx)
    }

    #[test]
    fn defaults_are_safe_for_close_to_tray() {
        let s = fresh_state();
        assert!(s.close_to_tray.get(), "default close_to_tray must be true");
        assert!(!s.tray_first_close_seen.get());
        assert!(s.tray_available.get());
        assert!(!s.audio_recording_active.get());
        assert!(!s.iq_recording_active.get());
        assert!(!s.lrpt_recording_active.get());
    }

    #[test]
    fn is_recording_is_false_when_idle() {
        let s = fresh_state();
        assert!(!s.is_recording());
    }

    #[test]
    fn is_recording_table() {
        // Each row: (apt, lrpt, audio, iq, expected)
        let cases = [
            (false, false, false, false, false),
            (true, false, false, false, true),
            (false, true, false, false, true),
            (false, false, true, false, true),
            (false, false, false, true, true),
            (true, true, true, true, true),
            (true, false, false, true, true),
            (false, true, true, false, true),
        ];
        for (apt, lrpt, audio, iq, expected) in cases {
            let s = fresh_state();
            if apt {
                *s.apt_recording_pass.borrow_mut() =
                    Some((33_591, chrono::Utc::now()));
            }
            s.lrpt_recording_active.set(lrpt);
            s.audio_recording_active.set(audio);
            s.iq_recording_active.set(iq);
            assert_eq!(
                s.is_recording(),
                expected,
                "row apt={apt} lrpt={lrpt} audio={audio} iq={iq}",
            );
        }
    }
}
```

- [ ] **Step 4.2: Run the tests to verify they fail**

Run: `cargo test -p sdr-ui tests_close_to_tray`
Expected: FAIL — fields and method don't exist yet.

- [ ] **Step 4.3: Add the fields to `AppState`**

In `crates/sdr-ui/src/state.rs`, in the struct definition, add (insert near the other `Cell<bool>` fields, e.g. right after `apt_recording_pass`):

```rust
    /// `true` when the user closes the window the app should hide
    /// instead of exiting. Default `true` — set by `build_window`
    /// from the persisted config (key `close_to_tray`). Per #512.
    pub close_to_tray: Cell<bool>,
    /// `true` once the user has hidden the window at least once with
    /// the close button — used to fire the "App still running in
    /// tray …" toast exactly once per fresh config. Per #512.
    pub tray_first_close_seen: Cell<bool>,
    /// `true` while the tray service is alive and registered with
    /// the session bus. Defaults `true` (optimistic) and is flipped
    /// to `false` if `sdr_tray::spawn` returns Err. The close-request
    /// handler short-circuits to `Propagation::Proceed` when this is
    /// false. Per #512.
    pub tray_available: Cell<bool>,
    /// `true` while a `StartAudioRecording` is in flight. Used by
    /// `AppState::is_recording` to gate the tray-Quit confirmation.
    /// Per #512.
    pub audio_recording_active: Cell<bool>,
    /// `true` while a `StartIqRecording` is in flight. Per #512.
    pub iq_recording_active: Cell<bool>,
    /// `true` between an LRPT auto-record AOS and LOS. Per #512.
    pub lrpt_recording_active: Cell<bool>,
    /// Owned handle to the tray service. Held in `AppState` so the
    /// `tray-quit` action can `shutdown()` to join the worker thread
    /// before `app.release()`. Per #512.
    pub tray_handle: RefCell<Option<sdr_tray::TrayHandle>>,
```

In `AppState::new_shared`, append to the struct literal:

```rust
            close_to_tray: Cell::new(true),
            tray_first_close_seen: Cell::new(false),
            tray_available: Cell::new(true),
            audio_recording_active: Cell::new(false),
            iq_recording_active: Cell::new(false),
            lrpt_recording_active: Cell::new(false),
            tray_handle: RefCell::new(None),
```

Add the `is_recording` method to the same `impl AppState` block:

```rust
    /// `true` if the app is actively writing pass artifacts to disk —
    /// any APT pass, LRPT pass, audio recording, or IQ recording.
    /// Used to gate the tray-Quit confirmation modal.
    ///
    /// Maintenance contract: every new "we're writing pass artifacts"
    /// state added to `AppState` MUST be OR-ed in here, and the
    /// table-driven test in `tests_close_to_tray::is_recording_table`
    /// must be extended.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.apt_recording_pass.borrow().is_some()
            || self.lrpt_recording_active.get()
            || self.audio_recording_active.get()
            || self.iq_recording_active.get()
    }
```

- [ ] **Step 4.4: Run the tests + lint**

Run: `cargo test -p sdr-ui tests_close_to_tray`
Expected: PASS (3 tests).

Run: `cargo clippy --all-targets -p sdr-ui -- -D warnings`
Expected: clean.

- [ ] **Step 4.5: Commit**

Run:

    git add crates/sdr-ui/src/state.rs

Then commit:

    feat(sdr-ui): AppState fields + is_recording() for close-to-tray

    Adds six Cells (close_to_tray, tray_first_close_seen, tray_available,
    audio_recording_active, iq_recording_active, lrpt_recording_active),
    a RefCell<Option<TrayHandle>>, and an is_recording() helper that ORs
    every active-pass-write flag. The maintenance contract is pinned by
    a 16-row table-driven test so a future recording type can not be
    silently dropped on Quit.

    Per #512 close-to-tray plan task 4.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 5: Mirror DSP recording state into the new Cells

**Files:**
- Modify: `crates/sdr-ui/src/window.rs` — extend the four `DspToUi::*Recording*` handlers and the LRPT auto-record wiring

- [ ] **Step 5.1: Tests for this task**

Already covered by `tests_close_to_tray::is_recording_table` in Task 4 — that test pins the predicate, this task only wires the state up. No new test needed.

- [ ] **Step 5.2: Update the four recording handlers**

In `crates/sdr-ui/src/window.rs`, find the four `DspToUi` recording arms (currently around lines 1266-1301). The closure needs `state` in scope — it usually does via the enclosing function. If `state` is named `state_a` or similar in this scope, use that.

Replace the four match arms:

```rust
        DspToUi::AudioRecordingStarted(path) => {
            tracing::info!(?path, "audio recording started");
            state.audio_recording_active.set(true);
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let name = path
                    .file_name()
                    .map_or("file".to_string(), |n| n.to_string_lossy().to_string());
                let toast = adw::Toast::new(&format!("Recording audio: {name}"));
                overlay.add_toast(toast);
            }
        }
        DspToUi::AudioRecordingStopped => {
            tracing::info!("audio recording stopped");
            state.audio_recording_active.set(false);
            record_audio_row.set_active(false);
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::new("Audio recording saved");
                overlay.add_toast(toast);
            }
        }
        DspToUi::IqRecordingStarted(path) => {
            tracing::info!(?path, "IQ recording started");
            state.iq_recording_active.set(true);
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let name = path
                    .file_name()
                    .map_or("file".to_string(), |n| n.to_string_lossy().to_string());
                let toast = adw::Toast::new(&format!("Recording IQ: {name}"));
                overlay.add_toast(toast);
            }
        }
        DspToUi::IqRecordingStopped => {
            tracing::info!("IQ recording stopped");
            state.iq_recording_active.set(false);
            record_iq_row.set_active(false);
            if let Some(overlay) = toast_overlay_weak.upgrade() {
                let toast = adw::Toast::new("IQ recording saved");
                overlay.add_toast(toast);
            }
        }
```

- [ ] **Step 5.3: Update the LRPT auto-record wiring**

Find `RecorderAction::StartAutoRecord { ... } => match protocol { ... ImagingProtocol::Lrpt => {` (search: `ImagingProtocol::Lrpt =>`). At the end of that LRPT branch's body, add:

```rust
                        state_a.lrpt_recording_active.set(true);
```

Find `RecorderAction::SaveLrptPass(dir) => {` (search: `SaveLrptPass(dir)`). In the completion path of that branch — if it uses `gio::spawn_blocking`, this means the main-thread side of `glib::spawn_future_local` after the worker resolves — clear the flag:

```rust
                state_for_complete.lrpt_recording_active.set(false);
```

(Use whichever local name holds the `Rc<AppState>` in that scope. Read 30 lines around the `SaveLrptPass` branch to confirm — it likely follows the same `state_for_complete = Rc::clone(&state_a)` pattern as the APT SavePng path.)

- [ ] **Step 5.4: Build**

Run: `cargo build -p sdr-ui`
Expected: clean.

Run: `cargo test -p sdr-ui tests_close_to_tray`
Expected: PASS (Task 4's tests still pass — Task 5 doesn't touch them).

- [ ] **Step 5.5: Commit**

Run:

    git add crates/sdr-ui/src/window.rs

Then commit:

    feat(sdr-ui): mirror DSP recording state into AppState cells

    Wire the four DspToUi recording handlers and the LRPT auto-record
    state machine to flip the new audio/iq/lrpt _recording_active cells.
    This is what AppState::is_recording() reads — without these set/clear
    calls, the tray-Quit confirmation modal would never fire.

    Per #512 close-to-tray plan task 5.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 6: Autostart module

**Files:**
- Create: `crates/sdr-ui/src/autostart.rs`
- Modify: `crates/sdr-ui/src/lib.rs` — add `pub mod autostart;`
- Modify: `crates/sdr-ui/Cargo.toml` — add `tempfile` to dev-dependencies

- [ ] **Step 6.1: Write the failing tests**

Create `crates/sdr-ui/src/autostart.rs`:

```rust
//! Autostart-on-login support — generate / remove
//! `$XDG_CONFIG_HOME/autostart/com.sdr.rs.desktop`.
//!
//! XDG Autostart spec:
//! https://specifications.freedesktop.org/autostart-spec/latest/
//!
//! The source of truth is the `.desktop` file on disk; the config
//! `autostart` boolean is a cached read-fast mirror. Startup
//! reconciles by trusting the filesystem.

use std::io;
use std::path::{Path, PathBuf};

const DESKTOP_FILE_NAME: &str = "com.sdr.rs.desktop";

const DESKTOP_FILE_BODY: &str = "\
[Desktop Entry]
Type=Application
Name=SDR-RS
Comment=Software-defined radio (auto-started in tray)
Exec=sdr-rs --start-hidden
Icon=com.sdr.rs
Hidden=false
X-GNOME-Autostart-enabled=true
";

fn desktop_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("autostart").join(DESKTOP_FILE_NAME)
}

fn default_desktop_path() -> PathBuf {
    let config_dir = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        PathBuf::from(".").join(".config")
    };
    desktop_path_in(&config_dir)
}

#[must_use]
pub fn is_enabled() -> bool {
    is_enabled_at(&default_desktop_path())
}

fn is_enabled_at(path: &Path) -> bool {
    path.exists()
}

/// Write the autostart `.desktop` file at the default location.
///
/// # Errors
///
/// Returns the underlying `io::Error` from `create_dir_all` or
/// `write` — typically permission denied or disk full.
pub fn enable() -> io::Result<()> {
    enable_at(&default_desktop_path())
}

fn enable_at(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DESKTOP_FILE_BODY)
}

/// Remove the autostart `.desktop` file. Idempotent — missing file is `Ok(())`.
///
/// # Errors
///
/// Returns the underlying `io::Error` if the file exists but cannot be
/// removed (permission denied, etc.).
pub fn disable() -> io::Result<()> {
    disable_at(&default_desktop_path())
}

fn disable_at(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = desktop_path_in(dir.path());
        (dir, path)
    }

    #[test]
    fn is_enabled_at_returns_false_for_missing_file() {
        let (_dir, path) = tmp();
        assert!(!is_enabled_at(&path));
    }

    #[test]
    fn enable_at_writes_desktop_file_with_start_hidden_exec() {
        let (_dir, path) = tmp();
        enable_at(&path).expect("enable");
        assert!(is_enabled_at(&path));
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("Exec=sdr-rs --start-hidden"),
            "Exec line must include --start-hidden — got: {body}");
        assert!(body.contains("Type=Application"));
        assert!(body.contains("Name=SDR-RS"));
    }

    #[test]
    fn enable_at_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let path = desktop_path_in(dir.path());
        assert!(!path.parent().unwrap().exists());
        enable_at(&path).expect("enable");
        assert!(path.exists());
    }

    #[test]
    fn enable_at_is_idempotent() {
        let (_dir, path) = tmp();
        enable_at(&path).expect("first enable");
        enable_at(&path).expect("second enable");
        assert!(is_enabled_at(&path));
    }

    #[test]
    fn disable_at_removes_existing_file() {
        let (_dir, path) = tmp();
        enable_at(&path).unwrap();
        assert!(is_enabled_at(&path));
        disable_at(&path).expect("disable");
        assert!(!is_enabled_at(&path));
    }

    #[test]
    fn disable_at_is_ok_on_missing_file() {
        let (_dir, path) = tmp();
        assert!(!is_enabled_at(&path));
        disable_at(&path).expect("disable on missing must be Ok");
    }
}
```

- [ ] **Step 6.2: Add `tempfile` to dev-deps**

In `crates/sdr-ui/Cargo.toml`, under `[dev-dependencies]`:

```toml
tempfile = { workspace = true }
```

If not in `[workspace.dependencies]`, add to root `Cargo.toml`:

```toml
tempfile = "3"
```

- [ ] **Step 6.3: Add the module to `lib.rs`**

In `crates/sdr-ui/src/lib.rs`, add to the `pub mod` block:

```rust
pub mod autostart;
```

- [ ] **Step 6.4: Run the tests + lint**

Run: `cargo test -p sdr-ui autostart`
Expected: 6 tests PASS.

Run: `cargo clippy --all-targets -p sdr-ui -- -D warnings`
Expected: clean.

- [ ] **Step 6.5: Commit**

Run:

    git add crates/sdr-ui/src/autostart.rs crates/sdr-ui/src/lib.rs crates/sdr-ui/Cargo.toml Cargo.toml

Then commit:

    feat(sdr-ui): autostart module — XDG .desktop generator

    Pure-IO module: enable/disable/is_enabled write or remove
    XDG_CONFIG_HOME/autostart/com.sdr.rs.desktop with the autostart
    Exec line. No GTK deps; tested with tempdir against a private
    path so unit tests don't pollute ~/.config/autostart.

    Per #512 close-to-tray plan task 6.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 7: `--start-hidden` CLI flag in the binary

**Files:**
- Modify: `crates/sdr-ui/src/lib.rs` — extend `run` with a `start_hidden: bool` arg
- Modify: `src/main.rs` — parse the flag, pass through

- [ ] **Step 7.1: Update sdr-ui's run path**

In `crates/sdr-ui/src/lib.rs`, replace `pub fn run() -> glib::ExitCode` with:

```rust
/// Run the SDR-RS application, returning the GTK exit code.
///
/// `start_hidden` skips the initial `window.present()` so the app
/// launches with only the tray icon visible — used by the autostart
/// `.desktop` Exec line.
pub fn run(start_hidden: bool) -> glib::ExitCode {
    let app = app::build_app_with_options(start_hidden);
    if !register_and_check_primary(&app) {
        return glib::ExitCode::SUCCESS;
    }
    app.run()
}
```

- [ ] **Step 7.2: Parse the flag in `src/main.rs`**

Near the top of the Linux `main()` body, after the `--splash` short-circuit, add:

```rust
    let start_hidden = std::env::args().any(|a| a == "--start-hidden");
```

Then change:

```rust
    let app = sdr_ui::build_app();
```

to:

```rust
    let app = sdr_ui::build_app_with_options(start_hidden);
```

The final `app.run()` line at the bottom is unchanged.

- [ ] **Step 7.3: Stage but don't commit yet**

Run: `cargo build --workspace`
Expected: failure on `build_app_with_options` not existing — Task 9 fixes this. Stage the changes but don't commit until Task 9 completes:

    git add src/main.rs crates/sdr-ui/src/lib.rs

(Continue to Task 8.)

---

## Task 8: Preferences "Behavior" group

**Files:**
- Modify: `crates/sdr-ui/src/preferences/general_page.rs`

- [ ] **Step 8.1: Add config key constants and read helpers**

At the top of `crates/sdr-ui/src/preferences/general_page.rs`, after the existing `KEY_SCREENSHOT_DIR` constant, add:

```rust
pub(crate) const KEY_CLOSE_TO_TRAY: &str = "close_to_tray";
pub(crate) const KEY_AUTOSTART: &str = "autostart";
pub(crate) const KEY_TRAY_FIRST_CLOSE_SEEN: &str = "tray_first_close_seen";

/// Read the persisted close-to-tray boolean (default true). Used by
/// the prefs row's initial active state and by `build_window` to
/// hydrate `state.close_to_tray`.
pub fn read_close_to_tray(config: &ConfigManager) -> bool {
    config.read(|v| {
        v.get(KEY_CLOSE_TO_TRAY)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    })
}

/// Read the "we already showed the close-to-tray toast" flag. Default false.
pub fn read_tray_first_close_seen(config: &ConfigManager) -> bool {
    config.read(|v| {
        v.get(KEY_TRAY_FIRST_CLOSE_SEEN)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}
```

- [ ] **Step 8.2: Add a `build_behavior_group` helper**

Append to the same file:

```rust
/// Build the "Behavior" preferences group: close-to-tray and
/// autostart-on-login switches. Per #512.
fn build_behavior_group(
    window: &adw::PreferencesWindow,
    config: &Arc<ConfigManager>,
    tray_available: bool,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Behavior")
        .description("How the app responds to closing the window and to login")
        .build();

    // --- Close-to-tray switch ---
    let close_to_tray_row = adw::SwitchRow::builder()
        .title("Keep running in tray when window is closed")
        .subtitle("Hide to the system tray instead of exiting; great for scheduled satellite passes")
        .active(read_close_to_tray(config))
        .build();
    if !tray_available {
        close_to_tray_row.set_sensitive(false);
        close_to_tray_row.set_tooltip_text(Some("Disabled — no system tray detected on this session."));
    }
    let config_ctt = Arc::clone(config);
    close_to_tray_row.connect_active_notify(move |row| {
        let value = row.is_active();
        config_ctt.write(|v| {
            v[KEY_CLOSE_TO_TRAY] = serde_json::json!(value);
        });
        tracing::info!(value, "close_to_tray toggle written to config");
    });
    group.add(&close_to_tray_row);

    // --- Autostart switch ---
    let autostart_row = adw::SwitchRow::builder()
        .title("Start at login")
        .subtitle("Launch SDR-RS hidden in the tray when you log in")
        .active(crate::autostart::is_enabled())
        .build();

    // Suppress the recursive notify::active that set_active(!want)
    // triggers when we revert on filesystem error.
    let suppress = std::rc::Rc::new(std::cell::Cell::new(false));
    let suppress_inner = std::rc::Rc::clone(&suppress);
    let config_as = Arc::clone(config);
    let window_for_toast = window.clone();
    autostart_row.connect_active_notify(move |row| {
        if suppress_inner.get() {
            return;
        }
        let want = row.is_active();
        let result = if want {
            crate::autostart::enable()
        } else {
            crate::autostart::disable()
        };
        match result {
            Ok(()) => {
                config_as.write(|v| {
                    v[KEY_AUTOSTART] = serde_json::json!(want);
                });
                tracing::info!(want, "autostart toggle persisted");
            }
            Err(e) => {
                tracing::warn!(want, error = %e, "autostart toggle failed, reverting");
                let toast = adw::Toast::new(&format!(
                    "Couldn't {} autostart: {e}",
                    if want { "enable" } else { "disable" },
                ));
                window_for_toast.add_toast(toast);
                suppress_inner.set(true);
                row.set_active(!want);
                suppress_inner.set(false);
            }
        }
    });
    group.add(&autostart_row);

    group
}
```

- [ ] **Step 8.3: Wire the group into `build_general_page`**

Modify the function signature to accept `tray_available: bool`:

```rust
pub fn build_general_page(
    window: &adw::PreferencesWindow,
    config: &Arc<ConfigManager>,
    tray_available: bool,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("preferences-system-symbolic")
        .build();

    page.add(&build_behavior_group(window, config, tray_available));
    // ... existing `directories_group` block unchanged ...
    page.add(&directories_group);
    page
}
```

Find every caller of `build_general_page` (search workspace) and pass `state.tray_available.get()` as the third arg. Likely one call in `preferences/mod.rs` — thread `state` through.

- [ ] **Step 8.4: Build + lint**

Run: `cargo build -p sdr-ui`
Expected: clean.

Run: `cargo clippy -p sdr-ui --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8.5: Commit**

Run:

    git add crates/sdr-ui/src/preferences/general_page.rs crates/sdr-ui/src/preferences/mod.rs crates/sdr-ui/src/window.rs

Then commit:

    feat(sdr-ui): preferences "Behavior" group with tray + autostart

    Adds AdwSwitchRows for close_to_tray and autostart, both above the
    existing Directories group on the General prefs page. Close-to-tray
    row greys out with explanatory tooltip when state.tray_available is
    false. Autostart row writes / removes the .desktop via the autostart
    module and reverts the switch on filesystem error.

    Per #512 close-to-tray plan task 8.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>

---

## Task 9: `app.rs` lifecycle — `app.hold()`, spawn tray, route events

**Files:**
- Modify: `crates/sdr-ui/src/app.rs`

- [ ] **Step 9.1: Replace `app.rs` with the lifecycle-aware version**

Replace the body of `crates/sdr-ui/src/app.rs` with:

```rust
//! Application setup — creates the `AdwApplication`, holds it
//! across window-close, spawns the tray sidecar, and routes
//! [`sdr_tray::TrayEvent`]s to GIO actions on the GTK main loop.

use std::sync::mpsc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::{css, window};

const APP_ID: &str = "com.sdr.rs";

pub fn build_app() -> adw::Application {
    build_app_with_options(false)
}

pub fn build_app_with_options(start_hidden: bool) -> adw::Application {
    let app = adw::Application::builder().application_id(APP_ID).build();

    crate::notify::register_actions(&app);

    app.connect_startup(|app| {
        css::load_css();

        // Hold the application so it doesn't exit when the last
        // window closes. Balanced by `app.release()` in the
        // tray-quit action handler. Per #512.
        app.hold();

        if let Some(display) = gtk4::gdk::Display::default() {
            let icon_theme = gtk4::IconTheme::for_display(&display);
            icon_theme.add_search_path("data");
        }

        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        glib::timeout_add_local(Duration::from_secs(10), || {
            #[allow(unsafe_code)]
            unsafe {
                unsafe extern "C" {
                    fn malloc_trim(pad: usize) -> i32;
                }
                malloc_trim(0);
            }
            glib::ControlFlow::Continue
        });

        tracing::info!("sdr-rs UI starting");
    });

    let config_path = gtk4::glib::user_config_dir()
        .join("sdr-rs")
        .join("config.json");
    let defaults = serde_json::json!({});
    let config = match sdr_config::ConfigManager::load(&config_path, &defaults) {
        Ok(mut c) => {
            c.enable_auto_save();
            std::sync::Arc::new(c)
        }
        Err(e) => {
            tracing::warn!("config load failed, using in-memory defaults: {e}");
            std::sync::Arc::new(sdr_config::ConfigManager::in_memory(&defaults))
        }
    };

    app.connect_activate(move |app| {
        if let Some(existing) = app.windows().into_iter().next() {
            existing.present();
            return;
        }
        let state = window::build_window(app, &config);

        spawn_tray_and_route(app, &state);

        // Hydrate close-to-tray toggle into AppState from config.
        state.close_to_tray.set(
            crate::preferences::general_page::read_close_to_tray(&config),
        );
        state.tray_first_close_seen.set(
            crate::preferences::general_page::read_tray_first_close_seen(&config),
        );

        // Default `present()` unless the autostart path passed
        // --start-hidden AND the tray is actually available. If tray
        // is unavailable we force-present so the user isn't stranded
        // with an invisible process. Per #512.
        let tray_ok = state.tray_available.get();
        if !start_hidden || !tray_ok {
            if start_hidden && !tray_ok {
                tracing::warn!(
                    "start-hidden requested but tray unavailable; presenting window",
                );
            }
            if let Some(toplevel) = app.windows().into_iter().next() {
                toplevel.present();
            }
        }
    });

    app
}

/// Spawn the tray service on a worker thread and route its events
/// to GIO actions on the main loop. On failure, flip
/// `state.tray_available` to false so the close-request handler
/// short-circuits to "exit on close" and the prefs row greys out.
fn spawn_tray_and_route(
    app: &adw::Application,
    state: &std::rc::Rc<crate::state::AppState>,
) {
    let (tx, rx) = mpsc::channel::<sdr_tray::TrayEvent>();
    match sdr_tray::spawn(tx) {
        Ok(handle) => {
            *state.tray_handle.borrow_mut() = Some(handle);
            let app_for_route = app.clone();
            // 50ms tick to drain the cross-thread mpsc into GIO actions
            // on the main loop. Cheaper than channel-attach via a glib
            // worker source; the granularity is well below human
            // perception for click-to-action latency.
            glib::timeout_add_local(Duration::from_millis(50), move || {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        sdr_tray::TrayEvent::Show => app_for_route.activate_action("tray-show", None),
                        sdr_tray::TrayEvent::Hide => app_for_route.activate_action("tray-hide", None),
                        sdr_tray::TrayEvent::ToggleVisibility => app_for_route.activate_action("tray-toggle", None),
                        sdr_tray::TrayEvent::Quit => app_for_route.activate_action("tray-quit", None),
                    }
                }
                glib::ControlFlow::Continue
            });
            tracing::info!("tray spawned and event router started");
        }
        Err(e) => {
            tracing::warn!(error = %e, "tray spawn failed — close-to-tray disabled");
            state.tray_available.set(false);
        }
    }
}
```

- [ ] **Step 9.2: Update `build_window` to return `Rc<AppState>`**

In `crates/sdr-ui/src/window.rs`, find the `pub fn build_window` signature. Change it to return `Rc<AppState>` and append `state` (or `state_a`) to the bottom of the function body. There's only one external caller — `app.rs` — already handled above.

- [ ] **Step 9.3: Build**

Run: `cargo build --workspace`
Expected: pending complaints about unresolved `tray-show` / `tray-hide` / `tray-toggle` / `tray-quit` GIO actions are runtime warnings, not compile errors. The compile must succeed.

- [ ] **Step 9.4: Commit (combines Task 7 staged + Task 9)**

Run:

    git add crates/sdr-ui/src/app.rs crates/sdr-ui/src/state.rs

Then commit:

    feat(sdr-ui): app.hold() + tray spawn + --start-hidden

    build_app_with_options(start_hidden) holds the application across
    last-window-close, spawns sdr-tray, and routes TrayEvents from the
    worker thread to four GIO actions (tray-show/hide/toggle/quit) via
    a 50ms glib::timeout_add_local pump. On tray spawn failure flips
    state.tray_available to false. --start-hidden CLI flag suppresses
    the initial window.present() except when tray is unavailable
    (force-present so the user isn't stranded).

    Per #512 close-to-tray plan tasks 7+9.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 10: window.rs — close-request hides + first-close toast

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 10.1: Replace the close-request handler**

Find the existing `window.connect_close_request(...)` block at line ~506. Replace with:

```rust
    let app_for_close = app.clone();
    let state_for_close = Rc::clone(&state);
    let config_for_close = Arc::clone(config);
    let toast_overlay_close = toast_overlay.downgrade();
    let transcription_engine_close = Rc::clone(&transcription_engine);
    let window_for_close = window.clone();
    window.connect_close_request(move |_| {
        let bt = std::backtrace::Backtrace::capture();
        tracing::info!(backtrace = ?bt, "main window close-request fired");

        // Close-to-tray: hide instead of destroy if both the user
        // toggle is on AND the tray is actually available. If the
        // tray failed to spawn, we MUST proceed-to-close — otherwise
        // the user is stuck with an invisible process. Per #512.
        if state_for_close.close_to_tray.get() && state_for_close.tray_available.get() {
            window_for_close.set_visible(false);
            // First-close toast: fire exactly once per fresh config.
            if !state_for_close.tray_first_close_seen.get() {
                state_for_close.tray_first_close_seen.set(true);
                config_for_close.write(|v| {
                    v[crate::preferences::general_page::KEY_TRAY_FIRST_CLOSE_SEEN] =
                        serde_json::json!(true);
                });
                if let Some(overlay) = toast_overlay_close.upgrade() {
                    let toast = adw::Toast::builder()
                        .title("App still running in tray — right-click tray icon, then Quit, or disable in Settings, then General, then Behavior")
                        .timeout(8)
                        .build();
                    overlay.add_toast(toast);
                }
            }
            return glib::Propagation::Stop;
        }

        // Real close — original teardown.
        app_for_close.remove_action(crate::notify::TUNE_SATELLITE_ACTION);
        transcription_engine_close.borrow_mut().shutdown_nonblocking();
        glib::Propagation::Proceed
    });
```

(Verify that `config` in this scope is `&Arc<ConfigManager>` so `Arc::clone(config)` works. If it's owned in this scope by another name, adapt.)

- [ ] **Step 10.2: Build + lint**

Run: `cargo build -p sdr-ui`
Run: `cargo clippy -p sdr-ui --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 10.3: Commit**

Run:

    git add crates/sdr-ui/src/window.rs

Then commit:

    feat(sdr-ui): close-request hides instead of destroying when tray on

    If close_to_tray is on AND tray_available is true, the close button
    hides the window via set_visible(false) and returns Propagation::Stop.
    First close also fires a one-shot 8-second toast pointing to the
    disable-in-Settings path, persisted via tray_first_close_seen so it
    never fires twice. Tray-unavailable case falls through to the
    original "shutdown transcription, Propagation::Proceed" path.

    Per #512 close-to-tray plan task 10.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>

---

## Task 11: window.rs — tray-show / tray-hide / tray-toggle / tray-quit GIO actions

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

- [ ] **Step 11.1: Register the four actions near the existing quit action block**

Find `let quit_action = gio::SimpleAction::new("quit", None);` (around line 12188). After the `app.add_action(&quit_action)` and `set_accels_for_action` lines, append:

```rust
    // --- tray-* actions (#512) ---

    let tray_show = gio::SimpleAction::new("tray-show", None);
    tray_show.connect_activate(glib::clone!(
        #[weak] window,
        move |_, _| { window.present(); }
    ));
    app.add_action(&tray_show);

    let tray_hide = gio::SimpleAction::new("tray-hide", None);
    tray_hide.connect_activate(glib::clone!(
        #[weak] window,
        move |_, _| { window.set_visible(false); }
    ));
    app.add_action(&tray_hide);

    let tray_toggle = gio::SimpleAction::new("tray-toggle", None);
    tray_toggle.connect_activate(glib::clone!(
        #[weak] window,
        move |_, _| {
            if window.is_visible() {
                window.set_visible(false);
            } else {
                window.present();
            }
        }
    ));
    app.add_action(&tray_toggle);

    let tray_quit = gio::SimpleAction::new("tray-quit", None);
    let app_for_quit = app.clone();
    let state_for_quit = Rc::clone(&state);
    let window_for_quit = window.clone();
    let transcription_for_quit = Rc::clone(&transcription_engine);
    tray_quit.connect_activate(move |_, _| {
        if state_for_quit.is_recording() {
            let dialog = adw::MessageDialog::builder()
                .transient_for(&window_for_quit)
                .modal(true)
                .heading("Recording in progress")
                .body("Quit anyway? The current pass will not be saved.")
                .build();
            dialog.add_response("cancel", "_Cancel");
            dialog.add_response("quit", "_Quit anyway");
            dialog.set_response_appearance("quit", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel"); // WM-close = Cancel
            let app_for_response = app_for_quit.clone();
            let state_for_response = Rc::clone(&state_for_quit);
            let window_for_response = window_for_quit.clone();
            let transcription_for_response = Rc::clone(&transcription_for_quit);
            dialog.connect_response(None, move |dlg, response| {
                if response == "quit" {
                    perform_real_quit(
                        &app_for_response,
                        &state_for_response,
                        &window_for_response,
                        &transcription_for_response,
                    );
                }
                dlg.close();
            });
            dialog.present();
            return;
        }
        perform_real_quit(
            &app_for_quit,
            &state_for_quit,
            &window_for_quit,
            &transcription_for_quit,
        );
    });
    app.add_action(&tray_quit);
```

Add the helper at the bottom of the same file (or in a sibling private module):

```rust
fn perform_real_quit(
    app: &adw::Application,
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    transcription_engine: &Rc<RefCell<TranscriptionEngine>>,
) {
    tracing::info!("tray-quit: shutting down");
    if let Some(mut handle) = state.tray_handle.borrow_mut().take() {
        handle.shutdown();
    }
    app.remove_action(crate::notify::TUNE_SATELLITE_ACTION);
    transcription_engine.borrow_mut().shutdown_nonblocking();
    window.destroy();
    // Balance the app.hold() from connect_startup.
    app.release();
}
```

- [ ] **Step 11.2: Build + lint**

Run: `cargo build -p sdr-ui`
Run: `cargo clippy -p sdr-ui --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 11.3: Commit**

Run:

    git add crates/sdr-ui/src/window.rs

Then commit:

    feat(sdr-ui): tray-show/hide/toggle/quit GIO actions

    Four new GApplication actions, fired from app.rs's TrayEvent router:
    - tray-show: window.present()
    - tray-hide: set_visible(false)
    - tray-toggle: visibility XOR (left-click on icon)
    - tray-quit: AdwMessageDialog confirmation if state.is_recording(),
      otherwise perform_real_quit() — joins tray_handle, removes
      tune-satellite action, shuts down transcription, destroys window,
      releases the application hold.

    Per #512 close-to-tray plan task 11.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>


---

## Task 12: tests/close_to_tray_config_round_trip.rs

**Files:**
- Create: `crates/sdr-ui/tests/close_to_tray_config_round_trip.rs`

- [ ] **Step 12.1: Write the test**

Create `crates/sdr-ui/tests/close_to_tray_config_round_trip.rs`:

```rust
//! Pin the persisted-config -> AppState hydration path. Per #512.

use sdr_config::ConfigManager;
use sdr_ui::preferences::general_page::{
    read_close_to_tray, read_tray_first_close_seen,
};

#[test]
fn close_to_tray_default_is_true() {
    let config = ConfigManager::in_memory(&serde_json::json!({}));
    assert!(read_close_to_tray(&config), "default must be true");
}

#[test]
fn close_to_tray_persisted_false_round_trips() {
    let config = ConfigManager::in_memory(&serde_json::json!({
        "close_to_tray": false,
    }));
    assert!(!read_close_to_tray(&config));
}

#[test]
fn tray_first_close_seen_default_is_false() {
    let config = ConfigManager::in_memory(&serde_json::json!({}));
    assert!(!read_tray_first_close_seen(&config));
}

#[test]
fn tray_first_close_seen_persisted_true_round_trips() {
    let config = ConfigManager::in_memory(&serde_json::json!({
        "tray_first_close_seen": true,
    }));
    assert!(read_tray_first_close_seen(&config));
}
```

- [ ] **Step 12.2: Run the tests + lint**

Run: `cargo test -p sdr-ui --test close_to_tray_config_round_trip`
Expected: 4 PASS.

Run: `cargo clippy --all-targets -p sdr-ui -- -D warnings`
Expected: clean.

- [ ] **Step 12.3: Commit**

Run:

    git add crates/sdr-ui/tests/close_to_tray_config_round_trip.rs

Then commit:

    test(sdr-ui): config round-trip for close_to_tray + first-close-seen

    Pin the persisted-bool -> read helper -> AppState path with four
    in-memory ConfigManager test cases. Both default fallbacks
    (close_to_tray=true, tray_first_close_seen=false) and explicit
    overrides round-trip correctly.

    Per #512 close-to-tray plan task 12.

    Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>

---

## Task 13: Final pre-smoke gates

**Files:** none (validation only)

- [ ] **Step 13.1: Workspace build**

Run: `cargo build --workspace`
Expected: clean.

- [ ] **Step 13.2: Workspace clippy with -D warnings**

Run: `cargo clippy --all-targets --workspace -- -D warnings`
Expected: clean.

- [ ] **Step 13.3: Full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass.

- [ ] **Step 13.4: cargo fmt check**

Run: `cargo fmt --all -- --check`
Expected: clean. If not, run `cargo fmt --all` and re-check.

- [ ] **Step 13.5: cargo deny + cargo audit (`make lint`)**

Run: `make lint`
Expected: no findings.

If any of 13.1-13.5 fail, fix in place and create a new fix commit per CLAUDE.md guidance (do NOT amend).

---

## Task 14: Manual smoke test (user runs the GTK app, Claude installs)

**Files:** none (manual testing only)

- [ ] **Step 14.1: Claude runs `make install` (release)**

Run:

    make install CARGO_FLAGS="--release"

Expected: clean install. Confirm with:

    stat -c '%y' /home/jherald/.cargo/bin/sdr-rs

- [ ] **Step 14.2: Verify the new code is in the installed binary**

Run:

    strings /home/jherald/.cargo/bin/sdr-rs | grep -E "tray-quit|close_to_tray|App still running in tray" | head -5

Expected: at least one match per pattern. If missing, the build was stale — re-run `make install CARGO_FLAGS="--release"`.

- [ ] **Step 14.3: Hand off the smoke checklist to the user**

Tell the user verbatim:

> `make install` complete and verified. Please run `sdr-rs` and walk through this checklist (mark each item ✓ / ✗ — note any unexpected behavior):
>
> 1. **Default close behavior** — fresh launch (close any existing instance first), click the window's close button. Window should hide; tray icon should appear in waybar/tray; toast `App still running in tray …` should appear briefly.
> 2. **Second close, no toast** — re-show via tray, close again. Window hides, no toast.
> 3. **Tray Show via left-click** — single-click the tray icon. Window presents.
> 4. **Tray Show via right-click menu** — right-click tray icon, then `Show / Hide` → window presents. Right-click again, then `Show / Hide` → window hides.
> 5. **Tray Quit (idle)** — right-click tray, then `Quit` → no modal, app exits, tray icon disappears, `pgrep sdr-rs` returns nothing.
> 6. **Tray Quit (recording)** — start audio recording from the radio panel, right-click tray, then `Quit` → modal "Recording in progress" appears. Cancel → app keeps running, recording continues. Repeat → `Quit anyway` → app exits.
> 7. **Re-launch while hidden** — close to hide, then `sdr-rs` from a terminal → existing window raises (single-instance forwarding works).
> 8. **Behavior toggle off** — Settings → General → Behavior → flip `Keep running in tray when window is closed` OFF → close window → app exits cleanly.
> 9. **Behavior toggle re-on** — re-launch, turn back on, close window → hides (no toast — flag persisted).
> 10. **Autostart enable** — Settings → General → Behavior → flip `Start at login` ON → check `cat ~/.config/autostart/com.sdr.rs.desktop` → contains `Exec=sdr-rs --start-hidden`. Reboot or log out / back in → app launches hidden, tray icon appears.
> 11. **Autostart disable** — flip `Start at login` OFF → `ls ~/.config/autostart/com.sdr.rs.desktop` → file gone.
> 12. **Tray-spawn failure simulation** — temporarily tank the SNI watcher (e.g., on GNOME without AppIndicator: it'll already fail). Launch the app → window builds normally, prefs Behavior `Keep running in tray …` row is greyed with tooltip "Disabled — no system tray detected", close button exits cleanly, no tray icon.
> 13. **DSP keeps running while hidden** — start a satellite auto-record session OR audio recording, hide window via close button, watch `~/sdr-recordings/` to confirm WAV grows. Show window → APT viewer image is intact.
>
> If any step fails, paste the failing step number plus your observation. I'll diagnose from there.

- [ ] **Step 14.4: Address smoke failures, then push**

If smoke is clean, run the final fmt gate and push:

Run: `cargo fmt --all -- --check`
Expected: clean.

Run: `git push -u origin feat/close-to-tray`

Then open the PR via `gh pr create` per the standard workflow.

---

## Self-review checklist

(Run before announcing the plan is ready.)

**Spec coverage:**
- Architecture (spec section: Architecture) → Tasks 1-3, 9, 10, 11
- Components / file layout (spec section: Components) → Task file structure header, Tasks 1, 6, 8
- Data flow startup paths (spec section: Data flow → Startup, Startup --start-hidden) → Tasks 7, 9
- Data flow window-close (spec section: Data flow → User clicks window close button) → Task 10
- Data flow tray-icon click (spec section: Data flow → User left-clicks tray icon) → Tasks 2, 9, 11
- Data flow tray Quit (spec section: Data flow → User selects tray menu Quit) → Task 11
- Data flow prefs toggles (spec section: Data flow → User flips toggles) → Task 8
- Data flow re-launch hidden (spec section: Data flow → Re-launch while hidden) → existing single-instance code stays untouched, verified by smoke step 14.3.7
- Data flow tray spawn failure (spec section: Data flow → Tray spawn failure) → Tasks 8, 9, 10
- Config schema (spec section: Config schema) → Tasks 4, 8, 12
- Error handling (spec section: Error handling) → Tasks 3, 8, 9, 11
- Testing unit (spec section: Testing) → Tasks 4, 6, 12
- Testing integration (spec section: Testing) → Task 2
- Testing manual smoke (spec section: Testing) → Task 14
- Out of scope items (spec section: Out of scope) → Not implemented (correct)

**Placeholder scan:** no "TBD", "TODO", "later", "implement later" in the plan body. ✓

**Type consistency:** TrayEvent, TrayHandle, SpawnError consistent across Tasks 1, 2, 9, 11. state.close_to_tray, tray_first_close_seen, tray_available, tray_handle, audio/iq/lrpt _recording_active consistent. is_recording() consistent. KEY_CLOSE_TO_TRAY, KEY_AUTOSTART, KEY_TRAY_FIRST_CLOSE_SEEN consistent. read_close_to_tray, read_tray_first_close_seen consistent. build_app_with_options(start_hidden) consistent across Tasks 7, 9. ✓

**Every step** has either exact code, an exact command, or a checklist item. No "Similar to Task N" references. ✓
