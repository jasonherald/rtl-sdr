# LRPT real-data regression gate

The original plan for this directory was a committed golden set
(`MeteorDemod` frame stream + composite PNG) compared byte-for-byte
and by SSIM from an `#[ignore]`d `golden_regression.rs`. That test
never got past a `todo!()` scaffold and no goldens were ever
committed, so the scaffold was removed in the Aug 2026 deep review
(#733). The regression gate that actually exists is the replay CLI
over a real pass:

```bash
cargo run -q -p sdr-lrpt --bin sdr-lrpt-replay --release -- \
    test-data/lrpt/recordings/kg.s <out_dir> soft-diff
```

- `test-data/lrpt/recordings/kg.s` — a `meteor_demod` soft-symbol
  capture of a real Meteor pass (legacy differential downlink, hence
  `soft-diff`).
- `test-data/lrpt/reference/kg_dbd_65.png` / `kg_dbd_68.png` —
  dbdexter's output for the same capture; compare visually against
  `<out_dir>/ch65.png` / `ch68.png`.
- The CLI's `fec:` line (`rotation_locks`, `rotation_rehunts`,
  `sync_timeouts`, `cadus_decoded`, `cadus_failed`) and the per-channel
  line counts are the numbers to quote in a PR before/after any FEC,
  framing or image-assembly change. A baseline from `main` is cheap to
  get with a scratch worktree:

```bash
git worktree add /tmp/wt-main main
(cd /tmp/wt-main && cargo run -q -p sdr-lrpt --bin sdr-lrpt-replay --release -- \
    /path/to/test-data/lrpt/recordings/kg.s /tmp/wt-main-out soft-diff)
git worktree remove --force /tmp/wt-main
```

A committed golden set would still be welcome; if one lands, pin the
reference decoder revision next to it and add the comparison as an
`#[ignore]`d integration test that early-returns when the fixtures
are absent.
