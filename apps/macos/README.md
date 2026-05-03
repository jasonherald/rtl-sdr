# SDR-RS — SwiftUI macOS frontend

Native macOS app that drives the headless `sdr-core` engine via
the `sdr-ffi` C ABI. Ships as `sdr-rs.app` to match the Linux
binary name and the shared `com.sdr.rs.*` bundle / desktop IDs.

This is the eventual consumer of the SwiftUI/Metal epic in
`docs/superpowers/specs/2026-04-12-swift-ui-macos-epic-design.md`.

## Layout

```text
apps/macos/
├── README.md                     — (you are here)
├── SDRMac.xcodeproj/             — Xcode project (shared pbxproj
│                                   under git; per-user
│                                   xcuserdata is .gitignored)
├── Packages/
│   └── SdrCoreKit/               — SwiftPM package: typed Swift
│                                   wrapper around sdr-ffi, used
│                                   by the Xcode project as a
│                                   local package dependency
├── SDRMac/                       — app source (Xcode module)
│   ├── SDRMacApp.swift           — @main App struct, Window/Settings scenes
│   ├── ContentView.swift         — top-level layout (HStack with
│   │                               activity bars + custom resize
│   │                               handles flanking the panels)
│   ├── Models/
│   │   ├── CoreModel.swift       — @Observable model wrapping SdrCore;
│   │   │                           also owns the sidebar session state
│   │   ├── BandPreset.swift      — quick-tune presets shown on the
│   │   │                           General panel
│   │   ├── Bookmark.swift        — bookmark record + decode helpers
│   │   ├── BookmarksStore.swift  — JSON-backed bookmark persistence
│   │   ├── AveragingMode.swift   — Display-panel averaging picker enum
│   │   ├── RtlTcpClientFavorite.swift — RTL-TCP client favorites store
│   │   ├── TranscriptionDriver.swift  — Apple SpeechAnalyzer pipeline
│   │   └── UsbHotplugMonitor.swift    — IOKit USB plug/unplug events
│   ├── Renderer/
│   │   ├── MetalSpectrumNSView.swift  — Metal hosted view + draw loop
│   │   ├── SpectrumRenderer.swift     — spectrum + waterfall renderer
│   │   ├── SpectrumWaterfallView.swift — SwiftUI wrapper
│   │   ├── Shaders.metal              — fragment + compute shaders
│   │   ├── Palettes.swift             — colormap tables
│   │   └── PowerModeObserver.swift    — low-power mode coalescing
│   ├── Views/
│   │   ├── ActivityBar/                — sidebar activity-bar pattern
│   │   │   ├── ActivityEntry.swift     — Left/RightActivity enums
│   │   │   ├── ActivityBarView.swift   — generic icon column
│   │   │   └── ActivityPanelHost.swift — switch routing each
│   │   │                                 activity → its panel view
│   │   ├── Panels/                     — one Form-based view per
│   │   │   │                             left activity
│   │   │   ├── GeneralPanelView.swift  — band presets + Source
│   │   │   ├── RadioPanelView.swift    — bandwidth/squelch/filters
│   │   │   ├── AudioPanelView.swift    — sink/volume/network/recording
│   │   │   ├── DisplayPanelView.swift  — FFT/waterfall/levels
│   │   │   └── ScannerPanelView.swift  — master switch + active + timing
│   │   ├── HeaderToolbar.swift         — play/stop, frequency, demod
│   │   ├── CenterView.swift            — spectrum + status host
│   │   ├── StatusBar.swift             — bottom status strip
│   │   ├── BookmarksPanel.swift        — right activity panel
│   │   ├── TranscriptionPanel.swift    — right activity panel
│   │   ├── SourceSection.swift         — Source rows reused on
│   │   │                                  the General panel
│   │   ├── RecordingSection.swift      — Recording rows reused on
│   │   │                                  the Audio panel
│   │   ├── RtlTcpServerSection.swift   — Share activity panel body
│   │   ├── SettingsView.swift          — Cmd-, settings scene
│   │   ├── SDRCommands.swift           — menu-bar commands
│   │   ├── BandwidthEntry.swift        — typed bandwidth field
│   │   ├── FrequencyDigitsEntry.swift  — 12-digit tuner display
│   │   ├── FrequencyAxis.swift         — axis labels
│   │   ├── SpectrumGridView.swift      — grid overlay
│   │   ├── VfoOverlayView.swift        — draggable VFO marker
│   │   ├── RadioReferenceDialog.swift  — RR search sheet
│   │   └── Formatters.swift            — shared display helpers
│   ├── Resources/
│   │   ├── Info.plist            — bundle metadata (CFBundle* names
│   │   │                           here are user-facing `sdr-rs`)
│   │   └── AppIcon.icns          — rasterized from data/com.sdr.rs.svg
│   └── Entitlements/
│       └── SDRMac.entitlements   — USB entitlement (non-sandbox)
├── SDRMacTests/                  — XCTest suite
└── scripts/
    └── make-app-icon.sh          — rasterize data/com.sdr.rs.svg → .icns
```

The `SDRMac` target builds the `sdr-rs.app` bundle — the target
name is `SDRMac` (matches the Swift module name; Swift forbids
hyphens in module identifiers), but `PRODUCT_NAME = sdr-rs` in
the build settings so the output Mach-O and `.app` wrapper are
named `sdr-rs` to match the Linux binary and
`com.sdr.rs.*` bundle / desktop IDs.

## Dev loop

From the repo root:

```bash
# Build release Rust + Xcode app
make mac-app

# Launch the app (use `open`, not the binary directly — a
# proper .app bundle needs LaunchServices to bootstrap the
# SwiftUI window-server connection)
open apps/macos/build/sdr-rs.app
```

`make mac-app` runs `cargo build --workspace --release`
(workspace scope so the transcription backend feature unifies
— see the `swift-test` Makefile target for the rationale), then
`xcodebuild -configuration Release` to produce a real `.app`
bundle with automatic asset compilation, `.metal →
default.metallib`, and ad-hoc codesign. The finished bundle is
copied to `apps/macos/build/sdr-rs.app` for easy launching.

### Debug build

```bash
make mac-app-debug
```

Runs the same pipeline with `cargo build` (debug) +
`xcodebuild -configuration Debug`. **Not useful for live
RTL-SDR streaming** — debug builds of the DSP chain drop USB
throughput to ~45% of configured source rate, producing
garbled audio. Only useful for non-streaming iteration (UI
layout, event wiring, config, lifecycle paths).

### Working inside Xcode directly

Open `apps/macos/SDRMac.xcodeproj` in Xcode. Before running,
either run `cargo build --workspace --release` (for release)
or `cargo build --workspace` (for debug) once to produce
`target/{debug,release}/libsdr_ffi.a`. The Xcode build then
links against whichever matches the scheme configuration.

SwiftUI previews work as long as `libsdr_ffi.a` exists — the
SdrCoreKit package's `linkerSettings` derive the path from
`#filePath` at manifest-execution time.

## Testing

```bash
# App-level unit tests via xcodebuild test
make mac-test

# SdrCoreKit FFI integration tests (standalone SwiftPM)
make swift-test
```

## Notes

- **The `.app` this produces is NOT shippable.** Ad-hoc signed,
  no Developer ID, no notarization. That's M6 (production
  signing + notarization + stapling + GitHub Actions).
- **macOS deployment target is 26 (Tahoe).** Bumped from the
  original 14 floor for the Apple-native transcription pipeline
  (`SpeechAnalyzer` + `SpeechTranscriber`, both new in macOS 26).
  Set in `Packages/SdrCoreKit/Package.swift` (`.macOS(.v26)`)
  and `MACOSX_DEPLOYMENT_TARGET = 26.0` in the Xcode project.
  The Rust side stays on `MACOSX_DEPLOYMENT_TARGET = "14.0"` in
  `.cargo/config.toml` — Rust has no macOS-26-only deps, so
  keeping its archive 14-compatible costs nothing and means the
  same `libsdr_ffi.a` would link against an older Swift host
  if we ever needed to.
- **Xcode 16 or newer is required to open the project.** The
  `.xcodeproj` uses `objectVersion = 77` and the
  `PBXFileSystemSynchronizedRootGroup` ISA added in Xcode 16 so
  source files auto-sync from the filesystem without per-file
  UUID churn in the pbxproj. Xcode 15 and earlier cannot parse
  the file. If you need to support older Xcodes, we'd revert
  to `objectVersion = 63` and maintain explicit `PBXGroup`
  members — not a regression worth taking on unless someone
  actually hits it.
- **Mach-O deployment-target pin** for the Rust archive lives in
  `.cargo/config.toml` (`MACOSX_DEPLOYMENT_TARGET = "14.0"`).
  The Swift host's pin is higher (26.0, see above) — that's
  fine because the linker honors the higher of the two.

## Sidebar architecture (macOS)

The macOS app uses the same VS Code-style activity-bar pattern as
the GTK frontend (epic [#441](https://github.com/jasonherald/rtl-sdr/issues/441),
GTK parallel [#420](https://github.com/jasonherald/rtl-sdr/issues/420)):
narrow icon strips on each window edge switch the adjacent panel
between "activities". Left bar hosts General / Radio / Audio /
Display / Scanner / Share; right bar hosts Transcript / Bookmarks.
See [`docs/design/sidebar-activity-bar-redesign.md`](../../docs/design/sidebar-activity-bar-redesign.md)
for the full design rationale — the same doc applies to both
frontends since the pattern was designed cross-platform.

### Key files

- `apps/macos/SDRMac/Views/ActivityBar/ActivityEntry.swift` — the
  `ActivityEntry` protocol plus the canonical `LeftActivity` and
  `RightActivity` enums (single source of truth for icon SF
  Symbol + display name + `⌘N` shortcut index + persistence
  string). Each case's `rawValue` is the persistence key and
  matches the Linux `ActivityBarEntry.name` field exactly.
- `apps/macos/SDRMac/Views/ActivityBar/ActivityBarView.swift` —
  generic `View` over `ActivityEntry` rendering the 44 pt icon
  column. Click semantics mirror the GTK
  `wire_activity_bar_clicks`: clicking a different icon swaps
  selection AND opens the panel; clicking the active icon
  toggles the panel while keeping the selection.
- `apps/macos/SDRMac/Views/ActivityBar/ActivityPanelHost.swift` —
  `LeftPanelHost` / `RightPanelHost` switch on the active
  activity and route to the matching panel view.
- `apps/macos/SDRMac/ContentView.swift` — top-level layout. An
  `HStack` nests the two activity bars around two conditional
  panel slots and the center column. The custom resize handle
  lives here as a private `resizeHandle(side:)` helper —
  `Color.white.opacity(0.001)` for hit-testability +
  `NSCursor.resizeLeftRight.push()` on hover + `DragGesture`
  for drag + `.onTapGesture(count: 2)` for double-click reset.
  See the [SwiftUI `Color.clear` hit-test](#swift-side-gotchas)
  note below.
- `apps/macos/SDRMac/Models/CoreModel.swift` — owns the sidebar
  session state (`sidebarLeftSelected` / `_Open` / `_Width` and
  the matching right-side trio). Setters write through to the
  shared `sdr-config` JSON via the `SdrCore.setConfig*` FFI
  surface (ABI 0.21+, [issue #449](https://github.com/jasonherald/rtl-sdr/issues/449)).

### Adding a new activity

1. Append a case to `LeftActivity` (or `RightActivity`) in
   `ActivityEntry.swift`. Keep existing cases' order + raw values
   stable — they're config keys read by the Linux side too.
   Match the SF Symbol pick to the Linux Adwaita icon by feel
   (the mapping is necessarily approximate; see the doc-comment
   table on `LeftActivity.systemImage` for the current picks).
   The `shortcutIndex` accessor must return the next free slot;
   `ActivityBarView` wires `⌘N` (left) or `⌘⇧N` (right) from it.
2. Add a panel view under `Views/Panels/` (one `Form`-of-
   `Section`s per the convention below) and route to it from
   the `switch` in `LeftPanelHost` / `RightPanelHost`.
3. If the panel needs new model state, add `@Observable`
   properties on `CoreModel` plus matching setters that write
   through to the FFI engine; reuse `setConfigString` /
   `setConfigBool` / `setConfigUInt32` for any config-persisted
   value so the state round-trips with the Linux side.

### Panel layout convention

Every left activity hosts a `Form` of `Section`s with `header:
Text(title)` + `footer: Text(short description).font(.caption)`.
We deliberately avoid `DisclosureGroup` for top-level sections —
matches the GTK decision to drop `AdwExpanderRow` since the
extra inset stacked on the group's own inset read cluttered.
"Always visible, scrollable" looks cleaner than "expanded by
default, collapsible." See `RadioPanelView.swift` for the
canonical example (five flat sections; conditional rows inside
each based on demod mode).

`SourceSection`, `RecordingSection`, and `RtlTcpServerSection`
are `View`-typed reusable bodies — older surfaces from before
the redesign that still slot cleanly into the per-activity
panels they belong to (`General` reuses `SourceSection`; `Audio`
reuses `RecordingSection`; `Share` is `RtlTcpServerSection`).

### Session persistence

Six config keys (three per side) live as static `let` constants
on `CoreModel`: `sidebarLeftSelectedKey` / `_OpenKey` /
`_WidthKey` plus the right-side trio. Values match the Linux
constants in `crates/sdr-ui/src/sidebar/activity_bar.rs`
(`ui_sidebar_{left,right}_{selected,open,width_px}`) so a user
running both frontends gets a consistent layout — the keys are
read from the same shared `sdr-config` JSON via the engine's
config FFI (ABI 0.21+, see [`include/sdr_core.h`](../../include/sdr_core.h)
and [`SdrCore.setConfigString`](Packages/SdrCoreKit/Sources/SdrCoreKit/SdrCore.swift)).

`CoreModel.bootstrap()` calls `loadSidebarSession()` after
constructing the `SdrCore` handle, restoring the persisted
values onto the observable properties before `ContentView`
first paints (no flash of default state). On change, the
`setSidebar*` setters update the observable AND push the new
value to the FFI; the `sdr-config` auto-save thread flushes
to disk on its tick.

### Resize behavior

Per-side clamp ranges live as static constants on `CoreModel`:
`sidebarLeftWidthRange = 220...640` and
`sidebarRightWidthRange = 360...840`, matching the Linux
constants. Defaults are `sidebarLeftDefaultWidth = 320` and
`sidebarRightDefaultWidth = 420` — the right is wider because
its panels (Transcript / Bookmarks) hold list rows that read
better with extra room. Drag clamps to the matching range
during the gesture; the model setter clamps again on commit so
a non-UI caller can't poison persistent state. Double-click on
the resize handle snaps that side to its default width.

The drag handle uses a custom 8 px hit target rather than
`HSplitView` or `NSSplitViewController` — see the [Swift-side
gotchas](#swift-side-gotchas) below for the rationale.

### Swift-side gotchas

The custom resize handle has two non-obvious lessons baked in
that are worth preserving when touching this code:

- **`Color.clear` is not hit-testable, even with
  `.contentShape(Rectangle())`.** SwiftUI's renderer treats
  `.clear` as "draws nothing AND skip hit-testing." For an
  invisible-but-hittable surface (drag handles, double-click
  targets), use `Color.white.opacity(0.001)` instead — visually
  identical, but the view draws (and therefore hits) normally.
  Any `opacity > 0` works; 0.001 stays inside the "no pixels
  drawn" optimization.
- **Don't reach for `NSSplitViewController` /
  `NSSplitViewItem` here.** Burned several iterations on
  [PR #500](https://github.com/jasonherald/rtl-sdr/pull/500)
  trying the "Apple-blessed" pattern via
  `NSViewControllerRepresentable`: `viewDidLoad` doesn't run
  until `controller.view` is first accessed (so the first
  `updateNSViewController` finds host controllers nil and every
  pane silently stays on `EmptyView()`); `sizeThatFits`
  returning `proposal.width ?? 0` collapses the view to 0×0
  on first paint; without `preferredThicknessFraction`,
  `NSSplitView`'s first-paint distribution gives the side
  panes their `maximumThickness` and pushes the center to
  negative width. The pure-SwiftUI custom-handle approach
  hits every spec acceptance criterion (cursor / drag / clamp
  / release-persist / double-click reset / Mac↔Linux config
  round-trip) without fighting the layout. `NSSplitViewController`
  belongs as a window's `contentViewController`, not embedded
  inside a SwiftUI hierarchy.

### Cross-references

- GTK contributor guide: [`CLAUDE.md` → Sidebar architecture](../../CLAUDE.md#sidebar-architecture).
  The top-level guide describes the GTK side of the same
  pattern. The two are close cousins; this document focuses on
  the Mac-specific bits (SwiftUI hosting, custom handle,
  Apple-native transcription).
- Design doc (cross-platform): [`docs/design/sidebar-activity-bar-redesign.md`](../../docs/design/sidebar-activity-bar-redesign.md).
- Tracking epic: [#441](https://github.com/jasonherald/rtl-sdr/issues/441)
  (Mac sidebar redesign). Per-sub-ticket land logs are in the
  closed PRs `#491` (scaffolding) → `#493` (General/Radio/Audio/
  Display) → `#497` (Scanner) → `#499` (session persistence) →
  `#500` (resize persistence). This doc closes #451.
