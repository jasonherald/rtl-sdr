#!/usr/bin/env bash
#
# bundle-mac-app.sh — wrap the SwiftPM-built SDRMac executable
# into a minimal `.app` bundle for developer iteration.
#
# This is NOT the production signing/notarization flow (that lives
# in M6 under `scripts/build-mac.sh` + Xcode). It's a lightweight
# helper that lets us `open SDRMac.app` during development to see
# the SwiftUI window actually render — SwiftPM produces a bare
# Mach-O executable which won't attach to the window server the
# way a proper `.app` does.
#
# Usage:
#   ./apps/macos/scripts/bundle-mac-app.sh [debug|release]
#
# Default is `release` — debug builds of the Rust DSP chain are
# too slow for live RTL-SDR streaming (tested: ~45% throughput vs
# release on macOS) so the dev loop should almost always use
# release. Only reach for the `debug` variant when iterating on
# non-streaming paths (UI, event wiring, config parsing).
#
# Produces:
#   apps/macos/build/SDRMac.app
#
# Assumes `cargo build --workspace [--release]` and `swift build
# [-c release]` (from apps/macos/) have already run — the script
# just copies the binary and plist into the bundle layout.

set -euo pipefail

CONFIG="${1:-release}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$APP_DIR/../.." && pwd)"

SWIFT_BUILD_DIR="$APP_DIR/.build/$CONFIG"
BUNDLE_DIR="$APP_DIR/build/SDRMac.app"

if [ ! -f "$SWIFT_BUILD_DIR/SDRMac" ]; then
    echo "error: $SWIFT_BUILD_DIR/SDRMac not found" >&2
    echo "       run 'cargo build --workspace' and 'swift build' first" >&2
    exit 1
fi

echo "==> bundling $BUNDLE_DIR"
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_DIR/Contents/MacOS"
mkdir -p "$BUNDLE_DIR/Contents/Resources"

cp "$SWIFT_BUILD_DIR/SDRMac" "$BUNDLE_DIR/Contents/MacOS/SDRMac"
cp "$APP_DIR/SDRMac/Resources/Info.plist" "$BUNDLE_DIR/Contents/Info.plist"

# Ad-hoc sign so the binary can load on recent macOS — unsigned
# .app bundles get blocked by the hardened-runtime defaults.
# Production signing with a Developer ID lives in M6.
echo "==> ad-hoc signing (dev only)"
codesign --force --sign - \
    --entitlements "$APP_DIR/SDRMac/Entitlements/SDRMac.entitlements" \
    "$BUNDLE_DIR/Contents/MacOS/SDRMac"

echo "==> bundle ready: $BUNDLE_DIR"
echo "    open with:  open '$BUNDLE_DIR'"
