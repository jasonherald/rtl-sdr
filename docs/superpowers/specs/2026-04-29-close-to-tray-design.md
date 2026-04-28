# Close-to-Tray + Keep-Running Design

**Issue:** #512
**Date:** 2026-04-29
**Status:** Spec approved, ready for implementation plan
**Scope:** Single bundled PR, Linux-only

## Goal

When the user closes the main window, hide it instead of exiting the process. Show a system-tray icon as the always-visible affordance to bring the window back. Keep the DSP thread alive in the background so satellite passes scheduled hours in advance still record.

## Decisions locked in (during brainstorming)

- **Single bundled PR** (not split into "lifecycle" + "tray" tickets) — bundling exposes more cross-cutting bugs to CodeRabbit and avoids shipping half a feature.
- **Default `close_to_tray: true`** — the close-to-tray behavior is the headline feature; users who want close-to-exit flip the toggle.
- **Tray menu = `Show/Hide` + `Quit`** (minimal MVP). No status line, no inline auto-record toggle, no dynamic icon glyph.
- **First-close UX = one-shot toast** (`App still running in tray …`), persisted by a `tray_first_close_seen: bool` config flag.
- **Quit-while-recording = confirmation modal**, but only when actively recording (`apt_recording_pass` or `lrpt_recording_pass` is `Some`, or any audio/IQ writer is open). Idle quit is silent.
- **Autostart = in scope** — separate `Start at login` toggle in the same Behavior prefs group, default off, opt-in.
- **Library = `ksni`** (pure-Rust StatusNotifierItem) in a new `sdr-tray` workspace crate.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│  sdr-ui (GTK4 main loop)                                 │
│                                                          │
│   AdwApplication                                         │
│     │  app.hold()  ──►  process stays alive without windows
│     │                                                    │
│     ├─ activate (re-launch / autostart) ──► present()    │
│     │                                                    │
│     └─ window                                            │
│         └─ close-request handler                         │
│              └─ hides window if `close_to_tray` ON       │
│              └─ shows one-shot toast if first close      │
│                                                          │
│   gio::SimpleAction "tray-show"  ──┐                     │
│   gio::SimpleAction "tray-hide"  ──┤  invoked from tray  │
│   gio::SimpleAction "tray-quit"  ──┘                     │
│         ▲                                                │
└─────────┼────────────────────────────────────────────────┘
          │ glib::idle_add_local (cross-thread → main loop)
          │
┌─────────┴────────────────────────────────────────────────┐
│  sdr-tray (NEW crate, Linux-only)                        │
│                                                          │
│   pub struct TrayHandle { stop_tx, ... }                 │
│   pub fn spawn(events: glib::Sender<TrayEvent>)          │
│        ──► std::thread                                   │
│             ├─ smol::block_on(...)                       │
│             ├─ ksni::TrayService running                 │
│             └─ on click → events.send(Show/Hide/Quit)    │
│                                                          │
│   Pure-Rust. No GTK deps. No knowledge of AppState.      │
└──────────────────────────────────────────────────────────┘
```

### Key invariants

- **Process keep-alive** comes from a single `app.hold()` at startup, balanced by an explicit `app.release()` only when the user picks Quit. Window destruction is an event, not the process-lifetime trigger.
- **Window-hide vs window-destroy.** Close button → `set_visible(false)` (window stays alive in GTK toplevel registry, GL contexts and FFT buffers retained). Tray Quit → real `window.destroy() + app.release()` → clean shutdown. The DSP thread is `mpsc`-only, so it doesn't care about window visibility.
- **`sdr-tray` is a pure sidecar.** It owns nothing the UI owns; communication is one-way `glib::Sender<TrayEvent>` from tray-thread to main loop. UI never holds tray state — if tray fails to spawn we degrade to "process exits when window closed".
- **Single-instance behavior** keeps working: `register_and_check_primary` already forwards `activate` from a re-launched second instance to the running primary. We add `present()` to the activate handler so `sdr-rs` from the terminal raises a hidden window.
- **Autostart** is `~/.config/autostart/com.sdr.rs.desktop` containing `Exec=sdr-rs --start-hidden`. The `--start-hidden` CLI flag suppresses the initial `window.present()` at activate.

## Components

### New crate: `crates/sdr-tray/`

```
crates/sdr-tray/
  Cargo.toml             # depends on: ksni, smol, tracing
  src/
    lib.rs               # pub fn spawn(events: glib::Sender<TrayEvent>) -> Result<TrayHandle, SpawnError>
                         # pub enum TrayEvent { Show, Hide, ToggleVisibility, Quit }
                         # pub struct TrayHandle { stop_tx: mpsc::Sender<()>, join: JoinHandle<()> }
                         # pub enum SpawnError { TrayWatcherUnavailable, ... }
    icon.rs              # tray icon ARGB32 byte buffer loaded via
                         # include_bytes!("../../../data/com.sdr.rs.tray22.argb32").
                         # The .argb32 asset is pre-baked from data/com.sdr.rs.svg
                         # by scripts/regen-tray-icon.sh — no runtime SVG deps.
                         # (Earlier draft used librsvg at runtime; reverted in
                         # commit b2595ba — librsvg drags ~80 transitive crates
                         # including unmaintained paste/fxhash.)
```

The crate is `#![cfg(target_os = "linux")]`. It has zero workspace deps — no `sdr-types`, no `sdr-config`, no `sdr-ui`. It knows nothing about satellites, recording, or DSP. Its entire API is "spawn me, give me a sender, I'll send Show/Hide/Quit events."

### `sdr-ui` modifications

- **`crates/sdr-ui/src/app.rs`** — call `app.hold()` in `connect_startup`; in `connect_activate`, parse `--start-hidden` from CLI args and skip `window.present()` if set; spawn `sdr-tray` and route `TrayEvent`s to the three GIO actions via `glib::idle_add_local`.
- **`crates/sdr-ui/src/window.rs`** — modify `connect_close_request` (currently at line 506) to check `state.close_to_tray.get()`; if true, hide instead of close (and trigger one-shot toast if `tray_first_close_seen == false`); if false, current behavior (`Propagation::Proceed` → app exits naturally on last-window-close).
- **`crates/sdr-ui/src/window.rs`** (action registration block, currently around line 12188) — add `tray-show`, `tray-hide`, `tray-quit` `SimpleAction`s. `tray-quit` checks `state.is_recording()` and shows confirmation modal if so.
- **`crates/sdr-ui/src/state.rs`** — new `Cell<bool>` for `close_to_tray`, `tray_first_close_seen`, `tray_available`; helper `fn is_recording(&self) -> bool` that returns the OR of every active-recording flag.
- **`crates/sdr-ui/src/preferences/general_page.rs`** — new `Behavior` `AdwPreferencesGroup` above `Directories` with two `AdwSwitchRow`s: `Keep running in tray when window is closed` and `Start at login`. Each row's `notify::active` writes config + does the side effect. The first row is greyed with explanatory tooltip when `state.tray_available == false`.

### New module in `sdr-ui`

- **`crates/sdr-ui/src/autostart.rs`** — `pub fn enable() -> io::Result<()>`, `pub fn disable() -> io::Result<()>`, `pub fn is_enabled() -> bool`. Pure I/O, no GTK. Tests inject the autostart-dir path so they don't write into the real `~/.config/autostart`.

### Workspace changes

- **`Cargo.toml`** (workspace) — add `sdr-tray = { path = "crates/sdr-tray" }` to `[workspace.dependencies]`; add `ksni`, `smol` to workspace deps.
- **`src/main.rs`** — add `--start-hidden` boolean CLI flag alongside the existing flags.

### Icon asset

`data/com.sdr.rs.tray22.argb32` — a pre-baked 1936-byte ARGB32 buffer (22 × 22 × 4) generated from `data/com.sdr.rs.svg` by `scripts/regen-tray-icon.sh` (uses `rsvg-convert` + Pillow). Committed to the repo so the runtime never has to rasterize. `icon.rs` loads it via `include_bytes!` with a compile-time `const_assert` that the byte length matches `width × height × 4`. Re-run the script whenever the SVG source changes.

Multi-size support (16/32/48/HiDPI) is tracked in [#573](https://github.com/jasonherald/rtl-sdr/issues/573) — would expand the script to generate multiple `.argb32` files and have ksni's `icon_pixmap()` return a `Vec` of `Icon`s. (An earlier draft of this spec called for runtime SVG rasterization via `librsvg`; that was reverted in commit `b2595ba` because `librsvg`'s ~80-crate transitive dep tree dragged in unmaintained `paste` / `fxhash`. The pre-bake approach has zero runtime SVG deps.)

## Data flow

### Startup (normal launch)

```
main → AdwApplication::run
  → connect_startup
      ├─ app.hold()                                 # process keep-alive
      ├─ load CSS, icons
      └─ spawn sdr-tray service ──┐
                                  │ glib::Sender<TrayEvent>
  → connect_activate              │
      ├─ build_window             │
      ├─ register tray-show/hide/quit SimpleActions ◄── routes TrayEvents
      └─ window.present()         #  unless --start-hidden
```

### Startup (autostart, `--start-hidden`)

```
~/.config/autostart/com.sdr.rs.desktop
  Exec=sdr-rs --start-hidden
  → connect_activate
      ├─ build_window
      └─ skip window.present()    # window exists, just not mapped;
                                  # tray icon is the only visible affordance
```

### User clicks window close button

```
GTK fires connect_close_request
  ├─ if !state.close_to_tray.get() || !state.tray_available.get():
  │       return Propagation::Proceed
  │       (window destroys, last-window-close → app released-by-default
  │        → process exits)
  └─ else:
      ├─ window.set_visible(false)
      ├─ if !state.tray_first_close_seen.get():
      │     show toast: "App still running in tray …"
      │     state.tray_first_close_seen.set(true)
      │     config.write(KEY_TRAY_FIRST_CLOSE_SEEN := true)
      └─ return Propagation::Stop
```

### User left-clicks tray icon (toggle convention)

```
ksni "activate" callback (left-click default action)
  → events.send(TrayEvent::ToggleVisibility)
  → glib::idle_add_local on main loop
  → if window.is_visible() { window.set_visible(false) }
    else                   { window.present() }
```

Right-click on the tray icon opens the `Show/Hide` + `Quit` menu via ksni's standard menu binding.

### User selects tray menu → Quit

```
ksni callback for menu item id="quit"
  → events.send(TrayEvent::Quit)
  → glib::idle_add_local
  → app.activate_action("tray-quit", None)
        if state.is_recording():
            modal: "Recording in progress. Quit anyway?" + secondary line if available
       (e.g., "NOAA 19 — ~4 min remaining")
            on Cancel:        return
            on Quit anyway:   proceed
            on WM-close:      treat as Cancel
        ──► tray_handle.shutdown()       # stop_tx send, join thread
        ──► transcription_engine.shutdown_nonblocking()
        ──► window.destroy()
        ──► app.release()                 # balances startup app.hold()
                                          # last reference released → mainloop exits
```

`is_recording()` is precisely:

```rust
fn is_recording(&self) -> bool {
    self.apt_recording_pass.borrow().is_some()
        || self.lrpt_recording_pass.borrow().is_some()
        || self.audio_writer_open.get()
        || self.iq_writer_open.get()
}
```

(The exact field names are placeholders; the predicate is the OR of every "we are actively writing pass artifacts to disk" condition. The implementation plan will pin them against the current `AppState`.)

### User flips "Keep running in tray" OFF in prefs

```
AdwSwitchRow notify::active fires
  ├─ config.write(KEY_CLOSE_TO_TRAY := false)
  ├─ state.close_to_tray.set(false)
  └─ tray icon stays visible — still useful for Show/Quit, just not
                              triggered by close button anymore
```

### User flips "Start at login" ON in prefs

```
AdwSwitchRow notify::active fires
  ├─ autostart::enable()
  │   → write ~/.config/autostart/com.sdr.rs.desktop
  │     [Desktop Entry]
  │     Type=Application
  │     Exec=sdr-rs --start-hidden
  │     Hidden=false
  │     X-GNOME-Autostart-enabled=true
  │     Name=SDR-RS
  │     Icon=com.sdr.rs
  ├─ on Err(io): toast "Couldn't enable autostart: <err>" + revert switch
  └─ config.write(KEY_AUTOSTART := true)
```

### Re-launch while hidden

```
secondary: app.register() detects primary → app.is_remote() == true
  ├─ secondary forwards "activate" signal to primary via D-Bus
  └─ secondary exits 0
primary: connect_activate fires again
  └─ window.present()              # raises hidden window
```

`sdr-rs` from a terminal raises the running app — convenient when the tray is buried in an overflow menu.

### Tray spawn failure (no SNI watcher)

```
sdr-tray::spawn returns Err(SpawnError::TrayWatcherUnavailable)
  ├─ tracing::warn!("tray spawn failed: {e} — close-to-tray disabled this session")
  ├─ state.tray_available.set(false)
  ├─ window's close-request handler skips the close→hide path (else the user
  │                                  loses the only way to interact with the
  │                                  running app)
  └─ prefs Behavior group renders the close-to-tray switch greyed with
     tooltip: "Disabled — no system tray detected"
```

## Config schema

Three new JSON keys in `~/.config/sdr-rs/config.json`. Constants live in `crates/sdr-ui/src/preferences/general_page.rs`:

```rust
const KEY_CLOSE_TO_TRAY: &str = "close_to_tray";
const KEY_AUTOSTART: &str = "autostart";
const KEY_TRAY_FIRST_CLOSE_SEEN: &str = "tray_first_close_seen";
```

| Key | Type | Default | Read at | Written by |
|---|---|---|---|---|
| `close_to_tray` | `bool` | `true` | startup → `state.close_to_tray` | prefs Behavior toggle |
| `autostart` | `bool` | `false` | startup, prefs panel hydration | prefs Behavior toggle (mirrors filesystem) |
| `tray_first_close_seen` | `bool` | `false` | startup → `state.tray_first_close_seen` | window close-request handler on first hide |

**`autostart` config + filesystem reconciliation:** the source of truth is the `~/.config/autostart/com.sdr.rs.desktop` *file*; the config bool is a cached read-fast mirror. Startup runs `state.autostart.set(autostart::is_enabled())` and writes the config to match if it disagrees. Writing the config simultaneously when the file is updated keeps the prefs row's initial draw cheap (no stat per startup).

## Error handling

| Failure | Detection | Behavior | User-visible signal |
|---|---|---|---|
| **Tray spawn fails** (no SNI watcher) | `sdr-tray::spawn` returns `Err(TrayWatcherUnavailable)` | App still launches, close-to-tray bypassed for this session, `state.tray_available = false`. **If `--start-hidden` was set on this launch**, force `window.present()` anyway so the user isn't left with an invisible process. | One-time toast at first failed close: `Tray unavailable — closing will exit the app. (Likely missing AppIndicator extension.)` Behavior switch greyed in prefs. If we override `--start-hidden`, additional toast at startup: `Tray unavailable — showing window instead.` |
| **D-Bus drops mid-runtime** | ksni's run loop returns; tray-thread exits | Log `tracing::warn`. Don't respawn. Flag `tray_available = false`. Window's close-button reverts to real exit. | None — silent fallback |
| **Tray icon byte buffer is wrong size** | `const_assert` in `icon.rs` fails compilation | Buffer length must equal `width × height × 4`; the regen script's post-write guard catches this earlier with a clearer error. Run `scripts/regen-tray-icon.sh` to recompute. | Compile-time error, never reaches the user |
| **Autostart enable: write fails** | `autostart::enable() → io::Result<()>` returns Err | Revert the switch (set_active false, suppressing recursive notify::active) | Toast: `Couldn't enable autostart: <err>` |
| **Autostart disable: remove fails** | Same | Revert switch | Toast: `Couldn't disable autostart: <err>` |
| **Autostart file missing but config says true** | Startup `is_enabled() == false`; config says true | Trust filesystem. Update config to false. | None — silent reconciliation |
| **Autostart file present but config says false** | Same, opposite direction | Trust filesystem. Update config to true. | None |
| **Quit-while-recording modal dismissed via WM close** | `gtk4::ResponseType::DeleteEvent` (or no response click) | Treat as Cancel. App keeps running. | None |
| **Toast already showing on first-close double-fire** | `tray_first_close_seen` cell flips to true *before* showing the toast | Double-fire short-circuits at the cell check | None |
| **`app.release()` before `app.hold()`** | glib warns to stderr | Pair strictly: `hold()` in `connect_startup` once, `release()` only in `tray-quit` action handler post-modal-confirm. Add `state.app_held: Cell<bool>` for dev-build assertion. | `tracing::error!` if assertion trips |

**General principle:** the tray is a *convenience layer* over the existing app. Every failure mode degrades to "the app behaves the way it did before this PR" — process exits cleanly when the window closes, autostart is just a missing file. We never block app startup or window close on tray health.

## Testing strategy

### Pure unit tests (cargo test, headless CI)

- **`crates/sdr-ui/src/autostart.rs`** — `enable()` / `disable()` / `is_enabled()` against a `tempdir()` config root. Inject the autostart-dir path. Assert: enable writes a `.desktop` with `Exec=sdr-rs --start-hidden`; disable removes it; is_enabled is `path.exists()`; double-enable is idempotent; disable on missing file is `Ok(())`.
- **`crates/sdr-ui/src/state.rs::is_recording`** — table-driven, every combination of `apt_recording_pass.is_some()`, `lrpt_recording_pass.is_some()`, audio_writer_open, iq_writer_open. 16 rows. OR-equivalence pinned. New recording-type maintainer notices when this test fails because they didn't extend it.
- **`crates/sdr-tray/src/lib.rs`** — `TrayHandle::shutdown` joins the thread cleanly; `spawn` returns `Err(TrayWatcherUnavailable)` when D-Bus session bus is unreachable. The "unreachable" test sets `DBUS_SESSION_BUS_ADDRESS=unix:abstract=/nonexistent` for the test process.

### Integration tests (cargo test, may require live D-Bus)

- **`crates/sdr-tray/tests/sni_smoke.rs`** — uses ksni's test harness or pokes `org.kde.StatusNotifierWatcher` via `zbus` to verify our SNI registers. Skipped (with `eprintln!` note) if `DBUS_SESSION_BUS_ADDRESS` is unset.
- **`crates/sdr-ui/tests/close_to_tray_config_round_trip.rs`** — write `close_to_tray: false` to a temp config, build state, assert `state.close_to_tray.get() == false`. Mirror for `tray_first_close_seen`.

### Manual smoke checklist

1. **Default close behavior** — fresh launch, click window close → window hides, tray icon visible, toast appears.
2. **Second close, no toast** — close again → window hides, no toast.
3. **Tray Show via left-click** — click tray icon → window presents.
4. **Tray Show via right-click menu** — right-click tray → `Show` → window presents. Then `Hide` → window hides.
5. **Tray Quit (idle)** — right-click tray → `Quit` → no modal, app exits, tray icon disappears, `pgrep sdr-rs` empty.
6. **Tray Quit (recording)** — start an APT pass auto-record (or fake one), right-click tray → `Quit` → modal `Recording in progress …`. Cancel → app keeps running. Re-do, `Quit anyway` → app exits, recording lost (acceptable).
7. **Re-launch while hidden** — close to hide, then `sdr-rs` from terminal → existing window raises.
8. **Behavior toggle off** — Settings → Behavior → flip `Keep running in tray when window is closed` off → close window → app exits cleanly.
9. **Behavior toggle re-on** — turn back on → close window → hides (no toast — flag was set in step 1).
10. **Autostart enable** — Settings → Behavior → flip `Start at login` on → verify `~/.config/autostart/com.sdr.rs.desktop` exists with `Exec=sdr-rs --start-hidden`. Reboot → app launches hidden, tray icon appears.
11. **Autostart disable** — flip off → verify file removed.
12. **Tray-spawn failure simulation** — tank SNI on this machine, launch the app → window builds normally, prefs Behavior switch greyed with tooltip, close-button exits cleanly, no tray icon.
13. **DSP keeps running while hidden** — start a satellite auto-record session, hide window, watch `~/sdr-recordings/` to confirm the WAV grows. Show window → APT viewer image is intact.

CI runs (1)-(3) implicitly via the unit tests. (4)-(13) are manual on a real desktop.

## Out of scope (deferred to follow-up tickets)

- **macOS tray** (`NSStatusItem` / `NSStatusBar`) — separate ticket once the Mac frontend lands.
- **Windows tray** (`Shell_NotifyIcon`) — same.
- **Dynamic tray status line** ("NOAA 19 pass in 0:42:13" / "Recording NOAA 15…") — issue draft option B; we picked minimal MVP. Easy follow-up: add a `set_tooltip(&str)` to `TrayHandle`, wire it from a 1Hz tick in window.rs.
- **State-aware icon glyph** — different glyph for idle/running/pass-imminent/recording. Same plan: add `set_icon_state(IconState)` to `TrayHandle`.
- **Inline tray menu toggles** — issue draft option B, we deferred. The menu would need dynamic property updates (D-Bus) to reflect current state — non-trivial.
- **Multi-size icon rasterization** — v1 ships 22×22 only. Follow-up adds 16/32/HiDPI.
- **Notification integration** — interactions between #510 (per-satellite notify) and the tray (e.g. clicking a notification raises the window) are scoped to #510 once this lands.
