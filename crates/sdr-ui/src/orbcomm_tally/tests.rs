use super::*;
use sdr_orbcomm::packet::{OrbcommPacket, PacketType};
use sdr_orbcomm::{OrbcommEvent, OrbcommEventKind};

fn packet_event(packet: OrbcommPacket) -> OrbcommEvent {
    OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet {
            packet,
            repaired: false,
        },
    }
}

#[test]
fn counts_each_packet_type() {
    let mut t = OrbcommTally::default();
    t.record(&packet_event(OrbcommPacket::Sync {
        code: 0,
        sat_id: 0x2C,
    }));
    t.record(&packet_event(OrbcommPacket::Sync {
        code: 0,
        sat_id: 0x2C,
    }));
    t.record(&packet_event(OrbcommPacket::Other {
        packet_type: PacketType::Message,
        bytes: vec![],
    }));
    let out = t.format_breakdown(0, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("Sync") && l.split_whitespace().last() == Some("2"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("Message") && l.split_whitespace().last() == Some("1"))
    );
}

#[test]
fn message_complete_events_are_not_tallied() {
    let mut t = OrbcommTally::default();
    t.record(&OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::MessageComplete {
            bytes: vec![1, 2, 3],
            partial: false,
        },
    });
    // No parsed packet ⇒ all type counts stay zero.
    assert_eq!(
        t.format_breakdown(0, 0),
        OrbcommTally::default().format_breakdown(0, 0)
    );
}

#[test]
fn reset_zeroes_counts() {
    let mut t = OrbcommTally::default();
    t.record(&packet_event(OrbcommPacket::Sync { code: 0, sat_id: 1 }));
    t.reset();
    assert_eq!(
        t.format_breakdown(0, 0),
        OrbcommTally::default().format_breakdown(0, 0)
    );
}

#[test]
fn breakdown_includes_checksum_fail_and_repaired() {
    let t = OrbcommTally::default();
    let out = t.format_breakdown(7, 3);
    assert!(out.contains("checksum"));
    assert!(out.contains('7'));
    assert!(out.contains("repaired"));
    assert!(out.contains('3'));
}
