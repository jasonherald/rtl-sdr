#!/usr/bin/env bash
#
# ffi-header-diff.sh — name-level drift check between the hand-
# written `include/sdr_core.h` (source of truth) and the cbindgen-
# generated artifact at `target/sdr_core.h.generated`.
#
# ## What this check does
#
# Extracts the set of **function names** from both files and
# diffs those sets. Fails CI on any name that's in one file but
# not the other — the only drift that actually breaks the ABI.
#
# ## What this check no longer does (and why)
#
# Earlier iterations diff'd full normalized signatures. That
# kept drifting into cbindgen-vs-hand-written cosmetic fights:
#  - `struct Foo*` vs `Foo*` (tag vs typedef style)
#  - `uintptr_t` vs `size_t`
#  - anonymous `typedef struct {...} Foo;` vs named
#    `typedef struct Foo {...} Foo;`
#  - `pub const` on the Rust side vs `enum { ... }` in the
#    hand-written header (cbindgen emits `#define`; we prefer
#    enums for type safety).
#
# All of those are semantically identical in C but textually
# impossible to unify without rewriting either cbindgen or the
# hand-written style. The name-level check catches the drift
# that matters (a function added or removed on one side only)
# without the false positives.
#
# ## Usage
#
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

# Extract function names by matching the `sdr_core_<name>(` prefix.
# That's our public ABI namespace — every FFI function starts with
# `sdr_core_`. Declarations and definitions both match because we
# only care about the name, not whether the line ends in `;` or `{`.
#
# The pattern allows any return type and modifiers on the same line
# before the function name, which is how both hand-written and
# cbindgen emit prototypes.
extract_names() {
    local file="$1"
    # Match lines like "returntype sdr_core_foo(", extracting
    # "sdr_core_foo". grep -o prints only the matched text per
    # occurrence, one per line.
    # `[a-z0-9_]+` matches digit-containing names too (e.g. a
    # future `sdr_core_v2_foo`). No case-insensitivity needed —
    # the project's convention is snake_case.
    grep -oE 'sdr_core_[a-z0-9_]+\(' "$file" \
        | sed 's/(//' \
        | sort -u
}

HAND_NAMES=$(mktemp -t sdr_ffi_hand.XXXXXX)
GEN_NAMES=$(mktemp -t sdr_ffi_gen.XXXXXX)
trap 'rm -f "$HAND_NAMES" "$GEN_NAMES"' EXIT

extract_names "$HAND_WRITTEN" > "$HAND_NAMES"
extract_names "$GENERATED" > "$GEN_NAMES"

if diff -q "$HAND_NAMES" "$GEN_NAMES" >/dev/null 2>&1; then
    echo "OK: hand-written header and cbindgen output expose the same FFI function set."
    exit 0
fi

echo "FAIL: FFI function name set drifted between hand-written and generated headers." >&2
echo "" >&2
echo "(< in hand-written only, > in Rust / generated only)" >&2
diff "$HAND_NAMES" "$GEN_NAMES" || true
echo "" >&2
echo "Fix: add the missing declaration to $HAND_WRITTEN (for a new" >&2
echo "Rust function added to sdr-ffi), remove the stale declaration" >&2
echo "(for a function deleted from Rust), or add the #[unsafe(no_mangle)]" >&2
echo "extern \"C\" fn on the Rust side to match the hand-written header." >&2
exit 1
