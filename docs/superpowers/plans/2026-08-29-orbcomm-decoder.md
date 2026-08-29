# Orbcomm Multi-Channel Decoder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decode all nine Orbcomm downlink channels (137.2–137.8 MHz, SDPSK 4800 bps) simultaneously from wideband IQ, surfacing typed packets + reassembled messages in a viewer window and "Heard via Orbcomm" rows in the Satellites panel.

**Architecture:** New pure-decode workspace crate `sdr-orbcomm` (channelizer → SDPSK demod → deframer → packet parse/reassembly), an `orbcomm_decode_tap` in sdr-core mirroring `acars_decode_tap`, and GTK wiring in sdr-ui. See the spec for the full protocol reference.

**Tech Stack:** Rust workspace crate (thiserror, workspace lints), `sdr-dsp` `RationalResampler` for channelization, GTK4/libadwaita UI following `acars_viewer.rs`.

**Spec:** `docs/superpowers/specs/2026-08-29-orbcomm-decoder-design.md`

## Global Constraints

- No `unwrap()`/`panic!()` in library crates; `thiserror` errors (never `anyhow` outside `src/main.rs`).
- No `println!` — `tracing` macros only.
- Named constants for all magic numbers; workspace lints (`[lints] workspace = true`), clippy pedantic, `unsafe_code` denied.
- Tests inline at file bottom in `#[cfg(test)] mod tests` (or `<module>/tests.rs` with `mod tests;` for big suites).
- Branch: `feat/orbcomm-decoder-865`. Frequent commits, TDD (write test → watch fail → implement → pass → commit).
- Protocol constants come from the spec — do not re-derive. Reference clone (gitignored): `original/ORBCOMM-receiver/`.
- Gates before push: `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`, `cargo fmt --all -- --check` LAST. Run gates as separate unpiped commands.

---

### Task 1: Crate scaffold

**Files:**
- Create: `crates/sdr-orbcomm/Cargo.toml`, `crates/sdr-orbcomm/src/lib.rs`
- Modify: root `Cargo.toml` (workspace `members` list ~line 99–120, and `[workspace.dependencies]` alongside `sdr-lrpt = { path = ... }` ~line 170)

**Interfaces:**
- Produces: crate `sdr-orbcomm` with `pub const ORBCOMM_CHANNELS_HZ: [f64; 9]`, `pub const SYMBOL_RATE_HZ: f64 = 4800.0`, `pub const CHANNEL_SAMPLE_RATE_HZ: f64 = 19_200.0`, `pub const SAMPLES_PER_SYMBOL: usize = 4`, and `pub enum OrbcommError` (thiserror).

- [ ] **Step 1: Write the failing test** (in `lib.rs` bottom)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_list_is_sorted_and_in_band() {
        assert_eq!(ORBCOMM_CHANNELS_HZ.len(), 9);
        for w in ORBCOMM_CHANNELS_HZ.windows(2) {
            assert!(w[0] < w[1]);
        }
        for f in ORBCOMM_CHANNELS_HZ {
            assert!((137_200_000.0..=137_800_000.0).contains(&f));
        }
        assert!((CHANNEL_SAMPLE_RATE_HZ / SYMBOL_RATE_HZ - SAMPLES_PER_SYMBOL as f64).abs() < f64::EPSILON);
    }
}
```

- [ ] **Step 2: Create the crate.** `Cargo.toml`:

```toml
[package]
name = "sdr-orbcomm"
version = "0.1.0"
edition = "2024"
license = "GPL-3.0-or-later"

[dependencies]
sdr-types.workspace = true
sdr-dsp.workspace = true
thiserror.workspace = true
tracing.workspace = true

[lints]
workspace = true
```

(Match `edition`/`license` to what `crates/sdr-lrpt/Cargo.toml` uses — copy that file's header verbatim.) `lib.rs`:

```rust
//! Orbcomm downlink decoder — SDPSK 4800 bps, multi-channel.
//! Pure decode: no I/O, no threads, no GTK. Issue #865; protocol
//! reference in docs/superpowers/specs/2026-08-29-orbcomm-decoder-design.md.

/// Active Orbcomm subscriber downlink channels (Hz), low to high.
pub const ORBCOMM_CHANNELS_HZ: [f64; 9] = [
    137_225_000.0, 137_250_000.0, 137_440_000.0, 137_460_000.0,
    137_662_500.0, 137_687_500.0, 137_717_500.0, 137_737_500.0,
    137_800_000.0,
];
/// SDPSK symbol rate.
pub const SYMBOL_RATE_HZ: f64 = 4800.0;
/// Per-channel complex sample rate after decimation (4 samples/symbol).
pub const CHANNEL_SAMPLE_RATE_HZ: f64 = 19_200.0;
/// Samples per symbol at [`CHANNEL_SAMPLE_RATE_HZ`].
pub const SAMPLES_PER_SYMBOL: usize = 4;

/// Errors surfaced by [`ChannelBank`] construction and processing.
#[derive(Debug, thiserror::Error)]
pub enum OrbcommError {
    /// A DSP building block failed to construct.
    #[error("orbcomm DSP init failed: {0}")]
    Dsp(#[from] sdr_dsp::DspError),
    /// No requested channel fits inside the source span.
    #[error("no orbcomm channel inside the source span (center {center_hz} Hz, rate {source_rate_hz} Hz)")]
    NoChannelsInSpan { center_hz: f64, source_rate_hz: f64 },
}
```

(If `sdr_dsp`'s error type has a different name/path, use the actual one — check `crates/sdr-dsp/src/lib.rs` exports.)

- [ ] **Step 3: Register in the workspace** — add `"crates/sdr-orbcomm"` to `members` and `sdr-orbcomm = { path = "crates/sdr-orbcomm" }` to `[workspace.dependencies]`.

- [ ] **Step 4: Run** `cargo test -p sdr-orbcomm` — PASS. `cargo clippy -p sdr-orbcomm --all-targets -- -D warnings` — clean.

- [ ] **Step 5: Commit** `feat(orbcomm): crate scaffold with channel plan constants (#865)` (add only `crates/sdr-orbcomm/` + root `Cargo.toml` + `Cargo.lock` — never `git add -A` in this repo).

---

### Task 2: Fletcher-16 checksum

**Files:**
- Create: `crates/sdr-orbcomm/src/packet.rs` (declare `pub mod packet;` in lib.rs)

**Interfaces:**
- Produces: `pub fn fletcher16(bytes: &[u8]) -> u16` (returns `(sum2 << 8) | sum1`, mod-256 sums; a valid Orbcomm packet checksums to 0).

- [ ] **Step 1: Failing tests** (bottom of `packet.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fletcher16_matches_reference_algorithm() {
        // Same algorithm as the reference decoder's fletcher_checksum
        // (mod-256 running sums over bytes). "abcde" is the classic
        // Wikipedia vector for the mod-255 variant; recompute for
        // mod-256 by hand: sums over [0x61,0x62,0x63,0x64,0x65]:
        // sum1 = (0x61+0x62+0x63+0x64+0x65) % 256 = 0xEF
        // sum2 = 0x61 + 0xC3 + 0x26(+256) ... assert via loop below.
        let mut sum1: u32 = 0;
        let mut sum2: u32 = 0;
        for b in b"abcde" {
            sum1 = (sum1 + u32::from(*b)) % 256;
            sum2 = (sum2 + sum1) % 256;
        }
        let expected = ((sum2 as u16) << 8) | sum1 as u16;
        assert_eq!(fletcher16(b"abcde"), expected);
    }

    #[test]
    fn packet_with_appended_check_bytes_sums_to_zero() {
        // Property the deframer relies on: append the two check bytes
        // (as the protocol does) and the whole packet folds to zero.
        let payload = [0x65u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let cksum = fletcher16(&payload);
        // check bytes chosen so the total folds to zero:
        let c0 = 255u16.wrapping_sub((cksum >> 8) + (cksum & 0xFF)) ;
        let _ = c0; // derivation done in impl-side helper below
        let (b0, b1) = fletcher16_check_bytes(&payload);
        let mut full = payload.to_vec();
        full.push(b0);
        full.push(b1);
        assert_eq!(fletcher16(&full), 0);
    }
}
```

- [ ] **Step 2: Run** `cargo test -p sdr-orbcomm packet` — FAIL (functions missing).

- [ ] **Step 3: Implement**

```rust
/// Fletcher-16 with mod-256 running sums, as used by the Orbcomm
/// downlink (reference: helpers.py::fletcher_checksum). A valid
/// packet (payload + 2 trailing check bytes) sums to zero.
#[must_use]
pub fn fletcher16(bytes: &[u8]) -> u16 {
    let mut sum1: u32 = 0;
    let mut sum2: u32 = 0;
    for b in bytes {
        sum1 = (sum1 + u32::from(*b)) % 256;
        sum2 = (sum2 + sum1) % 256;
    }
    ((sum2 as u16) << 8) | (sum1 as u16)
}

/// Check bytes that make `payload ++ [b0, b1]` fold to zero.
/// Used only by tests and synthetic-packet builders.
#[must_use]
pub fn fletcher16_check_bytes(payload: &[u8]) -> (u8, u8) {
    let ck = fletcher16(payload);
    let (s2, s1) = ((ck >> 8) as u32, (ck & 0xFF) as u32);
    // Solve for c0, c1 such that appending zeroes both sums (mod 256):
    // after c0: sum1' = s1 + c0; sum2' = s2 + s1 + c0
    // after c1: sum1'' = s1 + c0 + c1 == 0; sum2'' = s2 + s1 + c0 + sum1'' == 0
    let c0 = (256 * 2 - s2 - 2 * s1) % 256;
    let c1 = (256 * 2 - s1 - c0) % 256;
    (c0 as u8, c1 as u8)
}
```

(If the derivation is off by a modular step, fix `fletcher16_check_bytes` until the zero-fold test passes — `fletcher16` itself must stay exactly as written; it is the reference algorithm.)

- [ ] **Step 4: Run** `cargo test -p sdr-orbcomm packet` — PASS.
- [ ] **Step 5: Commit** `feat(orbcomm): Fletcher-16 checksum with zero-fold property (#865)`

---

### Task 3: Packet types, parse, and sat names

**Files:**
- Modify: `crates/sdr-orbcomm/src/packet.rs`
- Create: `crates/sdr-orbcomm/src/sat_names.rs`

**Interfaces:**
- Produces:
  - `pub enum PacketType { Sync, Message, UplinkInfo, DownlinkInfo, Network, Fill, Ephemeris, Orbital }` with `PacketType::from_header(u8) -> Option<Self>` and `pub fn packet_len(self) -> usize` (24 for `Ephemeris`, else 12) and `pub fn header_byte(self) -> u8`.
  - `pub enum OrbcommPacket { Sync { code: u32, sat_id: u8 }, Ephemeris(Ephemeris), Other { packet_type: PacketType, bytes: Vec<u8> } }` and `pub fn parse_packet(bytes: &[u8]) -> Option<OrbcommPacket>` (checksum already verified by caller). `Ephemeris` struct defined in Task 4 — for this task give it a placeholder-free minimal definition: `pub struct Ephemeris { pub sat_id: u8 }` extended in Task 4.
  - `pub fn sat_name(sat_id: u8) -> Option<&'static str>` in `sat_names.rs`.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn header_bytes_match_spec_table() {
    assert_eq!(PacketType::from_header(0x65), Some(PacketType::Sync));
    assert_eq!(PacketType::from_header(0x1A), Some(PacketType::Message));
    assert_eq!(PacketType::from_header(0x1B), Some(PacketType::UplinkInfo));
    assert_eq!(PacketType::from_header(0x1C), Some(PacketType::DownlinkInfo));
    assert_eq!(PacketType::from_header(0x1D), Some(PacketType::Network));
    assert_eq!(PacketType::from_header(0x1E), Some(PacketType::Fill));
    assert_eq!(PacketType::from_header(0x1F), Some(PacketType::Ephemeris));
    assert_eq!(PacketType::from_header(0x22), Some(PacketType::Orbital));
    assert_eq!(PacketType::from_header(0x00), None);
    assert_eq!(PacketType::Ephemeris.packet_len(), 24);
    assert_eq!(PacketType::Sync.packet_len(), 12);
}

#[test]
fn sync_packet_parses_code_and_sat_id() {
    // Spec field table (hex-char offsets incl. header): code (0,6) =
    // 3 bytes incl. header byte, sat_id (6,8) = byte 3.
    let mut p = vec![0x65u8, 0xAA, 0xBB, 0x2C, 0, 0, 0, 0, 0, 0];
    let (c0, c1) = fletcher16_check_bytes(&p);
    p.push(c0); p.push(c1);
    let parsed = parse_packet(&p).expect("valid sync packet");
    match parsed {
        OrbcommPacket::Sync { code, sat_id } => {
            assert_eq!(code, 0x65AABB);
            assert_eq!(sat_id, 0x2C);
        }
        other => panic!("expected Sync, got {other:?}"),
    }
}

#[test]
fn unknown_or_short_packets_return_none() {
    assert!(parse_packet(&[0x00; 12]).is_none());
    assert!(parse_packet(&[0x65]).is_none());
}
```

(`#[allow(clippy::panic)]` on the test fns using `panic!`, matching existing test style.)

- [ ] **Step 2: Run — FAIL.**

- [ ] **Step 3: Implement.** `PacketType` with explicit `header_byte` match table; `parse_packet` dispatch:

```rust
/// Parse a checksum-valid, aligned packet into its typed form.
/// Returns `None` for unknown headers or length mismatches.
#[must_use]
pub fn parse_packet(bytes: &[u8]) -> Option<OrbcommPacket> {
    let ty = PacketType::from_header(*bytes.first()?)?;
    if bytes.len() != ty.packet_len() {
        return None;
    }
    Some(match ty {
        PacketType::Sync => OrbcommPacket::Sync {
            code: u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]),
            sat_id: bytes[3],
        },
        PacketType::Ephemeris => OrbcommPacket::Ephemeris(Ephemeris { sat_id: bytes[1] }),
        _ => OrbcommPacket::Other { packet_type: ty, bytes: bytes.to_vec() },
    })
}
```

`sat_names.rs`: a `match` table ported from `original/ORBCOMM-receiver/sat_db.py` (sat_id byte → `"FM-xx"`); include a test asserting a couple of known entries from that file and `sat_name(0xFF).is_none()`. Note: ephemeris `sat_id` is byte 1 per the spec table (hex chars 2–4).

- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(orbcomm): typed packet parse + spacecraft name table (#865)`

---

### Task 4: Ephemeris decode

**Files:**
- Modify: `crates/sdr-orbcomm/src/packet.rs` (extend `Ephemeris`)

**Interfaces:**
- Produces: `pub struct Ephemeris { pub sat_id: u8, pub sat_time_unix: i64, pub lat_deg: f64, pub lon_deg: f64, pub alt_m: f64, pub vel_ms: f64 }` and `pub(crate) fn decode_ephemeris(bytes: &[u8]) -> Option<Ephemeris>` wired into `parse_packet`'s `Ephemeris` arm. Implausible decodes (per spec: |lat| > 90 unreachable by construction, altitude outside 400–1000 km, |vel−7 500| grossly off) return the raw `Other` form instead — implement by having `parse_packet` fall back to `Other` when `decode_ephemeris` returns `None`.

- [ ] **Step 1: Failing test — round-trip through a test-only encoder.** The encoder implements the spec scaling *forward* (GPS week/TOW + 20-bit scaled ECEF, byte-reversal layout exactly as the reference's string ops); the decoder must invert it. Port the layout literally from `original/ORBCOMM-receiver/file_decoder.py` lines 467–508 (payload = bytes 1..21 reversed; fields sliced as nibble runs; nibble-swapped 20-bit little-endian integers; `val = 2·raw·MAX/2^20 − MAX`, MAX_R = 8 378 155.0 m, MAX_V = 7 700.0 m/s; GPS epoch 1980-01-06, no leap-second correction — matching the reference).

```rust
#[cfg(test)]
fn encode_ephemeris_for_test(
    sat_id: u8, week: u16, tow_s: u32, ecef_pos: [f64; 3], ecef_vel: [f64; 3],
) -> Vec<u8> {
    // Inverse of decode_ephemeris. Build the 40-nibble payload in the
    // reference's reversed layout, prepend header+sat_id, append
    // Fletcher check bytes. (Write alongside the decoder so both read
    // the same layout comment block.)
    /* implemented in this task — see Step 3 */
    unimplemented!()
}

#[test]
fn ephemeris_round_trips_position_and_time() {
    // ~715 km circular orbit point, |v| ≈ 7.4 km/s.
    let pos = [3_000_000.0, 4_000_000.0, 4_900_000.0];
    let vel = [-5_000.0, 3_000.0, 2_000.0];
    let pkt = encode_ephemeris_for_test(0x2C, 2434, 400_000, pos, vel);
    let Some(OrbcommPacket::Ephemeris(e)) = parse_packet(&pkt) else {
        panic!("expected ephemeris");
    };
    assert_eq!(e.sat_id, 0x2C);
    // GPS epoch 315964800 + weeks + tow (no leap correction, as reference):
    assert_eq!(e.sat_time_unix, 315_964_800 + 2434 * 604_800 + 400_000);
    // 20-bit quantization: position step ≈ 16 m ⇒ lat/lon within ~0.001°,
    // altitude within ~50 m, velocity step ≈ 0.015 m/s.
    let expected_vel = (vel[0].powi(2) + vel[1].powi(2) + vel[2].powi(2)).sqrt();
    assert!((e.vel_ms - expected_vel).abs() < 1.0);
    assert!((e.alt_m - expected_alt_for(pos)).abs() < 100.0);
}

#[test]
fn implausible_ephemeris_degrades_to_raw() {
    // All-zero payload decodes to ECEF (−MAX,−MAX,−MAX) — altitude wildly
    // outside the 400–1000 km plausibility band ⇒ Other, not Ephemeris.
    let mut p = vec![0x1Fu8, 0x2C];
    p.extend_from_slice(&[0u8; 20]);
    let (c0, c1) = fletcher16_check_bytes(&p);
    p.push(c0); p.push(c1);
    match parse_packet(&p) {
        Some(OrbcommPacket::Other { packet_type: PacketType::Ephemeris, .. }) => {}
        other => panic!("expected raw fallback, got {other:?}"),
    }
}
```

`expected_alt_for` is a small test helper using the same WGS84 `ecef_to_lla` the decoder uses (exported `pub(crate)` for the test).

- [ ] **Step 2: Run — FAIL.**

- [ ] **Step 3: Implement** `decode_ephemeris` + `ecef_to_lla` (WGS84 constants `WGS84_A = 6_378_137.0`, `WGS84_INV_F = 298.257_223_563`, formulas per the reference's `helpers.py::ecef_to_lla`), the nibble-layout extraction, the plausibility gate (`(400_000.0..=1_000_000.0).contains(&(alt_m))`), and the test encoder as the exact inverse. Keep the nibble-layout description in ONE comment block shared by encoder+decoder.

- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(orbcomm): ephemeris decode (GPS time + scaled ECEF → WGS84) (#865)`

---

### Task 5: Deframer (bit stream → aligned, checksum-valid packets)

**Files:**
- Create: `crates/sdr-orbcomm/src/deframe.rs`

**Interfaces:**
- Consumes: `fletcher16`, `PacketType::{from_header, packet_len}`.
- Produces: `pub struct Deframer` with `pub fn new() -> Self`, `pub fn push_bit(&mut self, bit: bool) -> Option<DeframedPacket>`; `pub struct DeframedPacket { pub bytes: Vec<u8>, pub repaired: bool }`. Bits arrive LSB-first per byte (the demod hands over raw channel bits; byte assembly happens here). Synchronous per-byte/per-bit state machine — the ACARS lesson: never batch.

**Design (in module doc comment):** two states. `Searching`: append bits to a rolling buffer; at every bit boundary where ≥ 12 bytes are pending, test each of the last 96 bit-offsets: assemble bytes LSB-first, header must map to a `PacketType`, length per type, Fletcher folds to zero → emit, switch to `Locked` at that phase, drain consumed bits. `Locked { consecutive_bad: u8 }`: consume fixed strides (peek header byte → 12 or 24 bytes), verify checksum; on failure try single-bit repair (flip each of the ≤192 bits once, re-checksum — emit with `repaired: true` on success); `MAX_CONSECUTIVE_BAD = 4` failures → back to `Searching` keeping the buffer. Buffer capped at `MAX_PENDING_BITS = 4 * 24 * 8`.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn locks_and_emits_at_every_bit_offset() {
    let pkt = valid_sync_packet(); // helper: Sync + check bytes, 12 bytes
    for offset in 0..96 {
        let mut d = Deframer::new();
        let mut got = Vec::new();
        // prefix garbage of `offset` bits, then three packets back-to-back
        for _ in 0..offset { d.push_bit(false); }
        for _ in 0..3 {
            for bit in bits_lsb_first(&pkt) {
                if let Some(p) = d.push_bit(bit) { got.push(p); }
            }
        }
        assert!(got.len() >= 2, "offset {offset}: got {}", got.len());
        assert!(got.iter().all(|p| p.bytes == pkt && !p.repaired));
    }
}

#[test]
fn single_bit_error_is_repaired_when_locked() {
    let pkt = valid_sync_packet();
    let mut d = Deframer::new();
    for bit in bits_lsb_first(&pkt) { d.push_bit(bit); } // acquire lock
    let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
    corrupted[40] = !corrupted[40];
    let mut got = Vec::new();
    for bit in corrupted { if let Some(p) = d.push_bit(bit) { got.push(p); } }
    assert_eq!(got.len(), 1);
    assert!(got[0].repaired);
    assert_eq!(got[0].bytes, pkt);
}

#[test]
fn resyncs_after_consecutive_garbage() {
    let pkt = valid_sync_packet();
    let mut d = Deframer::new();
    for bit in bits_lsb_first(&pkt) { d.push_bit(bit); }
    // 5 stride-lengths of noise (> MAX_CONSECUTIVE_BAD), then clean packets
    for i in 0..(5 * 96) { d.push_bit(i % 3 == 0); }
    let mut got = Vec::new();
    for _ in 0..3 {
        for bit in bits_lsb_first(&pkt) {
            if let Some(p) = d.push_bit(bit) { got.push(p); }
        }
    }
    assert!(!got.is_empty(), "must reacquire after garbage");
}

#[test]
fn ephemeris_stride_is_24_bytes() {
    // A valid 24-byte ephemeris packet followed immediately by a sync
    // packet: both must emit (the locked stride must honor packet_len
    // of the *peeked* header, not a fixed 12).
    /* build with encode_ephemeris_for_test + valid_sync_packet */
}
```

Test helpers `valid_sync_packet()` and `bits_lsb_first(&[u8]) -> impl Iterator<Item = bool>` live in the test module; `bits_lsb_first` yields bit 0 (LSB) of each byte first — the same convention `Deframer` assembles with.

- [ ] **Step 2: Run — FAIL.** (helpers compile, `Deframer` missing)
- [ ] **Step 3: Implement** per the design block above. Alignment probe cost is bounded (96 offsets × 12 bytes, only in `Searching`); repair is 192 re-checksums worst case, only on locked failures.
- [ ] **Step 4: Run — PASS** including the all-offsets sweep.
- [ ] **Step 5: Commit** `feat(orbcomm): streaming deframer with checksum lock + 1-bit repair (#865)`

---

### Task 6: Message reassembly

**Files:**
- Create: `crates/sdr-orbcomm/src/reassembly.rs`

**Interfaces:**
- Consumes: `OrbcommPacket::Other { packet_type: PacketType::Message, bytes }` — fields per spec: `bytes[1]` = msg_total_length (hex chars 2–3 → low nibble of byte 1: use the reference layout — total length and packet number are the two nibbles of byte 1: `total = bytes[1] & 0x0F`, `num = bytes[1] >> 4`; verify nibble order against the reference during the fixture gate and flip in ONE place if reversed — the accessor functions `msg_total_len(bytes)` / `msg_seq_num(bytes)`), payload = `bytes[2..11]` (9 data bytes; check bytes excluded).
- Produces: `pub struct Reassembler` with `pub fn new(max_age_packets: u32) -> Self`, `pub fn push(&mut self, bytes: &[u8]) -> Option<CompletedMessage>`, `pub fn tick(&mut self) -> Option<CompletedMessage>` (age accounting per pushed packet; stale sequence flushes as partial); `pub struct CompletedMessage { pub bytes: Vec<u8>, pub partial: bool }`.

- [ ] **Step 1: Failing tests** — four cases from the spec: in-order completion (3-fragment message concatenates payloads in sequence order), out-of-order fragments still complete, missing fragment → stale flush with `partial: true` after `max_age_packets` further pushes, and a fresh sequence restarting after a flush. Write them with a `msg_fragment(seq, total, payload9)` test builder that produces checksum-valid Message packets.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** — single in-flight sequence per `Reassembler` (one per channel; Orbcomm interleaves rarely — a fragment with a different `total` restarts the sequence, flushing the old one as partial). `const DEFAULT_MAX_AGE_PACKETS: u32 = 50;`
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(orbcomm): multi-packet message reassembly (#865)`

---

### Task 7: SDPSK demodulator

**Files:**
- Create: `crates/sdr-orbcomm/src/demod.rs`

**Interfaces:**
- Consumes: complex samples at `CHANNEL_SAMPLE_RATE_HZ` (4 sps), type `sdr_types::Complex`.
- Produces: `pub struct SdpskDemod` with `pub fn new() -> Self`, `pub fn process(&mut self, samples: &[Complex], bits_out: &mut Vec<bool>)`. Internals: RRC matched filter (α = 0.4, span 8 symbols, 33 taps at 4 sps — generate with a local `fn rrc_taps(alpha: f64, sps: usize, span_symbols: usize) -> Vec<f32>` ported from the reference's `rrcosfilter`), Gardner timing-error detector driving a fractional resampler phase (mu-tracking over the 4 sps grid), then per-symbol `d[n] = s[n] · conj(s[n−1])`; `im(d) > 0` ⇒ +90° shift ⇒ coded bit 1; NRZ-M decode: `info = coded ^ prev_coded`. The two convention points (shift-sign→bit, NRZ-M on/off) are isolated in `fn bit_convention(coded: bool, prev: bool) -> bool` with a doc comment saying the real-capture fixture (Task 9) is the arbiter — if the fixture decodes nothing, flip HERE first (sign), THEN try disabling the XOR; never scatter convention changes.

- [ ] **Step 1: Failing test — loopback through a test-only modulator.**

```rust
#[cfg(test)]
fn modulate_sdpsk(bits: &[bool]) -> Vec<Complex> {
    // PDF-literal transmitter: info bits → NRZ-M encode
    // (coded[n] = info[n] ^ coded[n-1]) → phase step ±90°
    // (1 ⇒ +90°, 0 ⇒ −90°) → 4 sps impulse train → RRC pulse shaping
    // with the same rrc_taps as the receiver.
    /* implemented alongside the demod */
}

#[test]
fn loopback_clean() {
    let bits: Vec<bool> = (0..2048).map(|i| (i * 7 + 3) % 5 < 2).collect();
    let iq = modulate_sdpsk(&bits);
    let mut out = Vec::new();
    SdpskDemod::new().process(&iq, &mut out);
    assert_recovered(&bits, &out); // helper: alignment-tolerant compare,
                                   // ≥ 99.5% of bits after sync-up
}

#[test]
fn loopback_with_cfo_and_noise() {
    // ±3.5 kHz CFO is worst-case Doppler at 137 MHz — hmm, at 19.2 ksps
    // that is 0.18 rad/sample of rotation; delay-conj-multiply sees a
    // constant phase bias of 2π·3500/4800 per SYMBOL — that exceeds 90°!
    // ⇒ the channelizer must pre-correct coarse CFO; the demod itself is
    // specified for residual CFO ≤ ±800 Hz (phase bias < 60°, decision
    // margin retained). Test at ±800 Hz + AWGN at 15 dB SNR.
    for cfo_hz in [-800.0, 800.0] {
        let bits: Vec<bool> = (0..4096).map(|i| i % 3 == 0).collect();
        let iq = add_awgn(apply_cfo(modulate_sdpsk(&bits), cfo_hz), 15.0);
        let mut out = Vec::new();
        SdpskDemod::new().process(&iq, &mut out);
        assert_recovered(&bits, &out);
    }
}

#[test]
fn loopback_with_sample_clock_offset() {
    // ±50 ppm symbol-clock error over 4096 symbols: Gardner must track.
    /* resample the modulated signal by 1.00005 with sdr_dsp::RationalResampler,
       then demod and assert_recovered */
}
```

- [ ] **Step 2: Run — FAIL** (modulator + demod missing).
- [ ] **Step 3: Implement** demod + test modulator + `apply_cfo`/`add_awgn`/`assert_recovered` helpers. **Note the CFO discovery hard-coded into the test comment: the per-channel chain in Task 8 must include coarse frequency correction** (see Task 8's FLL) — the demod contract is residual CFO ≤ ±800 Hz.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(orbcomm): SDPSK demod (RRC + Gardner + delay-conjugate detector) (#865)`

---

### Task 8: Channelizer + ChannelBank

**Files:**
- Create: `crates/sdr-orbcomm/src/channelizer.rs`
- Modify: `crates/sdr-orbcomm/src/lib.rs` (public `ChannelBank`, `OrbcommEvent`, `ChannelStats`, module decls)

**Interfaces:**
- Consumes: everything above; `sdr_dsp::RationalResampler::new(in_rate, out_rate)`.
- Produces (the crate's whole public runtime API):

```rust
pub struct OrbcommEvent { pub channel_hz: f64, pub kind: OrbcommEventKind }
pub enum OrbcommEventKind {
    Packet { packet: OrbcommPacket, repaired: bool },
    MessageComplete { bytes: Vec<u8>, partial: bool },
}
#[derive(Clone, Debug)]
pub struct ChannelStats {
    pub freq_hz: f64, pub in_span: bool,
    pub packets_ok: u64, pub checksum_fail: u64, pub repaired: u64,
}
impl ChannelBank {
    pub fn new(source_rate_hz: f64, center_hz: f64, channels: &[f64]) -> Result<Self, OrbcommError>;
    pub fn process(&mut self, iq: &[sdr_types::Complex], events: &mut Vec<OrbcommEvent>);
    pub fn stats(&self) -> Vec<ChannelStats>;
}
```

**Per-channel chain:** complex mix by `channel_hz − center_hz` (phase-continuous NCO across blocks) → `RationalResampler(source_rate, CHANNEL_SAMPLE_RATE_HZ)` → **coarse CFO correction: FFT-free frequency-locked loop on the signal's mean delay-conjugate phase** — the mean of `arg(s[n]·conj(s[n−1]))` over a block, minus the expected zero-mean of balanced SDPSK, integrates into an NCO correction; bandwidth a few Hz, capture range ±3.5 kHz (Doppler), leaving residual within the demod's ±800 Hz contract → `SdpskDemod` → `Deframer` → `parse_packet` → per-channel `Reassembler`. Channels whose |offset| + half-bandwidth exceed source Nyquist are constructed as `in_span: false` stubs that ignore input. Error if ALL are out of span (`OrbcommError::NoChannelsInSpan`).

- [ ] **Step 1: Failing tests** — (a) `two_channels_decode_independently`: synthesize two SDPSK signals (the Task 7 test modulator, each carrying distinct valid Sync packets in a bit loop), mix to +100 kHz and −150 kHz offsets in a 2.4 Msps stream, feed `ChannelBank::new(2.4e6, 137.5125e6, &[137.6125e6, 137.3625e6])`, assert each channel's events carry its own sat_id; (b) `doppler_shifted_channel_still_decodes`: one channel offset by an extra +3 kHz "Doppler", still decodes (proves the FLL); (c) `out_of_span_channel_flagged`: `ChannelBank::new(240_000.0, 137.5e6, &ORBCOMM_CHANNELS_HZ)` — channels beyond ±120 kHz have `in_span == false`, no error; (d) `no_channels_in_span_errors`.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement.** Keep the NCO phase as `f64` accumulated modulo 2π; per-block processing allocates nothing (reuse scratch buffers in `self`).
- [ ] **Step 4: Run — PASS.** Also run the full crate suite.
- [ ] **Step 5: Commit** `feat(orbcomm): per-channel FLL + ChannelBank public API (#865)`

---

### Task 9: Real-capture fixture test

**Files:**
- Create: `scripts/orbcomm-mat-to-iq.py`, `crates/sdr-orbcomm/tests/real_capture.rs`

**Interfaces:**
- Consumes: `ChannelBank` public API.
- Produces: the convention arbiter for Task 7's `bit_convention`.

- [ ] **Step 1: Conversion script** (dev-side, uv-style header, scipy):

```python
#!/usr/bin/env python3
"""Convert an ORBCOMM-receiver .mat capture to raw interleaved f32 IQ.

Usage: uv run --with scipy scripts/orbcomm-mat-to-iq.py \
    original/ORBCOMM-receiver/data/1552071892p6.mat /tmp/orbcomm-fixture
Writes <out>.iq (f32 le interleaved) and <out>.json (center_hz,
sample_rate, sat metadata) for the ignored test to read.
"""
import json, sys
import numpy as np
from scipy.io import loadmat

mat = loadmat(sys.argv[1])
samples = mat["samples"].flatten().astype(np.complex64)
meta = {
    "center_hz": float(mat["fc"].flatten()[0]),
    "sample_rate": float(mat["fs"].flatten()[0]),
}
samples.view(np.float32).tofile(sys.argv[2] + ".iq")
json.dump(meta, open(sys.argv[2] + ".json", "w"))
print(f"{len(samples)} samples @ {meta['sample_rate']} Hz, center {meta['center_hz']/1e6} MHz")
```

(Key names `samples`/`fc`/`fs` must be checked against the actual .mat once — `loadmat(...)` keys print with `list(mat)`; adjust in the script, which is dev-side and unreviewed by CI.)

- [ ] **Step 2: Ignored integration test** reading `ORBCOMM_IQ_FIXTURE` env var (path prefix): load `.iq` + `.json`, run through `ChannelBank` with the recording's center/rate and the full channel list, assert ≥ 10 checksum-valid packets and ≥ 1 `Sync` with a known `sat_name`, and every decoded `Ephemeris` altitude within 400–1000 km. `#[ignore = "needs local IQ fixture — see scripts/orbcomm-mat-to-iq.py"]`.
- [ ] **Step 3: Run it for real** (`cargo test -p sdr-orbcomm --test real_capture -- --ignored` with the fixture built from the reference's `data/*.mat`). **This is the convention gate: if zero packets decode, flip `bit_convention` variants (sign, then XOR) and the Message nibble order until the capture decodes.** Record the winning convention in `demod.rs`'s doc comment.
- [ ] **Step 4: Commit** `test(orbcomm): real-capture fixture gate + .mat conversion script (#865)`

---

### Task 10: sdr-core tap + messages

**Files:**
- Modify: `crates/sdr-core/Cargo.toml` (add `sdr-orbcomm.workspace = true`), `crates/sdr-core/src/messages.rs`, `crates/sdr-core/src/controller.rs` (state fields + `process_iq_block` call + command dispatch)
- Create: `crates/sdr-core/src/controller/orbcomm.rs`
- Test: `crates/sdr-core/src/controller/tests/orbcomm.rs` (registered in the controller tests mod like `recording_acars.rs`)

**Interfaces:**
- Consumes: `sdr_orbcomm::{ChannelBank, ChannelStats, OrbcommEvent, ORBCOMM_CHANNELS_HZ}`.
- Produces:
  - `UiToDsp::SetOrbcommEnabled(bool)` (next to `SetAcarsEnabled`, messages.rs ~line 582).
  - `DspToUi::OrbcommEvent(Box<sdr_orbcomm::OrbcommEvent>)`, `DspToUi::OrbcommChannelStats(Box<[sdr_orbcomm::ChannelStats]>)`, `DspToUi::OrbcommEnabledChanged(bool)` (next to the Acars variants ~line 225).
  - `pub(super) fn orbcomm_decode_tap(bank: &mut Option<sdr_orbcomm::ChannelBank>, init_failed: &mut bool, source_rate_hz: f64, center_hz: f64, iq: &[sdr_types::Complex], dsp_tx: &mpsc::Sender<DspToUi>)` — clone of `acars_decode_tap`'s lazy-init/latch shape (crates/sdr-core/src/controller/acars.rs:34) minus the output writers; always uses `&ORBCOMM_CHANNELS_HZ`.
  - `DspState` fields: `orbcomm_bank: Option<sdr_orbcomm::ChannelBank>`, `orbcomm_init_failed: bool`, `orbcomm_enabled: bool`, `orbcomm_stats_emitted_at: Option<std::time::Instant>` (1 Hz throttle, same value as the ACARS stats throttle constant).
  - Handler: `SetOrbcommEnabled(v)` sets the flag, clears `orbcomm_bank`/`orbcomm_init_failed` on disable AND on enable (fresh geometry pickup), acks with `OrbcommEnabledChanged(v)`. Source stop/retune clears bank + latch exactly where the ACARS ones are cleared (grep `acars_bank = None` in controller/acars.rs:272,611,687,738 and mirror at the geometry-relevant sites — tune changes must rebuild the bank because `center_hz` is baked in).
- [ ] **Step 1: Failing tests** (`controller/tests/orbcomm.rs`): (a) `set_enabled_acks_and_clears_bank`; (b) `tap_lazy_inits_and_emits_events` — feed a synthetic block (reuse the crate's test modulator via a `#[cfg(test)]`-exported helper or synthesize inline with `sdr_orbcomm` public API by processing a pre-built IQ vector) and assert an `OrbcommEvent` lands on `dsp_tx`; (c) `init_failure_latches_once` — absurd source rate (0.0) → one error path, no repeat.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement** (tap file mirrors acars.rs including the `bytemuck` compile-time layout guards if the cast is used — copy those const asserts verbatim).
- [ ] **Step 4: Run — PASS**, plus `cargo test -p sdr-core`.
- [ ] **Step 5: Commit** `feat(core): orbcomm decode tap + enable plumbing (#865)`

---

### Task 11: Viewer window

**Files:**
- Create: `crates/sdr-ui/src/orbcomm_viewer.rs` (+ `orbcomm_viewer/tests.rs` for pure helpers)
- Modify: `crates/sdr-ui/src/lib.rs` (module decl), `crates/sdr-ui/Cargo.toml` (`sdr-orbcomm.workspace = true`), menu/action wiring where `connect_acars_action` lives (grep `acars_viewer` call sites in `crates/sdr-ui/src/window.rs` / menu builder), `crates/sdr-ui/src/window/dsp_events/` (new `orbcomm_events.rs` beside `acars_events.rs`)

**Interfaces:**
- Consumes: `DspToUi::{OrbcommEvent, OrbcommChannelStats, OrbcommEnabledChanged}`; sends `UiToDsp::SetOrbcommEnabled`.
- Produces: `pub fn open_orbcomm_viewer_if_needed(...)` mirroring the ACARS viewer's open function signature (copy its exact parameter list — it takes the app window, state handle, and UI→DSP sender), plus pure formatting helpers `fn format_packet_row(&OrbcommEvent) -> String` and `fn format_hexdump(bytes: &[u8]) -> String` (16 bytes/row, `|`-delimited printable-ASCII gutter, non-printables as `.`) that carry the unit tests.

- [ ] **Step 1: Failing tests for the pure helpers** (hexdump layout: exact expected string for a 20-byte input; packet row for an `Ephemeris` event: contains sat name, `°N`/`°S` hemisphere handling, `km` altitude, `Z` time; `repaired` marker `~`; `partial` marker on messages).
- [ ] **Step 2: Run — FAIL**, implement helpers, PASS, commit `feat(ui): orbcomm row/hexdump formatters (#865)`.
- [ ] **Step 3: Build the window** following `acars_viewer.rs` structure: header bar with an enable switch (sends `SetOrbcommEnabled`), a 9-cell channel-activity strip (label per channel: MHz + ok/fail counters, dimmed when `in_span == false`), scrolled monospace `GtkTextView` log capped at `MAX_LOG_LINES = 500` (drop from top), ephemeris/message rows via the tested formatters. Menu action + accelerator following the ACARS pattern (pick the next free `<Ctrl><Shift>` letter; check `activity_bar.rs`/help dialog for collisions).
- [ ] **Step 4: Wire `dsp_events/orbcomm_events.rs`** dispatching the three `DspToUi` variants to the viewer (and, next task, the panel).
- [ ] **Step 5:** `cargo clippy -p sdr-ui --all-targets -- -D warnings`, `cargo test -p sdr-ui`, commit `feat(ui): orbcomm viewer window + menu action (#865)`.

---

### Task 12: Satellites panel "Heard via Orbcomm"

**Files:**
- Modify: `crates/sdr-ui/src/sidebar/satellites_panel.rs` (+ its tests file), `crates/sdr-ui/src/window.rs::connect_satellites_panel` (or the dsp_events dispatcher from Task 11)

**Interfaces:**
- Consumes: `DspToUi::OrbcommEvent` (only `Sync`/`Ephemeris` kinds).
- Produces: a `HeardSatellites` model (pure, testable): `fn record(&mut self, sat_id: u8, position: Option<(f64, f64, f64)>, now: Instant)`, `fn rows(&self, now: Instant) -> Vec<HeardRow>` where `HeardRow { name: String, age_secs: u64, position: Option<(f64, f64, f64)> }`; rows expire after `HEARD_EXPIRY_SECS = 1200` (20 min ≈ one pass + margin). UI: an `AdwPreferencesGroup` titled "Heard via Orbcomm" with plain-English description, rows refreshed on a 5 s `glib::timeout_add_seconds` tick, group hidden when empty or decoder disabled.

- [ ] **Step 1: Failing tests for `HeardSatellites`** (record/rows/expiry/unknown-sat_id fallback to `FM-? (0xNN)` label).
- [ ] **Step 2: Run — FAIL**, implement, PASS.
- [ ] **Step 3: Build the group + wiring** (model lives behind `Rc<RefCell<...>>` like other panel state; the dsp_events handler feeds `record`, the tick re-renders).
- [ ] **Step 4: Gates:** `cargo clippy --all-targets --workspace -- -D warnings`, `cargo test --workspace`.
- [ ] **Step 5: Commit** `feat(ui): Heard-via-Orbcomm rows in the Satellites panel (#865)`

---

### Task 13: Live gate, docs, and ship

- [ ] **Step 1: CLAUDE.md** — add `sdr-orbcomm` to the crate table (one line: `sdr-orbcomm → Orbcomm downlink decoder — 9-channel SDPSK, packets/ephemeris/message reassembly (depends on: types, dsp)`), and a "Key files" bullet under a short Orbcomm note in the satellite section.
- [ ] **Step 2: Real-data gate part 2 (live):** `make install CARGO_FLAGS="--release --features whisper-cuda"`… **no — install the user's current daily flavor: `--release --no-default-features --features sherpa-cuda`.** Then the user tunes ~137.5 MHz with the V-dipole + USB-powered SAWbird, enables the decoder, and verifies: channel strip lights on active channels, Sync packets with real FM names, at least one plausible Ephemeris row, Satellites panel populates. (User runs the GTK smoke test; provide this checklist, do not launch the binary.)
- [ ] **Step 3: Full pre-push gates** as separate commands: workspace clippy (CI form), workspace tests, `cargo check --locked` (Cargo.toml changed), sherpa-cpu clippy (transcription untouched but CI runs it — cheap insurance), fmt LAST.
- [ ] **Step 4: Push branch, open PR** titled `feat(orbcomm): multi-channel Orbcomm downlink decoder (#865)` with the standard footer, referencing the spec path. Then the normal CodeRabbit/Codacy review workflow (batch fixes, reply, resolve).

---

## Self-review notes

- Spec coverage: channelizer/9 channels (T8), SDPSK chain (T7), deframe+repair (T5), packet types+Fletcher (T2–T3), ephemeris (T4), reassembly (T6), viewer+hexdump (T11), panel (T12), stats/error latch (T8/T10), fixture+live gates (T9/T13), non-goals untouched. CFO discovery (Doppler exceeds the delay-multiply margin at symbol rate) is resolved by the per-channel FLL in T8 with the demod contract pinned at ±800 Hz residual (T7).
- Type consistency: `OrbcommEvent{channel_hz, kind}` + `OrbcommEventKind::{Packet{packet, repaired}, MessageComplete{bytes, partial}}` used identically in T8/T10/T11/T12; `ChannelStats` fields match T8's definition everywhere; `fletcher16_check_bytes` used by T3–T6 test builders.
- The Message length/seq nibble order and the demod bit conventions are explicitly fixture-arbitrated (T9) with single-point flip sites — not silent assumptions.
