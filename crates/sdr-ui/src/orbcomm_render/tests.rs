use super::*;

/// Exact-layout test for [`format_hexdump`]: 20 bytes = one full
/// 16-byte row plus a 4-byte trailing row. Pins down the offset
/// format, the padded hex column, and the `|...|` ASCII gutter in
/// one shot — any layout regression breaks this string comparison
/// rather than a looser `contains` check.
#[test]
fn hexdump_exact_layout_for_20_bytes() {
    let bytes: Vec<u8> = (0..20).collect();
    let row1 = "00000000  00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F |................|";
    // Row 2 has 4 real bytes; the remaining 12 slots are padded with
    // 3 spaces apiece (the same width a real "XX " byte would take)
    // so the ASCII gutter lines up under row 1's.
    let pad = " ".repeat((HEXDUMP_BYTES_PER_ROW - 4) * 3);
    let row2 = format!("00000010  10 11 12 13 {pad}|....|");
    let expected = format!("{row1}\n{row2}");
    assert_eq!(format_hexdump(&bytes), expected);
}

#[test]
fn hexdump_printable_ascii_gutter() {
    let bytes = b"Hi!\x00\x7f";
    let dump = format_hexdump(bytes);
    // 'H', 'i', '!' are printable (0x20..=0x7E); 0x00 and 0x7F are not.
    assert!(dump.ends_with("|Hi!..|"));
}

fn sample_ephemeris(lat_deg: f64, lon_deg: f64) -> Ephemeris {
    Ephemeris {
        sat_id: 0x2C,
        // 1970-01-01T19:42:11Z.
        sat_time_unix: 19 * 3600 + 42 * 60 + 11,
        lat_deg,
        lon_deg,
        alt_m: 715_000.0,
        vel_ms: 7_450.0,
    }
}

#[test]
fn ephemeris_row_north_east_hemisphere() {
    let event = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet {
            packet: OrbcommPacket::Ephemeris(sample_ephemeris(51.2, 7.4)),
            repaired: false,
        },
    };
    assert_eq!(
        format_packet_row(&event),
        "Sat 0x2C · 51.2°N 7.4°E · 715 km · 7.45 km/s · 19:42:11Z"
    );
}

#[test]
fn ephemeris_row_south_west_hemisphere() {
    let event = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet {
            packet: OrbcommPacket::Ephemeris(sample_ephemeris(-33.9, -18.4)),
            repaired: false,
        },
    };
    assert_eq!(
        format_packet_row(&event),
        "Sat 0x2C · 33.9°S 18.4°W · 715 km · 7.45 km/s · 19:42:11Z"
    );
}

#[test]
fn ephemeris_row_repaired_gets_tilde_prefix() {
    let event = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet {
            packet: OrbcommPacket::Ephemeris(sample_ephemeris(51.2, 7.4)),
            repaired: true,
        },
    };
    assert!(format_packet_row(&event).starts_with('~'));
    assert_eq!(
        format_packet_row(&event),
        "~Sat 0x2C · 51.2°N 7.4°E · 715 km · 7.45 km/s · 19:42:11Z"
    );
}

#[test]
fn sync_row_format() {
    let event = OrbcommEvent {
        channel_hz: 137_460_000.0,
        kind: OrbcommEventKind::Packet {
            packet: OrbcommPacket::Sync {
                code: 0x0065_A8F9,
                sat_id: 0x2C,
            },
            repaired: false,
        },
    };
    assert_eq!(
        format_packet_row(&event),
        "Sync · Sat 0x2C · code 65A8F9 · 137.4600 MHz"
    );
}

#[test]
fn other_packet_row_format() {
    let event = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::Packet {
            packet: OrbcommPacket::Other {
                packet_type: PacketType::UplinkInfo,
                bytes: vec![0x1B, 0x0A, 0x0B],
            },
            repaired: false,
        },
    };
    assert_eq!(
        format_packet_row(&event),
        "UplinkInfo · 137.8000 MHz · 1B0A0B"
    );
}

#[test]
fn message_complete_partial_marker() {
    let event = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::MessageComplete {
            bytes: vec![0x1A, 0x0B],
            partial: true,
        },
    };
    let row = format_packet_row(&event);
    assert!(row.starts_with("Message complete · 137.8000 MHz [partial]\n"));
    assert!(row.contains("1A 0B"));
}

#[test]
fn message_complete_without_partial_has_no_marker() {
    let event = OrbcommEvent {
        channel_hz: 137_800_000.0,
        kind: OrbcommEventKind::MessageComplete {
            bytes: vec![0x1A, 0x0B],
            partial: false,
        },
    };
    let row = format_packet_row(&event);
    assert!(row.starts_with("Message complete · 137.8000 MHz\n"));
    assert!(!row.contains("[partial]"));
}
