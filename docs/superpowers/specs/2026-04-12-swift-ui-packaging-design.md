---
name: SwiftUI Packaging, Signing, and CI — Design
description: Xcode project layout under apps/macos, Rust static-lib build integration, universal binary, codesign + notarization, USB entitlements, GitHub Actions matrix
type: spec
---

# SwiftUI Packaging, Signing, and CI — Design

**Status:** Draft
**Date:** 2026-04-12
**Parent epic:** `2026-04-12-swift-ui-macos-epic-design.md`
**Depends on:** `2026-04-12-sdr-ffi-c-abi-design.md`, `2026-04-12-swift-ui-surface-design.md`
**Tracking issues:** TBD

---

## Goal

Define how the SwiftUI app gets *built*, *signed*, *notarized*, and *shipped* so that a user on a clean macOS 26 machine can download `SDRMac.app`, drag it to `/Applications`, plug in an RTL-SDR dongle, and run it. Plus the GitHub Actions setup that produces a notarized `.app` on every release tag.

This is the last milestone in the epic because everything else needs to exist before there's a `.app` to sign.

## Non-Goals

- **Mac App Store distribution.** Sandboxing limits USB device access in ways we don't want to fight in v1. Direct download via developer-ID + notarization is the path.
- **Sparkle / auto-update.** Out of scope for v1. Users redownload from the GitHub release page. Sparkle integration is a v2 issue.
- **Linux packaging changes.** This spec is macOS-only. Existing Linux packaging (Cargo, .desktop file, etc.) stays as-is.
- **Homebrew cask.** v2. Trivial to add once notarized binaries exist on GH releases.
- **Universal2 binary that includes Linux.** Not a thing. macOS only.
- **A second build system on top of Cargo.** We use Cargo to build Rust, Xcode to build Swift, and a single shell script (`scripts/build-mac.sh`) to glue them. No CMake, no Bazel, no xcodegen, no Tuist.

## Background

What ships:

```text
SDRMac.app/
├── Contents/
│   ├── Info.plist
│   ├── PkgInfo
│   ├── MacOS/
│   │   └── SDRMac                       (Mach-O universal: arm64 + x86_64)
│   ├── Resources/
│   │   ├── Assets.car                   (compiled asset catalog)
│   │   ├── AppIcon.icns
│   │   └── ...
│   ├── Frameworks/                      (empty in v1 — static lib, no embedded dylibs)
│   └── _CodeSignature/
│       └── CodeResources
```

The `SDRMac` binary statically links `libsdr_core.a`, so there's no `Frameworks/libsdr_core.dylib` to deal with — one binary, one signature, one notarization submission. This is the entire reason the FFI spec picked `staticlib` for v1.

## Repository Layout

```text
apps/macos/
├── README.md                            — quick "how to build" for contributors
├── Package.swift                        — SwiftPM root for SdrCoreKit (wrapper lib)
├── Packages/
│   └── SdrCoreKit/
│       ├── Package.swift
│       ├── Sources/
│       │   ├── sdr_core_c/              — systemModule wrapping include/sdr_core.h
│       │   │   └── module.modulemap
│       │   └── SdrCoreKit/              — Swift wrappers
│       │       └── *.swift
│       └── Tests/
│           └── SdrCoreKitTests/
├── SDRMac.xcodeproj/                    — Xcode project
│   └── project.pbxproj
├── SDRMac/                              — app source
│   ├── SDRMacApp.swift                  — @main
│   ├── ContentView.swift
│   ├── Models/
│   │   └── CoreModel.swift
│   ├── Views/
│   │   ├── HeaderToolbar.swift
│   │   ├── SourceSection.swift
│   │   ├── RadioSection.swift
│   │   ├── DisplaySection.swift
│   │   ├── SpectrumWaterfallView.swift  — NSViewRepresentable
│   │   ├── FrequencyScaleOverlay.swift
│   │   └── StatusBar.swift
│   ├── Renderer/                        — Metal pipeline (M4 output)
│   │   ├── SpectrumMTKView.swift
│   │   ├── Shaders.metal
│   │   └── Palettes.swift
│   ├── Resources/
│   │   ├── Assets.xcassets/
│   │   └── Info.plist
│   └── Entitlements/
│       └── SDRMac.entitlements
└── SDRMacTests/
    └── *Tests.swift

scripts/
├── build-mac.sh                         — builds Rust libs + drives xcodebuild
├── sign-mac.sh                          — codesign + notarytool
└── release-mac.sh                       — build + sign + notarize + staple

include/
└── sdr_core.h                           — same file the FFI spec produces

target/                                  — Cargo output
```

The Xcode project is checked in as a real `.xcodeproj` (not generated). Reasoning: contributors will open it daily, the project file rarely changes, and the projects-from-yaml ecosystem (xcodegen / Tuist) adds an external tool that has to be installed before you can do *anything*. We accept the merge-friction tax of the binary `.pbxproj` because the alternative is worse for newcomers.

## Build Pipeline

A single shell script does the entire build, callable from both developer machines and CI.

### `scripts/build-mac.sh`

```bash
#!/usr/bin/env bash
# Builds the macOS app end-to-end. Idempotent. Used by both devs and CI.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${REPO}/target"
LIPO_LIB="${TARGET_DIR}/universal/release/libsdr_core.a"

CONFIG="${1:-Debug}"        # Debug | Release

# 1. Build the Rust static lib for both architectures.
for triple in aarch64-apple-darwin x86_64-apple-darwin; do
    echo "==> cargo build --target ${triple}"
    rustup target add "${triple}" >/dev/null
    cargo build --release --package sdr-ffi --target "${triple}"
done

# 2. lipo into a universal static lib that Xcode can link.
mkdir -p "$(dirname "${LIPO_LIB}")"
lipo -create \
    "${TARGET_DIR}/aarch64-apple-darwin/release/libsdr_core.a" \
    "${TARGET_DIR}/x86_64-apple-darwin/release/libsdr_core.a" \
    -output "${LIPO_LIB}"

# 3. Build the Xcode project. The project's "Library Search Paths" build setting
#    points at $(SRCROOT)/../../target/universal/release.
xcodebuild \
    -project "${REPO}/apps/macos/SDRMac.xcodeproj" \
    -scheme SDRMac \
    -configuration "${CONFIG}" \
    -destination 'generic/platform=macOS' \
    -derivedDataPath "${TARGET_DIR}/xcode" \
    SDR_CORE_LIB_PATH="${LIPO_LIB}" \
    build
```

Why this is *not* an Xcode "Run Script Build Phase": pre-build phases that shell out to Cargo make Xcode's incremental builds slow, confuse the index store, and break Live Issues. Putting the cargo invocation in an outer script means you run `./scripts/build-mac.sh Debug` once after editing Rust, then iterate inside Xcode normally for Swift changes. CI just calls the same script.

The script *is* idempotent — if neither architecture's lib is out of date, cargo no-ops, lipo runs in milliseconds, and Xcode does an incremental Swift build.

> **Cross-compilation note:** building `x86_64-apple-darwin` from an Apple Silicon Mac requires only the rustup target — no separate toolchain. The `coreaudio-sys` build script and any other native deps work fine because Apple's `clang` is universal by default.

### Xcode Build Phase: Universal Library Selection

In `SDRMac.xcodeproj`, the `SDRMac` target's Build Settings include:

```text
LIBRARY_SEARCH_PATHS = $(SDR_CORE_LIB_PATH:dir)
OTHER_LDFLAGS        = -lsdr_core -lc++ -framework AudioUnit -framework CoreAudio -framework AudioToolbox -framework Metal -framework MetalKit
HEADER_SEARCH_PATHS  = $(SRCROOT)/../../include
ARCHS                = arm64 x86_64
ONLY_ACTIVE_ARCH     = NO     (for Release; YES for Debug to keep dev iteration fast)
```

`SDR_CORE_LIB_PATH` is injected by `build-mac.sh`. When developers open Xcode directly without running the script first, Xcode falls back to a sensible default (`$(SRCROOT)/../../target/universal/release/libsdr_core.a`) and produces a clear "library not found" error if the script hasn't been run yet, with a hint pointing at the script.

The `-lc++` is required because some `coreaudio-rs` symbols pull in C++ runtime bits (small, unavoidable). `-framework Metal -framework MetalKit` is needed for the renderer. `AudioUnit` / `CoreAudio` / `AudioToolbox` are needed by `libsdr_core.a` (CoreAudio sink).

## Info.plist & Entitlements

### `Info.plist` highlights

```xml
<key>CFBundleIdentifier</key>
<string>com.jasonherald.sdrmac</string>
<key>LSMinimumSystemVersion</key>
<string>26.0</string>
<key>NSHumanReadableCopyright</key>
<string>© 2026 Jason Herald</string>
<key>NSPrincipalClass</key>
<string>NSApplication</string>
<key>NSHighResolutionCapable</key>
<true/>
```

No USB-specific Info.plist key is required. `NSAppleEventsUsageDescription` controls Apple Events automation (cross-app scripting), not USB device access — it would be wrong here. There is no `NSUSBUsageDescription` key in the macOS API; USB device access for non-sandboxed apps doesn't go through a usage-description prompt at all.

### `SDRMac.entitlements`

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <!-- We are NOT sandboxed in v1. -->
    <!-- (No <com.apple.security.app-sandbox/> entry.) -->
</dict>
</plist>
```

The entitlements file is **deliberately empty** for v1. Reasoning:

- **`com.apple.security.device.usb`** is a *sandbox* entitlement. It only does anything when paired with `com.apple.security.app-sandbox`. We are not sandboxed in v1, so requesting it would be cargo-cult: it grants nothing additional and adds noise to the entitlement audit. We omit it.
- **`com.apple.security.cs.allow-jit`**, **`allow-unsigned-executable-memory`**, **`disable-library-validation`** all default to `false` under the hardened runtime. Spelling them out as `<false/>` is a no-op — Apple's documentation is explicit that the absence of these keys is the secure default, and adding `<false/>` entries doesn't strengthen anything. We omit them too.
- **Hardened runtime itself** is not an entitlements-file thing — it's a `codesign --options runtime` flag. The entitlements file does not need to declare it. The signing script (`scripts/sign-mac.sh` below) sets it.

If a future feature actually needs an entitlement (e.g., the v2 Mac App Store path needs `com.apple.security.app-sandbox` + `com.apple.security.device.usb`; or loading external dylibs at runtime would need `disable-library-validation`), it gets added then with a comment explaining the specific requirement. Until then, the file stays empty.

**Why not sandboxed:** sandbox + USB device access is a deep rabbit hole on macOS. Apps that need raw libusb access typically either go unsandboxed (our v1 choice) or use the `IOUSBHost` framework (Apple's modern alternative, requires reworking `rusb` usage). For v1 we ship unsandboxed; v2 considers IOUSBHost if and when we want App Store distribution.

**Why hardened runtime is on:** notarization requires it. The signing flag (`--options runtime`) enables it; the entitlements file does not need any keys to support a non-sandboxed, non-JIT app under the hardened runtime — that's the default secure configuration.

**USB validation spike (must run before M5 starts):** confirm that an unsandboxed, hardened-runtime, codesigned, notarized "hello world" app statically linked against `libsdr_core.a` (which embeds `rusb`/libusb via the static lib) can enumerate USB devices and open an RTL-SDR. Done as a 1-day spike on a real macOS box. The expected outcome is "yes, with no entitlements" — many open-source SDR apps on macOS have proven this works. If the spike fails, we revisit (likely candidate: `com.apple.security.cs.disable-library-validation` if the linking pulls in a runtime dylib we didn't expect, or `IOUSBHost` if we have to leave libusb behind entirely). The risk is real but bounded.

## Code Signing

Two configurations:

**Local development:** ad-hoc signing (`-`). The dev's machine accepts unsigned local builds. No certificates needed for `cargo build && open SDRMac.app`.

**Distribution:** Developer ID Application certificate, plus the corresponding Developer ID Installer certificate if we ever ship a `.pkg` (we don't, in v1).

### `scripts/sign-mac.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

APP_PATH="${1:?usage: sign-mac.sh path/to/SDRMac.app}"
IDENTITY="${SDR_SIGNING_IDENTITY:?must be set to a Developer ID Application name}"
ENTITLEMENTS="apps/macos/SDRMac/Entitlements/SDRMac.entitlements"

# Sign nested executables first (none in v1, but future-proof).
# Then sign the main bundle.
codesign --force --deep \
    --sign "${IDENTITY}" \
    --options runtime \
    --entitlements "${ENTITLEMENTS}" \
    --timestamp \
    "${APP_PATH}"

# Verify the signature.
codesign --verify --deep --strict --verbose=2 "${APP_PATH}"
spctl --assess --verbose=4 --type execute "${APP_PATH}" || true   # informational
```

`--options runtime` enables the hardened runtime. `--timestamp` is required by notarization. `--deep` is somewhat deprecated by Apple, but for an app with no nested signed content it's still the simplest correct choice; if we add embedded frameworks later we'll switch to per-component signing.

### Notarization with `notarytool`

```bash
# scripts/release-mac.sh (excerpt)
ZIP="$(mktemp -t SDRMac).zip"
ditto -c -k --sequesterRsrc --keepParent "${APP_PATH}" "${ZIP}"

xcrun notarytool submit "${ZIP}" \
    --apple-id     "${APPLE_ID}" \
    --team-id      "${APPLE_TEAM_ID}" \
    --password     "${APPLE_APP_PASSWORD}" \
    --wait \
    --timeout      30m

xcrun stapler staple "${APP_PATH}"
xcrun stapler validate "${APP_PATH}"
```

Stapling embeds the notarization ticket into the `.app` so Gatekeeper accepts it offline. After this the `.app` is ready to upload to the GitHub release.

## GitHub Actions

Two new workflow files:

### `.github/workflows/macos-build.yml` (per-PR build, no signing)

Triggers on every PR that touches `apps/macos/**`, `crates/sdr-ffi/**`, `crates/sdr-core/**`, `crates/sdr-sink-audio/**`, or `include/sdr_core.h`. Catches Rust↔Swift drift early.

```yaml
name: macOS build
on:
  pull_request:
    paths:
      - 'apps/macos/**'
      - 'crates/sdr-ffi/**'
      - 'crates/sdr-core/**'
      - 'crates/sdr-sink-audio/**'
      - 'include/sdr_core.h'
      - 'scripts/build-mac.sh'

jobs:
  build:
    runs-on: macos-26
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-darwin,x86_64-apple-darwin
      - uses: Swift-actions/setup-swift@v2
        with:
          swift-version: '6.1'
      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}

      - name: Build (Debug, ad-hoc signed)
        run: ./scripts/build-mac.sh Debug

      - name: Run SwiftPM tests
        working-directory: apps/macos/Packages/SdrCoreKit
        run: swift test

      - name: Header drift check
        run: make ffi-header-check

      - name: Run Xcode unit tests
        run: |
          xcodebuild test \
            -project apps/macos/SDRMac.xcodeproj \
            -scheme SDRMac \
            -destination 'platform=macOS' \
            -derivedDataPath target/xcode
```

### `.github/workflows/macos-release.yml` (tag-triggered, signs + notarizes)

```yaml
name: macOS release
on:
  push:
    tags: ['v*.*.*']

jobs:
  release:
    runs-on: macos-26
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-darwin,x86_64-apple-darwin

      # Import certificates from base64-encoded secrets
      - name: Import signing certs
        env:
          MACOS_CERT_P12_BASE64: ${{ secrets.MACOS_CERT_P12_BASE64 }}
          MACOS_CERT_P12_PASSWORD: ${{ secrets.MACOS_CERT_P12_PASSWORD }}
          KEYCHAIN_PASSWORD: ${{ secrets.MACOS_KEYCHAIN_PASSWORD }}
        run: |
          echo "${MACOS_CERT_P12_BASE64}" | base64 --decode > /tmp/cert.p12
          security create-keychain -p "${KEYCHAIN_PASSWORD}" build.keychain
          security default-keychain -s build.keychain
          security unlock-keychain -p "${KEYCHAIN_PASSWORD}" build.keychain
          security import /tmp/cert.p12 -k build.keychain -P "${MACOS_CERT_P12_PASSWORD}" -T /usr/bin/codesign
          security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "${KEYCHAIN_PASSWORD}" build.keychain

      - name: Build Release
        run: ./scripts/build-mac.sh Release

      - name: Sign + notarize + staple
        env:
          SDR_SIGNING_IDENTITY: ${{ secrets.MACOS_SIGNING_IDENTITY }}
          APPLE_ID:             ${{ secrets.APPLE_ID }}
          APPLE_TEAM_ID:        ${{ secrets.APPLE_TEAM_ID }}
          APPLE_APP_PASSWORD:   ${{ secrets.APPLE_APP_PASSWORD }}
        run: ./scripts/release-mac.sh

      - name: Create DMG
        run: |
          hdiutil create -volname "SDRMac" \
            -srcfolder target/xcode/Build/Products/Release/SDRMac.app \
            -ov -format UDZO target/SDRMac-${{ github.ref_name }}.dmg

      - name: Upload to release
        uses: softprops/action-gh-release@v1
        with:
          files: target/SDRMac-*.dmg
```

### Required GitHub secrets

| Secret name              | What                                                  |
|--------------------------|-------------------------------------------------------|
| `MACOS_CERT_P12_BASE64`  | Developer ID Application cert exported as `.p12`, base64'd |
| `MACOS_CERT_P12_PASSWORD`| Password protecting that `.p12`                       |
| `MACOS_KEYCHAIN_PASSWORD`| Throwaway password for the temp build keychain       |
| `MACOS_SIGNING_IDENTITY` | The cert's display name (e.g., `Developer ID Application: Jason Herald (TEAMID)`) |
| `APPLE_ID`               | Developer Apple ID (email)                            |
| `APPLE_TEAM_ID`          | 10-character team ID                                  |
| `APPLE_APP_PASSWORD`     | App-specific password from appleid.apple.com         |

These are configured once before the first release. Documented in `apps/macos/README.md`.

## Versioning

The Xcode project uses:

```text
MARKETING_VERSION         = 0.1.0      (CFBundleShortVersionString)
CURRENT_PROJECT_VERSION   = 1          (CFBundleVersion, increments per release)
```

Both are bumped manually for now. Tag → release script: `git tag v0.1.0 && git push --tags` triggers the release workflow. The tag and the marketing version stay in sync.

The Rust side uses `cargo workspace.package.version` for `sdr-core` and `sdr-ffi`, currently `0.1.0`. The C ABI version (`SDR_CORE_ABI_VERSION_MAJOR/MINOR`) is independent and only bumps on FFI surface changes, not on app releases.

## Local Developer Workflow

After this milestone lands, a contributor's macOS workflow is:

```bash
git clone https://github.com/jasonherald/rtl-sdr.git
cd rtl-sdr
./scripts/build-mac.sh Debug
open apps/macos/SDRMac.xcodeproj
# Hit Cmd-R in Xcode → app launches, ad-hoc signed, no Gatekeeper prompt
# (because it's running from Xcode's derived data, not /Applications)
```

For Rust changes, re-run `./scripts/build-mac.sh Debug` (cargo incremental keeps it fast). Swift changes are pure Xcode iteration.

A `make mac` target wraps the script for CLI lovers:

```makefile
# Makefile
.PHONY: mac mac-release ffi-header-check
mac:
	./scripts/build-mac.sh Debug
mac-release:
	./scripts/build-mac.sh Release
ffi-header-check:
	cbindgen --config crates/sdr-ffi/cbindgen.toml --crate sdr-ffi --output target/sdr_core.h.generated
	diff -u include/sdr_core.h target/sdr_core.h.generated
```

## Cargo-Side Changes

Two small changes to the workspace `Cargo.toml`:

1. Add `crates/sdr-core` and `crates/sdr-ffi` to `[workspace.members]` (when those crates land in M1/M2).
2. Add a profile override for `sdr-ffi` to ensure release builds are LTO'd:

```toml
[profile.release.package.sdr-ffi]
lto       = "fat"
codegen-units = 1
strip     = "debuginfo"
```

Smaller binary, faster code, no debug bloat in the shipped `.a`.

## Risks

| Risk | Mitigation |
|------|------------|
| GitHub macOS runners change versions and break the workflow | Pin `runs-on: macos-26` (or whatever the current LTS is). Rebuild on a new runner manually before changing the pin. |
| `notarytool` rejects the app for an entitlement we don't expect | The first release submission is done locally so the developer can iterate quickly on entitlement fixes. Once it works locally, the CI workflow is stable. |
| `coreaudio-sys` bindgen step fails on a fresh macOS runner because of missing system headers | Spike during M3 (CoreAudio sink) catches this. CI will reproduce within minutes if it breaks. |
| Universal binary build doubles the cargo cache | Acceptable. Use `actions/cache` keyed on `Cargo.lock` so cold-cache builds are rare. |
| Developer ID cert expires (5-year lifetime) | Tracked in a project README "ops" section. Renewal is a one-time admin task. |
| Notarization wait times spike during Apple outages | `--wait --timeout 30m` covers the typical case. If notarization is down, the workflow fails, the release is delayed, no damage done — just retry after Apple recovers. |
| User downloads the dmg, `xattr -p com.apple.quarantine` blocks launch | Stapling fixes this. We test stapling validity in the workflow before uploading the dmg. |

## Open Questions

- **Should the dmg include a background image and Applications symlink?** Standard polish; doable with `create-dmg` (npm) or hand-rolled `hdiutil` + `osascript`. **Lean: yes for v1, simple version (background + symlink, no fancy positioning).**
- **Universal binary or two separate downloads?** Universal is simpler for users (one download regardless of CPU) and the size cost is ~30 MB extra in our case. **Lean: universal.**
- **Where do release notes come from?** Hand-written in the release commit body; the workflow reads them via `softprops/action-gh-release`'s default GitHub-issue-style auto-generation. v1 is hand-written, v2 maybe auto.
- **Should we sign the Rust static lib itself?** No — `.a` archives can't be signed. Only the final binary that links them. macOS doesn't care.

## Implementation Sequencing

This is M6, the final milestone. Sub-PRs:

1. **`apps/macos/` skeleton** — empty Xcode project, `Package.swift`, `build-mac.sh`. Builds an empty SwiftUI window.
2. **Wire `libsdr_core.a` linkage** — Xcode build settings, OTHER_LDFLAGS, header search paths. Verifies the FFI is callable from Swift end-to-end via a smoke test that calls `sdr_core_abi_version()` and prints it.
3. **Code-signing scripts + entitlements file** — `sign-mac.sh`, ad-hoc signing works, `codesign --verify` clean.
4. **Notarization script + GitHub workflow** — `release-mac.sh` succeeds locally, then the workflow runs on tag.
5. **dmg packaging** — `release-mac.sh` produces the dmg, workflow uploads to release.
6. **First v0.1.0 release.** :tada:

## References

- [Apple — Hardened Runtime](https://developer.apple.com/documentation/security/hardened_runtime)
- [Apple — Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- [Apple — `notarytool`](https://developer.apple.com/documentation/security/customizing_the_notarization_workflow)
- [`coreaudio-rs`](https://github.com/RustAudio/coreaudio-rs) — informs framework link list
- `2026-04-12-sdr-ffi-c-abi-design.md` — explains why staticlib was chosen and what gets linked
- `2026-04-12-coreaudio-sink-design.md` — what `libsdr_core.a` needs from CoreAudio at link time
