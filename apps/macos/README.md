# SDR-RS — SwiftUI macOS frontend

Native macOS app that drives the headless `sdr-core` engine via
the `sdr-ffi` C ABI. Ships as `sdr-rs.app` to match the Linux
binary name and the shared `com.sdr.rs.*` bundle / desktop IDs.

This is the eventual consumer of the SwiftUI/Metal epic in
`docs/superpowers/specs/2026-04-12-swift-ui-macos-epic-design.md`.

## Layout

```text
apps/macos/
├── Package.swift                 — SwiftPM root. Product name
│                                   `sdr-rs`; module name
│                                   `SDRMac` (Swift identifier
│                                   rules — hyphens not allowed
│                                   in target names).
├── README.md                     — (you are here)
├── Packages/
│   └── SdrCoreKit/               — typed Swift wrapper around sdr-ffi
├── SDRMac/                       — app source (module name)
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
│   │   └── SDRCommands.swift     — menu-bar commands
│   ├── Resources/
│   │   └── Info.plist            — bundle metadata (CFBundle* names
│   │                               here are user-facing `sdr-rs`)
│   └── Entitlements/
│       └── SDRMac.entitlements   — USB + audio output
├── SDRMacTests/                  — XCTest suite
└── scripts/
    ├── bundle-mac-app.sh         — dev-only .app wrapper
    └── make-app-icon.sh          — rasterize data/com.sdr.rs.svg → .icns
```

## Dev loop

From the repo root:

```bash
# Build release Rust + Swift + wrap into .app (default, use this
# for anything that touches live RTL-SDR audio — release is the
# only mode that keeps up with 2 MSps streaming on macOS).
make mac-app

# Launch the app
open apps/macos/build/sdr-rs.app
```

`make mac-app` runs `cargo build --workspace --release`
(workspace scope so the transcription backend feature unifies),
then `swift build -c release` inside `apps/macos/`, then calls
`scripts/bundle-mac-app.sh release` to ad-hoc sign and wrap the
binary into a minimal `.app`.

### Debug build

```bash
make mac-app-debug
```

Builds cargo + swift in debug mode. **This will NOT keep up with
live RTL-SDR streaming** — debug builds of the DSP chain drop
USB throughput to ~45% of configured source rate, which
produces garbled audio. Debug is only useful for non-streaming
iteration (UI layout, event wiring, config, lifecycle paths).

## Testing

```bash
# Run app-level unit tests (CoreModel)
cd apps/macos && swift test

# Run SdrCoreKit FFI integration tests
make swift-test
```

## Notes

- **The `.app` this produces is NOT shippable.** Ad-hoc signed,
  no Developer ID, no notarization. That's M6 via the production
  `scripts/build-mac.sh` pipeline and an Xcode project. This flow
  is purely for developer iteration on the SwiftUI code.
- **macOS deployment target is 14 (Sonoma)** per the epic spec —
  `@Observable`, `NavigationSplitView`, modern AsyncStream
  semantics all need 14+.
- **The linker search path in `SdrCoreKit/Package.swift`** computes
  an absolute path from `#filePath` at manifest-execution time.
  This lets the package be used both as a standalone (`swift test`
  in `Packages/SdrCoreKit/`) and as a local dep from
  `apps/macos/Package.swift`. See the Package.swift comments.
