# SdrCoreKit

Swift wrapper around the hand-rolled `sdr-ffi` C ABI for the
`sdr-core` SDR engine. Lives in-tree at
`apps/macos/Packages/SdrCoreKit/` and is the only Swift consumer
of the C ABI declared in `include/sdr_core.h`.

The eventual SwiftUI macOS app (`apps/macos/SDRMac.xcodeproj`, M6)
depends on this package via a local path; no registry
distribution.

## Layout

```text
apps/macos/Packages/SdrCoreKit/
├── Package.swift
├── README.md                       (you are here)
├── Sources/
│   ├── sdr_core_c/                 C shim target (systemLibrary)
│   │   ├── module.modulemap
│   │   └── include/
│   │       └── sdr_core.h          symlink → /include/sdr_core.h
│   └── SdrCoreKit/                 Swift wrapper target
│       ├── SdrCore.swift           main class — create/destroy,
│       │                           typed commands, events stream,
│       │                           FFT pull
│       ├── SdrCoreError.swift      Swift Error type + checkRc helper
│       ├── SdrCoreEnums.swift      DemodMode / Deemphasis / FftWindow
│       └── SdrCoreEvent.swift      SdrCoreEvent enum, C → Swift
└── Tests/
    └── SdrCoreKitTests/
        └── SdrCoreTests.swift      lifecycle + event stream + FFT
```

The header under `Sources/sdr_core_c/include/sdr_core.h` is a
**symlink** to the repo-root `include/sdr_core.h` that sdr-ffi
also uses. Editing the real header at the repo root flows
through automatically; there's no copy to drift.

## Build

SwiftPM links against `libsdr_ffi.a`, which is produced by
`cargo build -p sdr-ffi`. The order matters — `swift build`
fails with "library not found for -lsdr_ffi" if the Rust side
hasn't been built first.

The one-command sequence from the repo root:

```bash
make swift-test
```

That target runs `cargo build --workspace` (for feature
unification with the transcription backend) and then
`swift test` in this directory. Use it instead of calling
`swift test` directly.

Manual equivalent:

```bash
cargo build --workspace
cd apps/macos/Packages/SdrCoreKit
swift build        # compile only
swift test         # compile + run the XCTest suite
```

## ABI contract

The source of truth for the C ABI is `include/sdr_core.h`
(hand-written). The drift linter `make ffi-header-check` runs
`cbindgen` against `crates/sdr-ffi/src/` and diffs the generated
signatures against the hand-written header; drift between the
two fails CI rather than the first Swift test call.

When adding a new FFI function:

1. Write the Rust `#[unsafe(no_mangle)] extern "C"` function in
   `crates/sdr-ffi/src/`.
2. Add its declaration to `include/sdr_core.h` by hand.
3. Add a Swift wrapper method on `SdrCore` in `SdrCore.swift`.
4. Run `make ffi-header-check` to verify the Rust / header
   signatures match.
5. Add a SwiftPM test exercising the new method.
