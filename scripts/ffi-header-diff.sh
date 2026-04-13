#!/usr/bin/env bash
#
# ffi-header-diff.sh — signature-only comparison between the
# hand-written `include/sdr_core.h` (source of truth) and the
# cbindgen-generated artifact at `target/sdr_core.h.generated`.
#
# The hand-written header carries explanatory comments, section
# dividers, and human-friendly ordering that cbindgen would flatten.
# Diffing the raw files line-by-line would fail on every commit for
# cosmetic reasons. This script normalizes both files to their
# *signatures only* — typedef names, struct fields, enum discriminants,
# and function prototypes — and diffs those.
#
# Failure modes we want to catch:
#   - A new `#[unsafe(no_mangle)] extern "C"` function in Rust that
#     someone forgot to declare in the hand-written header.
#   - A hand-written header declaration that no longer has a Rust
#     implementation (a deleted function).
#   - A struct field added or removed on one side without the other.
#   - An enum discriminant renumbered on one side.
#
# Failure modes we deliberately do NOT catch:
#   - Comment drift (the hand-written header has long explanatory
#     comments cbindgen never produces).
#   - Ordering differences (the hand-written header groups functions
#     into "Lifecycle / Commands / Events / FFT" sections; cbindgen
#     alphabetizes).
#   - Formatting whitespace.
#
# Usage:
#   ffi-header-diff.sh <hand_written_header> <generated_header>
#
# Exits 0 on match, 1 on drift. Prints the diff on failure.

set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <hand_written_header> <generated_header>" >&2
    exit 2
fi

HAND_WRITTEN="$1"
GENERATED="$2"

if [ ! -f "$HAND_WRITTEN" ]; then
    echo "error: hand-written header not found: $HAND_WRITTEN" >&2
    exit 2
fi
if [ ! -f "$GENERATED" ]; then
    echo "error: generated header not found: $GENERATED" >&2
    echo "(did cbindgen run successfully?)" >&2
    exit 2
fi

# Extract the "signature surface" from a header:
#   - lines that look like function prototypes (end in `);`)
#   - lines that look like struct field declarations
#   - enum discriminant lines
# Then normalize whitespace, strip block comments, and sort.
#
# This is deliberately loose — the goal is to catch obvious drift,
# not to be a formal C parser. If we grow into needing that, swap
# this for a real tool like `clang-query` or `pycparser`.
normalize() {
    local file="$1"
    # 1. Strip /* ... */ block comments (including multi-line).
    #    awk BEGIN/END dance to handle multi-line comment state.
    # 2. Strip // single-line comments.
    # 3. Strip leading/trailing whitespace.
    # 4. Collapse runs of internal whitespace to single spaces.
    # 5. Drop blank lines and preprocessor lines (#include, #ifdef,
    #    #define — those are structure, not surface).
    # 6. Sort deterministically so ordering differences don't break
    #    the diff.
    awk '
        BEGIN { in_block = 0 }
        {
            line = $0
            # Remove inline /* */ on the same line.
            while (match(line, /\/\*.*\*\//)) {
                line = substr(line, 1, RSTART - 1) substr(line, RSTART + RLENGTH)
            }
            # Handle block comments spanning multiple lines.
            if (in_block) {
                if (match(line, /\*\//)) {
                    line = substr(line, RSTART + RLENGTH)
                    in_block = 0
                } else {
                    next
                }
            }
            if (match(line, /\/\*/)) {
                line = substr(line, 1, RSTART - 1)
                in_block = 1
            }
            # Strip // comments.
            if (match(line, /\/\//)) {
                line = substr(line, 1, RSTART - 1)
            }
            print line
        }
    ' "$file" \
    | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
    | tr -s '[:space:]' ' ' \
    | grep -vE '^$' \
    | grep -vE '^#' \
    | sort
}

HAND_NORM=$(mktemp -t sdr_ffi_hand.XXXXXX)
GEN_NORM=$(mktemp -t sdr_ffi_gen.XXXXXX)
trap 'rm -f "$HAND_NORM" "$GEN_NORM"' EXIT

normalize "$HAND_WRITTEN" > "$HAND_NORM"
normalize "$GENERATED" > "$GEN_NORM"

if diff -q "$HAND_NORM" "$GEN_NORM" >/dev/null 2>&1; then
    echo "OK: hand-written header and cbindgen output match (signature-only)."
    exit 0
fi

echo "FAIL: hand-written header and cbindgen output disagree." >&2
echo "" >&2
echo "Signature diff (< hand-written, > generated):" >&2
diff "$HAND_NORM" "$GEN_NORM" || true
echo "" >&2
echo "Fix: add the missing declaration to $HAND_WRITTEN (for a new" >&2
echo "Rust function), remove the stale declaration (for a deleted" >&2
echo "Rust function), or update the Rust side to match the header." >&2
echo "Run 'make ffi-header-regen' for a fresh cbindgen dump to copy" >&2
echo "from." >&2
exit 1
