# ACARS Aviation UI Implementation Plan (sub-project 3 of epic #474)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the user-facing UI for ACARS reception — an Aviation activity entry in the sidebar, an aviation-panel page (toggle + summary + per-channel stats rows), and a floating ACARS viewer window with a scrollable column-view of decoded messages — all consuming the AppState fields and DspToUi messages that PR #584 (sub-project 2) shipped.

**Architecture:** The AppState ACARS fields are already wired and updated by `handle_dsp_message` in `window.rs` (rounds shipped in #584). Sub-project 3 adds three pure-UI components: (1) a sidebar `aviation_panel.rs` showing toggle + 6 per-channel rows, refreshed at ~4 Hz from `acars_channel_stats`; (2) a top-level `acars_viewer.rs` with a `GtkColumnView` of recent messages backed by a `GListStore`, with header-bar pause/clear/filter; and (3) the wiring in `window.rs` that updates both surfaces from incoming `DspToUi::AcarsMessage` / `AcarsChannelStats` / `AcarsEnabledChanged`. Adds one new AppState field (`acars_viewer_window`), one new activity-bar entry (with **Ctrl+8** — see Task 1), and a toast on engage-failure that sub-project 2 deliberately deferred.

**Tech Stack:** Rust 2024, GTK4 4.x via `gtk4`/`glib`, libadwaita 1.x via `libadwaita`, `sdr_acars::{AcarsMessage, ChannelStats, ChannelLockState}` (sub-project 1 API), `sdr_core::messages::{DspToUi, UiToDsp}` (sub-project 2 message variants), `sdr_acars::label::lookup` (currently always `None` — fallback display per the v1 stub).

---

## File structure

| Path | Responsibility |
|---|---|
| `crates/sdr-ui/src/sidebar/activity_bar.rs` | **MODIFIED.** One new `ActivityBarEntry` for `"aviation"` appended to `LEFT_ACTIVITIES`. Ctrl+8 (NOT Ctrl+7 from the spec — that's taken by Satellites). |
| `crates/sdr-ui/src/sidebar/aviation_panel.rs` | **NEW.** `AviationPanel` struct + `build_aviation_panel()` returning a flat `AdwPreferencesPage` with two groups (ACARS toggle + status; per-channel rows). Pure widget construction; no signal wiring (that's `window.rs::connect_aviation_panel`). |
| `crates/sdr-ui/src/sidebar/mod.rs` | **MODIFIED.** `pub mod aviation_panel;` declaration. |
| `crates/sdr-ui/src/acars_viewer.rs` | **NEW.** Viewer window: `open_acars_viewer_if_needed(state)` (no-op if already open, builds + presents otherwise), `AcarsMessageObject` glib subclass wrapping `AcarsMessage`, `GListStore` model, `GtkColumnView` with seven columns, header-bar pause/clear/filter, status label, close-request handler, `ViewerHandles` struct stashed on `AppState` so the message-append site in `window.rs` can fill the store. ~400 LOC budget. |
| `crates/sdr-ui/src/lib.rs` | **MODIFIED.** `pub mod acars_viewer;` declaration. |
| `crates/sdr-ui/src/state.rs` | **MODIFIED.** Add `pub acars_viewer_window: RefCell<Option<glib::WeakRef<adw::Window>>>` field + initializer. |
| `crates/sdr-ui/src/window.rs` | **MODIFIED.** New `connect_aviation_panel` function (switch-row → `UiToDsp::SetAcarsEnabled`, status-row throttled refresh, per-channel-row refresh, "Open ACARS Window" button → `open_acars_viewer_if_needed`). Wire toast in `DspToUi::AcarsEnabledChanged(Err(_))` arm (sub-project 2 left this stubbed). Append to `GListStore` on `DspToUi::AcarsMessage` if viewer is open. |

---

## Constants (locked across the plan)

```rust
// In crates/sdr-ui/src/acars_viewer.rs
const ACARS_VIEWER_WINDOW_WIDTH: i32 = 1100;
const ACARS_VIEWER_WINDOW_HEIGHT: i32 = 600;

// In crates/sdr-ui/src/sidebar/aviation_panel.rs
/// Sidebar status-row refresh cadence (per spec section
/// "AcarsPanel structure" — subtitle live-updated, ~4 Hz).
const SIDEBAR_STATUS_REFRESH_MS: u64 = 250;

/// Per-channel row glyphs (per spec section "Group 2 — Channels").
const GLYPH_LOCKED: &str = "●";
const GLYPH_IDLE: &str = "○";
const GLYPH_SIGNAL: &str = "⚠";
```

(Channel-stats refresh is driven by `DspToUi::AcarsChannelStats` arrival, which the DSP side already throttles to ~1 Hz via `ACARS_STATS_EMIT_INTERVAL_MS = 1_000` in `acars_airband_lock`. Sub-project 3 just consumes those events; no separate refresh timer needed for the per-channel rows.)

---

## Task 0: Branch verification + Ctrl+N slot resolution

**Files:** none (sanity check + design decision)

- [ ] **Step 1: Confirm branch state**

```bash
git rev-parse --abbrev-ref HEAD
# Expected: feat/acars-aviation-panel

git log --oneline main -1
# Expected: 53f4dad Merge pull request #584 from jasonherald/feat/acars-pipeline-integration
```

- [ ] **Step 2: Verify Ctrl+N slot availability**

The spec proposed Ctrl+7 for Aviation, but Satellites already owns it. Confirm the current LEFT_ACTIVITIES order:

```bash
grep -A 3 "ActivityBarEntry {" crates/sdr-ui/src/sidebar/activity_bar.rs | grep -E "name:|accelerator:" | head -20
```

Expected: 7 existing entries with `<Ctrl>1` through `<Ctrl>7` taken (general / radio / audio / display / scanner / share / satellites). **Aviation will use `<Ctrl>8`** — that's the next free slot. The spec section "Sidebar registration" (line 255) says `<Ctrl>7`; we deviate from the spec because the slot was taken between spec-write and now.

- [ ] **Step 3: Confirm AppState ACARS fields landed in PR #584**

```bash
grep -n "acars_enabled\|acars_recent\|acars_total_count\|acars_channel_stats\|acars_pre_lock_state" crates/sdr-ui/src/state.rs | head
```

Expected output: 5 field declarations + 5 initializer lines. If anything is missing, stop and surface — sub-project 3 assumes these exist.

---

## Task 1: AppState `acars_viewer_window` field

**Files:**
- Modify: `crates/sdr-ui/src/state.rs`

The viewer window needs a weak-ref slot on AppState so `open_acars_viewer_if_needed` can detect "already open → just present()". Mirrors `lrpt_viewer_window` (state.rs:247).

- [ ] **Step 1: Add the field declaration**

In `crates/sdr-ui/src/state.rs`, find the `acars_pre_lock_state` field (around line 271). Immediately after it, add:

```rust
    /// Currently-open ACARS viewer window, or `None` when no
    /// viewer is open. `glib::WeakRef` so the AppState slot
    /// doesn't keep the window alive past its natural
    /// lifetime. Set by [`crate::acars_viewer::open_acars_viewer_if_needed`];
    /// cleared by the window's `close-request` handler.
    pub acars_viewer_window: RefCell<Option<gtk4::glib::WeakRef<libadwaita::Window>>>,
```

- [ ] **Step 2: Initialize in `AppState::new_shared`**

Find the `new_shared` constructor's field initializer block. The existing ACARS fields end with `acars_pre_lock_state: RefCell::new(None),` (around line 314). Add immediately after:

```rust
            acars_viewer_window: RefCell::new(None),
```

- [ ] **Step 3: Build to confirm**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Extend `acars_defaults_pin_initializer_contract` test**

The CR-round-3 test in `state.rs` pins the ACARS field defaults. Add the new field. Find the test (search for `fn acars_defaults_pin_initializer_contract`) and append to the existing assertions:

```rust
        assert!(
            state.acars_viewer_window.borrow().is_none(),
            "no viewer window until first open"
        );
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p sdr-ui --features whisper-cpu --lib state::tests::acars_defaults_pin_initializer_contract 2>&1 | tail -10
```

Expected: 1 test passes.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-ui/src/state.rs
git commit -m "feat(sdr-ui): AppState acars_viewer_window weak-ref slot"
```

---

## Task 2: Activity-bar Aviation entry

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/activity_bar.rs`

- [ ] **Step 1: Read the existing LEFT_ACTIVITIES**

```bash
sed -n '117,175p' crates/sdr-ui/src/sidebar/activity_bar.rs
```

Note the exact ordering and the canonical `ActivityBarEntry { name, icon_name, display_name, shortcut_label, accelerator }` shape. Confirm Ctrl+1 through Ctrl+7 are all taken and Ctrl+8 is free.

- [ ] **Step 2: Append the Aviation entry to LEFT_ACTIVITIES**

In `crates/sdr-ui/src/sidebar/activity_bar.rs`, find the closing `];` of the `LEFT_ACTIVITIES` array. Add the new entry as the last element (before the `];`):

```rust
    ActivityBarEntry {
        name: "aviation",
        icon_name: "airplane-mode-symbolic",
        display_name: "Aviation",
        shortcut_label: "Ctrl+8",
        accelerator: "<Ctrl>8",
    },
```

(The spec section "Sidebar registration" said `<Ctrl>7`, but Satellites owns that slot. `airplane-mode-symbolic` is confirmed present in Adwaita per `/usr/share/icons/Adwaita/symbolic/status/airplane-mode-symbolic.svg`.)

- [ ] **Step 3: Build to confirm**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

Expected: clean. The activity bar's GtkStack panel for `aviation` doesn't exist yet — that's wired in Task 4 (panel build) + Task 5 (window.rs connect call). For now, the activity-bar icon would render but clicking it would crash. We'll fix that before Task 4 finishes.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/sidebar/activity_bar.rs
git commit -m "feat(sdr-ui): Aviation activity-bar entry (Ctrl+8)"
```

---

## Task 3: AcarsEnabledChanged Err arm — surface a toast

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

Sub-project 2 deliberately deferred toast wiring (the Err arm just logs `tracing::warn!`). Now is the time. The arm at `window.rs:~1997` preserves `acars_enabled` state on Err (correct, per CR round 1 on PR #584); the only change here is to also fire a toast.

- [ ] **Step 1: Locate the Err arm**

```bash
grep -n "ACARS enable failed" crates/sdr-ui/src/window.rs
```

Should land in `handle_dsp_message` around line 1997.

- [ ] **Step 2: Add the toast emission**

Find the existing arm:

```rust
                Err(err) => {
                    tracing::warn!("ACARS enable failed: {err}");
                    // Preserve the last-known `acars_enabled`
                    // state — `Err` doesn't tell us whether the
                    // transition was an engage attempt (so off
                    // is correct) or a disengage attempt (where
                    // the DSP may still be locked, so off would
                    // mis-state the UI). Sub-project 3 wires a
                    // toast off this and the panel toggle handler
                    // can clear the state explicitly when it
                    // knows which transition the user requested.
                }
```

Replace with:

```rust
                Err(err) => {
                    tracing::warn!("ACARS enable failed: {err}");
                    // Surface the failure as a toast so the user
                    // sees the actionable error (e.g. "scanner is
                    // running" or "RTL-SDR required"). Preserve
                    // `acars_enabled` per CR round 1 on PR #584:
                    // Err doesn't disambiguate engage-vs-disengage
                    // failure, so silently flipping the toggle off
                    // could mis-state the UI.
                    if let Some(overlay) = toast_overlay_weak.upgrade() {
                        overlay.add_toast(adw::Toast::new(&format!(
                            "ACARS: {err}"
                        )));
                    }
                }
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(sdr-ui): toast on AcarsEnabledChanged(Err) (deferred from sub-project 2)"
```

---

## Task 4: Aviation panel widget construction

**Files:**
- Create: `crates/sdr-ui/src/sidebar/aviation_panel.rs`
- Modify: `crates/sdr-ui/src/sidebar/mod.rs`

Pure-widget construction; no AppState references, no signal wiring. Mirrors the existing `satellites_panel.rs` shape.

- [ ] **Step 1: Add the module declaration**

In `crates/sdr-ui/src/sidebar/mod.rs`, find the existing `pub mod ...;` block. Add (alphabetical):

```rust
pub mod aviation_panel;
```

- [ ] **Step 2: Create the panel module**

Create `crates/sdr-ui/src/sidebar/aviation_panel.rs`:

```rust
//! Aviation sidebar activity panel (epic #474, sub-project 3).
//!
//! Pure widget construction — no AppState references, no
//! signal wiring. The connect-up logic (switch-row → DSP
//! command, status-row live refresh, channel-row refresh from
//! `DspToUi::AcarsChannelStats`) lives in
//! `crate::window::connect_aviation_panel`. Same separation
//! the other sidebar panels use.

use libadwaita as adw;
use libadwaita::prelude::*;

/// Per-channel row glyphs for the lock-state column. Per spec
/// section "Group 2 — Channels":
///
/// - `LOCKED` ●  — receiving valid frames within the recent window
/// - `IDLE`   ○  — no signal detected
/// - `SIGNAL` ⚠  — RF energy present but no valid frames
pub const GLYPH_LOCKED: &str = "●";
pub const GLYPH_IDLE: &str = "○";
pub const GLYPH_SIGNAL: &str = "⚠";

/// Sidebar status-row refresh cadence (per spec section
/// "AcarsPanel structure" — subtitle live-updated, ~4 Hz).
/// Drives the `glib::timeout_add_local` tick in
/// `crate::window::connect_aviation_panel`.
pub const SIDEBAR_STATUS_REFRESH_MS: u64 = 250;

/// Aviation activity panel built widgets. Returned to
/// `build_window` so signal handlers can wire to specific
/// rows; the module itself does no wiring.
pub struct AviationPanel {
    /// Root `AdwPreferencesPage` to install in the activity-bar
    /// stack.
    pub widget: adw::PreferencesPage,
    /// "Enable ACARS" switch — drives `UiToDsp::SetAcarsEnabled`.
    pub enable_switch: adw::SwitchRow,
    /// Status row showing "Decoded N · Last: Ts ago" subtitle.
    /// Subtitle is live-updated at ~4 Hz from
    /// `crate::window::connect_aviation_panel`.
    pub status_row: adw::ActionRow,
    /// "Open ACARS Window" button — drives
    /// `crate::acars_viewer::open_acars_viewer_if_needed`.
    pub open_viewer_button: gtk4::Button,
    /// Six per-channel rows (one per US-6 channel). Subtitles
    /// are live-updated from `DspToUi::AcarsChannelStats`
    /// arrivals (~1 Hz cadence per the DSP-side throttle).
    pub channel_rows: [adw::ActionRow; 6],
}

/// Build the Aviation activity panel. Pure widget assembly.
#[must_use]
pub fn build_aviation_panel() -> AviationPanel {
    let page = adw::PreferencesPage::new();

    // ─── Group 1: ACARS toggle + status + open-window ───
    let acars_group = adw::PreferencesGroup::builder()
        .title("ACARS")
        .description(
            "Decode aircraft text-message broadcasts (130 MHz US airband). \
             Forces 2.5 MSps source rate and disables the VFO while on.",
        )
        .build();

    let enable_switch = adw::SwitchRow::builder()
        .title("Enable ACARS")
        .subtitle("Locks airband geometry and starts the 6-channel decoder")
        .build();
    acars_group.add(&enable_switch);

    let status_row = adw::ActionRow::builder()
        .title("Status")
        .subtitle("Disabled")
        .build();
    acars_group.add(&status_row);

    let open_viewer_row = adw::ActionRow::builder()
        .title("ACARS messages window")
        .subtitle("Live log of decoded aircraft messages")
        .build();
    let open_viewer_button = gtk4::Button::builder()
        .label("Open")
        .valign(gtk4::Align::Center)
        .build();
    open_viewer_row.add_suffix(&open_viewer_button);
    open_viewer_row.set_activatable_widget(Some(&open_viewer_button));
    acars_group.add(&open_viewer_row);

    page.add(&acars_group);

    // ─── Group 2: per-channel status rows ───
    let channels_group = adw::PreferencesGroup::builder()
        .title("Channels (US-6)")
        .description(&format!(
            "{GLYPH_LOCKED} Locked   {GLYPH_IDLE} Idle   {GLYPH_SIGNAL} Signal-no-decode"
        ))
        .build();

    let channel_rows: [adw::ActionRow; 6] = std::array::from_fn(|_| {
        let row = adw::ActionRow::builder()
            .title("—")
            .subtitle("—")
            .build();
        channels_group.add(&row);
        row
    });

    page.add(&channels_group);

    AviationPanel {
        widget: page,
        enable_switch,
        status_row,
        open_viewer_button,
        channel_rows,
    }
}
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

Expected: clean. The panel widget compiles in isolation; nothing wires to it yet.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/sidebar/aviation_panel.rs crates/sdr-ui/src/sidebar/mod.rs
git commit -m "feat(sdr-ui): aviation_panel.rs widget construction"
```

---

## Task 5: Wire the Aviation panel into the activity-bar GtkStack

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

The activity-bar entry from Task 2 is registered, but clicking the icon would currently fail because the `aviation` page isn't installed in the GtkStack. Find where other panels are added to the stack and add Aviation alongside.

- [ ] **Step 1: Locate the stack-population site**

```bash
grep -n "build_satellites_panel\|stack.add_named\|stack_left.add_named" crates/sdr-ui/src/window.rs | head -10
```

Should reveal a block like `let satellites_panel = build_satellites_panel(); ... stack_left.add_named(&satellites_panel.widget, Some("satellites"));` (or similar — confirm by reading).

- [ ] **Step 2: Add the Aviation panel to the stack**

Immediately after the Satellites panel build/add block, add:

```rust
    let aviation_panel = sidebar::aviation_panel::build_aviation_panel();
    stack_left.add_named(&aviation_panel.widget, Some("aviation"));
```

(Use whatever the local stack variable is named — likely `stack_left` or `left_stack`. Read the surrounding code to confirm.)

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(sdr-ui): install Aviation panel in activity-bar stack"
```

---

## Task 6: Aviation panel signal wiring

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

Connect the panel's `enable_switch`, `status_row`, `channel_rows`, and `open_viewer_button` to AppState + the DSP channel.

- [ ] **Step 1: Add `connect_aviation_panel` function**

Find the existing `connect_*_panel` functions in `window.rs` (search for `fn connect_satellites_panel` as a reference; mirror its signature). Add a new function near them:

```rust
fn connect_aviation_panel(
    panel: &sidebar::aviation_panel::AviationPanel,
    state: &Rc<AppState>,
) {
    use crate::sidebar::aviation_panel::{
        GLYPH_IDLE, GLYPH_LOCKED, GLYPH_SIGNAL, SIDEBAR_STATUS_REFRESH_MS,
    };
    use sdr_acars::ChannelLockState;

    // ─── Toggle: switch-row → SetAcarsEnabled ───
    {
        let state = Rc::clone(state);
        panel
            .enable_switch
            .connect_active_notify(move |row| {
                state.send_dsp(sdr_core::messages::UiToDsp::SetAcarsEnabled(row.is_active()));
            });
    }

    // ─── Toggle: ack from AppState → switch row visual state ───
    // The DspToUi::AcarsEnabledChanged arm in handle_dsp_message
    // updates state.acars_enabled. Push that out to the switch row
    // via a 4 Hz refresh tick (same cadence as the status subtitle).
    // This is simpler than wiring a glib::Sender notification — we
    // already have the timer for the status row.
    let panel_ref = AviationPanelRef {
        enable_switch: panel.enable_switch.clone(),
        status_row: panel.status_row.clone(),
        channel_rows: panel.channel_rows.clone(),
    };

    let state_for_tick = Rc::clone(state);
    glib::timeout_add_local(
        std::time::Duration::from_millis(SIDEBAR_STATUS_REFRESH_MS),
        move || {
            let state = &state_for_tick;
            let enabled = state.acars_enabled.get();
            // Mirror Cell→switch one direction only (manual toggles
            // already round-trip through SetAcarsEnabled). Suppress
            // notify so we don't loop back into send_dsp.
            if panel_ref.enable_switch.is_active() != enabled {
                let signal_id = panel_ref
                    .enable_switch
                    .block_signal(&panel_ref.enable_switch_handler_id());
                panel_ref.enable_switch.set_active(enabled);
                let _ = signal_id;
            }
            // Status subtitle.
            let total = state.acars_total_count.get();
            let last_label = state
                .acars_recent
                .borrow()
                .back()
                .map(|m| format!("Last: {}", format_relative_age(m.timestamp)));
            let subtitle = if enabled {
                match last_label {
                    Some(s) => format!("Decoded {total} · {s}"),
                    None => format!("Decoded {total} · Awaiting first message"),
                }
            } else {
                "Disabled".to_string()
            };
            panel_ref.status_row.set_subtitle(&subtitle);

            // Per-channel rows.
            let stats = state.acars_channel_stats.borrow();
            for (idx, ch) in stats.iter().enumerate() {
                let row = &panel_ref.channel_rows[idx];
                let glyph = match ch.lock_state {
                    ChannelLockState::Locked => GLYPH_LOCKED,
                    ChannelLockState::Idle => GLYPH_IDLE,
                    ChannelLockState::Signal => GLYPH_SIGNAL,
                };
                row.set_title(&format!("{glyph}  {:.3} MHz", ch.freq_hz / 1_000_000.0));
                row.set_subtitle(&format!(
                    "{} msgs · {:.1} dB · {}",
                    ch.msg_count,
                    ch.level_db,
                    ch.last_msg_at
                        .map(format_relative_age)
                        .unwrap_or_else(|| "—".to_string())
                ));
            }
            glib::ControlFlow::Continue
        },
    );

    // ─── Open ACARS window button ───
    {
        let state = Rc::clone(state);
        panel.open_viewer_button.connect_clicked(move |_| {
            crate::acars_viewer::open_acars_viewer_if_needed(&state);
        });
    }
}

/// Format a `SystemTime` as a relative age string ("5s ago",
/// "2m ago", "1h ago"). Returns "—" if the timestamp is in the
/// future or unrepresentable.
fn format_relative_age(ts: std::time::SystemTime) -> String {
    let Ok(elapsed) = ts.elapsed() else {
        return "—".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}
```

> **Implementer note:** The `AviationPanelRef` helper struct + `enable_switch_handler_id()` helper are not strictly necessary if you instead use `panel.enable_switch.set_active(enabled)` directly without blocking the signal — GtkSwitch's `notify::active` emission is idempotent on no-change. Delete the AviationPanelRef scaffolding and just clone the rows individually if it's simpler. The first-pass implementation can use the simpler form; the signal-blocking matters only if you observe a feedback loop in smoke-testing.
>
> Recommended simpler form for first pass:
>
> ```rust
> let switch = panel.enable_switch.clone();
> let status = panel.status_row.clone();
> let rows = panel.channel_rows.clone();
> let state_for_tick = Rc::clone(state);
> glib::timeout_add_local(
>     std::time::Duration::from_millis(SIDEBAR_STATUS_REFRESH_MS),
>     move || {
>         let enabled = state_for_tick.acars_enabled.get();
>         if switch.is_active() != enabled {
>             switch.set_active(enabled);
>         }
>         // ... status subtitle + channel rows as above ...
>         glib::ControlFlow::Continue
>     },
> );
> ```

- [ ] **Step 2: Call `connect_aviation_panel` from `build_window`**

Find where `connect_satellites_panel` is called in `build_window`. Immediately after, add:

```rust
    connect_aviation_panel(&aviation_panel, &state);
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -10
```

Fix any compile errors surfaced. The `format_relative_age` function may already exist elsewhere; if so, reuse it instead of defining a duplicate (grep for `fn format_relative_age` first).

- [ ] **Step 4: Run workspace clippy**

```bash
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
```

Fix any new lints.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(sdr-ui): connect Aviation panel (toggle, status, channel rows)"
```

---

## Task 7: ACARS message glib object wrapper

**Files:**
- Create: `crates/sdr-ui/src/acars_viewer.rs`
- Modify: `crates/sdr-ui/src/lib.rs`

`GListStore` requires a `glib::Object` model type. We define a thin glib subclass `AcarsMessageObject` that wraps an `AcarsMessage`. This is a one-time scaffold; the rest of the viewer attaches to it.

- [ ] **Step 1: Add the module declaration**

In `crates/sdr-ui/src/lib.rs`, add (alphabetical order):

```rust
pub mod acars_viewer;
```

- [ ] **Step 2: Create the viewer module with the wrapper type**

Create `crates/sdr-ui/src/acars_viewer.rs`:

```rust
//! ACARS viewer window (epic #474, sub-project 3).
//!
//! Floating top-level `adw::Window` showing decoded ACARS
//! messages in a scrollable `GtkColumnView`. Same lifecycle
//! pattern as `lrpt_viewer` / `apt_viewer`: opened from the
//! Aviation panel button, weakly held in
//! `AppState::acars_viewer_window` so a second click presents
//! the existing window rather than spawning a duplicate.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::glib::subclass::prelude::*;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::state::AppState;

/// Default window dimensions (per spec `acars_viewer.rs` budget).
const ACARS_VIEWER_WINDOW_WIDTH: i32 = 1100;
const ACARS_VIEWER_WINDOW_HEIGHT: i32 = 600;

// ── glib::Object wrapper around an AcarsMessage ────────────────

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AcarsMessageObject {
        pub inner: RefCell<Option<sdr_acars::AcarsMessage>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AcarsMessageObject {
        const NAME: &'static str = "AcarsMessageObject";
        type Type = super::AcarsMessageObject;
    }

    impl ObjectImpl for AcarsMessageObject {}
}

glib::wrapper! {
    /// Glib subclass wrapping an `AcarsMessage`. `GListStore`
    /// requires a `glib::Object` model type; the viewer's
    /// column-view factories pull the inner `AcarsMessage`
    /// back out via `obj.message().expect(...)` per render.
    pub struct AcarsMessageObject(ObjectSubclass<imp::AcarsMessageObject>);
}

impl AcarsMessageObject {
    /// Wrap an `AcarsMessage` for insertion into a `GListStore`.
    #[must_use]
    pub fn new(msg: sdr_acars::AcarsMessage) -> Self {
        let obj: Self = glib::Object::new();
        *obj.imp().inner.borrow_mut() = Some(msg);
        obj
    }

    /// Borrow the wrapped message. Returns `None` only if a
    /// caller called `take()` (we don't); callers may
    /// `expect()` in factory closures.
    #[must_use]
    pub fn message(&self) -> Option<sdr_acars::AcarsMessage> {
        self.imp().inner.borrow().clone()
    }
}

// ── Public API: open / present-if-already-open ─────────────────

/// Open the ACARS viewer window if not already open. If a
/// viewer window already exists (held weakly in
/// `state.acars_viewer_window`), present it instead of opening
/// a second one. Mirror of `open_lrpt_viewer_if_needed` in
/// `lrpt_viewer.rs`.
pub fn open_acars_viewer_if_needed(state: &Rc<AppState>) {
    // If a viewer is already open, present it.
    if let Some(weak) = state.acars_viewer_window.borrow().as_ref()
        && let Some(window) = weak.upgrade()
    {
        window.present();
        return;
    }
    // First-open path: build a new window, stash a weak ref,
    // and connect the close-request handler to clear the slot.
    let window = build_acars_viewer_window(state);
    *state.acars_viewer_window.borrow_mut() = Some(window.downgrade());
    window.present();
}

// (build_acars_viewer_window + per-feature wiring lands in
// Tasks 8-12; this task ships only the wrapper + open helper.)

fn build_acars_viewer_window(_state: &Rc<AppState>) -> adw::Window {
    // Placeholder — Task 8 fills this in.
    adw::Window::builder()
        .title("ACARS")
        .default_width(ACARS_VIEWER_WINDOW_WIDTH)
        .default_height(ACARS_VIEWER_WINDOW_HEIGHT)
        .modal(false)
        .build()
}
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

Expected: clean. (The `_state` underscore-prefix silences the unused-arg lint until Task 8 wires it.)

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs crates/sdr-ui/src/lib.rs
git commit -m "feat(sdr-ui): acars_viewer.rs scaffold + AcarsMessageObject wrapper"
```

---

## Task 8: Viewer window structure — header bar + ColumnView shell

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

Build the window's content: header bar with pause/clear/filter widgets (placeholders), and the `GtkColumnView` with seven columns. No data flowing in yet (Task 12 wires the GListStore append from `DspToUi::AcarsMessage`).

- [ ] **Step 1: Replace `build_acars_viewer_window` with the full structure**

Replace the placeholder `build_acars_viewer_window` body with:

```rust
fn build_acars_viewer_window(state: &Rc<AppState>) -> adw::Window {
    let window = adw::Window::builder()
        .title("ACARS")
        .default_width(ACARS_VIEWER_WINDOW_WIDTH)
        .default_height(ACARS_VIEWER_WINDOW_HEIGHT)
        .modal(false)
        .build();

    // ─── Header bar ───
    let header = adw::HeaderBar::new();
    let pause_button = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause appending new messages (existing rows stay visible)")
        .build();
    let clear_button = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Clear all messages from the view (does not disable ACARS)")
        .build();
    let filter_entry = gtk4::SearchEntry::builder()
        .placeholder_text("Filter aircraft / label / text…")
        .build();
    let status_label = gtk4::Label::builder().label("0 / 0 messages").build();

    header.pack_start(&pause_button);
    header.pack_start(&clear_button);
    header.set_title_widget(Some(&filter_entry));
    header.pack_end(&status_label);

    // ─── Column view ───
    let store = gtk4::gio::ListStore::new::<AcarsMessageObject>();
    let filter = gtk4::CustomFilter::new(|_obj| true);
    let filter_model =
        gtk4::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    let selection = gtk4::NoSelection::new(Some(filter_model.clone()));
    let column_view = gtk4::ColumnView::builder()
        .model(&selection)
        .show_column_separators(true)
        .show_row_separators(true)
        .build();

    // Seven columns per spec section "Content":
    //   Time | Freq | Aircraft | Mode | Label | Block | Text
    let columns: [(&str, fn(&sdr_acars::AcarsMessage) -> String, bool); 7] = [
        ("Time", render_time, false),
        ("Freq", render_freq, false),
        ("Aircraft", render_aircraft, false),
        ("Mode", render_mode, false),
        ("Label", render_label, false),
        ("Block", render_block, false),
        ("Text", render_text, true),
    ];

    for (title, render, expand) in columns {
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(move |_factory, item| {
            let label = gtk4::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            item.downcast_ref::<gtk4::ListItem>()
                .expect("setup item is a ListItem")
                .set_child(Some(&label));
        });
        factory.connect_bind(move |_factory, item| {
            let item = item
                .downcast_ref::<gtk4::ListItem>()
                .expect("bind item is a ListItem");
            let label = item
                .child()
                .and_then(|w| w.downcast::<gtk4::Label>().ok())
                .expect("setup installed a Label child");
            let obj = item
                .item()
                .and_then(|o| o.downcast::<AcarsMessageObject>().ok())
                .expect("model row is an AcarsMessageObject");
            if let Some(msg) = obj.message() {
                label.set_text(&render(&msg));
            }
        });
        let column = gtk4::ColumnViewColumn::builder()
            .title(title)
            .factory(&factory)
            .resizable(true)
            .expand(expand)
            .build();
        column_view.append_column(&column);
    }

    let scroll = gtk4::ScrolledWindow::builder()
        .child(&column_view)
        .vexpand(true)
        .hexpand(true)
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&scroll);
    window.set_content(Some(&content));

    // Wire close-request to clear the AppState weak-ref slot.
    {
        let state = Rc::clone(state);
        window.connect_close_request(move |_| {
            *state.acars_viewer_window.borrow_mut() = None;
            glib::Propagation::Proceed
        });
    }

    // Stash references on the window via property storage for
    // Task 9-12 to hook into. We use the existing
    // `AppState.acars_viewer_window` weak handle; the per-window
    // mutable bits (pause toggle, store handle, filter, status
    // label) ride on a dedicated `Rc<ViewerHandles>` captured by
    // the message-append closure in `window.rs::handle_dsp_message`.
    let handles = Rc::new(ViewerHandles {
        store,
        filter,
        filter_model,
        status_label: status_label.clone(),
        pause_button: pause_button.clone(),
        filter_entry: filter_entry.clone(),
    });
    *state.acars_viewer_handles.borrow_mut() = Some(handles);

    window
}

/// Per-viewer handles needed by the `DspToUi::AcarsMessage`
/// append site in `window.rs::handle_dsp_message`. Stored on
/// `AppState` (a sibling field of `acars_viewer_window`) so the
/// append site can fetch them without re-walking the widget
/// tree. Cleared on the window's close-request.
pub struct ViewerHandles {
    pub store: gtk4::gio::ListStore,
    pub filter: gtk4::CustomFilter,
    pub filter_model: gtk4::FilterListModel,
    pub status_label: gtk4::Label,
    pub pause_button: gtk4::ToggleButton,
    pub filter_entry: gtk4::SearchEntry,
}

fn render_time(msg: &sdr_acars::AcarsMessage) -> String {
    use std::time::SystemTime;
    let dt: chrono::DateTime<chrono::Local> = SystemTime::from(msg.timestamp).into();
    dt.format("%H:%M:%S").to_string()
}
fn render_freq(msg: &sdr_acars::AcarsMessage) -> String {
    format!("{:.3}", msg.freq_hz / 1_000_000.0)
}
fn render_aircraft(msg: &sdr_acars::AcarsMessage) -> String {
    msg.aircraft.to_string()
}
fn render_mode(msg: &sdr_acars::AcarsMessage) -> String {
    (msg.mode as char).to_string()
}
fn render_label(msg: &sdr_acars::AcarsMessage) -> String {
    let raw = std::str::from_utf8(&msg.label).unwrap_or("??").to_string();
    match sdr_acars::label::lookup(msg.label) {
        Some(name) => format!("{raw} ({name})"),
        None => raw,
    }
}
fn render_block(msg: &sdr_acars::AcarsMessage) -> String {
    (msg.block_id as char).to_string()
}
fn render_text(msg: &sdr_acars::AcarsMessage) -> String {
    msg.text.clone()
}
```

> **Implementer note:** The `state.acars_viewer_handles` field doesn't exist yet — add it to `AppState` alongside `acars_viewer_window` in this task's commit. The field type is `RefCell<Option<Rc<ViewerHandles>>>`. The `Rc` wrap lets the close-request closure share with the message-append site without lifetime juggling. (We didn't add this in Task 1 because we wanted to discover the exact handle set we'd need by writing Task 8 first; that's idiomatic for greenfield viewers in this codebase.)
>
> Add to `state.rs` immediately after `acars_viewer_window`:
>
> ```rust
>     /// Per-viewer mutable handles (column-view store, filter,
>     /// status label, etc). `Some` only while a viewer window
>     /// is open. Set by `acars_viewer::build_acars_viewer_window`;
>     /// cleared by the window's close-request handler. Held in
>     /// `Rc` so the close-request closure and the message-append
>     /// site in `window.rs` can both reach it without lifetime
>     /// juggling.
>     pub acars_viewer_handles: RefCell<Option<Rc<crate::acars_viewer::ViewerHandles>>>,
> ```
>
> Initialize in `AppState::new_shared`: `acars_viewer_handles: RefCell::new(None),`.
>
> Update the close-request handler in `build_acars_viewer_window` to also clear `*state.acars_viewer_handles.borrow_mut() = None;` alongside the window slot.

- [ ] **Step 2: Add `chrono` to sdr-ui's deps if not already present**

```bash
grep "chrono" crates/sdr-ui/Cargo.toml
```

If not present, add to `[dependencies]`:

```toml
chrono.workspace = true
```

(It's already a workspace dep — used by `satellites_recorder.rs` etc. — so this only affects sdr-ui's per-crate imports.)

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -10
```

Expected: clean. The window builds in isolation; nothing fills the store yet.

- [ ] **Step 4: Add `Rc` import to state.rs if needed**

```bash
grep "use std::rc::Rc" crates/sdr-ui/src/state.rs
```

If absent, add to the `use` block at top of file.

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs crates/sdr-ui/src/state.rs crates/sdr-ui/Cargo.toml
git commit -m "feat(sdr-ui): viewer window structure (header + ColumnView shell)"
```

---

## Task 9: Pause/Resume button

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

The pause toggle, when active, suppresses appending new messages to the GListStore. Resume re-enables appending. Per spec: "pause buffers — see lifecycle below" — but rather than buffering pending messages and draining on resume, sub-project 3 just freezes the visible store and lets the bounded ring continue. On resume, new messages append from that point forward; the ring's existing contents stay (so the user doesn't lose anything they already saw).

This is simpler than the spec's "resume drains the gap" semantic and matches what users actually want from a pause button — freeze, then continue. If the resume-with-drain semantic is needed later, it can be added without breaking the AppState contract.

- [ ] **Step 1: Wire the pause toggle to a state cell**

The `pause_button` is already created in Task 8. It exposes `is_active()` to the message-append site (Task 12), which checks it before pushing into the store. No additional wiring needed here — Task 12 reads `handles.pause_button.is_active()` per message.

Keep this task as a deliberate no-op on the viewer side and instead, in the same commit, **document the pause semantic** with a doc comment near the button creation:

```rust
    let pause_button = gtk4::ToggleButton::builder()
        .icon_name("media-playback-pause-symbolic")
        .tooltip_text("Pause appending new messages (existing rows stay visible)")
        .build();
    // PAUSE SEMANTIC: when active, the message-append site in
    // `window.rs::handle_dsp_message` skips pushing into `store`.
    // The bounded ring (`AppState::acars_recent`) keeps growing
    // — pausing the view does NOT pause the DSP. Resume appends
    // from that point forward; we deliberately do NOT drain
    // gap messages from the ring (simpler + matches user
    // intuition; deferred-item issue if drain-on-resume is
    // wanted later).
```

- [ ] **Step 2: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -3
```

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "docs(sdr-ui): document pause-button semantic in viewer"
```

---

## Task 10: Clear button

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

Per spec: "empties both ListStore and AppState ring; doesn't disable ACARS".

- [ ] **Step 1: Wire the clear button**

In `build_acars_viewer_window`, after the existing close-request handler block, add (using `state` and `handles` already in scope — note `handles` was constructed in Task 8 just before being stashed; reorder if needed so `clear_button.connect_clicked` runs while you still have `handles` and `state`):

```rust
    {
        let state = Rc::clone(state);
        let handles = Rc::clone(&handles);
        clear_button.connect_clicked(move |_| {
            handles.store.remove_all();
            state.acars_recent.borrow_mut().clear();
            // Don't reset acars_total_count — that's the
            // running total since toggle-on, distinct from the
            // visible count. Status label refresh in Task 11
            // recomputes "filtered / total" from the now-empty
            // filter_model + total_count.
            handles.status_label.set_label("0 / 0 messages");
        });
    }
```

You'll need to move the `let handles = Rc::new(ViewerHandles { ... });` block UP (before the `connect_close_request` block) so both that closure and the new clear-button closure can `Rc::clone(&handles)`. Reorganize as needed — the structural goal is:

```rust
let handles = Rc::new(ViewerHandles { ... });
*state.acars_viewer_handles.borrow_mut() = Some(Rc::clone(&handles));

// Wire pause button (no-op — see Task 9 doc).

// Wire clear button — uses Rc::clone(&handles) + Rc::clone(state).
clear_button.connect_clicked(move |_| { ... });

// Wire close-request — uses Rc::clone(state) only (handles
// ref through state.acars_viewer_handles is fine).
window.connect_close_request(move |_| {
    *state.acars_viewer_window.borrow_mut() = None;
    *state.acars_viewer_handles.borrow_mut() = None;
    glib::Propagation::Proceed
});
```

- [ ] **Step 2: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "feat(sdr-ui): viewer Clear button (drops store + ring)"
```

---

## Task 11: Filter entry + status label

**Files:**
- Modify: `crates/sdr-ui/src/acars_viewer.rs`

Live substring filter on aircraft + label + text columns. Status label shows `<filtered> / <total>` count.

- [ ] **Step 1: Wire the filter**

In the same area as the clear button (i.e., where `handles` is in scope), add:

```rust
    // Filter: live substring match on aircraft + label + text.
    {
        let filter = handles.filter.clone();
        let entry = handles.filter_entry.clone();
        entry.connect_search_changed(move |entry| {
            let needle = entry.text().to_lowercase();
            let needle_str: String = needle.into();
            filter.set_filter_func(move |obj| {
                let Some(obj) = obj.downcast_ref::<AcarsMessageObject>() else {
                    return false;
                };
                let Some(msg) = obj.message() else {
                    return false;
                };
                if needle_str.is_empty() {
                    return true;
                }
                let needle = &needle_str;
                msg.aircraft.to_lowercase().contains(needle)
                    || std::str::from_utf8(&msg.label)
                        .map(|s| s.to_lowercase().contains(needle))
                        .unwrap_or(false)
                    || msg.text.to_lowercase().contains(needle)
            });
        });
    }
```

- [ ] **Step 2: Wire the status label refresh**

The status label needs to update on every store change (append from message-append site OR clear). Hook the `filter_model`'s `n-items` notify:

```rust
    // Status label: <filtered> / <total>. Re-evaluated on every
    // store change. `n-items` fires on append AND on filter
    // re-evaluation, so this catches both cases.
    {
        let status = handles.status_label.clone();
        let filter_model = handles.filter_model.clone();
        let store = handles.store.clone();
        let refresh = move || {
            let filtered = filter_model.n_items();
            let total = store.n_items();
            status.set_label(&format!("{filtered} / {total} messages"));
        };
        filter_model.connect_items_changed(move |_, _, _, _| refresh());
    }
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-ui/src/acars_viewer.rs
git commit -m "feat(sdr-ui): viewer filter entry + status label"
```

---

## Task 12: Append from `DspToUi::AcarsMessage` to viewer store

**Files:**
- Modify: `crates/sdr-ui/src/window.rs`

The viewer's GListStore needs to fill from incoming messages. The existing `AcarsMessage` arm in `handle_dsp_message` (window.rs:~1957) currently pushes to `state.acars_recent` only. Add the viewer-store push.

- [ ] **Step 1: Locate the existing AcarsMessage arm**

```bash
grep -n "DspToUi::AcarsMessage" crates/sdr-ui/src/window.rs
```

Should land around line 1957.

- [ ] **Step 2: Append to the viewer store after the ring push**

Modify the arm to:

```rust
        DspToUi::AcarsMessage(msg) => {
            // Bounded ring: pop oldest if at cap.
            let cap = crate::acars_config::default_recent_keep() as usize;
            let mut ring = state.acars_recent.borrow_mut();
            if ring.len() >= cap {
                ring.pop_front();
            }
            ring.push_back((*msg).clone());
            drop(ring);
            state
                .acars_total_count
                .set(state.acars_total_count.get().saturating_add(1));

            // Mirror to the viewer store if a viewer is open and
            // not paused. Sub-project 3 wires the store handles
            // through `AppState::acars_viewer_handles`. The pause
            // semantic per `acars_viewer.rs::build_acars_viewer_window`:
            // toggle active = skip append; ring keeps growing.
            if let Some(handles) = state.acars_viewer_handles.borrow().as_ref()
                && !handles.pause_button.is_active()
            {
                handles
                    .store
                    .append(&crate::acars_viewer::AcarsMessageObject::new((*msg).clone()));
            }

            tracing::trace!(
                "ACARS msg {} ({}, label {:?})",
                state.acars_total_count.get(),
                msg.aircraft.as_str(),
                msg.label
            );
        }
```

- [ ] **Step 3: Build**

```bash
cargo build -p sdr-ui --features whisper-cpu 2>&1 | tail -5
```

- [ ] **Step 4: Workspace clippy**

```bash
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -3
```

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-ui/src/window.rs
git commit -m "feat(sdr-ui): wire DspToUi::AcarsMessage → viewer GListStore"
```

---

## Task 13: Workspace gates

**Files:** none

- [ ] **Step 1: Run all tests**

```bash
cargo test --workspace --features sdr-transcription/whisper-cpu 2>&1 | grep -E "FAIL|test result.*[0-9]+ passed.*0 failed" | tail -10
```

Expected: every line ends `0 failed`.

- [ ] **Step 2: Workspace clippy**

```bash
cargo clippy --workspace --features sdr-transcription/whisper-cpu --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: clean. If there are new lints, fix in place — don't add `#[allow(...)]` without a `reason = "..."` string per the codebase convention.

- [ ] **Step 3: fmt check (last gate before push per `feedback_fmt_check_immediately_before_push.md`)**

```bash
cargo fmt --all -- --check 2>&1 | tail -3
```

Expected: silent. If anything is off, `cargo fmt --all` then commit as a `chore: cargo fmt` follow-up commit.

- [ ] **Step 4: `make lint` if practical**

```bash
make lint 2>&1 | tail -5
```

(Optional but matches the project's full-lint suite. Skip if it adds significant time and the per-tool gates above are clean.)

---

## Task 14: Manual GTK smoke (USER ONLY)

Per `feedback_smoke_test_workflow.md`: the user runs the GTK smoke test manually. Claude installs and provides the checklist; never launches the binary.

**Files:** none

- [ ] **Step 1: Build + install**

```bash
make install CARGO_FLAGS="--release --features whisper-cuda"
```

(Per `feedback_make_install_release_flag.md` — `--release` is required, and per `project_current_state.md` the user's daily driver is whisper-cuda.)

- [ ] **Step 2: Confirm the new binary contains the changes**

```bash
strings ~/.cargo/bin/sdr-rs 2>/dev/null | grep -E "Aviation|airplane-mode-symbolic|ACARS messages window" | head -3
```

Expected: at least one match. If empty, the install is stale.

- [ ] **Step 3: Provide the smoke checklist verbatim to the user**

```text
ACARS Aviation UI smoke checklist:

PHASE A — sidebar Aviation activity

1. Launch app. The left activity bar should show 8 icons now;
   the bottom one is an airplane (airplane-mode-symbolic).
   Hover tooltip: "Aviation (Ctrl+8)".
2. Click the airplane icon. The sidebar opens to the Aviation
   panel. Confirm:
   - "ACARS" group with "Enable ACARS" switch (off), Status row
     ("Disabled"), "ACARS messages window" row with "Open" button.
   - "Channels (US-6)" group with 6 placeholder rows (titles "—",
     subtitles "—") + a description showing the legend
     (● Locked  ○ Idle  ⚠ Signal-no-decode).
3. Press Ctrl+8 — same panel toggles via shortcut.

PHASE B — engage flow (RTL-SDR connected)

4. Plug in an RTL-SDR. Start the source (Play button).
5. Flip "Enable ACARS" to ON.
   - Tracing log: "ACARS engaged: airband lock active".
   - Source rate flips to 2.5 MSps, center to 130.337500 MHz
     (visible in Source / Header).
   - Status row subtitle updates to "Decoded N · ..." (~4 Hz).
   - Per-channel rows populate with frequencies (131.550 MHz,
     131.525, 130.025, 130.425, 130.450, 129.125) and glyphs.
   - VFO frequency selector / Tune controls grey out (engaged
     → DSP rejects geometry commands per round 14).
6. Wait for live aircraft messages (may take 30-300s depending
   on traffic). Channel rows should flip from ○ Idle to ●
   Locked as messages arrive; sidebar status row shows the
   running decoded count + "Last: Ns ago".

PHASE C — viewer window

7. Click the "Open" button in the ACARS group. A new floating
   window appears titled "ACARS", ~1100×600. Confirm:
   - Header bar: pause toggle, clear button, filter entry
     (placeholder "Filter aircraft / label / text…"), status
     label "0 / 0 messages" or "N / N messages" if any have
     already arrived.
   - Below: 7-column table — Time | Freq | Aircraft | Mode |
     Label | Block | Text. Headers click to sort? (Optional —
     default GtkColumnView behavior, low priority.)
8. As new messages arrive, rows should append at the bottom
   in real time. Verify the count in the status label updates.
9. Type a 4-character aircraft prefix (e.g. ".N12") in the
   filter. Rows filter live; status label changes to "X / Y".
   Clear the filter — full set returns.
10. Click the pause toggle. Verify new arriving messages do NOT
    append (count stays). Click pause again. New messages
    resume appending FROM THAT POINT (gap during pause is
    intentionally lost from the visible view; the bounded ring
    in AppState still has them).
11. Click clear. Both the visible table AND the AppState ring
    empty. Status: "0 / 0 messages".
12. Close the viewer window (X). Re-click Open from the panel.
    The viewer reopens fresh (count starts from 0 since clear).
13. Open the viewer, do NOT close it, click Open again. The
    existing window should `present()` (be brought forward),
    not spawn a duplicate.

PHASE D — disengage + cleanup

14. Flip "Enable ACARS" to OFF.
    - Source rate restores to whatever was set pre-engage.
    - Status row → "Disabled".
    - Channel rows still show last-known data (no auto-clear;
      next engage repopulates).
    - VFO controls re-enable.
15. Switch source type (e.g. to a Network source).
    - Tracing log: "ACARS auto-disabling: source type changing
      to non-RTL-SDR".
    - Toast appears: "ACARS: ..." (engage error from the
      source-type gate).

PHASE E — startup persistence

16. With ACARS engaged and source running, quit the app cleanly.
17. Re-launch.
    - Sidebar should remember the Aviation activity was the
      most-recent (per LEFT_ACTIVITIES persistence).
    - acars_enabled was persisted = true (from the engage); on
      DSP-ready signal, "ACARS startup-replay" log fires; ACARS
      re-engages.

PHASE F — error toast

18. Disconnect the RTL-SDR. Stop + restart the source. Engage
    the scanner (any channel). Now flip Enable ACARS ON.
    - DSP refuses with AcarsEnableError::ScannerActive.
    - Toast appears: "ACARS: ACARS cannot engage while the
      scanner is running".
    - Toggle does NOT persist as on (preserved-state per CR
      round 1).

What this smoke does NOT cover:
- Aircraft-grouped tab (deferred — issue #579).
- Multi-block ETB chaining (deferred — issue #580).
- Per-label structured field parsers (deferred — issue #577).
- International channel sets (deferred — issue #581).

Tail tracing logs: RUST_LOG=info or debug.
```

- [ ] **Step 4: Wait for user pass**

Per `feedback_smoke_test_workflow.md`: do NOT proceed to push until the user reports the smoke pass. If failures, loop fix → re-install → re-smoke per `feedback_pre_commit_cr_review.md`.

---

## Task 15: Final pre-push sweep + push

**Files:** none

- [ ] **Step 1: Final fmt check (LAST gate)**

```bash
cargo fmt --all -- --check 2>&1 | tail -3
```

- [ ] **Step 2: Branch state**

```bash
git status
git log --oneline main..HEAD | head -20
```

Expected: clean tree; ~12-14 commits ahead of main covering Tasks 0-12.

- [ ] **Step 3: Push**

```bash
git push -u origin feat/acars-aviation-panel 2>&1 | tail -5
```

- [ ] **Step 4: Open the PR**

PR title: `feat(sdr-ui): ACARS Aviation panel + viewer window (#474, sub-project 3)`. Body: 1-3 bullet summary + smoke-checklist mirror per the project's `gh pr create` workflow in CLAUDE.md.

- [ ] **Step 5: Wait for CodeRabbit**

Per `feedback_coderabbit_workflow.md`. Sub-project 3 closes epic #474 once merged.

---

## Spec coverage cross-check

| Spec section | Tasks |
|---|---|
| Sidebar registration (LEFT_ACTIVITIES entry) | Task 2 (with Ctrl+8 deviation noted) |
| AcarsPanel structure — Group 1 (toggle + status + open button) | Task 4 + Task 6 |
| AcarsPanel structure — Group 2 (channel rows + glyphs) | Task 4 (rows) + Task 6 (refresh wiring) |
| ACARS viewer window — header bar | Task 8 (structure) + Task 9 (pause) + Task 10 (clear) + Task 11 (filter) |
| ACARS viewer window — content (ColumnView, 7 columns) | Task 8 |
| ACARS viewer window — label name display (`H1 (Crew)`) | Task 8 (`render_label`) — falls back to raw code since `lookup` is still stubbed |
| Lifecycle: ring push + viewer append + sidebar refresh | Task 6 (sidebar) + Task 12 (viewer) |
| Lifecycle: pause/resume buffering | Task 9 (deliberately simpler than spec — documented deviation) |
| Persistence: `ui_sidebar_left_aviation_open` | NOT NEEDED — global `KEY_LEFT_OPEN` covers panel open/close (per Explore-agent finding); per-activity persistence is just `KEY_LEFT_SELECTED = "aviation"`. |
| Toast on engage failure | Task 3 |

If a spec requirement is missing from this table, stop and add a task before implementation.

---

## Implementation notes

- **Fresh subagent per task** is the recommended execution mode. Tasks 7-12 build on each other (all touching `acars_viewer.rs`), so each subagent should re-read the file at the top of its task to see the current state from prior commits.
- **GTK widget code is not unit-testable.** Tasks 4, 6, 7, 8, 9, 10, 11, 12 lean on the smoke checklist (Task 14) for verification. The only TDD-style test in this plan is the AppState defaults extension in Task 1 step 4. This is normal for GTK4 UI work in this codebase — the existing viewer modules (`apt_viewer.rs`, `lrpt_viewer.rs`) follow the same pattern.
- **No backwards-compat shims.** Per CLAUDE.md / port-fidelity memory: AcarsMessageObject is brand new, the AppState fields are new, the panel is new. Just build it; don't pre-feature-flag anything.
- **No deferred-item creep.** The deferred items list (issues #577-#582) explicitly carves out aircraft grouping, multi-block reassembly, per-label parsers, international channel sets, and ADS-B integration. If you find yourself reaching for any of those, stop and discuss with the user.
- **Pause semantic intentionally simpler than spec.** The spec's "resume drains the gap" semantic adds buffering complexity without obvious user value. Sub-project 3 ships the simpler "freeze visible / continue ring" semantic (documented inline in Task 9). If drain-on-resume becomes a real user request, it's an additive change.
