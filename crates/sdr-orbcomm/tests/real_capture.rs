//! Real-capture gate for the Orbcomm decoder (#865, Task 9).
//!
//! Runs the whole [`ChannelBank`] chain over an off-air RTL-SDR recording and
//! asserts the decoder produces real, checksum-valid traffic. This is the test
//! that arbitrated the physical-layer bit convention documented in
//! `demod.rs::bit_convention` and the Message nibble order in `reassembly.rs`:
//! synthetic loopback tests are self-consistent by construction and cannot tell
//! a differential-decode convention from its complement, so only a recording of
//! the actual air interface can.
//!
//! # Running it
//!
//! The fixture is a pair of files built from one of the two captures shipped
//! with the reference receiver (`original/ORBCOMM-receiver/data/*.mat`, a
//! gitignored reference clone), so this test is `#[ignore]`d and never runs in
//! CI:
//!
//! ```sh
//! uv run --with scipy --with numpy scripts/orbcomm-mat-to-iq.py \
//!     original/ORBCOMM-receiver/data/1552071892p6.mat /tmp/orbcomm/1552071892p6
//! ORBCOMM_IQ_FIXTURE=/tmp/orbcomm/1552071892p6 \
//!     cargo test -p sdr-orbcomm --test real_capture -- --ignored --nocapture
//! ```
//!
//! `ORBCOMM_IQ_FIXTURE` is a **path prefix**: `<prefix>.iq` is little-endian
//! f32 interleaved IQ and `<prefix>.json` is the capture metadata the converter
//! wrote. Both are required — a missing variable or file fails the test loudly
//! rather than skipping, because the test only ever runs when asked for by name.

// This is a diagnostic harness, not library code: it prints a full decode
// report under `--nocapture` and asserts rather than propagating errors.
#![allow(clippy::print_stdout, clippy::panic)]

use std::path::PathBuf;

use sdr_orbcomm::packet::{OrbcommPacket, PacketType};
use sdr_orbcomm::{
    ChannelBank, ORBCOMM_CHANNELS_HZ, OrbcommEvent, OrbcommEventKind, reassembly, sat_names,
};
use sdr_types::Complex;

/// Environment variable holding the fixture path prefix.
const FIXTURE_ENV: &str = "ORBCOMM_IQ_FIXTURE";
/// Source samples pushed per `process` call. Nothing depends on the value —
/// `block_fragmentation_does_not_change_the_output` pins that — but feeding a
/// 2 s capture in one 20 MB slab would be an odd way to exercise a streaming
/// API.
const FEED_BLOCK: usize = 65_536;

/// Checksum-valid packets a passing capture must yield.
const MIN_PACKETS: usize = 10;
/// Sync beacons a passing capture must yield.
const MIN_SYNC_PACKETS: usize = 1;
/// Plausible Orbcomm altitude band, metres — the same bound `decode_ephemeris`
/// applies internally, re-asserted here so a future widening of that gate
/// cannot quietly let a nonsense position through this test.
const MIN_ALT_M: f64 = 400_000.0;
/// Upper bound of the plausible altitude band, metres.
const MAX_ALT_M: f64 = 1_000_000.0;

/// The capture metadata `scripts/orbcomm-mat-to-iq.py` writes alongside the IQ.
struct Metadata {
    center_hz: f64,
    sample_rate: f64,
    timestamp: f64,
    sats: Vec<String>,
    tles: Vec<String>,
}

/// Read `<prefix>.json` and `<prefix>.iq`.
fn load_fixture() -> (Metadata, Vec<Complex>) {
    let prefix = std::env::var(FIXTURE_ENV).unwrap_or_else(|_| {
        panic!(
            "{FIXTURE_ENV} is not set. Build a fixture with \
             `uv run --with scipy --with numpy scripts/orbcomm-mat-to-iq.py \
             original/ORBCOMM-receiver/data/<capture>.mat <prefix>` and set \
             {FIXTURE_ENV}=<prefix>."
        )
    });

    let json_path = PathBuf::from(format!("{prefix}.json"));
    let iq_path = PathBuf::from(format!("{prefix}.iq"));

    let json_text = std::fs::read_to_string(&json_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", json_path.display()));
    let json: serde_json::Value = serde_json::from_str(&json_text)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", json_path.display()));

    let number = |key: &str| -> f64 {
        json.get(key)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or_else(|| panic!("{} has no numeric `{key}`", json_path.display()))
    };
    let strings = |key: &str| -> Vec<String> {
        json.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or_default().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };

    let meta = Metadata {
        center_hz: number("center_hz"),
        sample_rate: number("sample_rate"),
        timestamp: number("timestamp"),
        sats: strings("sats"),
        tles: strings("tles"),
    };

    let raw = std::fs::read(&iq_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", iq_path.display()));
    assert!(
        raw.len() % 8 == 0 && !raw.is_empty(),
        "{} is {} bytes — not a non-empty run of interleaved f32 pairs",
        iq_path.display(),
        raw.len()
    );
    let iq: Vec<Complex> = raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| {
            // Indexing a `[u8; 8]` with constants is checked at compile time, so
            // this splits the interleaved pair without a fallible conversion.
            let re = [c[0], c[1], c[2], c[3]];
            let im = [c[4], c[5], c[6], c[7]];
            Complex::new(f32::from_le_bytes(re), f32::from_le_bytes(im))
        })
        .collect();

    (meta, iq)
}

/// Human-readable name of a decoded packet's type.
fn type_name(packet: &OrbcommPacket) -> &'static str {
    match packet {
        OrbcommPacket::Sync { .. } => "Sync",
        OrbcommPacket::Ephemeris(_) => "Ephemeris",
        OrbcommPacket::Other { packet_type, .. } => match packet_type {
            PacketType::Sync => "Sync(raw)",
            PacketType::Message => "Message",
            PacketType::UplinkInfo => "UplinkInfo",
            PacketType::DownlinkInfo => "DownlinkInfo",
            PacketType::Network => "Network",
            PacketType::Fill => "Fill",
            PacketType::Ephemeris => "Ephemeris(raw)",
            PacketType::Orbital => "Orbital",
        },
    }
}

/// Format bytes as uppercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        // Writing to a `String` cannot fail; the result is discarded rather than
        // unwrapped so this stays panic-free.
        let _ = write!(out, "{b:02X}");
        out
    })
}

#[test]
#[ignore = "needs local IQ fixture — see scripts/orbcomm-mat-to-iq.py"]
#[allow(clippy::too_many_lines)]
fn real_capture_decodes() {
    let (meta, iq) = load_fixture();

    println!("=== capture ===");
    println!("  samples      {}", iq.len());
    println!("  sample rate  {} Hz", meta.sample_rate);
    println!("  centre       {} Hz", meta.center_hz);
    println!("  timestamp    {}", meta.timestamp);
    println!("  satellites   {}", meta.sats.join(", "));
    for line in &meta.tles {
        println!("  tle          {line}");
    }

    let mut bank = ChannelBank::new(meta.sample_rate, meta.center_hz, &ORBCOMM_CHANNELS_HZ)
        .expect("the capture's span must cover at least one Orbcomm channel");

    let mut events: Vec<OrbcommEvent> = Vec::new();
    for block in iq.chunks(FEED_BLOCK) {
        bank.process(block, &mut events);
    }

    println!("\n=== per-channel stats ===");
    for s in bank.stats() {
        if !s.in_span {
            continue;
        }
        println!(
            "  {:>11.4} MHz  in_span  packets_ok {:>4}  checksum_fail {:>5}  repaired {:>4}",
            s.freq_hz / 1e6,
            s.packets_ok,
            s.checksum_fail,
            s.repaired
        );
    }

    let packets: Vec<(&OrbcommPacket, f64, bool)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            OrbcommEventKind::Packet { packet, repaired } => {
                Some((packet, e.channel_hz, *repaired))
            }
            OrbcommEventKind::MessageComplete { .. } => None,
        })
        .collect();

    println!("\n=== packet types ===");
    let mut kinds: Vec<&str> = packets.iter().map(|(p, ..)| type_name(p)).collect();
    kinds.sort_unstable();
    kinds.dedup();
    for kind in kinds {
        let n = packets
            .iter()
            .filter(|(p, ..)| type_name(p) == kind)
            .count();
        println!("  {kind:<16} {n}");
    }
    let repaired = packets.iter().filter(|(_, _, r)| *r).count();
    println!(
        "  total {} packets ({repaired} via single-bit repair)",
        packets.len()
    );

    println!("\n=== sync beacons ===");
    for (packet, channel_hz, repaired) in &packets {
        if let OrbcommPacket::Sync { code, sat_id } = packet {
            println!(
                "  {:.4} MHz  code {code:06X}  sat_id {sat_id:02X} ({}){}",
                channel_hz / 1e6,
                sat_names::sat_label(*sat_id),
                if *repaired { "  [repaired]" } else { "" }
            );
        }
    }

    println!("\n=== ephemeris ===");
    let ephemerides: Vec<_> = packets
        .iter()
        .filter_map(|(p, ..)| match p {
            OrbcommPacket::Ephemeris(e) => Some(e),
            _ => None,
        })
        .collect();
    for e in &ephemerides {
        println!(
            "  sat_id {:02X}  t {}  lat {:8.4}  lon {:9.4}  alt {:6.1} km  |v| {:6.1} m/s",
            e.sat_id,
            e.sat_time_unix,
            e.lat_deg,
            e.lon_deg,
            e.alt_m / 1000.0,
            e.vel_ms
        );
    }
    // Checksum-valid ephemeris packets whose position failed `decode_ephemeris`'s
    // plausibility gate come back as `Other` — worth surfacing, since a wrong
    // nibble layout would show up here rather than as a decode failure.
    for (packet, ..) in &packets {
        if let OrbcommPacket::Other {
            packet_type: PacketType::Ephemeris,
            bytes,
        } = packet
        {
            println!("  IMPLAUSIBLE ephemeris payload: {}", hex(bytes));
        }
    }

    println!("\n=== message reassembly ===");
    let completions: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            OrbcommEventKind::MessageComplete { bytes, partial } => Some((bytes, *partial)),
            OrbcommEventKind::Packet { .. } => None,
        })
        .collect();
    println!("  {} completed messages", completions.len());
    for (bytes, partial) in completions.iter().take(8) {
        println!(
            "  {:>8} {} bytes: {}",
            if *partial { "partial" } else { "complete" },
            bytes.len(),
            hex(bytes)
        );
    }
    // The fragment header byte, both ways round, for the nibble-order arbitration.
    let fragments: Vec<&Vec<u8>> = packets
        .iter()
        .filter_map(|(p, ..)| match p {
            OrbcommPacket::Other {
                packet_type: PacketType::Message,
                bytes,
            } => Some(bytes),
            _ => None,
        })
        .collect();
    println!("  {} Message fragments seen", fragments.len());
    for bytes in fragments.iter().take(12) {
        let b1 = bytes.get(1).copied().unwrap_or(0);
        println!(
            "    byte1 {b1:02X}  high {}  low {}  (total {:?} seq {:?})  payload {}",
            b1 >> 4,
            b1 & 0x0F,
            reassembly::msg_total_len(bytes),
            reassembly::msg_seq_num(bytes),
            hex(reassembly::msg_payload(bytes).unwrap_or_default())
        );
    }

    // --- the gate ---------------------------------------------------------
    assert!(
        packets.len() >= MIN_PACKETS,
        "only {} checksum-valid packets decoded from the capture (need {MIN_PACKETS}) — \
         the physical-layer bit convention is probably wrong; see demod.rs::bit_convention",
        packets.len()
    );
    let sync_count = packets
        .iter()
        .filter(|(p, ..)| matches!(p, OrbcommPacket::Sync { .. }))
        .count();
    assert!(
        sync_count >= MIN_SYNC_PACKETS,
        "no Sync beacon in {} decoded packets (need {MIN_SYNC_PACKETS})",
        packets.len()
    );
    for e in &ephemerides {
        assert!(
            (MIN_ALT_M..=MAX_ALT_M).contains(&e.alt_m),
            "ephemeris altitude {:.1} km outside the plausible Orbcomm band",
            e.alt_m / 1000.0
        );
        assert!(
            e.lat_deg.is_finite() && e.lon_deg.is_finite() && e.vel_ms.is_finite(),
            "ephemeris carries a non-finite field: {e:?}"
        );
    }
}
