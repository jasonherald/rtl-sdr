#!/usr/bin/env bash
#
# Regenerate the pre-rasterized tray icon ARGB32 buffer from
# data/com.sdr.rs.svg. We commit the buffer (not the SVG renderer)
# so sdr-tray doesn't need a runtime SVG dep — the tray icon ships
# as raw bytes that go straight to ksni without decoding.
#
# Run from the repo root:
#   scripts/regen-tray-icon.sh
#
# Output: data/com.sdr.rs.tray22.argb32 (1936 bytes — 22*22*4)
#
# Byte layout: row-major, network-byte-order ARGB32 (A, R, G, B per
# pixel) per the StatusNotifierItem spec. ksni accepts these bytes
# verbatim through `Icon { width, height, data }`.
#
# Why we ship the bytes rather than rasterize at runtime: pulling
# librsvg in introduces a ~80-crate transitive dep tree that drags
# unmaintained crates (paste, fxhash) flagged by RUSTSEC. The icon
# is 22x22 and never changes during a session — pre-baking is the
# right call. Re-run this script if data/com.sdr.rs.svg ever changes.
# Per #512.

set -euo pipefail

cd "$(dirname "$0")/.."

SVG=data/com.sdr.rs.svg
OUT=data/com.sdr.rs.tray22.argb32
SIZE=22

if [[ ! -f $SVG ]]; then
    echo "error: $SVG not found (run from repo root)" >&2
    exit 1
fi

TMP_PNG=$(mktemp --suffix=.png)
trap 'rm -f "$TMP_PNG"' EXIT

# Step 1: SVG -> PNG via rsvg-convert (rsvg has the best SVG support
# of the system tools; ImageMagick's SVG handling is hit-or-miss).
rsvg-convert -w $SIZE -h $SIZE "$SVG" -o "$TMP_PNG"

# Step 2: PNG -> ARGB32 raw bytes via Pillow. PIL gives us RGBA in
# memory; we swap to ARGB (network byte order) for SNI.
python3 - "$TMP_PNG" "$OUT" $SIZE <<'PY'
import sys
from pathlib import Path
from PIL import Image

src, dst, size = sys.argv[1], sys.argv[2], int(sys.argv[3])
img = Image.open(src).convert("RGBA")
if img.size != (size, size):
    raise SystemExit(f"unexpected image size {img.size}, want ({size}, {size})")

out = bytearray(size * size * 4)
for i, (r, g, b, a) in enumerate(img.getdata()):
    out[4 * i + 0] = a
    out[4 * i + 1] = r
    out[4 * i + 2] = g
    out[4 * i + 3] = b

Path(dst).write_bytes(bytes(out))
print(f"wrote {dst} ({len(out)} bytes)")
PY

# Post-write guard: confirm the output is exactly width*height*4
# bytes before the Rust compile-time assertion in icon.rs catches
# it. Failing here gives a clearer error than `cargo build` would.
# Per CR round 1 on PR #572.
expected_bytes=$((SIZE * SIZE * 4))
actual_bytes=$(wc -c < "$OUT")
if [[ "$actual_bytes" -ne "$expected_bytes" ]]; then
    echo "error: $OUT size mismatch: got $actual_bytes, expected $expected_bytes" >&2
    exit 1
fi
