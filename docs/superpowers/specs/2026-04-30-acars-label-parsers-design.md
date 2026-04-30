# ACARS Label Parsers (issue #577)

> Faithful Rust port of `original/acarsdec/label.c`. Extracts
> Out-Off-On-In (OOOI) metadata from message text per ACARS
> label code. Foundation for #578 (JSON output) and #579
> (aircraft-grouped viewer tab).

## Goal

For each ACARS message that carries one of ~40 known label codes,
extract the structured OOOI metadata embedded in the message body
text at fixed byte offsets — origin/destination airport codes plus
the timestamps for the four OOOI events (gate-out, wheels-off,
wheels-on, gate-in) and ETA. Surface the parse result on
`AcarsMessage` so downstream consumers (CLI JSON output, aircraft
tab) can show structured fields without re-parsing.

## Non-goals

- **JSON serialization.** Deferred to #578.
- **UI surface for parsed fields.** Deferred to #579.
- **Per-label name lookup table.** Separate concern in `label.rs`,
  not changed by this PR.
- **New labels beyond what `label.c` ships.** Faithful port —
  no scope expansion.

## Architecture

### New module: `crates/sdr-acars/src/label_parsers.rs`

One file. Holds the public `Oooi` struct, the public `decode_label`
dispatch fn, and the 39 private per-label parser fns. Re-exported
from `crate::lib` as `pub use label_parsers::{Oooi, decode_label};`.

### `Oooi` struct

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Oooi {
    /// Station of origin (4-char airport code).
    pub sa: Option<ArrayString<4>>,
    /// Destination airport (4-char airport code).
    pub da: Option<ArrayString<4>>,
    /// Gate-out time (4-char HHMM UTC).
    pub gout: Option<ArrayString<4>>,
    /// Wheels-off time.
    pub woff: Option<ArrayString<4>>,
    /// Wheels-on time.
    pub won: Option<ArrayString<4>>,
    /// Gate-in time.
    pub gin: Option<ArrayString<4>>,
    /// Estimated time of arrival.
    pub eta: Option<ArrayString<4>>,
}
```

Field type `ArrayString<4>` matches the existing AcarsMessage
fields (`aircraft: ArrayString<8>`, etc.) — no heap, fixed size.
Each field is `Option<...>` because most parsers populate only a
subset.

### Public API

```rust
pub fn decode_label(label: [u8; 2], text: &str) -> Option<Oooi>;
```

Returns `Some(Oooi)` when:
1. The label has a parser (one of the 40 known cases), AND
2. Validation passes (e.g. expected separator chars at expected
   offsets), AND
3. At least one field extracts (text long enough, slice valid
   UTF-8 at byte boundary).

Returns `None` when:
- Unknown label (label not in C's switch).
- Prefix-validation fails (e.g. `label_20` requires `"RST"`
  prefix — without it, parse aborts).
- Separator-validation fails (e.g. `label_12` requires
  `txt[4] == ','`).
- Text too short for the parser's hardcoded offsets.

This mirrors C's `int DecodeLabel(...)` returning `0` (failed) or
`1` (succeeded), where `1` means at least one OOOI field was
populated.

### Per-label parsers (39 fns, all private)

Faithful 1:1 port of every `label_*` function in `label.c`:

**'1' family** (6 fns): `label_10`, `label_11`, `label_12`,
`label_15`, `label_17`, `label_1g`.

**'2' family** (5 fns): `label_20`, `label_21`, `label_26`,
`label_2n`, `label_2z`.

**'3' family** (2 fns): `label_33`, `label_39`.

**'4' family** (2 fns): `label_44`, `label_45`.

**'8' family** (5 fns): `label_80`, `label_83`, `label_8d`,
`label_8e`, `label_8s`.

**'R' family** (1 alias): `RB` → reuses `label_26` parser
(verbatim from C dispatch).

**'Q' family** (19 fns): `label_q1`, `label_q2`, `label_qa`
through `label_qh`, `label_qk` through `label_qt` (skipping
`QI`/`QJ` per C).

Total: 39 unique parser fns (40 dispatch arms counting `RB`
alias).

### Dispatch (`decode_label` body)

Match on `label[0]`:
- `b'1'` → sub-match on `label[1]` (`b'0'`/`b'1'`/`b'2'`/`b'5'`/`b'7'`/`b'G'`)
- `b'2'` → sub-match (`b'0'`/`b'1'`/`b'6'`/`b'N'`/`b'Z'`)
- `b'3'` → sub-match (`b'3'`/`b'9'`)
- `b'4'` → sub-match (`b'4'`/`b'5'`)
- `b'8'` → sub-match (`b'0'`/`b'3'`/`b'D'`/`b'E'`/`b'S'`)
- `b'R'` → if `b'B'` then `label_26`
- `b'Q'` → sub-match on every Q-letter

Default arm: `None`. Verbatim mirror of C's nested switch on
`msg->label[0..2]`.

### Bounds-safe text access

The C code uses raw indexing like `txt[28]` and `memcpy(dst, &txt[24], 4)`.
That trusts the caller to pass text long enough — UB on short
text. Rust mirror uses safe accessors:

```rust
fn byte_at(text: &str, idx: usize) -> Option<u8> {
    text.as_bytes().get(idx).copied()
}

fn slice4(text: &str, start: usize) -> Option<ArrayString<4>> {
    text.get(start..start + 4)
        .and_then(|s| ArrayString::from(s).ok())
}
```

`text.get(start..end)` returns `None` if either bound is past the
end OR if the bound isn't on a UTF-8 char boundary — both are
safe failure modes for the parser. ACARS payloads are 7-bit
ASCII, so non-boundary failures are unreachable in practice but
returning `None` is correct.

Each parser is then a sequence of `?` chains:
```rust
fn label_q1(text: &str) -> Option<Oooi> {
    Some(Oooi {
        sa: slice4(text, 0),
        gout: slice4(text, 4),
        woff: slice4(text, 8),
        won: slice4(text, 12),
        gin: slice4(text, 16),
        da: slice4(text, 24),
        ..Default::default()
    })
    .filter(|o| o.has_any())
}
```

Where `has_any` returns `true` if at least one field is `Some`.
The `.filter(...)` mirrors C's "return 1 only if something
populated" semantics — without it, a too-short text would return
`Some(empty_oooi)` which is meaningless.

### Validation arms

Some parsers do prefix or separator validation before extracting.
Mirror exactly:

```rust
// Mirrors `if(memcmp(txt,"RST",3)) return 0;` in label_20.
fn label_20(text: &str) -> Option<Oooi> {
    if !text.starts_with("RST") {
        return None;
    }
    Some(Oooi {
        sa: slice4(text, 22),
        da: slice4(text, 26),
        ..Default::default()
    })
    .filter(|o| o.has_any())
}

// Mirrors `if(txt[4]!=',') return 0;` in label_12.
fn label_12(text: &str) -> Option<Oooi> {
    if byte_at(text, 4) != Some(b',') {
        return None;
    }
    Some(Oooi {
        sa: slice4(text, 0),
        da: slice4(text, 5),
        ..Default::default()
    })
    .filter(|o| o.has_any())
}
```

The trickiest parser is `label_26` (used for `26` and `RB`): it
walks the text looking for `\n`-separated lines (`SCH/...`, then
optional `ETA/...`). Port the C's `strchr` walk verbatim using
`text.find('\n')` and substring slicing.

`label_44` is also non-trivial: optional `00` prefix, then either
`POS0` or `ETA0` prefix, then validates digit at offset 4, then
six commas at fixed offsets. Mirror verbatim.

## Wiring into `AcarsMessage`

Add field to `AcarsMessage` in `frame.rs`:

```rust
pub struct AcarsMessage {
    /* existing fields ... */
    /// Parsed OOOI metadata (origin/destination airports + event
    /// times) extracted from `text` based on `label`. `None` if
    /// the label has no parser or validation failed. Populated
    /// post-reassembly so multi-block messages parse the
    /// concatenated text. Issue #577.
    pub parsed: Option<Oooi>,
}
```

Default-constructed `parsed: None` in `FrameParser::emit_frame`.

### Population point

`ChannelBank::process` in `channel.rs` is the single-source-of-
truth for emitting messages downstream. Populate `parsed` there,
just before `on_message(emitted)`:

```rust
for emitted in ch.assembler.observe(msg, now) {
    let mut emitted = emitted;
    emitted.parsed = label_parsers::decode_label(emitted.label, &emitted.text);
    /* stats updates ... */
    on_message(emitted);
}
```

Same one-line addition at the `drain_timeouts` emit site.

**Why post-assembler?** Multi-block ACARS messages are
concatenated by `MessageAssembler` (issue #580). Parsing
post-reassembly means the OOOI fields are extracted from the
final combined text, not just the first block. The parser
reads byte offsets — block boundaries shift them — so post-
reassembly is the only correct moment.

**Why not in `FrameParser`?** Pre-assembler messages are
not yet final (multi-block ETB messages would parse
incomplete text). Centralising at `ChannelBank::process`
covers single-block (passthrough) and multi-block
(reassembled) paths with one call site.

## Testing

### Unit tests in `label_parsers::tests`

One test per parser fn (39 total). Each test:

1. Hand-crafts a fixture string at the offsets the C parser
   expects (e.g. `label_q1` test: `"KORD0830094510201245KSFO1830"`).
2. Calls `decode_label([b'Q', b'1'], fixture)`.
3. Asserts the returned `Oooi` has the expected fields populated
   with the expected values.

Example:
```rust
#[test]
fn label_q1_extracts_all_six_fields() {
    let txt = "KORD0830094510201245    KSFO";
    //         sa  gout woff won gin   skip da
    //         0   4    8    12  16    20  24
    let oooi = decode_label([b'Q', b'1'], txt).expect("Q1 parses");
    assert_eq!(oooi.sa.as_deref(), Some("KORD"));
    assert_eq!(oooi.gout.as_deref(), Some("0830"));
    assert_eq!(oooi.woff.as_deref(), Some("0945"));
    assert_eq!(oooi.won.as_deref(), Some("1020"));
    assert_eq!(oooi.gin.as_deref(), Some("1245"));
    assert_eq!(oooi.da.as_deref(), Some("KSFO"));
    assert!(oooi.eta.is_none());
}
```

### Dispatch tests

- `decode_label([b'X', b'X'], "...")` → `None` (unknown label).
- `decode_label([b'2', b'0'], "no-rst-prefix...")` → `None`
  (validation fails).
- `decode_label([b'1', b'2'], "ABCD-no-comma")` → `None`
  (separator fails — comma expected at offset 4, got `-`).
- `decode_label([b'R', b'B'], "...sched-text...")` → routes to
  `label_26` (alias verified).
- Short text: `decode_label([b'Q', b'1'], "KO")` → `None` (every
  field slice fails bounds check, `has_any()` is false).

### Integration test

**Out of scope for #577.** Acceptance criterion bullet 3 says
"Integration test on recorded IQ confirms structured fields
appear in CLI JSON output **(when JSON output ships — see related
issue)**." JSON output is #578's problem. This PR ships parsers +
unit tests only.

## File layout

```
crates/sdr-acars/src/
├── label_parsers.rs   ← NEW (~430 LOC)
├── lib.rs             ← +1 mod, +1 re-export
├── frame.rs           ← +1 field on AcarsMessage, default None
└── channel.rs         ← +2 lines (populate parsed at emit sites)
```

## Estimated diff

~440 lines added, ~5 lines modified. Single bundled PR. No CR
wrangling expected — port-fidelity is straightforward; the only
judgment call is `has_any()` semantics (justified above).

## Open questions

None at design time. Parser-level edge cases discovered during
implementation will get inline comments + targeted unit tests.

## References

- `original/acarsdec/label.c` — source of truth (427 LOC,
  ~340 of which is the parser fns we port)
- `original/acarsdec/acarsdec.h` — `oooi_t` struct definition
- ACARS protocol references in `docs/research/07-acars-aviation-datalink.md`
- Issue #577 acceptance criteria
- Sibling specs:
  - `docs/superpowers/specs/2026-04-28-acars-design.md` (epic root)
  - Sub-project 1 (DSP/parser) — shipped via PR #583
  - Sub-project 2 (controller integration) — shipped via PR #584
  - Sub-project 3 (UI surface) — shipped via PR #587
