# Orbcomm Multi-Channel Decoder — Design (issue #865)

Date: 2026-08-29
Status: approved (design discussion in-session; user picked scope C + channelizer A)

## Goal

Decode the Orbcomm satellite downlink (137.2–137.8 MHz, SDPSK 4800 bps)
across all nine active channels simultaneously, surfacing:

1. a live packet-viewer window (ACARS-viewer style), and
2. "Heard via Orbcomm" annotations in the Satellites activity panel
   (satellite name, last-heard age, self-reported position).

The M2M message payloads are proprietary; we decode protocol structure,
spacecraft ID, and the self-broadcast ephemeris — the established
hobbyist scope. Reference implementation:
[fbieberly/ORBCOMM-receiver](https://github.com/fbieberly/ORBCOMM-receiver)
(MIT-style; see its LICENSE), plus the Decode Systems Orbcomm page and
the Orbcomm Serial Interface Specification E80050015 Rev F (both in the
reference repo's `literature/`).

## Protocol reference (captured so implementation needs no re-research)

### Physical layer

- SDPSK at **4800 baud**: bit 0 = −90° phase shift, bit 1 = +90° shift,
  pulse-shaped with a **0.4 roll-off root-raised-cosine** filter.
- Information bits are additionally **NRZ-M differentially encoded**
  (each bit XOR previous). Data words are 8 bits, **LSB transmitted
  first**. RHCP polarization.
- Minor frame = 1 second = 4800 bits = 600 bytes; major frame = 16
  minor frames. (Frames are informational — packet alignment below does
  not depend on frame boundaries.)
- Active subscriber downlink channels (MHz): **137.2250, 137.2500,
  137.4400, 137.4600, 137.6625, 137.6875, 137.7175, 137.7375,
  137.8000**.

### Link layer

- Packets are **12 bytes**, except **Ephemeris packets = 24 bytes**.
  There is no dedicated frame-sync word: the known packet header bytes
  themselves are the sync, found by scoring bit offsets modulo 96 bits.
- Header byte (first byte) identifies the type:

  | Type          | Header | Notable fields (hex-char offsets, per reference) |
  |---------------|--------|--------------------------------------------------|
  | Sync          | `0x65` | code (0,6), sat_id (6,8)                         |
  | Message       | `0x1A` | msg_total_length (2,3), msg_packet_num (3,4), data (4,22) |
  | Uplink info   | `0x1B` | same layout as Message                           |
  | Downlink info | `0x1C` | same layout as Message                           |
  | Network       | `0x1D` | same layout as Message                           |
  | Fill          | `0x1E` | data (2,22)                                      |
  | Ephemeris     | `0x1F` | sat_id (2,4), data (4,48) — 24-byte packet       |
  | Orbital       | `0x22` | (opaque)                                         |

- Checksum: **Fletcher-16 (mod-256 sums) over the entire packet must
  equal 0x0000**. The reference optionally brute-forces single-bit
  errors by flipping each bit until the checksum zeroes; we include
  this (cheap, and marked as "repaired" in the output).

### Ephemeris payload

- Byte-reversed payload; GPS epoch **Jan 6 1980**: 2-byte week number +
  3-byte time-of-week (seconds) → current satellite UTC time.
- Position and velocity: six 20-bit fields (x/y/z ECEF position, then
  velocity), scaled as `value = 2·raw·MAX/2^20 − MAX` with
  **MAX_R = 8,378,155.0 m** and **MAX_V = 7,700.0 m/s**, following the
  serial-interface spec. ECEF → WGS84 lat/lon/alt via the standard
  ellipsoid conversion (a = 6378137.0, 1/f = 298.257223563).
- `sat_id` maps to the public "FM-xx" spacecraft names via the
  reference's `sat_db.py` table.

## Architecture

```
wideband IQ (pre-VFO, processed_buf)
   └── orbcomm_decode_tap (sdr-core, sibling of acars_decode_tap)
         └── sdr_orbcomm::ChannelBank
               ├── per-channel: mix → decimate (~19.2 ksps, 4 sps)
               │     → RRC matched filter (α=0.4)
               │     → symbol timing recovery
               │     → delay-conjugate-multiply demod (±90° → bit)
               │     → bit conventions (NRZ-M / LSB-first, per reference)
               │     → synchronous per-byte packet state machine
               │     → Fletcher-16 verify (+ 1-bit repair)
               └── typed OrbcommEvent stream
                     ├── DspToUi → orbcomm_viewer window
                     └── DspToUi → Satellites panel "Heard via Orbcomm"
```

### Crate: `crates/sdr-orbcomm`

Pure decode — no I/O, no threads, no GTK. Depends on `sdr-types` +
`sdr-dsp` only (reuse multirate decimators + filter design; the
sdr-acars channelizer is an external LGPL crate and ACARS-specific, so
the channelizer here is our own mix+decimate per fixed channel).

Public API:

- `ORBCOMM_CHANNELS_HZ: [f64; 9]` — the fixed channel list.
- `ChannelBank::new(source_rate_hz, center_hz, &channels) -> Result<Self, OrbcommError>`
  (channels outside the source span are skipped with a per-channel flag,
  matching ACARS behavior).
- `ChannelBank::process(&[Complex]) -> Vec<OrbcommEvent>`.
- `OrbcommEvent { channel_hz, packet: OrbcommPacket, repaired: bool }`.
- `OrbcommPacket` enum: `Sync { code, sat_id }`,
  `Ephemeris { sat_id, sat_time_utc, lat_deg, lon_deg, alt_m, vel_ms }`,
  `Message/UplinkInfo/DownlinkInfo/Network/Fill/Orbital { hex payload }`.
- `sat_name(sat_id: u8) -> Option<&'static str>`.
- Per-channel stats accessor for the activity strip (packet + checksum
  counters).

Module layout: `channelizer.rs`, `demod.rs` (SDPSK chain),
`deframe.rs` (bit → aligned packet bytes), `packet.rs` (types,
Fletcher, ephemeris math), `sat_names.rs`, `tests/` inline per module
plus crate-level fixtures.

### sdr-core integration

- `controller/orbcomm.rs`: `orbcomm_decode_tap` mirroring
  `acars_decode_tap` exactly (lazy-init bank + `init_failed` latch,
  cleared on source stop; zero-copy cast guard; throttled channel-stats
  emission).
- Messages: `UiToDsp::SetOrbcommEnabled(bool)`;
  `DspToUi::OrbcommPacket(...)` and `DspToUi::OrbcommChannelStats(...)`.
- No file/UDP output writers in V1 (unlike ACARS) — the viewer and
  panel are the only consumers. JSONL export is a listed non-goal.

### UI

- `crates/sdr-ui/src/orbcomm_viewer.rs`: window modeled on
  `acars_viewer.rs` — per-channel activity strip across the top
  (9 fixed channels), monospace packet log below. Ephemeris rows render
  as `FM-114 · 51.2°N 7.4°E · 715 km · 7.45 km/s · 19:42:11Z`; other
  packets as `type · sat/channel · hex`. Checksum-failed packets are
  counted in the strip, not listed. Repaired packets get a marker.
  Menu action + accelerator following the ACARS/SSTV pattern.
- Satellites panel: new "Heard via Orbcomm" `AdwPreferencesGroup` —
  one row per heard spacecraft: name, last-heard relative age, last
  self-reported position. Rows age out after a pass (no persistence).
  Only visible while the decoder is engaged.

## Error handling

- Bank construction failure latches `init_failed` (LRPT/ACARS pattern);
  surfaced once via a `DspToUi` error, not per-block.
- Checksum failures are counters, never log spam.
- Ephemeris fields are range-checked (|lat| ≤ 90, plausible altitude
  band 400–1000 km); implausible decodes are demoted to a raw-hex event
  rather than fed to the Satellites panel.

## Testing

Unit (TDD per layer):

- Fletcher-16 vectors (including the zero-sum whole-packet property and
  a known-good packet from the reference recordings).
- Ephemeris decode: known 24-byte packet → expected time/lat/lon/alt
  (ground truth from running the reference decoder on its own sample).
- Deframe alignment: synthetic bit streams at all 96 offsets, with
  injected bit errors (exercising the 1-bit repair) and mid-stream
  resync.
- Demod: synthesized SDPSK bursts with AWGN, CFO up to ±3.5 kHz
  (worst-case Doppler at 137 MHz), and sample-clock offset; require
  clean BER at reasonable SNR.
- Channel bank: two simultaneous synthetic channels decode
  independently; out-of-span channels skipped.

Real-data gate (required before "done" — DSP rule):

1. The reference repo's `.mat` captures (complex64 + center freq +
   sample rate + TLE ground truth) converted by a small dev-side script
   into a raw IQ fixture; an `#[ignore]`-by-default test decodes it and
   asserts checksum-valid packets with ephemeris within tens of km of
   the TLE position.
2. Live capture from the station antenna (`rtl_sdr` or in-app), decoded
   end-to-end through the app with the viewer and panel populated.

## Non-goals (V1)

- Decoding/storing M2M message payload contents (proprietary; raw hex
  only).
- JSONL/UDP export, persistence of heard satellites across sessions.
- Doppler-corrected per-satellite channel tracking; TLE cross-check
  UI beyond the position row.
- Uplink (148 MHz) anything.
