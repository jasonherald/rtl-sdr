# ACARS test fixtures

## acars_test.wav

4-channel WAV at 12500 Hz, 16-bit signed PCM. ~430 KB. Each channel
carries one ACARS RF channel post-AM-demod-and-decimation to the
12.5 kHz IF rate. Provided as a reference recording for end-to-end
decoder validation.

**Source:** `original/acarsdec/test.wav` (from
<https://github.com/TLeconte/acarsdec>, GPLv2). Vendored here so our
test suite doesn't depend on the C reference repo being checked out
locally — same pattern `crates/sdr-dsp/tests/data/` uses for APT
recordings.

**Used by:**
- `tests/e2e_acarsdec_compat.rs` — diffs `sdr-acars-cli` output
  against a committed acarsdec snapshot on this same input.
- Any future correctness tests (sub-project 1's primary correctness
  oracle, since synthesizing real ACARS-grade MSK in Rust is
  non-trivial).
