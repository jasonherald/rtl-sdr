# ACARS DSP Crate Implementation Plan (sub-project 1 of epic #474)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `sdr-acars` — a pure-DSP Rust crate that decodes VHF ACARS messages (multi-channel, MSK demod, frame parser with CRC + parity FEC, label-name lookup) — plus a `sdr-acars-cli` binary that takes a WAV or IQ file and prints decoded messages in the same text format as the C `acarsdec` reference, validated by a byte-equal diff test on the shipped `test.wav` fixture.

**Architecture:** Faithful port of `original/acarsdec/{msk.c, acars.c, label.c, syndrom.h}`. Multi-channel from day one (the `ChannelBank` API runs N channels in parallel from a single source-rate IQ stream — same pattern acarsdec uses in `air.c`). Library crate (`sdr-acars`) holds the DSP + parser; binary (`sdr-acars-cli`) wraps it for offline validation. Sub-projects 2 (pipeline integration + airband lock) and 3 (Aviation activity + ACARS viewer) consume the same library API later.

**Tech Stack:** Rust 2024, `num-complex` for `Complex<f32>`, `arrayvec` for fixed-length string fields, `hound` for WAV reading (already in workspace deps), `clap` for CLI args, `thiserror` for error variants, `tracing` for non-panicking diagnostic logs.

---

## File structure

| Path | Responsibility |
|---|---|
| `crates/sdr-acars/Cargo.toml` | Crate manifest, deps, lints inherited from workspace |
| `crates/sdr-acars/src/lib.rs` | Public API: `ChannelBank`, `AcarsMessage`, `ChannelStats`, `ChannelLockState`, re-exports |
| `crates/sdr-acars/src/error.rs` | `AcarsError` (`thiserror`) — all fallible operations return this |
| `crates/sdr-acars/src/crc.rs` | CRC-CCITT-16 KERMIT (poly 0x1021 reflected = 0x8408, init 0x0000). Standalone, no deps. |
| `crates/sdr-acars/src/syndrom.rs` | Static `SYNDROM[256][4]` table from `syndrom.h` + `fixprerr` / `fixdberr` correction |
| `crates/sdr-acars/src/label.rs` | `Lbl[]` table (~150 entries: code → human name) + `lookup(label) -> Option<&'static str>` |
| `crates/sdr-acars/src/msk.rs` | `MskDemod`: 12.5 kHz real-audio → bits via PLL + matched filter (port of `msk.c`) |
| `crates/sdr-acars/src/frame.rs` | `FrameParser`: bits → `AcarsMessage` (state machine WSYN→SYN2→SOH1→TXT→CRC1→CRC2, parity, FEC, CRC) |
| `crates/sdr-acars/src/channel.rs` | `Channel`: source-rate complex IQ → per-channel oscillator+decimator → 12.5 kHz real audio → `MskDemod` → `FrameParser`. `ChannelBank` is `Vec<Channel>` + `process()` orchestrator. |
| `crates/sdr-acars/src/bin/sdr-acars-cli.rs` | CLI: clap args, WAV-or-IQ dispatch, drives `ChannelBank` (or per-channel `MskDemod` for WAV input), prints in acarsdec text format |
| `crates/sdr-acars/tests/e2e_acarsdec_compat.rs` | Diff-test harness: runs `sdr-acars-cli` on `original/acarsdec/test.wav`, strips volatile fields, asserts byte-equal against committed snapshot |
| `crates/sdr-acars/tests/multichannel_synthetic.rs` | Synthesize 2.4 MSps IQ with two MSK signals at known offsets; confirm both channels decode independently with no cross-talk |
| `crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt` | Pre-captured acarsdec output for `test.wav`, volatile fields stripped — committed alongside a `REGENERATE.md` documenting how to regenerate |

---

## Task 0: Branch verification

**Files:** none (sanity check)

- [ ] **Step 1: Confirm we're on the right branch with the spec committed**

```bash
git rev-parse --abbrev-ref HEAD
# Expected: feat/acars-dsp-crate

git log --oneline -3
# Expected (recent first):
#   07df356 docs: link ACARS deferred items to filed issues (#577-#582)
#   b0cfd08 docs: add ACARS reception design spec (#474)
#   ... (parent on main)
```

- [ ] **Step 2: Confirm the C reference is present**

```bash
ls original/acarsdec/{msk.c,acars.c,label.c,syndrom.h,test.wav}
# All five files must exist. test.wav: RIFF, 4 channels, 12500 Hz.
```

If any are missing, stop and ask the user to clone the reference repo.

---

## Task 1: Scaffold the `sdr-acars` crate

**Files:**
- Create: `crates/sdr-acars/Cargo.toml`
- Create: `crates/sdr-acars/src/lib.rs`
- Create: `crates/sdr-acars/src/error.rs`
- Modify: `Cargo.toml` (root) — add `crates/sdr-acars` to `[workspace.members]`, add missing workspace deps
- Modify: `Cargo.toml` (root) — add `arrayvec`, `num-complex`, `clap` to `[workspace.dependencies]` if not already present

- [ ] **Step 1: Check which workspace deps already exist**

```bash
grep -E '^(arrayvec|num-complex|clap)\s*=' Cargo.toml
# Note which are present, which need to be added.
```

- [ ] **Step 2: Add missing workspace deps to root `Cargo.toml`**

In the `[workspace.dependencies]` section, add (only the ones missing per Step 1):

```toml
arrayvec = "0.7"
num-complex = "0.4"
clap = { version = "4", features = ["derive"] }
```

Keep them in alphabetical order with the existing entries.

- [ ] **Step 3: Add the new crate to `[workspace.members]`**

In the root `Cargo.toml`'s `[workspace.members]` array, add `"crates/sdr-acars"` in alphabetical order with the existing members.

- [ ] **Step 4: Create `crates/sdr-acars/Cargo.toml`**

```toml
[package]
name = "sdr-acars"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "ACARS (VHF aircraft datalink) DSP, frame parser, and CLI"

[lints]
workspace = true

[dependencies]
arrayvec = { workspace = true }
clap = { workspace = true }
hound = { workspace = true }
num-complex = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
# (Empty for now; tests use std-only assertions plus workspace fixtures.)

[[bin]]
name = "sdr-acars-cli"
path = "src/bin/sdr-acars-cli.rs"
```

- [ ] **Step 5: Create `crates/sdr-acars/src/error.rs`**

```rust
//! Error type for the `sdr-acars` crate.
//!
//! Per project library-crate rules: all fallible paths return
//! `Result<_, AcarsError>` — no `unwrap()`, no `panic!()`, no
//! stringly-typed errors.

use thiserror::Error;

/// All ways `sdr-acars` can fail.
#[derive(Debug, Error)]
pub enum AcarsError {
    /// `ChannelBank::new` got an invalid configuration: empty
    /// channel list, source rate / center freq combination that
    /// can't fit all channels, or per-channel rate mismatch.
    #[error("invalid channel configuration: {0}")]
    InvalidChannelConfig(String),

    /// Decimation factor isn't an integer for the requested
    /// source rate / IF rate combo. Source rate must be an
    /// integer multiple of 12_500 Hz.
    #[error("source rate {source_rate_hz} Hz is not an integer multiple of IF rate {if_rate_hz} Hz")]
    NonIntegerDecimation { source_rate_hz: f64, if_rate_hz: f64 },

    /// CLI / file I/O — failed to read input file.
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// CLI — input file format isn't recognized (WAV header
    /// missing, IQ file size not a multiple of 4 bytes for
    /// interleaved i16 I/Q, etc.).
    #[error("invalid input format: {0}")]
    InvalidInput(String),
}
```

- [ ] **Step 6: Create `crates/sdr-acars/src/lib.rs` skeleton**

```rust
//! ACARS (Aircraft Communications Addressing and Reporting
//! System) decoder. Faithful Rust port of
//! [acarsdec](https://github.com/TLeconte/acarsdec) — pure DSP +
//! parsing, no GTK, no SDR-driver dependency.
//!
//! The crate exposes one entry point: [`ChannelBank::new`] +
//! [`ChannelBank::process`] for multi-channel parallel decode
//! from a single source-rate IQ stream. Decoded
//! [`AcarsMessage`]s are emitted via a callback.
//!
//! Sub-modules ([`msk`], [`frame`], [`channel`]) are public so
//! the CLI binary can drive them directly for WAV input (which
//! arrives pre-decimated to 12.5 kHz IF rate, bypassing
//! `ChannelBank`'s oscillator + decimator stage).

pub mod channel;
pub mod crc;
pub mod error;
pub mod frame;
pub mod label;
pub mod msk;
pub mod syndrom;

pub use error::AcarsError;
```

- [ ] **Step 7: Verify the workspace builds**

```bash
cargo build -p sdr-acars
# Expected: clean build with no errors. Warnings about empty modules are OK.

cargo build --workspace
# Expected: clean build of the entire workspace.
```

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/sdr-acars/
git commit -m "$(cat <<'EOF'
feat(sdr-acars): scaffold new crate (#474, sub-project 1)

Scaffold for the ACARS DSP crate. Module declarations only;
implementations land in subsequent commits per the plan at
docs/superpowers/plans/2026-04-28-acars-dsp-crate.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: CRC-CCITT-16

**Files:**
- Create: `crates/sdr-acars/src/crc.rs`

ACARS uses CRC-CCITT-16 (KERMIT variant) with polynomial `0x1021` (reflected `0x8408`) and **initial value `0x0000`**, computed over the message bytes from `Mode` through the end of `Text` (not including the BCS bytes themselves). The receiver verifies by feeding the entire frame including BCS through the same CRC; the result must be `0`. NOTE: this corrects an earlier draft of this plan that said init=`0xFFFF` (the X-25 variant); the actual `acarsdec` source at `acars.c:159` initializes `crc = 0`. The Task 2 implementer caught this and the correction propagates here for downstream tasks.

C reference: `original/acarsdec/acars.c` — search for `update_crc` and the trailing CRC verification at the end of `decodeAcars()`.

- [ ] **Step 1: Read the C reference**

```bash
grep -n 'update_crc\|crc' original/acarsdec/acars.c | head -20
```

Note the polynomial constant and the byte-feeding direction (LSB-first matters for ACARS).

- [ ] **Step 2: Write the failing test**

In `crates/sdr-acars/src/crc.rs`:

```rust
//! CRC-CCITT-16 (poly 0x1021, init 0xFFFF) for ACARS frames.
//!
//! ACARS feeds bytes LSB-first into the CRC register, matching
//! the on-the-wire bit order. Receiver verification: feeding
//! the entire frame including the trailing 2-byte BCS through
//! the same CRC yields 0 if the frame is intact.

/// Update a running CRC-CCITT-16 register with one byte.
/// Bytes are consumed LSB-first (ACARS wire convention).
#[must_use]
pub fn update(crc: u16, byte: u8) -> u16 {
    let mut crc = crc ^ u16::from(byte);
    for _ in 0..8 {
        if crc & 0x0001 != 0 {
            crc = (crc >> 1) ^ 0x8408; // 0x1021 reflected
        } else {
            crc >>= 1;
        }
    }
    crc
}

/// Compute CRC over a slice from the standard ACARS init value.
#[must_use]
pub fn compute(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0xFFFF_u16, |crc, &b| update(crc, b))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_known_test_vector() {
        // Vector from common CRC-CCITT (KERMIT) test page:
        // input "123456789" → CRC = 0x8921.
        let crc = compute(b"123456789");
        assert_eq!(crc, 0x8921, "CRC-CCITT (KERMIT) of '123456789'");
    }

    #[test]
    fn crc_is_zero_after_appending_its_own_value() {
        // Receiver-side property: feeding the frame plus its
        // computed BCS yields zero.
        let payload = b"HELLO ACARS";
        let crc = compute(payload);
        let mut frame = payload.to_vec();
        frame.push((crc & 0xFF) as u8);   // BCS low
        frame.push((crc >> 8) as u8);     // BCS high
        // Fold the entire frame through the CRC; result MUST be zero
        // for a correctly-formed transmission.
        assert_eq!(compute(&frame), 0);
    }
}
```

- [ ] **Step 3: Run the tests, see them fail because the module isn't wired in lib.rs**

```bash
cargo test -p sdr-acars crc::tests 2>&1 | tail -20
```

It will fail compilation because `crc.rs` was already declared in `lib.rs` Step 6 — actually wait, it WAS declared in Step 6's `pub mod crc;`. So this should compile and pass.

```bash
cargo test -p sdr-acars crc::tests
# Expected: 2 passed.
```

If it fails because the polynomial or bit direction is wrong, consult `original/acarsdec/acars.c` for the canonical implementation and adjust.

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-acars/src/crc.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): CRC-CCITT-16 (KERMIT variant) for ACARS frames

LSB-first byte feed, init 0xFFFF, reflected polynomial 0x8408.
Verified against the canonical "123456789" → 0x8921 vector and
the receiver-side "frame + its CRC = 0" property.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Syndrom FEC table + parity-error correction

**Files:**
- Create: `crates/sdr-acars/src/syndrom.rs`

ACARS uses 7-bit ASCII with odd parity per character. Bytes that fail parity are flagged for correction. `acars.c` runs two correction passes:

1. `fixprerr`: for bytes with parity error, try flipping each of 8 bit positions; check if the resulting frame's CRC syndrome matches a known single-bit-error pattern from `syndrom.h`. Recursive over multiple parity errors (up to `MAXPERR = 3`).
2. `fixdberr`: for frames where parity all looks fine but CRC fails, try every pair of bit flips in any byte, checking the syndrom table for a match.

C reference:
- `original/acarsdec/syndrom.h` — `static const struct sd { unsigned char b[2]; unsigned char e; } SD[256][4]` table, ~295 lines
- `original/acarsdec/acars.c:45-50` — `fixprerr` (recursive single-parity-error fixer)
- `original/acarsdec/acars.c:66-90` — `fixdberr` (double-error fixer)

- [ ] **Step 1: Read syndrom.h to understand the table layout**

```bash
head -50 original/acarsdec/syndrom.h
```

The structure: each entry `SD[s][i]` (where `s` is a CRC syndrome byte, `i` is one of up to 4 alternative locations) maps a syndrome to the byte position(s) and bit mask(s) where the corresponding error occurred.

- [ ] **Step 2: Port the table verbatim**

Create `crates/sdr-acars/src/syndrom.rs`:

```rust
//! Reed-Solomon-style FEC for ACARS character frames. Uses a
//! pre-computed syndrome → error-location table to map a CRC
//! syndrome (post-frame-CRC) back to a probable single-byte
//! error and its bit position. Multiple parity errors are
//! handled by recursive single-bit fixes; if parity all looks
//! clean but the CRC fails, [`fix_double_error`] tries pairs
//! of bit flips.
//!
//! Faithful port of `original/acarsdec/syndrom.h` (table) and
//! `original/acarsdec/acars.c::fixprerr` / `fixdberr` (logic).
//! Per CR memory `feedback_port_fidelity.md`: don't simplify
//! beyond what's needed for safe Rust.

/// One alternative error-location entry. `byte_offset` is the
/// position within the frame (counting from the start of Mode);
/// `bit_mask` is the single-bit XOR that recovers the original
/// byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorLocation {
    pub byte_offset: u8,
    pub bit_mask: u8,
}

/// Up to 4 alternative locations per syndrome value (some
/// syndromes are ambiguous in ACARS' code geometry).
pub type SyndromeEntry = [Option<ErrorLocation>; 4];

/// Generated from `original/acarsdec/syndrom.h`. 256 entries
/// (one per syndrome byte), each with up to 4 alternatives.
///
/// CONVERSION INSTRUCTIONS for the implementer: open
/// `original/acarsdec/syndrom.h`, read the `SD[256][4]` array,
/// and produce this table. Each C entry `{0xAA, 0xBB, 0xCC}`
/// becomes `Some(ErrorLocation { byte_offset: 0xAA, bit_mask: 0xCC })`
/// (the second C byte is unused per acarsdec's structure layout
/// — verify by reading the header). Empty C entries (all zero)
/// become `None`.
///
/// This is mechanical translation. Don't paraphrase. The full
/// table is ~256 entries × 4 alternatives = up to 1024
/// `Option<ErrorLocation>` values; expect ~10-20 KB of source.
pub const SYNDROM_TABLE: [SyndromeEntry; 256] = {
    // IMPLEMENTER: paste the translated table here.
    // The compile will fail until the placeholder is replaced.
    [[None; 4]; 256]
};

/// Maximum number of parity errors `fix_parity_errors` will
/// attempt to correct. Matches acarsdec's `MAXPERR` define.
pub const MAX_PARITY_ERRORS: usize = 3;

/// Try to recover a frame with one or more parity errors by
/// flipping bits at the syndrome-indicated positions. Returns
/// `Some(corrected_count)` on success, `None` if the syndrome
/// doesn't match any known pattern after exhausting
/// alternatives. Caller passes a mutable frame buffer; this
/// function modifies it in place when corrections are applied.
///
/// `parity_error_offsets` is the list of byte positions that
/// failed parity check. Mirrors `fixprerr` in acars.c:45-50.
pub fn fix_parity_errors(
    frame: &mut [u8],
    parity_error_offsets: &[usize],
    initial_syndrome: u16,
) -> Option<u8> {
    // IMPLEMENTER: port `fixprerr` from acars.c. Recursive
    // backtracking over each alternative in SYNDROM_TABLE for
    // each parity-flagged byte. Stack depth bounded by
    // MAX_PARITY_ERRORS.
    let _ = (frame, parity_error_offsets, initial_syndrome);
    None
}

/// For frames with no parity errors but a non-zero CRC
/// syndrome, try every pair of bit flips in every byte and
/// check whether the result satisfies the syndrome. Returns
/// `Some(())` on success (with corrections applied to `frame`),
/// `None` otherwise. Mirrors `fixdberr` in acars.c:66-90.
pub fn fix_double_error(frame: &mut [u8], syndrome: u16) -> Option<()> {
    // IMPLEMENTER: port `fixdberr` from acars.c.
    let _ = (frame, syndrome);
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn table_size_matches_spec() {
        // Pin the table dimensions so a future format change
        // (e.g. wider syndromes) can't silently shrink it.
        assert_eq!(SYNDROM_TABLE.len(), 256);
        assert_eq!(SYNDROM_TABLE[0].len(), 4);
    }

    #[test]
    fn at_least_one_syndrome_has_a_concrete_location() {
        // Sanity check that the table isn't all-None — catches
        // the placeholder being left in by mistake.
        let any_some = SYNDROM_TABLE
            .iter()
            .flat_map(|alts| alts.iter())
            .any(|alt| alt.is_some());
        assert!(any_some, "SYNDROM_TABLE is all-None — table not ported");
    }

    // IMPLEMENTER: add per-syndrome tests for at least 5
    // representative entries, picking syndromes from acarsdec's
    // syndrom.h that have known error patterns. Use:
    //   1. A known-good 13-byte frame (Mode + 7-byte Address +
    //      1-byte ACK + 2-byte Label + 1-byte Block ID + STX).
    //   2. Flip one bit at a known position.
    //   3. Compute the CRC syndrome.
    //   4. Verify SYNDROM_TABLE[syndrome] points to that
    //      bit position.
}
```

- [ ] **Step 3: Run the tests, see them fail at the all-None assert**

```bash
cargo test -p sdr-acars syndrom::tests
# Expected: `at_least_one_syndrome_has_a_concrete_location` FAILS — placeholder still in.
```

- [ ] **Step 4: Translate the table from `syndrom.h`**

Read `original/acarsdec/syndrom.h` and replace `[[None; 4]; 256]` with the translated entries.

This is mechanical — don't paraphrase, don't reorder. If the C struct has fields you can't readily map, stop and ask: ambiguous translation here is much worse than asking.

- [ ] **Step 5: Implement `fix_parity_errors` and `fix_double_error`**

Port `fixprerr` (acars.c:45-50) and `fixdberr` (acars.c:66-90) faithfully. The recursion in `fixprerr` is bounded by `MAX_PARITY_ERRORS`; convert to iterative with an explicit stack if Rust's borrow checker fights you, but prefer a recursive translation that mirrors the C structure.

- [ ] **Step 6: Add the per-syndrome tests as marked in the test stub**

Pick 5 representative syndromes. Use the procedure in the comment.

- [ ] **Step 7: Run all tests**

```bash
cargo test -p sdr-acars syndrom::tests
# Expected: all pass.
```

- [ ] **Step 8: Commit**

```bash
git add crates/sdr-acars/src/syndrom.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): syndrom table + parity-error FEC correction

Faithful port of original/acarsdec/{syndrom.h,acars.c} —
SYNDROM_TABLE[256][4] mapping CRC syndromes to error locations,
plus fix_parity_errors (acars.c::fixprerr) and fix_double_error
(acars.c::fixdberr). Pinned with table-size + concrete-location
asserts plus 5 per-syndrome correction round-trips.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Label name lookup table

**Files:**
- Create: `crates/sdr-acars/src/label.rs`

A static `(label_code, human_name)` table. ACARS labels are 2 ASCII characters; the table maps each known label to a human-readable description. Per-label *field parsers* (extracting structured data from the message body per label) are out of scope here — see issue #577.

C reference: `original/acarsdec/label.c` — search for the `Lbl[]` array (a `struct { char* l; char* n; }` static initializer near the top of the file, ~150 entries).

- [ ] **Step 1: Find the table in `label.c`**

```bash
grep -n 'Lbl\[\]\|^{$\|"Q' original/acarsdec/label.c | head -20
```

- [ ] **Step 2: Write the failing test**

In `crates/sdr-acars/src/label.rs`:

```rust
//! Label name lookup. Each ACARS message carries a 2-byte
//! label that identifies its category (Q0 = link test, H1 =
//! crew message, B1 = weather, etc.). This module ships the
//! human-readable name for each known label. Per-label
//! structured-field parsers are deferred to issue #577.
//!
//! Faithful port of `original/acarsdec/label.c::Lbl[]`.

/// One row in the label table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelEntry {
    /// 2-byte label code as transmitted (e.g. `b"H1"`).
    pub code: [u8; 2],
    /// Human-readable description.
    pub name: &'static str,
}

/// All known ACARS labels with descriptions, in the order
/// `acarsdec` ships them. ~150 entries.
pub const LABELS: &[LabelEntry] = &[
    // IMPLEMENTER: paste the translated `Lbl[]` from
    // `original/acarsdec/label.c` here. Each C entry of the
    // shape { "H1", "Message to/from terminal" } translates to:
    //   LabelEntry { code: *b"H1", name: "Message to/from terminal" },
    // Preserve the C ordering so the table is grep-comparable
    // against the reference.
];

/// Look up the human-readable name for a 2-byte label code.
/// Returns `None` if the label isn't in the table.
#[must_use]
pub fn lookup(code: [u8; 2]) -> Option<&'static str> {
    LABELS
        .iter()
        .find(|entry| entry.code == code)
        .map(|entry| entry.name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn table_is_non_empty() {
        // Catches the placeholder being left in.
        assert!(!LABELS.is_empty(), "LABELS table not ported");
    }

    #[test]
    fn well_known_labels_resolve() {
        // Spot-check the most-common labels. If acarsdec's
        // table renames or removes any of these we want to
        // know loudly.
        assert!(lookup(*b"H1").is_some(), "H1 (crew message) missing");
        assert!(lookup(*b"Q0").is_some(), "Q0 (link test) missing");
        assert!(lookup(*b"_d").is_some(), "_d (misc downlink) missing");
        assert!(lookup(*b"B1").is_some(), "B1 (weather) missing");
    }

    #[test]
    fn unknown_label_returns_none() {
        // Sentinel: any non-ASCII pair must miss.
        assert_eq!(lookup([0xFF, 0xFF]), None);
    }
}
```

- [ ] **Step 3: Run, see it fail at `table_is_non_empty`**

```bash
cargo test -p sdr-acars label::tests
# Expected: FAIL.
```

- [ ] **Step 4: Translate `Lbl[]` from `label.c`**

Mechanical translation. Preserve order. ~150 entries.

- [ ] **Step 5: Run, expect pass**

```bash
cargo test -p sdr-acars label::tests
# Expected: 3 passed.
```

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-acars/src/label.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): port Lbl[] label-name table from acarsdec

~150 labels with descriptions, lookup() helper, spot-check
tests for H1/Q0/_d/B1. Per-label structured-field parsers
deferred to #577.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: MSK demodulator

**Files:**
- Create: `crates/sdr-acars/src/msk.rs`

This is the heart of the decoder. Port `original/acarsdec/msk.c` (138 LOC). The function `demodMSK(channel_t *ch, int len)` consumes `ch->dm_buffer` (real `f32` samples at 12500 Hz) and emits one bit per call to `putbit()` whenever the bit clock crosses 3π/2.

Internal state: VCO phase, bit clock, `inb[FLEN]` circular buffer of complex baseband samples, matched-filter coefficients `h[FLENO]` (FLENO = FLEN×MFLTOVER+1), PLL frequency offset `MskDf`, sequence counter `MskS`, level accumulator.

Constants:
- `INTRATE = 12500` (acarsdec.h:31)
- `FLEN = INTRATE / 1200 + 1 = 12` (msk.c:25)
- `MFLTOVER = 12` (msk.c:26)
- `FLENO = FLEN * MFLTOVER + 1 = 145` (msk.c:27)
- `PLLG = 38e-4`, `PLLC = 0.52` (msk.c:65-66)

C reference:
- `original/acarsdec/msk.c:30-51` — `initMsk` (one-time init, builds `h[]`)
- `original/acarsdec/msk.c:53-63` — `putbit` (shift-into-byte, emit byte to frame parser)
- `original/acarsdec/msk.c:67-137` — `demodMSK` main loop

- [ ] **Step 1: Read `msk.c` end-to-end**

```bash
cat original/acarsdec/msk.c
```

Note carefully: `inb[]` is `float complex` (C99), `dm_buffer[]` is `float`, `h[]` is `float`. The demod consumes a real signal and internally builds a complex baseband via the `cexp(-p*I)` mixer.

- [ ] **Step 2: Sketch the Rust types**

In `crates/sdr-acars/src/msk.rs`:

```rust
//! MSK (minimum-shift keying) demodulator at 2400 baud over
//! 1200/2400 Hz tones. Faithful port of
//! `original/acarsdec/msk.c`.
//!
//! Consumes real `f32` audio at 12500 Hz (the IF rate after
//! per-channel decimation). Internally builds a complex
//! baseband via a 1800 Hz VCO mixer, applies a 145-tap
//! matched filter, and emits one bit per 5.2 audio samples
//! (= 12500 / 2400). Bit timing is recovered by a Gardner-
//! style PLL on the matched-filter quadrature output.
//!
//! Output bits are pushed to a [`BitSink`] one at a time;
//! the [`crate::frame::FrameParser`] is the production sink.

use num_complex::Complex32;

/// IF sample rate this demod expects. Source-rate IQ must be
/// decimated to this rate before reaching the demod.
pub const IF_RATE_HZ: u32 = 12_500;

/// Matched-filter length in IF samples (~one bit at 1200 Hz).
const FLEN: usize = (IF_RATE_HZ as usize / 1200) + 1;

/// Matched-filter oversampling factor (acarsdec MFLTOVER).
const MFLT_OVER: usize = 12;

/// Total length of the upsampled matched filter coefficients.
const FLEN_OVERSAMPLED: usize = FLEN * MFLT_OVER + 1;

/// PLL gain (acarsdec PLLG).
const PLL_GAIN: f32 = 38e-4;
/// PLL low-pass coefficient (acarsdec PLLC).
const PLL_COEF: f32 = 0.52;

/// Receiver of demodulated bits from [`MskDemod`]. The frame
/// parser implements this; tests can implement it to capture
/// the output.
pub trait BitSink {
    /// One bit per call. `value > 0.0` is a binary 1, `<= 0.0`
    /// is a binary 0 (acarsdec convention — see msk.c::putbit).
    fn put_bit(&mut self, value: f32);
}

/// MSK demodulator state for a single ACARS channel.
pub struct MskDemod {
    /// VCO phase (radians).
    msk_phi: f64,
    /// Bit-clock phase accumulator.
    msk_clk: f64,
    /// Bit-position counter (acarsdec MskS).
    msk_s: u32,
    /// PLL frequency offset (acarsdec MskDf).
    msk_df: f32,
    /// Circular buffer of post-mixer baseband samples.
    inb: [Complex32; FLEN],
    /// Write index into `inb`.
    idx: usize,
    /// Per-frame matched-filter level accumulator.
    pub(crate) lvl_sum: f32,
    /// Bit-count for the current level window.
    pub(crate) bit_count: u32,
    /// Matched-filter coefficients, oversampled.
    /// One copy per channel — small (145 floats, ~580 bytes).
    /// Acarsdec's static singleton is a C optimization we
    /// don't replicate; the per-channel cost is negligible.
    h: [f32; FLEN_OVERSAMPLED],
}

impl MskDemod {
    /// Create a new demodulator with cleared state.
    #[must_use]
    pub fn new() -> Self {
        let mut h = [0.0_f32; FLEN_OVERSAMPLED];
        for (i, slot) in h.iter_mut().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let arg = 2.0 * core::f32::consts::PI * 600.0
                / (IF_RATE_HZ as f32 * MFLT_OVER as f32)
                * (i as f32 - (FLEN_OVERSAMPLED as f32 - 1.0) / 2.0);
            let c = arg.cos();
            *slot = if c < 0.0 { 0.0 } else { c };
        }
        Self {
            msk_phi: 0.0,
            msk_clk: 0.0,
            msk_s: 0,
            msk_df: 0.0,
            inb: [Complex32::new(0.0, 0.0); FLEN],
            idx: 0,
            lvl_sum: 0.0,
            bit_count: 0,
            h,
        }
    }

    /// Consume `samples` (real f32 at IF_RATE_HZ) and emit
    /// bits via `sink`. Mirrors `demodMSK(ch, len)` in
    /// msk.c:67-137. See that function for the algorithm.
    pub fn process(&mut self, samples: &[f32], sink: &mut impl BitSink) {
        // IMPLEMENTER: faithful translation of msk.c:67-137.
        // The Rust port is structurally identical to the C —
        // VCO advance, mixer into inb[], bit-clock check,
        // matched-filter inner product, normalize, quadrature
        // discriminator for dphi, PLL update, putbit().
        let _ = (samples, sink);
        unimplemented!("port msk.c:67-137 here");
    }
}

impl Default for MskDemod {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Sink that captures bits into a Vec for assertions.
    struct CapturingSink {
        bits: Vec<bool>,
    }

    impl BitSink for CapturingSink {
        fn put_bit(&mut self, value: f32) {
            self.bits.push(value > 0.0);
        }
    }

    #[test]
    fn demod_produces_no_bits_from_silence() {
        // Property: zero-amplitude input shouldn't generate
        // spurious bit transitions through the bit-clock
        // window. Catches obvious bugs like clock-divides-by-
        // zero or NaN propagation.
        let mut demod = MskDemod::new();
        let mut sink = CapturingSink { bits: Vec::new() };
        let silence = vec![0.0_f32; 12_500]; // 1 second of silence
        demod.process(&silence, &mut sink);
        // The PLL ticks regardless of input; bits will fire,
        // but the values shouldn't be NaN. We check that
        // `process` doesn't panic and the level accumulator
        // stays finite.
        assert!(demod.lvl_sum.is_finite(), "lvl_sum became NaN/Inf");
    }

    #[test]
    fn demod_advances_phase_state() {
        // After a non-empty `process` call, internal state must
        // have moved. Catches a no-op implementation.
        let mut demod = MskDemod::new();
        let mut sink = CapturingSink { bits: Vec::new() };
        let initial_phi = demod.msk_phi;
        demod.process(&vec![0.0_f32; 1000], &mut sink);
        assert_ne!(demod.msk_phi, initial_phi, "VCO phase did not advance");
    }

    // NOTE: MSK correctness on real signals is validated by the
    // e2e test against acarsdec's test.wav (Task 11). Synthetic
    // MSK generation for unit testing is non-trivial; we trust
    // the e2e diff for the correctness oracle and keep
    // unit tests here to lifecycle invariants only.
}
```

- [ ] **Step 3: Run the tests, see them fail at `unimplemented!`**

```bash
cargo test -p sdr-acars msk::tests 2>&1 | tail -10
# Expected: panic on unimplemented.
```

- [ ] **Step 4: Port `demodMSK`**

Translate `msk.c:67-137` line-by-line into `MskDemod::process`. Type mapping:

| C | Rust |
|---|---|
| `float complex v` | `Complex32` (`num_complex::Complex<f32>`) |
| `cexp(-p*I)` | `Complex32::from_polar(1.0, -p as f32)` (or build directly: `Complex32::new(p.cos() as f32, -(p.sin() as f32))`) |
| `cabsf(v)` | `v.norm()` |
| `crealf(v)` | `v.re` |
| `cimagf(v)` | `v.im` |
| `(j+idx)%FLEN` | `(j + idx) % FLEN` (Rust's `%` matches C for non-negative inputs) |

The bit-clock threshold check `if (ch->MskClk >= 3*M_PI/2.0 - s/2)` and the level/dphi/putbit logic must be byte-faithful — this is the bit-recovery PLL and small deviations will desync.

- [ ] **Step 5: Run lifecycle tests**

```bash
cargo test -p sdr-acars msk::tests
# Expected: 2 passed.
```

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-acars/src/msk.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): port demodMSK from acarsdec/msk.c

PLL + matched filter at 12.5 kHz IF rate, emits bits through a
BitSink trait the frame parser will implement. Lifecycle tests
pin no-NaN-on-silence and phase-advance-on-non-empty-input;
correctness on real signals is validated by the e2e test
against acarsdec's test.wav (Task 11).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Frame parser state machine

**Files:**
- Create: `crates/sdr-acars/src/frame.rs`

Streaming bit→byte→frame consumer. Implements `BitSink` (so it can be plugged into `MskDemod`). Its state machine:

```
WSYN  → seen 0x16  → SYN2
SYN2  → seen 0x16  → SOH1   (or seen 0xE9 = ~SYN → flip MskS, retry)
SOH1  → seen 0x01  → TXT
TXT   → accumulate up to 240 bytes; on 0x03 (ETX) or 0x17 (ETB)
                    → CRC1
CRC1  → 1 byte     → CRC2
CRC2  → 1 byte     → END    (validate parity, CRC, FEC; emit AcarsMessage)
END   → reset for next frame
```

C reference:
- `original/acarsdec/acars.c:88-90` — state enum
- `original/acarsdec/acars.c:138` — parity check (`numbits[byte] & 1`)
- `original/acarsdec/acars.c:159-165` — CRC verify
- `original/acarsdec/acars.c:225` — message-queue thread (we don't need this in a single-threaded port; just emit synchronously)
- `original/acarsdec/acars.c:230-end` — main `decodeAcars()`
- `original/acarsdec/acars.c:259, 274` — invert-SYN handling (toggle `MskS ^= 2` to recover from 180° phase slip)

- [ ] **Step 1: Read `acars.c::decodeAcars`**

```bash
sed -n '200,388p' original/acarsdec/acars.c
```

- [ ] **Step 2: Define the public types**

In `crates/sdr-acars/src/frame.rs`:

```rust
//! ACARS frame parser. Bit-by-bit streaming state machine that
//! consumes the output of [`crate::msk::MskDemod`] and emits
//! [`AcarsMessage`]s when complete frames pass parity + CRC
//! (with optional FEC recovery via [`crate::syndrom`]).
//!
//! Faithful port of `original/acarsdec/acars.c::decodeAcars`,
//! restructured into a single-threaded sync emitter (the C
//! version uses a worker thread + condition variable; we
//! pass messages out via a callback to keep the API simple
//! and avoid threading constraints inside the library crate).

use std::time::SystemTime;

use arrayvec::ArrayString;

use crate::msk::BitSink;

/// One decoded ACARS message.
#[derive(Clone, Debug)]
pub struct AcarsMessage {
    /// Wall-clock time when the closing bit arrived.
    pub timestamp: SystemTime,
    /// Channel index this message came from. `0` for the
    /// single-channel WAV-input path; `0..N` for `ChannelBank`.
    pub channel_idx: u8,
    /// Channel center frequency (Hz). `0.0` if unknown
    /// (e.g. WAV input where no center is supplied).
    pub freq_hz: f64,
    /// Matched-filter output magnitude in dB. Volatile —
    /// stripped from e2e diff.
    pub level_db: f32,
    /// Number of bytes corrected by parity FEC. Volatile —
    /// stripped from e2e diff.
    pub error_count: u8,
    /// Mode character (acarsdec field).
    pub mode: u8,
    /// 2-byte label code (e.g. b"H1").
    pub label: [u8; 2],
    /// Block ID (acarsdec field).
    pub block_id: u8,
    /// ACK character (acarsdec field).
    pub ack: u8,
    /// Aircraft registration including leading dot, e.g.
    /// ".N12345". 7 chars + leading dot = up to 8 chars.
    pub aircraft: ArrayString<8>,
    /// Optional flight ID (downlink only). 6 chars max.
    pub flight_id: Option<ArrayString<7>>,
    /// Optional message number. 4 chars max.
    pub message_no: Option<ArrayString<5>>,
    /// Variable-length text body. Up to ~220 bytes.
    pub text: String,
    /// `true` if the closing byte was `ETX` (final block);
    /// `false` if `ETB` (multi-block, more to come — see #580).
    pub end_of_message: bool,
}

/// Internal state of the byte-level state machine. Mirrors
/// the enum in acars.c:88.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    WaitingSyn,
    Syn2,
    SeekingSoh,
    Text,
    Crc1,
    Crc2,
    End,
}

/// Frame parser. One per channel.
pub struct FrameParser {
    state: State,
    /// Bits accumulated for the current byte (LSB-first).
    out_bits: u8,
    /// How many bits remain to fill `out_bits`.
    n_bits: u8,
    /// Bytes accumulated for the current frame (Mode through
    /// last text byte, NOT including the trailing CRC).
    buf: Vec<u8>,
    /// Per-character parity error positions in `buf`.
    parity_errors: Vec<usize>,
    /// Running CRC over `buf` plus the two CRC bytes.
    crc: u16,
    /// Channel index to stamp into emitted messages.
    channel_idx: u8,
    /// Channel center frequency to stamp into emitted messages.
    channel_freq_hz: f64,
}

impl FrameParser {
    /// Create a parser stamping the given channel index + freq
    /// onto every emitted message.
    #[must_use]
    pub fn new(channel_idx: u8, channel_freq_hz: f64) -> Self {
        Self {
            state: State::WaitingSyn,
            out_bits: 0,
            n_bits: 8,
            buf: Vec::with_capacity(256),
            parity_errors: Vec::new(),
            crc: 0x0000,  // ACARS uses KERMIT init=0, not X-25 init=0xFFFF — see crc.rs Task 2 implementer notes
            channel_idx,
            channel_freq_hz,
        }
    }

    /// Reset to look for the next frame's preamble. Called
    /// internally on END or on a hard sync loss.
    fn reset_to_idle(&mut self) {
        self.state = State::WaitingSyn;
        self.out_bits = 0;
        self.n_bits = 8;
        self.buf.clear();
        self.parity_errors.clear();
        self.crc = 0x0000;
    }

    /// Consume one fully-assembled byte. Drives the state
    /// machine; emits an `AcarsMessage` via `on_message` when
    /// CRC2 closes a successful frame. Mirrors the byte-level
    /// switch in acars.c::decodeAcars.
    fn consume_byte<F: FnMut(AcarsMessage)>(
        &mut self,
        byte: u8,
        on_message: &mut F,
    ) {
        // IMPLEMENTER: port the byte-level state machine from
        // acars.c::decodeAcars. Key transitions:
        //   * WaitingSyn:  if byte == 0x16, transition to Syn2.
        //   * Syn2:        if byte == 0x16, transition to SeekingSoh.
        //                  else if byte == !0x16 (= 0xE9), reset
        //                  AND signal MSK polarity flip to caller
        //                  (TODO: how do we communicate this back?
        //                  store a `polarity_flip_pending: bool`
        //                  field; FrameParser exposes a public
        //                  `take_polarity_flip()` that the channel
        //                  layer polls each `process()` cycle).
        //   * SeekingSoh:  if byte == 0x01 (SOH), → Text + buf=[byte].
        //   * Text:        push byte to buf; check parity; on
        //                  parity error append index to
        //                  parity_errors. If byte == 0x03 (ETX) or
        //                  0x17 (ETB), → Crc1.
        //   * Crc1:        record first CRC byte → Crc2.
        //   * Crc2:        record second CRC byte; verify CRC over
        //                  buf+crc1+crc2 == 0; if not, attempt FEC
        //                  via syndrom::fix_parity_errors (then
        //                  fix_double_error if still bad). If a
        //                  good frame falls out, parse the field
        //                  layout (Mode, Address, ACK, Label, etc.
        //                  per acars.c) into AcarsMessage and call
        //                  on_message. Reset to WaitingSyn.
        let _ = (byte, on_message);
        unimplemented!("port acars.c::decodeAcars byte handler here");
    }

    /// Convenience: drive the parser with a sequence of fully-
    /// formed bytes, useful for unit tests.
    pub fn feed_bytes<F: FnMut(AcarsMessage)>(
        &mut self,
        bytes: &[u8],
        mut on_message: F,
    ) {
        for &b in bytes {
            self.consume_byte(b, &mut on_message);
        }
    }
}

impl BitSink for FrameParser {
    fn put_bit(&mut self, value: f32) {
        // Shift the bit into the byte register LSB-first
        // (matches acarsdec putbit). When the byte fills, hand
        // it to the state machine. The state machine itself is
        // driven by `consume_byte` rather than embedded here so
        // unit tests can inject hand-crafted byte sequences.
        self.out_bits >>= 1;
        if value > 0.0 {
            self.out_bits |= 0x80;
        }
        self.n_bits -= 1;
        if self.n_bits == 0 {
            self.n_bits = 8;
            let byte = self.out_bits;
            self.out_bits = 0;
            // We need a way to call `consume_byte` here — but
            // BitSink::put_bit doesn't have access to the
            // user's `on_message` callback.
            //
            // IMPLEMENTER: there are two clean ways to resolve
            // this (pick one):
            //
            //   (a) Buffer completed bytes into `self.buf_pending`
            //       Vec<u8>; have the caller drain via a
            //       `FrameParser::drain(on_message)` method
            //       called after each MskDemod::process round.
            //
            //   (b) Make put_bit itself store a callback. This
            //       requires changing the BitSink trait or
            //       using Box<dyn FnMut> on the parser.
            //
            // RECOMMEND (a) — keeps BitSink simple and matches
            // the "callback at the API edge" pattern already in
            // ChannelBank::process. Add a `pending_bytes:
            // Vec<u8>` field, drain in `drain(on_message)`.
            //
            // Update consume_byte's call sites accordingly:
            // tests call feed_bytes(); production calls
            // drain(on_message) after each demod block.
            let _ = byte;
        }
    }
}
```

- [ ] **Step 3: Resolve the BitSink↔callback impedance per recommendation (a) above**

Refine the design as recommended in the comment: add a `pending_bytes: Vec<u8>` field and a `pub fn drain<F>(&mut self, on_message: F)`. `BitSink::put_bit` stays callback-free; production code calls `drain` after each `MskDemod::process` round.

- [ ] **Step 4: Implement `consume_byte` per the C reference**

Port `acars.c:230-end` faithfully. The field-layout parsing inside Crc2's success branch is the meatiest part — read the C carefully.

For the polarity-flip signal (Syn2 sees `~SYN`), expose a `take_polarity_flip(&mut self) -> bool` so the channel layer can read it and update its `MskDemod`'s `MskS` accordingly.

- [ ] **Step 5: Write unit tests with hand-crafted bytes**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Build a known-good ACARS frame as a byte sequence ready
    /// to feed into FrameParser. Address ".N12345", label "H1",
    /// block "0", text "TEST".
    fn synthesize_minimal_frame() -> Vec<u8> {
        // [SYN][SYN][SOH][Mode][Addr×7][ACK][Label×2][BlockID]
        //   [STX][text...][ETX][CRC1][CRC2]
        let mut buf = vec![0x16, 0x16, 0x01];
        buf.push(b'2');                 // Mode
        buf.extend_from_slice(b".N12345"); // Address (7 bytes)
        buf.push(b'!');                 // ACK = 0x21
        buf.extend_from_slice(b"H1");   // Label
        buf.push(b'0');                 // Block ID
        buf.push(0x02);                 // STX
        buf.extend_from_slice(b"TEST"); // text
        buf.push(0x03);                 // ETX
        // The frame from Mode through ETX is what gets CRC'd.
        // CRC bytes follow — compute them.
        let crc_payload = &buf[3..]; // Mode through ETX
        let crc = crate::crc::compute(crc_payload);
        buf.push((crc & 0xFF) as u8);
        buf.push((crc >> 8) as u8);
        buf
    }

    /// Parity-byte every character in `s`. ACARS uses 7-bit
    /// ASCII with odd parity in bit 7.
    fn add_odd_parity(bytes: &mut [u8]) {
        for b in bytes.iter_mut() {
            let parity = (b.count_ones() & 1) ^ 1;
            *b |= (parity as u8) << 7;
        }
    }

    #[test]
    fn parses_a_known_good_frame() {
        let mut bytes = synthesize_minimal_frame();
        // Apply odd parity to the inner payload (Mode through ETX).
        // Don't touch SYN/SOH/CRC — those are not parity-protected.
        add_odd_parity(&mut bytes[3..bytes.len() - 2]);
        // Recompute CRC over the parity-applied payload.
        let payload_len = bytes.len() - 2;
        let crc = crate::crc::compute(&bytes[3..payload_len]);
        let n = bytes.len();
        bytes[n - 2] = (crc & 0xFF) as u8;
        bytes[n - 1] = (crc >> 8) as u8;

        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(&bytes, |msg| decoded.push(msg));

        assert_eq!(decoded.len(), 1, "expected exactly one frame");
        let msg = &decoded[0];
        assert_eq!(msg.mode, b'2');
        assert_eq!(&msg.aircraft[..], ".N12345");
        assert_eq!(msg.label, *b"H1");
        assert_eq!(msg.block_id, b'0');
        assert_eq!(msg.text, "TEST");
        assert!(msg.end_of_message);
    }

    #[test]
    fn rejects_a_corrupted_frame_when_fec_cant_recover() {
        let mut bytes = synthesize_minimal_frame();
        add_odd_parity(&mut bytes[3..bytes.len() - 2]);
        // Wreck the CRC bytes.
        let n = bytes.len();
        bytes[n - 2] = 0x00;
        bytes[n - 1] = 0x00;

        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(&bytes, |msg| decoded.push(msg));

        assert!(decoded.is_empty(), "corrupted frame must not decode");
    }

    #[test]
    fn ignores_bytes_outside_a_frame() {
        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(b"\x00\xFF\x00\xFF\x00", |msg| decoded.push(msg));
        assert!(decoded.is_empty());
    }
}
```

- [ ] **Step 6: Run, expect pass**

```bash
cargo test -p sdr-acars frame::tests
# Expected: 3 passed.
```

- [ ] **Step 7: Commit**

```bash
git add crates/sdr-acars/src/frame.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): port ACARS frame parser state machine

Bit-by-bit streaming consumer (BitSink for MskDemod), drain()-
based byte processing, full state machine WSYN→SYN2→SOH1→TXT→
CRC1→CRC2→END with parity + CRC + FEC recovery (via syndrom
module). Hand-crafted unit tests pin minimal-frame parse,
corrupted-CRC rejection, and ignore-outside-frame.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Per-channel decimator + `ChannelBank`

**Files:**
- Create: `crates/sdr-acars/src/channel.rs`

Source-rate complex IQ → per-channel oscillator+decimator → 12.5 kHz real audio → `MskDemod` → `FrameParser`. The oscillator is a pre-computed `Vec<Complex32>` of length `decim_factor` (the number of input samples per output sample). The decimator accumulates `iq[i] * osc[i]` over one decim period and outputs the magnitude (AM detection). `ChannelBank` is a `Vec<Channel>` plus a `process()` orchestrator.

C reference: `original/acarsdec/air.c` and `rtl.c`. Key snippets:
- `air.c:278-284` — per-channel `wf[]` oscillator init
- `air.c:300-340` — main per-block loop: complex mixer + accumulator + AM-detect → `dm_buffer` → `demodMSK`

- [ ] **Step 1: Read `air.c` (channel-level decimation)**

```bash
sed -n '250,360p' original/acarsdec/air.c
```

- [ ] **Step 2: Sketch and implement**

In `crates/sdr-acars/src/channel.rs`:

```rust
//! Multi-channel ACARS decoder. Source-rate complex IQ feeds
//! N parallel per-channel pipelines (oscillator + decimator
//! → AM detect → MSK demod → frame parser).
//!
//! Faithful port of `original/acarsdec/air.c` per-channel
//! decimation — the IQ-fork pattern. Single-threaded inline
//! processing per `process()` call; no internal threads, no
//! mutex.

use num_complex::Complex32;

use crate::error::AcarsError;
use crate::frame::{AcarsMessage, FrameParser};
use crate::msk::{IF_RATE_HZ, MskDemod};

/// Per-channel state. Owns its oscillator, decimator
/// accumulator, MSK demod, and frame parser.
pub struct Channel {
    /// Channel center freq (Hz).
    freq_hz: f64,
    /// Frequency offset from source center (Hz).
    offset_hz: f64,
    /// Pre-computed complex exponential at `offset_hz`,
    /// stepped by source-rate sample period.
    oscillator: Vec<Complex32>,
    /// Where in `oscillator` we are this block.
    osc_idx: usize,
    /// Decimation accumulator state.
    accum: Complex32,
    /// Counter within the current decim period.
    decim_count: u32,
    /// Decimation factor (source_rate / IF_RATE_HZ).
    decim_factor: u32,
    /// Buffer of decimated IF samples to feed into MskDemod.
    /// Sized = max expected IF samples per process() call.
    if_buffer: Vec<f32>,
    msk: MskDemod,
    parser: FrameParser,
}

/// Per-channel statistics for the UI panel and CLI status.
#[derive(Clone, Copy, Debug)]
pub struct ChannelStats {
    pub freq_hz: f64,
    pub last_msg_at: Option<std::time::SystemTime>,
    pub msg_count: u32,
    pub level_db: f32,
    pub lock_state: ChannelLockState,
}

/// Three-state indicator for the sidebar glyph (●/○/⚠).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelLockState {
    /// No RF energy detected.
    Idle,
    /// RF energy present but no decoded frames within the
    /// recent window.
    Signal,
    /// Recent frames decoded successfully.
    Locked,
}

/// Multi-channel orchestrator. One source-rate IQ stream feeds
/// N narrowband channels in parallel.
pub struct ChannelBank {
    channels: Vec<Channel>,
    stats: Vec<ChannelStats>,
}

impl ChannelBank {
    /// Build a bank for `channels` (Hz), where the source IQ is
    /// at `source_rate_hz` centered on `center_hz`. Source rate
    /// must be an integer multiple of [`IF_RATE_HZ`] (12500 Hz).
    /// Each channel's offset from `center_hz` must fit within
    /// the source bandwidth.
    pub fn new(
        source_rate_hz: f64,
        center_hz: f64,
        channels: &[f64],
    ) -> Result<Self, AcarsError> {
        if channels.is_empty() {
            return Err(AcarsError::InvalidChannelConfig(
                "channel list is empty".into(),
            ));
        }
        let if_rate = f64::from(IF_RATE_HZ);
        let decim_f = source_rate_hz / if_rate;
        if decim_f.fract().abs() > 1e-6 {
            return Err(AcarsError::NonIntegerDecimation {
                source_rate_hz,
                if_rate_hz: if_rate,
            });
        }
        let decim_factor = decim_f as u32;

        let mut built = Vec::with_capacity(channels.len());
        let mut stats = Vec::with_capacity(channels.len());
        for (idx, &freq_hz) in channels.iter().enumerate() {
            let offset_hz = freq_hz - center_hz;
            // Channel must fit in source bandwidth (Nyquist).
            if offset_hz.abs() > source_rate_hz / 2.0 {
                return Err(AcarsError::InvalidChannelConfig(format!(
                    "channel {freq_hz} Hz outside source bandwidth ({source_rate_hz} Hz centered on {center_hz} Hz)"
                )));
            }
            // Build the oscillator: complex exp at -offset_hz,
            // sampled at source rate. `decim_factor` samples
            // long (one decim period) — the actual "free
            // running" extension uses (osc_idx + n) wrap-around.
            let mut oscillator = Vec::with_capacity(decim_factor as usize);
            for n in 0..decim_factor {
                #[allow(clippy::cast_precision_loss)]
                let phase =
                    -2.0 * core::f64::consts::PI * offset_hz * f64::from(n) / source_rate_hz;
                #[allow(clippy::cast_possible_truncation)]
                oscillator.push(Complex32::new(
                    phase.cos() as f32,
                    phase.sin() as f32,
                ));
            }
            #[allow(clippy::cast_possible_truncation)]
            let idx_u8 = idx as u8;
            built.push(Channel {
                freq_hz,
                offset_hz,
                oscillator,
                osc_idx: 0,
                accum: Complex32::new(0.0, 0.0),
                decim_count: 0,
                decim_factor,
                if_buffer: Vec::with_capacity(4096),
                msk: MskDemod::new(),
                parser: FrameParser::new(idx_u8, freq_hz),
            });
            stats.push(ChannelStats {
                freq_hz,
                last_msg_at: None,
                msg_count: 0,
                level_db: -120.0,
                lock_state: ChannelLockState::Idle,
            });
        }
        Ok(Self { channels: built, stats })
    }

    /// Drain `iq` through every channel's pipeline, emitting
    /// any decoded messages via `on_message`. Mirrors `air.c`'s
    /// per-block accumulator loop, then drives MSK + frame
    /// parsing per channel.
    pub fn process<F: FnMut(AcarsMessage)>(
        &mut self,
        iq: &[Complex32],
        mut on_message: F,
    ) {
        for ch in &mut self.channels {
            ch.if_buffer.clear();
            for &sample in iq {
                let osc = ch.oscillator[ch.osc_idx];
                ch.osc_idx = (ch.osc_idx + 1) % ch.oscillator.len();
                ch.accum += sample * osc;
                ch.decim_count += 1;
                if ch.decim_count >= ch.decim_factor {
                    // AM-detect: magnitude of the accumulator.
                    let am_sample = ch.accum.norm();
                    ch.if_buffer.push(am_sample);
                    ch.accum = Complex32::new(0.0, 0.0);
                    ch.decim_count = 0;
                }
            }
            // Drive the MSK demod with the decimated IF samples.
            ch.msk.process(&ch.if_buffer, &mut ch.parser);
            // Drain any complete bytes accumulated in the parser.
            ch.parser.drain(|msg| on_message(msg));
            // Apply pending polarity flip if the parser detected
            // an inverted-SYN at frame start (acars.c:259,274).
            if ch.parser.take_polarity_flip() {
                ch.msk.toggle_polarity();
            }
        }
        // Stats refresh (level, lock state) is done lazily in
        // channels(); we just bump message counts here.
        // IMPLEMENTER: per-message stat updates can land here
        // in the on_message wrapper if useful.
        let _ = &self.stats; // silence unused warning until stats logic lands
    }

    /// Snapshot of per-channel stats.
    #[must_use]
    pub fn channels(&self) -> &[ChannelStats] {
        &self.stats
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_channel_list() {
        let err = ChannelBank::new(2_400_000.0, 130_450_000.0, &[]).unwrap_err();
        assert!(matches!(err, AcarsError::InvalidChannelConfig(_)));
    }

    #[test]
    fn rejects_non_integer_decimation() {
        let err = ChannelBank::new(2_400_001.0, 130_450_000.0, &[131_550_000.0])
            .unwrap_err();
        assert!(matches!(err, AcarsError::NonIntegerDecimation { .. }));
    }

    #[test]
    fn rejects_channel_outside_source_bandwidth() {
        let err = ChannelBank::new(
            2_400_000.0,
            130_450_000.0,
            &[200_000_000.0], // far outside 2.4 MHz window
        )
        .unwrap_err();
        assert!(matches!(err, AcarsError::InvalidChannelConfig(_)));
    }

    #[test]
    fn accepts_valid_us_six_config() {
        let bank = ChannelBank::new(
            2_400_000.0,
            130_450_000.0,
            &[
                129_125_000.0, 130_025_000.0, 130_425_000.0, 130_450_000.0,
                131_525_000.0, 131_550_000.0,
            ],
        )
        .unwrap();
        assert_eq!(bank.channels().len(), 6);
        assert_eq!(bank.channels()[0].freq_hz, 129_125_000.0);
    }

    #[test]
    fn process_silent_iq_doesnt_panic() {
        let mut bank = ChannelBank::new(
            2_400_000.0,
            130_450_000.0,
            &[131_550_000.0],
        )
        .unwrap();
        let silent = vec![Complex32::new(0.0, 0.0); 2400];
        bank.process(&silent, |_msg| {
            panic!("silence shouldn't produce messages");
        });
    }
}
```

NOTE on the `MskDemod::toggle_polarity()` call above: that method needs to exist on `MskDemod`. Add it in this same task as a small extension to `msk.rs`:

```rust
impl MskDemod {
    /// Flip the bit-polarity counter (acarsdec MskS ^= 2).
    /// Called by ChannelBank when the frame parser detects an
    /// inverted-SYN preamble, indicating the demodulator has a
    /// 180° phase ambiguity.
    pub fn toggle_polarity(&mut self) {
        self.msk_s ^= 2;
    }
}
```

- [ ] **Step 3: Run unit tests**

```bash
cargo test -p sdr-acars channel::tests
# Expected: 5 passed.
```

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-acars/src/channel.rs crates/sdr-acars/src/msk.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): per-channel decimator + ChannelBank orchestrator

Source-rate complex IQ → per-channel oscillator+decimator → AM
detect → 12.5 kHz IF → MskDemod → FrameParser. ChannelBank::new
validates source-rate / center-freq / channel-list configs;
ChannelBank::process drains all channels per call. Polarity-flip
handshake between FrameParser and MskDemod via toggle_polarity.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Public API + lib.rs re-exports

**Files:**
- Modify: `crates/sdr-acars/src/lib.rs`

Surface what consumers (sub-projects 2 + 3 and the CLI binary) actually need.

- [ ] **Step 1: Update `crates/sdr-acars/src/lib.rs`**

Replace its body with:

```rust
//! ACARS (Aircraft Communications Addressing and Reporting
//! System) decoder. Faithful Rust port of
//! [acarsdec](https://github.com/TLeconte/acarsdec) — pure
//! DSP + parsing, no GTK, no SDR-driver dependency.
//!
//! # Example: multi-channel decode from a 2.4 MSps complex IQ stream
//!
//! ```no_run
//! use num_complex::Complex32;
//! use sdr_acars::ChannelBank;
//!
//! const US_ACARS: &[f64] = &[
//!     129_125_000.0, 130_025_000.0, 130_425_000.0,
//!     130_450_000.0, 131_525_000.0, 131_550_000.0,
//! ];
//!
//! # fn read_iq_block() -> Vec<Complex32> { Vec::new() }
//! let mut bank =
//!     ChannelBank::new(2_400_000.0, 130_450_000.0, US_ACARS)?;
//! loop {
//!     let iq: Vec<Complex32> = read_iq_block();
//!     if iq.is_empty() { break; }
//!     bank.process(&iq, |msg| {
//!         println!("{} {} {}", msg.aircraft, &msg.label[..], msg.text);
//!     });
//! }
//! # Ok::<(), sdr_acars::AcarsError>(())
//! ```
//!
//! For pre-decimated 12.5 kHz IF input (e.g. WAV files written
//! by acarsdec's `--save` mode, one channel per WAV channel),
//! drive [`msk::MskDemod`] + [`frame::FrameParser`] directly
//! instead — see `bin/sdr-acars-cli.rs` for the WAV path.

pub mod channel;
pub mod crc;
pub mod error;
pub mod frame;
pub mod label;
pub mod msk;
pub mod syndrom;

pub use channel::{ChannelBank, ChannelLockState, ChannelStats};
pub use error::AcarsError;
pub use frame::{AcarsMessage, FrameParser};
pub use label::lookup as lookup_label;
pub use msk::{IF_RATE_HZ, MskDemod};
```

- [ ] **Step 2: Verify the doctest compiles**

```bash
cargo test -p sdr-acars --doc
# Expected: pass (the example uses #[no_run] so it just compiles, doesn't execute).
```

- [ ] **Step 3: Verify the whole crate still builds clean**

```bash
cargo build -p sdr-acars
cargo test -p sdr-acars
cargo clippy -p sdr-acars --all-targets -- -D warnings
# All green.
```

- [ ] **Step 4: Commit**

```bash
git add crates/sdr-acars/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(sdr-acars): public API + crate-level docs

Re-export ChannelBank, AcarsMessage, ChannelStats,
ChannelLockState, AcarsError, MskDemod, FrameParser, IF_RATE_HZ,
and lookup_label. Crate doc shows the multi-channel usage
pattern that sub-project 2 will adopt.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: CLI binary — file readers + acarsdec text printer + main

**Files:**
- Create: `crates/sdr-acars/src/bin/sdr-acars-cli.rs`

Two input modes:

1. **WAV input** (positional arg, e.g. `original/acarsdec/test.wav`): N-channel WAV at 12500 Hz, `i16` samples. Each WAV channel is one ACARS frequency, **already decimated** to IF rate. Bypass `ChannelBank`'s decimator stage; drive each channel's `MskDemod` + `FrameParser` directly. (acarsdec's `soundfile.c` does the same.) The `--channels` flag lists the per-WAV-channel center frequencies for the printer's "F:" line; if absent, default to the US-6 frequencies indexed by channel order.
2. **IQ input** (`--iq path --rate <Hz> --center <Hz> --channels <list>`): raw interleaved-`i16` complex samples (the `cs16` convention used by `rtl_sdr` recordings). Drive through `ChannelBank::new` + `process()` end-to-end.

Output format mirrors `acarsdec -o 1` (text mode). Per `original/acarsdec/output.c::printmsg`, the format is:

```
[#<chan> (L:<level> E:<errors>) <YYYY/MM/DD HH:MM:SS.mmm> ----------------]
Mode : <c> Label : <c><c> Id : <c> Ack : <c>
Aircraft reg: <addr> Flight id: <flight>
No: <msgno>
[ text body, possibly multi-line ]

```

(blank line trailing each message.)

Volatile fields (stripped from the e2e diff): the `#<chan>` sequence, `L:<level>`, `E:<errors>`, and the timestamp.

C reference: `original/acarsdec/output.c:30-180` (rough range — search for `printmsg`).

- [ ] **Step 1: Read `output.c::printmsg`**

```bash
grep -n 'printmsg\|fmtMsg\|fmt_text' original/acarsdec/output.c | head -10
```

- [ ] **Step 2: Implement the CLI**

Create `crates/sdr-acars/src/bin/sdr-acars-cli.rs`:

```rust
//! sdr-acars-cli — read a WAV or IQ file, decode ACARS
//! messages, print in the same text format as `acarsdec -o 1`.
//! Used as the validation harness for the Rust port: diffing
//! this binary's output against `acarsdec`'s on shared input
//! (with volatile fields stripped) is the acceptance test for
//! the DSP/parser correctness.

use std::{
    fs::File,
    io::{BufReader, Read, Write},
    path::PathBuf,
    time::SystemTime,
};

use clap::Parser;
use num_complex::Complex32;
use sdr_acars::{
    AcarsError, AcarsMessage, ChannelBank, FrameParser, MskDemod, IF_RATE_HZ,
};

/// US-6 default channel set (matches the spec).
const US_ACARS_CHANNELS: &[f64] = &[
    131_550_000.0, 131_525_000.0, 130_025_000.0, 130_425_000.0,
    130_450_000.0, 129_125_000.0,
];

#[derive(Parser, Debug)]
#[command(version, about = "ACARS decoder (Rust port of acarsdec)")]
struct Cli {
    /// WAV file (multi-channel @ IF_RATE_HZ). Positional.
    /// Mutually exclusive with --iq.
    #[arg(value_name = "WAV", conflicts_with = "iq")]
    wav: Option<PathBuf>,

    /// Raw cs16 IQ file (interleaved i16 I/Q at --rate).
    #[arg(long, value_name = "PATH", conflicts_with = "wav")]
    iq: Option<PathBuf>,

    /// Source sample rate in Hz (IQ mode only).
    #[arg(long, default_value_t = 2_400_000)]
    rate: u32,

    /// Source center frequency in Hz (IQ mode only).
    #[arg(long, default_value_t = 130_450_000)]
    center: u32,

    /// Channel list as comma-separated MHz (e.g.
    /// "131.550,131.525"). For WAV mode, indexes WAV channels
    /// in order; defaults to the US-6 set.
    #[arg(long, value_delimiter = ',', value_parser = parse_mhz)]
    channels: Option<Vec<f64>>,
}

fn parse_mhz(s: &str) -> Result<f64, String> {
    s.parse::<f64>()
        .map(|mhz| mhz * 1_000_000.0)
        .map_err(|e| format!("invalid frequency '{s}': {e}"))
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sdr-acars-cli: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), AcarsError> {
    let mut stdout = std::io::stdout().lock();
    let mut seq = 0_u32;

    if let Some(wav_path) = &cli.wav {
        decode_wav(wav_path, cli.channels.as_deref(), &mut stdout, &mut seq)
    } else if let Some(iq_path) = &cli.iq {
        decode_iq(
            iq_path,
            f64::from(cli.rate),
            f64::from(cli.center),
            cli.channels.as_deref().unwrap_or(US_ACARS_CHANNELS),
            &mut stdout,
            &mut seq,
        )
    } else {
        Err(AcarsError::InvalidInput(
            "no input file: pass a WAV path or --iq <PATH>".into(),
        ))
    }
}

/// Read an N-channel WAV at IF_RATE_HZ. Each channel is one
/// ACARS freq pre-decimated to IF rate; drive MskDemod +
/// FrameParser directly per channel.
fn decode_wav(
    path: &std::path::Path,
    user_channels: Option<&[f64]>,
    out: &mut impl Write,
    seq: &mut u32,
) -> Result<(), AcarsError> {
    let mut reader = hound::WavReader::open(path).map_err(|e| AcarsError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;
    let spec = reader.spec();
    if spec.sample_rate != IF_RATE_HZ {
        return Err(AcarsError::InvalidInput(format!(
            "WAV sample rate {} ≠ expected IF rate {IF_RATE_HZ}",
            spec.sample_rate
        )));
    }
    let n_channels = spec.channels as usize;
    let channels: Vec<f64> = match user_channels {
        Some(cs) if cs.len() == n_channels => cs.to_vec(),
        Some(cs) => {
            return Err(AcarsError::InvalidInput(format!(
                "WAV has {} channels but --channels provided {}",
                n_channels,
                cs.len()
            )));
        }
        None => US_ACARS_CHANNELS
            .iter()
            .copied()
            .take(n_channels)
            .collect(),
    };
    if channels.len() < n_channels {
        return Err(AcarsError::InvalidInput(format!(
            "WAV has {n_channels} channels but US-6 default only covers \
             {} — pass --channels explicitly",
            channels.len()
        )));
    }

    // One demod + parser per channel.
    let mut demods: Vec<MskDemod> = (0..n_channels).map(|_| MskDemod::new()).collect();
    let mut parsers: Vec<FrameParser> = channels
        .iter()
        .enumerate()
        .map(|(i, &f)| {
            #[allow(clippy::cast_possible_truncation)]
            FrameParser::new(i as u8, f)
        })
        .collect();

    // hound returns interleaved samples — split per channel.
    let mut per_channel: Vec<Vec<f32>> = vec![Vec::with_capacity(8192); n_channels];
    for (i, sample_result) in reader.samples::<i16>().enumerate() {
        let sample = sample_result.map_err(|e| AcarsError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        per_channel[i % n_channels].push(f32::from(sample) / f32::from(i16::MAX));
    }

    for (i, samples) in per_channel.iter().enumerate() {
        demods[i].process(samples, &mut parsers[i]);
        let mut emit_buf: Vec<AcarsMessage> = Vec::new();
        parsers[i].drain(|msg| emit_buf.push(msg));
        for msg in emit_buf {
            print_message(&msg, channels[i], seq, out)?;
        }
    }
    Ok(())
}

/// Read raw cs16 (interleaved i16 I/Q at `rate`) and drive
/// through ChannelBank.
fn decode_iq(
    path: &std::path::Path,
    rate: f64,
    center: f64,
    channels: &[f64],
    out: &mut impl Write,
    seq: &mut u32,
) -> Result<(), AcarsError> {
    let mut bank = ChannelBank::new(rate, center, channels)?;
    let file = File::open(path).map_err(|e| AcarsError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut reader = BufReader::new(file);
    let mut buf = vec![0_u8; 4096 * 4]; // 4096 IQ samples per block
    let mut block: Vec<Complex32> = Vec::with_capacity(4096);
    let mut emit_buf: Vec<(AcarsMessage, f64)> = Vec::new();

    loop {
        let n = reader.read(&mut buf).map_err(|e| AcarsError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 { break; }
        if n % 4 != 0 {
            return Err(AcarsError::InvalidInput(format!(
                "IQ file size mod 4 ≠ 0 (got partial sample, read {n} bytes)"
            )));
        }
        block.clear();
        for chunk in buf[..n].chunks_exact(4) {
            #[allow(clippy::cast_possible_wrap)]
            let i = i16::from_le_bytes([chunk[0], chunk[1]]);
            #[allow(clippy::cast_possible_wrap)]
            let q = i16::from_le_bytes([chunk[2], chunk[3]]);
            block.push(Complex32::new(
                f32::from(i) / f32::from(i16::MAX),
                f32::from(q) / f32::from(i16::MAX),
            ));
        }
        bank.process(&block, |msg| {
            let chan_freq = bank.channels()[msg.channel_idx as usize].freq_hz;
            emit_buf.push((msg, chan_freq));
        });
        for (msg, chan_freq) in emit_buf.drain(..) {
            print_message(&msg, chan_freq, seq, out)?;
        }
    }
    Ok(())
}

/// Format an AcarsMessage as one acarsdec-text record. Mirrors
/// `original/acarsdec/output.c::printmsg`. Volatile fields
/// (sequence, level, error count, timestamp) are emitted but
/// the e2e test strips them before diffing.
fn print_message(
    msg: &AcarsMessage,
    chan_freq_hz: f64,
    seq: &mut u32,
    out: &mut impl Write,
) -> Result<(), AcarsError> {
    *seq = seq.wrapping_add(1);
    let stamp = format_timestamp(msg.timestamp);
    writeln!(
        out,
        "[#{seq} (L:{:+.0} E:{}) {stamp} --------------------------------",
        msg.level_db, msg.error_count
    )
    .map_err(io_err)?;
    writeln!(
        out,
        "F:{:.3} Mode : {} Label : {} Id : {} Ack : {}",
        chan_freq_hz / 1_000_000.0,
        msg.mode as char,
        std::str::from_utf8(&msg.label).unwrap_or("??"),
        msg.block_id as char,
        msg.ack as char,
    )
    .map_err(io_err)?;
    let flight = msg.flight_id.as_deref().unwrap_or("");
    let msgno = msg.message_no.as_deref().unwrap_or("");
    writeln!(
        out,
        "Aircraft reg: {} Flight id: {flight}",
        msg.aircraft.as_str()
    )
    .map_err(io_err)?;
    if !msgno.is_empty() {
        writeln!(out, "No: {msgno}").map_err(io_err)?;
    }
    writeln!(out, "{}", msg.text).map_err(io_err)?;
    writeln!(out).map_err(io_err)?;
    Ok(())
}

fn format_timestamp(ts: SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    match ts.duration_since(UNIX_EPOCH) {
        Ok(d) => format!(
            "{}.{:03}",
            d.as_secs(),
            d.subsec_millis()
        ),
        Err(_) => "0.000".to_string(),
    }
}

fn io_err(e: std::io::Error) -> AcarsError {
    AcarsError::Io {
        path: PathBuf::from("<stdout>"),
        source: e,
    }
}
```

NOTE: this CLI binary depends on `tracing-subscriber`, which is in workspace deps; add `tracing-subscriber = { workspace = true }` to `crates/sdr-acars/Cargo.toml`'s `[dependencies]` section.

- [ ] **Step 3: Build the binary**

```bash
cargo build -p sdr-acars --bin sdr-acars-cli
# Expected: clean build.
```

- [ ] **Step 4: Smoke-test against `test.wav`**

```bash
cargo run -p sdr-acars --bin sdr-acars-cli -- original/acarsdec/test.wav | head -30
# Expected: at least a few decoded messages in acarsdec-style format.
# If empty: the demod or parser has a bug. Don't proceed to e2e until something decodes.
```

- [ ] **Step 5: Commit**

```bash
git add crates/sdr-acars/src/bin/sdr-acars-cli.rs crates/sdr-acars/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(sdr-acars): sdr-acars-cli binary (WAV + IQ inputs)

Two input modes: positional WAV file (multi-channel @
IF_RATE_HZ, bypasses ChannelBank's decimator) and --iq for
raw cs16 source-rate IQ (drives ChannelBank end-to-end).
Output format mirrors acarsdec -o 1 text mode for diff-test
in subsequent task. Smoke-test against test.wav decoded
messages.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: E2E diff test against acarsdec snapshot

**Files:**
- Create: `crates/sdr-acars/tests/e2e_acarsdec_compat.rs`
- Create: `crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt`
- Create: `crates/sdr-acars/tests/fixtures/REGENERATE.md`

The test runs `sdr-acars-cli original/acarsdec/test.wav`, strips volatile fields (sequence number, level, error count, timestamp), and diffs against a committed snapshot. The snapshot is regenerated manually from the C `acarsdec` (documented in REGENERATE.md) — committing a fixture rather than calling `acarsdec` at test time keeps CI deterministic and removes a tooling dependency.

- [ ] **Step 1: Generate the snapshot**

This is a one-time manual step — but capture the exact commands so the user (or a future engineer) can regenerate.

```bash
# The user must have acarsdec installed (e.g. AUR `acarsdec` on Arch).
cd original/acarsdec
acarsdec -f ./test.wav > /tmp/acarsdec_raw.txt 2>&1
# Strip volatile fields. The pattern below targets:
#   * `[#<seq> (L:<level> E:<errors>) <timestamp> ` → `[--`
sed -E 's/\[#[0-9]+ \(L:[+-]?[0-9.]+ E:[0-9]+\) [0-9./: ]+/[--/' \
    /tmp/acarsdec_raw.txt > \
    /data/source/rtl-sdr/crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt
cd /data/source/rtl-sdr
```

If `acarsdec` isn't available, build it from `original/acarsdec/`:

```bash
cd original/acarsdec
cmake -B build && cmake --build build
./build/acarsdec -f ./test.wav > /tmp/acarsdec_raw.txt
# (then sed as above)
```

If neither works, ask the user.

- [ ] **Step 2: Sanity-check the snapshot**

```bash
head -5 crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt
# Expected: lines starting with `[--`, then `Mode :`, etc.
wc -l crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt
# Expected: ~hundreds of lines for test.wav.
```

- [ ] **Step 3: Write the diff test**

Create `crates/sdr-acars/tests/e2e_acarsdec_compat.rs`:

```rust
//! End-to-end compatibility test: run sdr-acars-cli on the
//! shipped acarsdec test.wav, strip volatile fields, diff
//! against the committed acarsdec snapshot. This is the
//! correctness oracle for the entire DSP + parser stack.
//!
//! The snapshot is regenerated manually — see
//! `tests/fixtures/REGENERATE.md`. Running this test never
//! invokes the C acarsdec; that's intentional (deterministic
//! CI, no external tool dependency).

use std::{path::PathBuf, process::Command};

/// Strip volatile fields from acarsdec-format output. Matches
/// the regex used in REGENERATE.md so committed snapshot and
/// fresh CLI output are normalized identically.
fn strip_volatile(s: &str) -> String {
    // Replace `[#<seq> (L:<level> E:<errors>) <timestamp> ` → `[--`
    // (keeping the trailing dashes that acarsdec appends).
    let header = regex_lite::Regex::new(
        r"\[#\d+ \(L:[+-]?[0-9.]+ E:\d+\) [0-9./: ]+",
    )
    .expect("regex compiles");
    header.replace_all(s, "[--").into_owned()
}

#[test]
fn sdr_acars_cli_matches_acarsdec_on_test_wav() {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ has parent")
        .parent()
        .expect("workspace root exists")
        .to_path_buf();
    let test_wav = project_root.join("original/acarsdec/test.wav");
    assert!(
        test_wav.exists(),
        "test.wav missing at {test_wav:?} — clone the acarsdec ref repo"
    );

    let cli_bin = env!("CARGO_BIN_EXE_sdr-acars-cli");
    let output = Command::new(cli_bin)
        .arg(&test_wav)
        .output()
        .expect("running sdr-acars-cli");

    assert!(
        output.status.success(),
        "sdr-acars-cli failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let actual = strip_volatile(&String::from_utf8_lossy(&output.stdout));
    let expected_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/acarsdec_test_wav_expected.txt");
    let expected = strip_volatile(
        &std::fs::read_to_string(&expected_path)
            .expect("snapshot fixture readable"),
    );

    if actual != expected {
        // On mismatch, write actual side-by-side for diagnosis.
        let actual_dump =
            std::env::temp_dir().join("sdr-acars-actual.txt");
        std::fs::write(&actual_dump, &actual).ok();
        panic!(
            "sdr-acars-cli output differs from acarsdec snapshot.\n\
             Snapshot: {expected_path:?}\n\
             Actual (stripped): {actual_dump:?}\n\
             Run: diff <( <snapshot)  <actual_dump>"
        );
    }
}
```

NOTE: `regex_lite` is a stripped-down regex crate without backtracking — sufficient for this stripper. If not in workspace deps, add `regex-lite = "0.1"` to `[workspace.dependencies]` in the root Cargo.toml and `regex-lite = { workspace = true }` to `crates/sdr-acars/Cargo.toml`'s `[dev-dependencies]`. Alternative: write the strip with a hand-rolled `str::find` loop and skip the regex dep; the regex is just clearer. Pick whichever the workspace already trends toward.

- [ ] **Step 4: Document regeneration**

Create `crates/sdr-acars/tests/fixtures/REGENERATE.md`:

```markdown
# Regenerating the acarsdec snapshot

The e2e test `sdr_acars_cli_matches_acarsdec_on_test_wav` diffs
the Rust port's output against a snapshot of the C `acarsdec`'s
output on `original/acarsdec/test.wav`. This file documents how
to refresh that snapshot — needed when:

- The acarsdec project upstream changes its output format.
- We add/remove fields from our printer that should match.

## Procedure

```bash
# 1. Ensure acarsdec is built.
cd original/acarsdec
cmake -B build && cmake --build build

# 2. Generate raw output.
./build/acarsdec -f ./test.wav > /tmp/acarsdec_raw.txt

# 3. Strip volatile fields and write the snapshot.
cd /data/source/rtl-sdr
sed -E 's/\[#[0-9]+ \(L:[+-]?[0-9.]+ E:[0-9]+\) [0-9./: ]+/[--/' \
    /tmp/acarsdec_raw.txt > \
    crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt

# 4. Verify the test still passes.
cargo test -p sdr-acars --test e2e_acarsdec_compat
```

## Volatile fields

The strip regex covers everything that depends on wall-clock or
hardware state:

- `#<seq>` — per-message sequence counter
- `L:<level>` — matched-filter signal level (dB)
- `E:<errors>` — bytes corrected by parity FEC
- `<timestamp>` — wall-clock at decode time

Everything else (Mode, Label, Aircraft, Flight ID, Block ID,
ACK, message body, ETX/ETB) must match exactly.
```

- [ ] **Step 5: Run the test**

```bash
cargo test -p sdr-acars --test e2e_acarsdec_compat
# Expected: pass.
```

If it fails, the diff output names a tempfile path you can inspect:

```bash
diff crates/sdr-acars/tests/fixtures/acarsdec_test_wav_expected.txt /tmp/sdr-acars-actual.txt
```

Common failure modes:
- A field is mis-named (e.g. "Aircraft Reg" vs "Aircraft reg"). Fix in `print_message` to match acarsdec exactly.
- Extra/missing whitespace. Same fix.
- Decoded text differs: real correctness bug in MSK or frame parser; investigate.

- [ ] **Step 6: Commit**

```bash
git add crates/sdr-acars/tests/
git commit -m "$(cat <<'EOF'
test(sdr-acars): e2e diff test against acarsdec snapshot

Runs sdr-acars-cli on original/acarsdec/test.wav, strips
volatile fields (#seq, L:, E:, timestamp), diffs against
committed snapshot. Snapshot regen documented in
tests/fixtures/REGENERATE.md.

This is the correctness oracle for the entire DSP + parser
stack — byte-equal output against the C reference is what
"faithful port" means here.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Multi-channel synthetic test

**Files:**
- Create: `crates/sdr-acars/tests/multichannel_synthetic.rs`

Synthesize a 2.4 MSps complex IQ buffer that contains two MSK signals at known offsets from the center frequency, each carrying a known short ACARS frame. Confirm that `ChannelBank::process` decodes both messages independently with no cross-talk.

The test is intentionally aspirational — synthesizing a realistic-enough MSK signal in Rust to actually decode requires careful work. The test scaffolds the concept and serves as a smoke check; if it can't reliably decode the synthetic signal, that's a clue the multi-channel path has a bug worth investigating, but the e2e test (Task 10) is the definitive correctness oracle.

- [ ] **Step 1: Implement the test**

Create `crates/sdr-acars/tests/multichannel_synthetic.rs`:

```rust
//! Multi-channel synthetic IQ test for ChannelBank. Builds a
//! 2.4 MSps IQ buffer carrying two MSK transmissions on
//! distinct ACARS frequencies, asserts that both get decoded
//! into messages on the right channels with no cross-talk.
//!
//! Caveat: faithfully synthesizing ACARS-grade MSK in test
//! code is non-trivial — bit-sync, parity, CRC all need to be
//! constructed correctly. This test documents the scaffold;
//! the e2e test against acarsdec's test.wav (Task 10) is the
//! definitive correctness oracle. If this synthetic test
//! fails but the e2e test passes, treat as a synthesis-side
//! bug rather than a ChannelBank bug — but investigate to be
//! sure.

use num_complex::Complex32;
use sdr_acars::ChannelBank;

const SOURCE_RATE_HZ: f64 = 2_400_000.0;
const CENTER_HZ: f64 = 130_450_000.0;

/// Synthesize a few seconds of complex IQ at SOURCE_RATE_HZ
/// containing nothing but white noise. Used as a baseline:
/// noise alone shouldn't produce decoded messages.
fn synth_noise(seconds: f64) -> Vec<Complex32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = (seconds * SOURCE_RATE_HZ) as usize;
    let mut out = Vec::with_capacity(n);
    // Deterministic LCG so the test is reproducible.
    let mut s: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let i = (s as f32) / (u64::MAX as f32) - 0.5;
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let q = (s as f32) / (u64::MAX as f32) - 0.5;
        out.push(Complex32::new(i * 0.01, q * 0.01)); // -40 dBFS noise
    }
    out
}

#[test]
fn pure_noise_produces_no_messages() {
    let mut bank = ChannelBank::new(
        SOURCE_RATE_HZ,
        CENTER_HZ,
        &[131_550_000.0, 131_525_000.0],
    )
    .expect("valid 2-channel config");
    let noise = synth_noise(2.0);
    bank.process(&noise, |msg| {
        panic!("noise should not decode: {msg:?}");
    });
}

// IMPLEMENTER: a proper "decode a synthesized MSK signal" test
// would build a 2400-baud MSK waveform on top of one of the
// channel offsets, confirm decode happens on that channel, and
// confirm the OTHER channel stays silent. Synthesis takes:
//
//   1. Build a proper ACARS frame: SYN+SYN+SOH+Mode+Addr+ACK+
//      Label+BlockID+STX+text+ETX+CRC (with odd parity per
//      character, frame-CRC at the end).
//   2. MSK-encode each bit at 1200/2400 Hz tones, 12.5 kHz audio
//      sample rate.
//   3. Upsample to SOURCE_RATE_HZ via zero-stuff + LPF.
//   4. Mix to channel offset (multiply by complex exp at offset).
//   5. Sum onto the IQ buffer.
//
// Step 2 is the intricate part. The acarsdec ref doesn't ship
// a synthesizer, so we'd be writing one from spec. Defer this
// to a follow-up if the e2e test (Task 10) is sufficient
// correctness coverage; otherwise build it here.
```

- [ ] **Step 2: Run the noise-only sanity test**

```bash
cargo test -p sdr-acars --test multichannel_synthetic
# Expected: 1 passed.
```

- [ ] **Step 3: Commit**

```bash
git add crates/sdr-acars/tests/multichannel_synthetic.rs
git commit -m "$(cat <<'EOF'
test(sdr-acars): multi-channel synthetic IQ scaffold

Pure-noise pass produces no messages — sanity check that the
ChannelBank doesn't false-positive on white noise. Full MSK
synthesis for a real decode test is left as a follow-up;
e2e test against acarsdec snapshot is the correctness oracle.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Final lint pass + workspace lints

**Files:** none (verification + final commit)

- [ ] **Step 1: Workspace test**

```bash
cargo test --workspace --features whisper-cpu
# Expected: all pass. (whisper-cpu is the default feature flag for
# the binary crate; sub-project 1 doesn't touch transcription, but
# the workspace test runs everything together.)
```

- [ ] **Step 2: Workspace clippy**

```bash
cargo clippy --all-targets --workspace --features whisper-cpu -- -D warnings
# Expected: clean.
```

- [ ] **Step 3: Workspace fmt**

```bash
cargo fmt --all -- --check
# Expected: clean.
```

- [ ] **Step 4: Workspace lint (cargo deny + audit)**

```bash
make lint
# Expected: clean. If a new transitive dep adds an advisory, address
# per project policy (don't ignore advisories — see CLAUDE.md memory).
```

- [ ] **Step 5: If all gates pass, push the branch**

```bash
git push -u origin feat/acars-dsp-crate
```

- [ ] **Step 6: Open the PR**

```bash
gh pr create --title 'feat(sdr-acars): DSP + frame parser + CLI (#474, sub-project 1)' --body "$(cat <<'EOF'
## Summary

Sub-project 1 of ACARS epic #474 — pure DSP + frame parser, no UI integration. Faithful port of \`original/acarsdec/{msk.c, acars.c, label.c, syndrom.h}\` into a new \`sdr-acars\` crate plus a \`sdr-acars-cli\` binary that takes a WAV or IQ file and prints decoded messages in the same text format as the C reference.

Approved design: \`docs/superpowers/specs/2026-04-28-acars-design.md\`
Plan: \`docs/superpowers/plans/2026-04-28-acars-dsp-crate.md\`

What ships here:

- New crate \`crates/sdr-acars\` with \`channel.rs\`, \`crc.rs\`, \`error.rs\`, \`frame.rs\`, \`label.rs\`, \`msk.rs\`, \`syndrom.rs\`
- Binary \`sdr-acars-cli\`: WAV and raw cs16 IQ inputs, acarsdec-text output
- Multi-channel \`ChannelBank\` for source-rate IQ → N parallel decoders
- FEC parity-error correction via the syndrom table
- Label name lookup table

What's deferred (filed as separate issues): #577 per-label parsers · #578 output formatters / airframes.io feeding · #579 aircraft-grouped tab · #580 multi-block reassembly · #581 international channel sets · #582 ADS-B integration.

## Test plan

- [x] Unit tests per module (CRC vectors, syndrom table integrity, label lookups, frame state-machine round-trip)
- [x] E2E diff test against committed \`acarsdec\` snapshot on \`test.wav\` — byte-equal modulo volatile fields (sequence, level, error count, timestamp)
- [x] Multi-channel synthetic noise sanity test
- [x] \`cargo test --workspace\` clean
- [x] \`cargo clippy --all-targets --workspace -- -D warnings\` clean
- [x] \`cargo fmt --all -- --check\` clean
- [x] \`make lint\` (deny + audit) clean

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Plan self-review (per writing-plans skill)

**Spec coverage check:**

- [x] **Module structure** (lib.rs, channel.rs, msk.rs, frame.rs, crc.rs, syndrom.rs, label.rs, error.rs) — Tasks 1, 2, 3, 4, 5, 6, 7, 8
- [x] **CLI binary** (WAV + IQ inputs, acarsdec text output) — Task 9
- [x] **Public API** (`ChannelBank::new` / `::process` / `::channels`, `AcarsMessage`, `ChannelStats`, `ChannelLockState`) — Task 8
- [x] **Scope of port v1**: MSK demod ✅, bit timing ✅ (inline in MSK), frame state machine + parity ✅, CRC ✅, FEC ✅ (syndrom + fixprerr/fixdberr), multi-channel ✅, label name lookup ✅. Per-label parsers and output formatters explicitly excluded — Tasks 5/6/7
- [x] **Acceptance test** (byte-equal diff against acarsdec snapshot on test.wav, volatile fields stripped: timestamp, signal level, error count, sequence number) — Task 10
- [x] **Multi-channel test** (synthesize 2.4 MSps IQ with two MSK signals, confirm independent decode) — Task 11 (scaffolded; full synth deferred)
- [x] **No GTK, no rtlsdr crate dependency** — confirmed in Task 1's Cargo.toml dep list

**Placeholder scan:**

The plan asks the implementer to "translate `Lbl[]` from `label.c` here" (Task 4 Step 4) and "paste the translated table" (Task 3 Step 4) — these are *deliberate* manual translation steps with concrete instructions and contracts (table size pinned, sentinel tests pinned). They are not placeholders in the prohibited sense.

The "IMPLEMENTER" comments in `MskDemod::process` (Task 5) and `FrameParser::consume_byte` (Task 6) point to specific C reference line ranges and ask the implementer to faithfully translate — same pattern: concrete instructions, not "TBD". The implementer reads the C and writes the Rust.

The synth-MSK section in Task 11 explicitly defers the full synthesizer with a rationale (e2e test is the correctness oracle).

**Type consistency:**

- `ChannelBank::process<F: FnMut(AcarsMessage)>` (Task 7) matches the API in `lib.rs` (Task 8).
- `BitSink::put_bit(&mut self, value: f32)` (Task 5) is implemented by `FrameParser` (Task 6) — signature consistent.
- `MskDemod::toggle_polarity` (Task 7's note) added in the same task — signature consistent with the call site in `ChannelBank::process`.
- `FrameParser::take_polarity_flip` referenced in Task 7 — must be added when implementing Task 6's `consume_byte`. The Task 6 stub mentions this; the implementer adds it.
- `FrameParser::drain` referenced in Tasks 7 + 9 — must be added in Task 6 per the explicit "RECOMMEND (a)" decision in Task 6 Step 2.

No type mismatches found.

**Result:** Plan is internally consistent and covers the spec.
