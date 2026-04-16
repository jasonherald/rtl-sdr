// swift-tools-version:5.9
//
// SdrCoreKit — Swift wrapper around the hand-rolled `sdr-ffi`
// C ABI for the sdr-core SDR engine.
//
// This package lives in-tree at `apps/macos/Packages/SdrCoreKit/`
// and is the only Swift consumer of the C ABI in `include/sdr_core.h`.
// The eventual SwiftUI app in `apps/macos/SDRMac.xcodeproj` depends
// on this package via a local path; no registry distribution
// (v1 is ship-from-git).
//
// ## Build model
//
// Swift code imports `sdr_core_c` (a systemLibrary target) which
// references the hand-written C header. The Rust side of the FFI
// must be built *before* `swift build` runs:
//
//     cargo build --release -p sdr-ffi
//
// That produces `target/release/libsdr_ffi.a`. The linker settings
// below point at the workspace's `target/debug/` and
// `target/release/` directories via `unsafeFlags` so the Swift
// target can find the static archive. `unsafeFlags` is the SwiftPM
// escape hatch for build settings that can't be expressed via the
// normal typed API — in exchange SwiftPM won't allow this package
// to be published to a registry, which is fine because we're
// in-tree only.
//
// For dev workflow, the repo-root `Makefile` has a `swift-test`
// target that runs `cargo build -p sdr-ffi` first and then
// `swift test` in this package directory. Running `swift test`
// directly (without building the Rust side first) will fail at
// the link step with a "library not found for -lsdr_ffi" error.

import PackageDescription

// Relative path from this Package.swift (which lives at
// `apps/macos/Packages/SdrCoreKit/`) back to the workspace
// `target/` directory. Count up seven levels: Package.swift →
// SdrCoreKit → Packages → macos → apps → repo-root → target.
// Wait — SdrCoreKit IS the directory containing Package.swift,
// so we count from there up. Six levels up from
// Packages/SdrCoreKit/ gets us to repo-root, then `target`.
let workspaceTarget = "../../../../target"

let package = Package(
    name: "SdrCoreKit",
    platforms: [
        // macOS 26 floor per the epic spec. Locks in the
        // minimum OS for modern SwiftUI / @Observable /
        // latest AsyncStream semantics.
        .macOS(.v14),
    ],
    products: [
        .library(
            name: "SdrCoreKit",
            targets: ["SdrCoreKit"]
        ),
    ],
    targets: [
        // C-side shim: a systemLibrary target whose headers come
        // from `Sources/sdr_core_c/include/sdr_core.h` (a symlink
        // to the repo-root `include/sdr_core.h` that sdr-ffi also
        // uses). A module.modulemap makes the C surface importable
        // as `sdr_core_c` from Swift code.
        .systemLibrary(
            name: "sdr_core_c",
            path: "Sources/sdr_core_c"
        ),

        // Swift wrapper. Imports the C shim and re-exports a
        // typed Swift API (actor, AsyncStream, closure-based FFT
        // pull, throwing wrappers over the C error codes).
        .target(
            name: "SdrCoreKit",
            dependencies: ["sdr_core_c"],
            linkerSettings: [
                // Debug build search path. `cargo build -p sdr-ffi`
                // writes libsdr_ffi.a here.
                .unsafeFlags([
                    "-L", "\(workspaceTarget)/debug",
                    "-L", "\(workspaceTarget)/release",
                ]),
                // Link the static archive.
                .linkedLibrary("sdr_ffi"),
                // libc++ — whisper.cpp (pulled in transitively via
                // sdr-transcription's whisper-cpu default backend)
                // is C++, and ggml / whisper.cpp use a handful of
                // libc++ symbols that don't come for free from a
                // pure-Rust static lib. whisper-rs-sys's build.rs
                // emits `cargo:rustc-link-lib=dylib=c++` which flows
                // into libsdr_ffi.a's link metadata, but Swift
                // doesn't see that — we re-state it here so the
                // final binary links against /usr/lib/libc++.dylib.
                .linkedLibrary("c++"),
                // Accelerate — whisper.cpp's ggml uses vDSP and
                // cblas routines from the Accelerate framework for
                // vector math on macOS. Same situation as libc++:
                // whisper-rs-sys emits a framework link directive
                // which is honored in a Rust binary link but not
                // propagated through to a Swift consumer of our
                // static archive.
                .linkedFramework("Accelerate"),
                // macOS system frameworks that libsdr_ffi pulls in
                // transitively via sdr-sink-audio (CoreAudio on
                // this target). Declaring them explicitly here
                // means the linker finds the symbols even though
                // they're not part of the Rust-side dep graph
                // from Swift's perspective.
                .linkedFramework("CoreAudio"),
                .linkedFramework("AudioUnit"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreFoundation"),
                // libusb via `rusb` needs IOKit on macOS for USB
                // device enumeration.
                .linkedFramework("IOKit"),
                // Security framework for TLS (rustls / reqwest
                // transitive from sdr-radioreference).
                .linkedFramework("Security"),
                // SystemConfiguration for reqwest network config.
                .linkedFramework("SystemConfiguration"),
            ]
        ),

        // Tests exercising SdrCore lifecycle, command dispatch,
        // and event AsyncStream consumption end-to-end against
        // the real static library.
        .testTarget(
            name: "SdrCoreKitTests",
            dependencies: ["SdrCoreKit"]
        ),
    ]
)
