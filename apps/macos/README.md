# SDRMac — SwiftUI macOS frontend

Native macOS app that drives the headless `sdr-core` engine via
the `sdr-ffi` C ABI. This is the eventual consumer of the
SwiftUI/Metal epic described in
`docs/superpowers/specs/2026-04-12-swift-ui-macos-epic-design.md`.

## Layout

```text
apps/macos/
├── Package.swift                 — SwiftPM root for the SDRMac app
├── README.md                     — (you are here)
├── Packages/
│   └── SdrCoreKit/               — typed Swift wrapper around sdr-ffi
├── SDRMac/                       — app source
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
│   │   └── Info.plist            — bundle metadata
│   └── Entitlements/
│       └── SDRMac.entitlements   — USB + audio output
├── SDRMacTests/                  — XCTest suite
└── scripts/
    └── bundle-mac-app.sh         — dev-only .app wrapper
```

## Dev loop

From the repo root:

```bash
# Build Rust static lib + Swift app + wrap into .app
make mac-app

# Launch the app
open apps/macos/build/SDRMac.app
```

`make mac-app` runs `cargo build --workspace` (workspace scope so
the transcription backend feature unifies), then `swift build`
inside `apps/macos/`, then calls `scripts/bundle-mac-app.sh` to
ad-hoc sign and wrap the binary into a minimal `.app`.

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
