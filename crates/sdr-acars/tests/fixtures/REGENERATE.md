# Regenerating the acarsdec snapshot

The e2e test `sdr_acars_cli_matches_acarsdec_on_test_wav`
(`crates/sdr-acars/tests/e2e_acarsdec_compat.rs`) diffs the Rust
port's output against a snapshot of the C `acarsdec`'s output on
`crates/sdr-acars/tests/data/acars_test.wav`. This file documents
how to refresh that snapshot — needed when:

- The acarsdec project upstream changes its output format.
- We add/remove fields from our printer that should match.

The snapshot is committed (rather than running `acarsdec` at test
time) so CI is deterministic and we don't drag the C tool into the
test toolchain.

## Procedure

```bash
# 1. Ensure acarsdec is built. The vendored source is in
#    `original/acarsdec/` (gitignored — see docs/PROJECT.md).
cd original/acarsdec
cmake -B build && cmake --build build
cd ../..

# 2. Generate raw output. `-o 2` selects the one-line + body
#    plain-text printer that our CLI matches; redirect stderr
#    to drop the "exiting ..." banner.
./original/acarsdec/build/acarsdec \
    -o 2 \
    -f crates/sdr-acars/tests/data/acars_test.wav \
    > /tmp/acarsdec_raw.txt 2>/dev/null

# 3. Strip volatile fields and write the snapshot. The regex must
#    stay byte-equal to `strip_volatile_line` in
#    `tests/e2e_acarsdec_compat.rs` — keep them in sync.
sed -E 's/^\[#[0-9]+ \(L:[^)]+\)[ 0-9.]*--/[#X (L:N E:N) --/' \
    /tmp/acarsdec_raw.txt > \
    crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt

# 4. Sanity-check: 7 ACARS messages on the fixture wav.
grep -c '^\[#X' crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt
# Expected: 7

# 5. Verify the test still passes.
cargo test -p sdr-acars --test e2e_acarsdec_compat
```

## Volatile fields

The strip regex covers everything that depends on wall-clock or
hardware state:

- `#<seq>` — per-message sequence counter (1-indexed)
- `L:<level>` — matched-filter signal level in dB
- `E:<errors>` — bytes corrected by parity FEC
- `<timestamp>` — wall-clock at decode time (acarsdec emits two
  trailing spaces with `printdate` off; our CLI emits a Unix
  epoch with millis — the regex handles both)

Everything else (Mode, Label, Aircraft, Flight ID, Block ID, Ack,
message body, ETX/ETB) must match the C reference exactly.
