# Regenerating golden fixtures

Goldens live in `crates/sdr-lrpt/tests/fixtures/golden/` and are
reference outputs from a known-good external decoder
(`MeteorDemod`, `medet`, or SatDump). They are committed to the
repo and asserted byte-equality against our own output in the
integration tests under `crates/sdr-lrpt/tests/golden_regression.rs`.

## When to regenerate

- The reference decoder version we used has been superseded and
  the new version produces materially different output (rare).
- Test fixtures (input IQ, synthetic CADU streams) change.
- A bug fix in a reference decoder changes the canonical output.

## How to regenerate

### Frame stream goldens (CCSDS layer)

Run `MeteorDemod` against a known IQ recording captured from a
real Meteor pass, dump the post-RS-decode frame stream, and copy
into our fixtures:

```bash
# In a scratch dir outside the repo:
git clone https://github.com/Digitelektro/MeteorDemod.git
cd MeteorDemod && mkdir build && cd build && cmake .. && make
./MeteorDemod -m oqpsk -i path/to/known_pass.iq -o out

# Copy the relevant artefacts into our fixtures:
cp out/frames.bin /path/to/sdr-rs/crates/sdr-lrpt/tests/fixtures/golden/frames.bin
cp out/composite.png /path/to/sdr-rs/crates/sdr-lrpt/tests/fixtures/golden/composite.png
```

### IQ recording

The IQ recording itself isn't committed (~30-50 MB per pass).
It's a real Meteor-M 2-3 pass captured locally with the app's
auto-record flow. If a fresh recording is needed, capture one
during a real overhead pass — `~/sdr-recordings/` is the
convention.

## What the test asserts

`crates/sdr-lrpt/tests/golden_regression.rs` runs our pipeline
on the same IQ, compares frame-stream byte-equality and PNG SSIM
(>0.99 threshold). A regression in either is a hard fail.

The golden_regression test is `#[ignore]`-gated until a real-pass
golden lands (committed alongside the user's overnight smoke-test
capture). Run on demand:

```bash
cargo test -p sdr-lrpt -- --ignored real_pass
```
