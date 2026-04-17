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
│   ├── ContentView.swift         — top-level NavigationSplitView
│   ├── Models/
│   │   └── CoreModel.swift       — @Observable model wrapping SdrCore
│   ├── Views/
│   │   ├── HeaderToolbar.swift   — play/stop, center frequency, mode
│   │   ├── SourceSection.swift   — RTL-SDR tuner sidebar panel
│   │   ├── RadioSection.swift    — demod/squelch/volume sidebar panel
│   │   ├── DisplaySection.swift  — FFT size/window/dB range sidebar
│   │   ├── CenterView.swift      — spectrum+waterfall placeholder (M4)
│   │   ├── StatusBar.swift       — bottom status strip
│   │   ├── SettingsView.swift    — Cmd-, settings scene
│   │   ├── SDRCommands.swift     — menu-bar commands
│   │   └── Formatters.swift      — shared display helpers (formatRate, …)
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
- **macOS deployment target is 14 (Sonoma)** per the epic spec —
  `@Observable`, `NavigationSplitView`, modern AsyncStream
  semantics all need 14+.
- **Xcode 16 or newer is required to open the project.** The
  `.xcodeproj` uses `objectVersion = 77` and the
  `PBXFileSystemSynchronizedRootGroup` ISA added in Xcode 16 so
  source files auto-sync from the filesystem without per-file
  UUID churn in the pbxproj. Xcode 15 and earlier cannot parse
  the file. If you need to support older Xcodes, we'd revert
  to `objectVersion = 63` and maintain explicit `PBXGroup`
  members — not a regression worth taking on unless someone
  actually hits it.
- **Mach-O deployment-target pin** lives in `.cargo/config.toml`
  (`MACOSX_DEPLOYMENT_TARGET = "14.0"`), matching the Xcode
  project's setting so the Rust static archive's object files
  and the Swift-linked host agree on the min-OS stamp.
