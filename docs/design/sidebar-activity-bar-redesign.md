# Sidebar Activity-Bar Redesign

**Status:** Design-complete, implementation not started
**Author:** jasonherald
**Last updated:** 2026-04-23

## 1. Summary

Replace the single scrollable sidebar with a **VS Code-style activity-bar + slide-out panel** pattern. A narrow (≈44 px) strip of icons lives against the left edge of the window and switches which task-focused panel slides out next to it. The right side gets the same treatment — icon strip on the far-right edge, transcript panel slides out from there. Both activity bars stay visible at all times; the panels themselves are toggleable, resizable via drag, and remember their state across sessions.

**Why now.** The current sidebar is a single long scrollable stack of `AdwPreferencesGroup`s (source + audio + radio + display + scanner). Every new feature adds more vertical real estate, the user has to scroll to find settings, and "related" controls for one task (e.g. scanning) are mixed in with unrelated config (e.g. RTL-TCP server). Activity bars turn the sidebar into task views — one click lands the user in the right context with only the controls that matter for that task.

**Why the VS Code pattern specifically.** It's familiar, scales cleanly with feature growth (adding a sixth panel = adding a sixth icon, nothing else), and keeps the primary screen real estate (spectrum + waterfall) uncluttered. libadwaita doesn't ship an activity-bar widget natively, but the pattern is straightforward to build on top of `GtkBox` + `GtkToggleButton` + `GtkStack`.

**Scope for v1:** the left bar with five activities, the right bar with one activity (Transcript), resizable panels, session persistence. Narrow-screen "only one side open at a time" is explicitly out of scope → filed separately.

---

## 2. Layout

```
┌──┬──────────────────────────────────────────────┬──┐
│  │ Header bar: [▶] 146.520.000 Hz [NFM▾] [🔊]  │  │
│  ├─────────────────────────────────────────────┬┤  │
│🏠│ ┌──────────────────────────────────────┐    ││💬│
│▶ │ │ ▾ Band                               │    ││  │
│🎚│ │    [2m] [70cm] [FM] [Air] [NOAA]     │    ││  │
│📊│ │ ▾ Bookmarks                          │    ││  │
│🔄│ │    (list of stations)                │◀─▶ ││  │
│  │ │ ▾ Source                             │    ││  │
│  │ │    Device + Gain + Sample rate + ..  │    ││  │
│  │ └──────────────────────────────────────┘    ││  │
│  │                                              ││  │
│  │         SPECTRUM + WATERFALL                 ││  │
│  │                                              ││  │
│  │ Status bar: Level · SR · Demod · Freq · λ/2 ││  │
└──┴──────────────────────────────────────────────┴──┘
  ▲                  ▲                             ▲  ▲
  activity-bar-left  resize handle                 │  activity-bar-right
                                                   resize handle
```

### 2.1 Left activity bar (icons)

Order matters — most-common at top:

| Order | Icon         | Name     | Contains (see §3 for per-panel detail)                               | Shortcut   |
|-------|--------------|----------|-----------------------------------------------------------------------|------------|
| 1     | 🏠 `go-home` | General  | Band presets, bookmarks, source device + gain + sample-rate + etc.    | `Ctrl+1`   |
| 2     | ▶ `audio-input-microphone` or similar | Radio    | Demod mode, bandwidth, squelch, filters, deemphasis, IF-NR, AGC          | `Ctrl+2`   |
| 3     | 🎚 `audio-speakers` | Audio    | Audio sink, volume (persistent), network audio output                  | `Ctrl+3`   |
| 4     | 📊 `view-display` (or waterfall-ish)  | Display  | FFT size + window + rate, waterfall colormap, dB range, averaging mode | `Ctrl+4`   |
| 5     | 🔄 `view-refresh` (or scanner icon)   | Scanner  | Channel list, scan mode, priority, lockouts, timing                    | `Ctrl+5`   |

Exact icon choice is a sub-ticket decision — libadwaita ships a full GNOME symbolic icon set; we pick the clearest match per activity.

### 2.2 Right activity bar (icons)

| Order | Icon         | Name       | Contains                            | Shortcut         |
|-------|--------------|------------|-------------------------------------|------------------|
| 1     | 💬 `user-available-symbolic` or similar | Transcript | Live captions, model picker, VAD / Auto Break controls, display mode | `Ctrl+Shift+1`   |

One icon today. The strip exists as a single-icon strip so (a) the layout is symmetric with the left, (b) future right-side features add icons without another major redesign. Candidates for future right-side activities: Recordings browser, Event log, RadioReference search, Per-client rtl_tcp server stats, Favorites popover (currently in the header).

### 2.3 Activity-bar widget specifics

- **Width:** 44 px. Matches GTK header-bar button height so the icon is ~24 px with 10 px padding on each side — readable but narrow.
- **Icon size:** 16 px symbolic (GNOME convention).
- **Selected indicator:** libadwaita `accent` CSS class on the toggle button — same visual treatment as other selected-state controls in the app (Play button when running, role badge accent, etc.). Optionally add a 2 px vertical accent-colored bar on the inside edge of the button matching VS Code's "selected strip" idiom, which reinforces the "this is where you are" signal.
- **Tooltip on hover:** full activity name + keyboard shortcut (e.g., `"Radio (Ctrl+2)"`).
- **Hover effect:** standard GTK button hover — subtle background lightening.
- **Packing:** `GtkBox` orientation=vertical, spacing=0. Icons flush against the top of the bar, leaving bottom empty for future utility icons (settings gear? about? — not v1).

### 2.4 Panel slide-out

- Each activity's panel is a child of a `GtkStack` sibling to the activity bar.
- `StackTransitionType::None` (or `crossfade` if it feels abrupt) — we don't need slide animations because the panels are all at the same left edge, they just swap content.
- **Toggle behavior:** clicking the currently-selected icon collapses the panel entirely (sets the whole stack invisible). Clicking a different icon switches the stack's visible child without changing visibility.
- **Resize:** drag handle on the panel's inner edge (the edge away from the activity bar). Pattern mirrors the existing `transcript_scroll` `width_request` but with a user-draggable `GtkPaned`-style divider. Minimum width 220 px, maximum limited by window width minus the spectrum's minimum 320 px.
- **Scroll:** panel content is wrapped in a `GtkScrolledWindow` so long panels (Display with all FFT + colormap + averaging controls, for example) scroll internally without resizing the panel's width.

### 2.5 Panel internal layout

Each panel is a vertical stack of collapsible sections:

- **Widget:** `AdwExpanderRow` — the existing widget we already use for rtl_tcp server discovery, bookmarks slide-out, etc. Familiar look, already styled in the app's CSS.
- **Expanded-by-default:** yes, per the user's ask. Makes the default "everything is here and scannable" rather than "hunt for what you want" — collapse is still available for users who want to focus.
- **Collapse persistence:** remembered per-section in config. See §6.

---

## 3. Panel Inventory

Each section below enumerates what currently lives in today's sidebar and where it goes. Nothing is being cut; this is purely reorganization.

### 3.1 General (🏠)

- **Band preset buttons** — one-click tune to common band centres. Currently lives in the frequency-selector popover or header; we promote it into the General panel as a first-class section.
  - 2 m ham (144.0 MHz), 70 cm ham (435.0 MHz), FM broadcast (97.5 MHz), Airband (124.0 MHz), NOAA APT (137.5 MHz), HF 40m (7.2 MHz), HF 20m (14.2 MHz), plus an "Edit presets…" row for future custom presets.
  - Clicking a preset also updates the demod mode (`NFM` for 2m/70cm/Air/NOAA, `WFM` for FM, `USB` for HF by convention) so the user lands in a ready-to-listen state.
- **Bookmarks** — currently the header popover. Move the full list here as a collapsible section with inline Add / Edit / Delete. Header popover can stay as a quick-access duplicate (parallel UX, not mutually exclusive).
- **Source** — today's `SourcePanel` lives here. All of its sub-sections (device, gain, sample rate, IQ correction, decimation, rtl_tcp discovery) go into expander rows.
- **Last-connected / Discovered servers** — the rtl_tcp discovery expander moves in as a sub-section under Source.

### 3.2 Radio (▶)

- Demod mode (already in header; keep there, but also surface in this panel for muscle memory).
- Bandwidth `AdwSpinRow` + reset-to-default button.
- Squelch: threshold, enabled, auto-squelch, voice squelch (syllabic / SNR).
- Filters: noise blanker (on/off + level), FM IF NR, high-pass, notch filter (freq + enabled).
- Deemphasis mode combo.
- AGC selector (hardware / software / off) + gain slider (mutex-managed).
- CTCSS / DCS subaudible tone controls.

### 3.3 Audio (🎚)

- Audio output sink picker (local / network).
- Volume slider — fixes bug #419 (persist across sessions).
- Network audio sink: hostname, port, protocol, status row.
- Audio recording toggle + location.

### 3.4 Display (📊)

- FFT size combo.
- FFT window combo.
- FFT rate spin.
- Waterfall colormap combo.
- dB range (min / max) dual sliders.
- Averaging mode (None / PeakHold / RunningAvg / MinHold).
- Future: signal-history dB plot toggle, waterfall history depth.

### 3.5 Scanner (🔄)

- Master scanner enable switch.
- Channel list (AdwExpanderRow with each channel expandable for per-channel bandwidth / demod / CTCSS / priority overrides).
- Scan timing (min dwell, max dwell, re-hit grace, etc.).
- Active-channel label + lockout button (currently exists — stays here).
- Future: priority channel interrupt (#365), Phase 2 simultaneous monitor (#318), Phase 3 hybrid (#319).

### 3.6 Transcript (💬, right side)

Unchanged from today — just moved behind an activity-bar icon on the right edge. Contains:

- Enable/disable switch.
- Model picker (with live reload for sherpa models).
- Display mode (Live captions / Final only).
- VAD threshold slider.
- Auto Break enable + timing sliders (min-open, tail, min-segment).
- Transcript text view.
- Clear button.

---

## 4. Behavior

### 4.1 On launch

1. Read session state from config: which left activity was selected, which right activity was selected, whether each panel was open or collapsed, their pixel widths, which subsections inside each panel were expanded.
2. Set the corresponding activity-bar toggle buttons `active` to match, set the stacks' visible children, set the split-view reveal states, set the expander-row expanded states.
3. Fresh-install default: left = General selected + open, right = closed (no transcript by default; existing users will have their existing transcript-open state restored once the new persistence key lands).

### 4.2 Clicking a left activity icon

- **Different activity from currently selected:** set the new activity's toggle `active`, uncheck the previous, swap stack's visible child. Panel stays open; only the content changes.
- **Same activity (already selected):** toggle the panel between open and collapsed. Activity-bar icon stays selected (so re-opening doesn't hunt for the icon again).
- **Panel was collapsed, user clicks any icon:** opens the panel to that activity.

### 4.3 Right side

Mirrors the left exactly. Transcript is the only icon today; clicking it toggles the transcript panel open/closed. Adding a second future icon (say, Recordings) makes the single-icon-toggle case grow into full VS Code behavior.

### 4.4 Resize

- **Drag handle:** thin (4–6 px) invisible-but-interactive region on the panel's inner edge (right edge of the left panel, left edge of the right panel). `GtkGestureDrag` on that region, cursor changes to `col-resize` on hover.
- **Width clamp:** min 220 px (narrower than that makes even the smallest `AdwPreferencesGroup` wrap awkwardly), max = window width − 320 px (spectrum reserves 320 px minimum).
- **Writes persist on drag-end** (not on every pixel — config I/O is cheap but let's not thrash the disk).

### 4.5 Keyboard shortcuts

`Ctrl+1` through `Ctrl+5` → select left activities General / Radio / Audio / Display / Scanner. `Ctrl+Shift+1` → toggle right transcript. `Esc` when focus is in a panel → collapse that panel (close). All of these wire through `GtkShortcutController` on the window.

Existing shortcuts (F9 for sidebar toggle, etc.) remain — but F9's meaning shifts to "toggle the left panel" (since "the sidebar" is now plural). A future release can tune this.

### 4.6 Narrow-screen behavior (v1)

Activity bars always visible on both sides — they're only 44 px each so the 88 px total fits even at the 800 px breakpoint. Panels overlay the spectrum (same behavior as today's sidebar at narrow width). Nothing exotic.

**Out of v1 (filed separately):** on narrow screens, "only one side can be open at a time" — opening the right panel would force the left panel to close, and vice versa. That's a better narrow-screen UX but an extra state machine layer; easier to ship v1 without it.

---

## 5. Config Persistence

New keys under a fresh `ui.sidebar` namespace:

```
ui.sidebar.left.selected        = "general" | "radio" | "audio" | "display" | "scanner"
ui.sidebar.left.open            = true | false
ui.sidebar.left.width_px        = 320
ui.sidebar.left.expanded[<panel>][<section>] = true | false

ui.sidebar.right.selected       = "transcript"          # future-proofed for growth
ui.sidebar.right.open           = true | false
ui.sidebar.right.width_px       = 360
ui.sidebar.right.expanded[<panel>][<section>] = true | false
```

- **Default selected:** left = `"general"`, right = `"transcript"` (even though closed by default on fresh installs).
- **Default open:** left = `true` (user lands on General panel), right = `false` (Transcript opt-in).
- **Default widths:** left = 320 px (current sidebar width), right = 360 px (current transcript width).
- **Default expansion:** everything expanded on fresh install, per the user's ask.

Migration: on first launch against an existing config, read any legacy `sidebar.collapsed` key the app previously wrote, translate to `ui.sidebar.left.open = !collapsed`; then stop reading the legacy key. One-way migration; no parallel writes.

---

## 6. GTK4 / libadwaita Widget Mapping

| Region | Widget |
|--------|--------|
| Window root | `AdwApplicationWindow` (unchanged) |
| Toolbar view | `AdwToolbarView` with header bar (unchanged) |
| Main content box | **New** `GtkBox` orientation=horizontal, replacing the current `AdwOverlaySplitView` |
| Left activity bar | **New** `GtkBox` orientation=vertical, css_class=`"activity-bar"` |
| Left panel container | **New** `AdwOverlaySplitView` (left side) — sidebar = panel stack, content = spectrum+right |
| Left panel stack | **New** `GtkStack` — one child per activity |
| Right panel container | **New** `AdwOverlaySplitView` (right side, `sidebar_position=End`) |
| Right panel stack | **New** `GtkStack` — one child per right-activity |
| Right activity bar | **New** `GtkBox` orientation=vertical, css_class=`"activity-bar"` |
| Spectrum area | Existing `SpectrumHandle.widget` (unchanged) |
| Status bar | Existing `StatusBar.widget` (unchanged) |
| Each panel's scrolled content | `GtkScrolledWindow` > `AdwPreferencesPage` > N × `AdwPreferencesGroup` each wrapping sections as `AdwExpanderRow`s |

**Why double `AdwOverlaySplitView`:** libadwaita's split view gives us (a) free narrow-screen collapse-to-overlay behavior, (b) built-in `show-sidebar` property to toggle open/closed, (c) native resize if we use the split view's own divider, (d) muscle-memory consistency with the current UI. Nesting two of them (one left, one right) is the simplest way to get both sides resizable + collapsible without hand-rolling the split machinery.

**CSS additions** (new entries in `crates/sdr-ui/src/css.rs`):

```css
.activity-bar {
    background-color: alpha(@theme_bg_color, 0.95);
    border-right: 1px solid alpha(@borders, 0.4);   /* left bar only — right bar uses border-left */
    padding: 6px 2px;
}
.activity-bar button {
    min-width: 40px;
    min-height: 40px;
    padding: 8px;
    border-radius: 4px;
    margin: 2px 0;
}
.activity-bar button.accent {
    border-left: 2px solid @accent_color;           /* selected-strip idiom */
}
.activity-bar-right {
    border-left: 1px solid alpha(@borders, 0.4);
    border-right: none;
}
.activity-bar-right button.accent {
    border-left: none;
    border-right: 2px solid @accent_color;
}
```

---

## 7. Migration Strategy

This is a big refactor — touching `window.rs` is unavoidable but the *panel content* already exists as independent modules (`sidebar::source_panel`, `sidebar::radio_panel`, etc.). Strategy:

1. **Build the scaffolding first** (sub-ticket 1): the empty activity-bar widget, the empty stacks, the split-view nesting, the CSS. No functional panels yet — every activity icon loads a placeholder `Label("General coming soon")`. Ships behind no flag; it's just broken until panels land.
2. **Migrate panels one at a time** (sub-tickets 2–6): General first (since it's the default landing), then Radio, Audio, Display, Scanner. Each migration is its own PR with a standalone smoke test. After the last migration lands, the old single-sidebar build path gets deleted.
3. **Right side** (sub-ticket 7): transcript moves from the existing slide-out to the right-activity-bar stack. Single-icon strip; single panel.
4. **Persistence** (sub-ticket 8): config keys, migration from legacy key, restore on launch, write on change.
5. **Resize handles** (sub-ticket 9): the split-view's own dividers handle this if we use them; otherwise custom `GtkGestureDrag`. Decide during sub-ticket 1 (scaffolding) — if `AdwOverlaySplitView` dividers look right, skip the custom code.
6. **Narrow-screen single-panel constraint** (sub-ticket 10, follow-up epic): not v1.

Rolling the migration in panel-sized PRs instead of one giant refactor lets CodeRabbit review each chunk cleanly and gives smoke-test checkpoints along the way.

---

## 8. Out of Scope (v1)

- **Narrow-screen "only one side at a time" constraint** — filed as a follow-up ticket. Extra state machine layer; not needed to ship v1.
- **Reorder or hide activities** — v1 has a fixed 5-icon left bar + 1-icon right bar. A future "customize sidebar" preferences-dialog page can add reorder / hide later.
- **Activity bar on the bottom / top** — VS Code supports moving the panel; we don't need that flexibility yet.
- **Panel tabs** — VS Code's panels can themselves have tabs (e.g. Explorer tabs). Our panels are single-view; not needed.
- **Deep integration with GNOME's shell bar** — the activity bar is app-internal, not OS-level.
- **Bookmarks migration from the header popover** — keep the popover as a second entry point. v1 adds the Bookmarks section to the General panel but doesn't delete the popover.
- **Per-panel icon customization** — the icon set is picked in-spec and shipped as-is.

---

## 9. Risks

- **Screen real estate squeeze at narrow widths.** 88 px of activity-bar + 220 px minimum panel + 320 px minimum spectrum = 628 px minimum usable width. Below that, the existing `AdwBreakpoint`-driven overlay behavior kicks in and panels overlay the spectrum. Should be fine at 800 px (the existing breakpoint) but worth verifying with a `gtk4-app-preview` or similar narrow-window smoke test during sub-ticket 1.
- **Discoverability of collapsed panels.** If a user collapses the General panel and can't find their source picker, the activity-bar icon is right there, but it's a learned behavior. Mitigated by (a) panels start open on fresh install, (b) tooltip on hover.
- **Keyboard shortcut collisions.** `Ctrl+1` through `Ctrl+5` may collide with existing shortcuts (tab switching? numeric entry in frequency selector?). Audit during sub-ticket 1.
- **Focus management.** Tab-cycling through the now-nested widget tree needs to stay sensible — spectrum → status bar → panel → activity bar or similar. Verify on each panel migration.
- **Regression surface.** Any control that lives in the sidebar today is a candidate for "moved, user can't find it" feedback during smoke tests. Each sub-ticket's PR description should list every control touched so smoke tests are comprehensive.
- **Adwaita idiom vs VS Code idiom.** libadwaita users expect certain patterns (expander rows, preferences pages) — we're grafting a VS Code-shaped activity bar onto that. Risk is the hybrid feels off. Mitigation: stick to libadwaita widgets *inside* the panels; the activity bars themselves are the only net-new visual idiom.

---

## 10. Follow-up Tickets (after v1 ships)

- **Narrow-screen single-panel constraint** — opening the right panel closes the left, and vice versa, when window width < 800 px. Separate epic.
- **Customize sidebar** — user-reorderable + hideable activities via a preferences-dialog page.
- **Per-activity badge notifications** — e.g. a small dot on the Scanner icon when the scanner detected a new hit, or on Transcript when a new line landed while the panel was closed.
- **Second right-activity** — likely Recordings browser or Event log. Trivial addition once the scaffolding is in.
- **macOS port mirror** — SwiftUI version of the activity-bar redesign for the Mac app. Separate Mac-session ticket.

---

## 11. Implementation Epic (sub-tickets)

Filed under epic #420. Order = implementation order. Sub-issues: #421–#430.

1. **Activity-bar widget + scaffolding** (`sdr-ui/src/sidebar/activity_bar.rs` new module) — the `GtkBox` + `ToggleButton` + `GtkStack` wiring, double nested `AdwOverlaySplitView`, CSS additions, keyboard-shortcut wire-up, no panels yet (placeholder labels). Proof that the frame works.
2. **General panel** — first real panel. Band presets + Bookmarks + Source (migrated from today's `SourcePanel`). Landing activity, so it needs to look right before anything else.
3. **Radio panel migration** — existing `RadioPanel` content moves behind the Radio icon.
4. **Audio panel migration** — existing audio settings + volume persistence (#419 lands as part of this PR or just before).
5. **Display panel migration** — FFT + waterfall + colormap + averaging.
6. **Scanner panel migration** — existing scanner controls behind the Scanner icon.
7. **Right activity bar** — transcript panel moves behind the right icon. Also establishes the `ui.sidebar.right.*` config shape for future growth.
8. **Session persistence** — config keys, read on launch, write on change. Panel expansion state per section.
9. **Resize handles** — panel-edge drag-to-resize, width clamp, persistence. Decide early (in sub-ticket 1) whether `AdwOverlaySplitView`'s own divider suffices or we need custom gesture.
10. **Documentation + CLAUDE.md update** — project guide needs a "Sidebar architecture" section explaining the activity bar for future contributors.

Minimum viable ship: sub-tickets 1 through 8. Sub-ticket 9 can be deferred one release if the split-view divider is good enough at static widths. Sub-ticket 10 is a documentation refresh PR that can run parallel.

---

## 12. References

- [VS Code Activity Bar docs](https://code.visualstudio.com/docs/getstarted/userinterface#_activity-bar) — the pattern this spec references.
- [libadwaita `AdwOverlaySplitView`](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.OverlaySplitView.html) — the widget we use for the panel containers.
- [`GtkStack`](https://docs.gtk.org/gtk4/class.Stack.html) — panel content switching.
- [`AdwExpanderRow`](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/class.ExpanderRow.html) — collapsible sub-section widget.
- Related issue: #419 (volume persistence — should land alongside the Audio panel migration).
- Related pattern in-repo: the existing rtl_tcp discovery `AdwExpanderRow` + per-row action buttons is the closest in-app analogue for the expandable-section pattern the panels use.
