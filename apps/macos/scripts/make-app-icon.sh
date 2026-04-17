#!/usr/bin/env bash
#
# make-app-icon.sh — build AppIcon.icns from the project SVG.
#
# The source SVG (`data/com.sdr.rs.svg`) is the shared project
# icon used by both the Linux desktop files and the macOS app
# bundle. macOS wants a packed `.icns` file containing PNGs at
# a set of standard sizes, so we rasterize the SVG via `rsvg-
# convert` (preferred, sharper) or `sips` (Apple's built-in
# fallback), then pack with `iconutil`.
#
# Usage:
#   apps/macos/scripts/make-app-icon.sh <out-path>
#
# Exits 0 on success, 1 on failure. Writes:
#   <out-path>/AppIcon.icns
#
# Idempotent: overwrites existing output.

set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <out-dir>" >&2
    exit 2
fi

OUT_DIR="$1"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
SVG="$REPO_ROOT/data/com.sdr.rs.svg"

if [ ! -f "$SVG" ]; then
    echo "error: source SVG not found: $SVG" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

# Work in a temp iconset directory; iconutil reads this layout.
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
trap 'rm -rf "$(dirname "$ICONSET")"' EXIT

# Apple's expected iconset filenames — iconutil uses these exact
# names to pack the final .icns. Sizes cover the full retina +
# non-retina range from 16 pt (Finder list) to 512 pt (App Store).
#
# Format: "size@scale  filename"
SIZES=(
    "16    icon_16x16.png"
    "32    icon_16x16@2x.png"
    "32    icon_32x32.png"
    "64    icon_32x32@2x.png"
    "128   icon_128x128.png"
    "256   icon_128x128@2x.png"
    "256   icon_256x256.png"
    "512   icon_256x256@2x.png"
    "512   icon_512x512.png"
    "1024  icon_512x512@2x.png"
)

if command -v rsvg-convert >/dev/null 2>&1; then
    RENDER="rsvg"
elif command -v sips >/dev/null 2>&1; then
    # `sips` ships with macOS and handles SVG via Image I/O on
    # macOS 12+. Tested: valid 128×128 RGBA PNG output from the
    # project SVG. Slightly softer antialiasing than librsvg but
    # fine for the app icon at the sizes we render.
    RENDER="sips"
else
    echo "error: neither rsvg-convert nor sips available" >&2
    echo "       install rsvg-convert with: brew install librsvg" >&2
    echo "       (sips is built into macOS, so this branch should" >&2
    echo "        only trigger on a dev machine with PATH stripped)" >&2
    exit 1
fi

for entry in "${SIZES[@]}"; do
    size="${entry%% *}"
    # everything after the last space; avoid awk for portability.
    name="${entry##* }"
    out="$ICONSET/$name"
    if [ "$RENDER" = "rsvg" ]; then
        if ! rsvg-convert -w "$size" -h "$size" "$SVG" -o "$out"; then
            echo "error: rsvg-convert failed rendering $name (${size}x${size})" >&2
            exit 1
        fi
    else
        # Let sips' own stderr flow through on failure rather
        # than swallowing it — previous versions of this script
        # piped sips to /dev/null, which turned a silent failure
        # into an empty .icns. Noise on success is acceptable;
        # sips is conservative about warnings.
        if ! sips -s format png -z "$size" "$size" "$SVG" --out "$out" >/dev/null; then
            echo "error: sips failed rendering $name (${size}x${size})" >&2
            exit 1
        fi
        if [ ! -s "$out" ]; then
            echo "error: sips produced an empty file for $name" >&2
            exit 1
        fi
    fi
done

iconutil --convert icns "$ICONSET" --output "$OUT_DIR/AppIcon.icns"
echo "==> wrote $OUT_DIR/AppIcon.icns"
