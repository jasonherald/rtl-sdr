// swift-tools-version:6.2
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
import Foundation

// Absolute path to the workspace `target/` directory.
//
// SwiftPM's `-L` linker search paths are resolved relative to the
// *build root* (the package being built), NOT to this Package.swift.
// A hardcoded relative like `../../../../target` only works when
// this package is the root (`swift test` run from
// `apps/macos/Packages/SdrCoreKit/`). Consumers — notably the
// `apps/macos/SDRMac.xcodeproj` project that references this
// package via `XCLocalSwiftPackageReference` — would resolve
// the same relative path against their own build root, which
// is wrong.
//
// Compute it from `#filePath` instead — that's this Package.swift's
// on-disk location — so the `-L` paths stay correct no matter
// which build host is consuming the package.
let workspaceTarget: String = {
    let me = URL(fileURLWithPath: #filePath)
    // #filePath → .../apps/macos/Packages/SdrCoreKit/Package.swift
    // Go up 4 levels to repo root, then into `target/`.
    let repoRoot = me
        .deletingLastPathComponent()  // SdrCoreKit
        .deletingLastPathComponent()  // Packages
        .deletingLastPathComponent()  // macos
        .deletingLastPathComponent()  // apps
        .deletingLastPathComponent()  // repo root
    return repoRoot.appendingPathComponent("target").path
}()

let package = Package(
    name: "SdrCoreKit",
    platforms: [
        // macOS 26 (Tahoe) floor. Bumped from macOS 14 for the
        // transcription panel (issue #314) which uses the
        // SpeechAnalyzer / SpeechTranscriber frameworks shipped
        // in macOS 26 (WWDC 2025). Apple renumbered all platform
        // versions to the release year at WWDC 2025, so macOS 26
        // is the post-Sequoia release; no backwards-compat path.
        .macOS(.v26),
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
                // Link the Rust static archive from the cargo
                // target dir that matches the SwiftPM build
                // configuration:
                //   - `swift build`          → cargo debug
                //   - `swift build -c release` → cargo release
                //
                // **Order matters** when the build host is Xcode
                // — Xcode's SwiftPM integration emits BOTH
                // `.when(configuration:)` branches to the linker
                // regardless of the active config (tested on
                // Xcode 26.4). The linker picks the first match,
                // so we list release FIRST. That way an Xcode
                // release build always picks up
                // `target/release/libsdr_ffi.a` (what we want for
                // live RTL-SDR streaming — debug Rust can't keep
                // up on any build host), and an Xcode debug build
                // also picks release Rust (fine — Rust debug
                // info isn't what anyone debugs here; debug
                // Swift host is what matters for UI iteration).
                //
                // Standalone `swift test` run from inside
                // `Packages/SdrCoreKit/` still honours `.when`
                // correctly — only the appropriate path is
                // emitted for that config.
                .unsafeFlags(
                    ["-L", "\(workspaceTarget)/release"],
                    .when(configuration: .release)
                ),
                .unsafeFlags(
                    ["-L", "\(workspaceTarget)/debug"],
                    .when(configuration: .debug)
                ),
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
