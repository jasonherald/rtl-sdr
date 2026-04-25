# Regenerating golden fixtures

Goldens live in `crates/sdr-lrpt/tests/fixtures/golden/` and are
reference outputs from a known-good external decoder
(`MeteorDemod`, `medet`, or `SatDump`). Once landed, they become
the byte-equality / SSIM reference for the integration tests
under `crates/sdr-lrpt/tests/golden_regression.rs`.

**Status as of Task 4 (this PR):** the goldens directory is
empty and `golden_regression.rs` is still a `todo!()`-scaffolded
`#[ignore]` test that early-returns when the fixtures are
missing. The full byte-equality + SSIM-comparison harness lands
in Task 5 alongside the first set of real-pass goldens. This
doc is the regeneration playbook for that landing and any
subsequent updates.

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
# Step 1: bootstrap the pinned-revision file if this is the
# first regeneration (Task 5's initial landing). After that the
# file already exists and you skip to step 2.
PIN_FILE=/path/to/sdr-rs/crates/sdr-lrpt/tests/fixtures/golden/REFERENCE_REVISION.txt
if [ ! -f "$PIN_FILE" ]; then
    # Pick the SHA you want to anchor against — typically the
    # current HEAD of MeteorDemod's master branch, OR a known-
    # good tag.
    git ls-remote https://github.com/Digitelektro/MeteorDemod.git HEAD \
        | awk '{print $1}' > "$PIN_FILE"
fi

# Step 2: clone + check out the pinned revision.
# In a scratch dir outside the repo:
git clone https://github.com/Digitelektro/MeteorDemod.git
cd MeteorDemod
git checkout "$(cat "$PIN_FILE")"
mkdir build && cd build && cmake .. && make
./MeteorDemod -m oqpsk -i path/to/known_pass.iq -o out

# Step 3: copy artefacts into our fixtures.
cp out/frames.bin /path/to/sdr-rs/crates/sdr-lrpt/tests/fixtures/golden/frames.bin
cp out/composite.png /path/to/sdr-rs/crates/sdr-lrpt/tests/fixtures/golden/composite.png
```

The pinned-revision pattern keeps regenerated goldens
byte-identical across machines and times. To intentionally
update the pinned revision (e.g. after a `MeteorDemod` bug fix):

```bash
# In your MeteorDemod checkout:
git rev-parse HEAD > /path/to/sdr-rs/crates/sdr-lrpt/tests/fixtures/golden/REFERENCE_REVISION.txt
# ... then regenerate all goldens against the new revision and
# commit the lot together.
```

### IQ recording

The IQ recording itself isn't committed (~30-50 MB per pass).
It's a real Meteor-M 2-3 pass captured locally with the app's
auto-record flow. If a fresh recording is needed, capture one
during a real overhead pass — `~/sdr-recordings/` is the
convention.

## What the test will assert (Task 5)

Once Task 5 ships the full `LrptPipeline` and the first golden
fixture lands, `crates/sdr-lrpt/tests/golden_regression.rs` will
run our pipeline on the same IQ and compare:

- Frame stream: byte-equality against `frames.bin`
- Composite PNG: SSIM > 0.99 against `composite.png`

Either failing is a hard test fail. The test is currently
`#[ignore]`-gated and `todo!()`-scaffolded — it compiles and
runs, but early-returns when the goldens directory is empty.
Run on demand once goldens land:

```bash
cargo test -p sdr-lrpt -- --ignored frames_match_golden
```
