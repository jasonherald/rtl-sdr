//! Pure, GTK-free rendering helpers for the Orbcomm surface: packet /
//! ephemeris / message-complete log lines, the classic hexdump block,
//! coordinate + UTC formatting, and channel-cell label text. Extracted
//! from the former `orbcomm_viewer` window so the Orbcomm activity
//! panel (`sidebar/orbcomm_panel.rs`) reuses one tested formatter set.

use sdr_orbcomm::channelizer::ChannelStats;
use sdr_orbcomm::packet::{Ephemeris, OrbcommPacket, PacketType};
use sdr_orbcomm::sat_names::sat_label;
use sdr_orbcomm::{OrbcommEvent, OrbcommEventKind};

/// Bytes rendered per hexdump row.
pub(crate) const HEXDUMP_BYTES_PER_ROW: usize = 16;

/// Hz per MHz — channel frequencies arrive in Hz and every row in this
/// viewer renders them in MHz.
pub(crate) const HZ_PER_MHZ: f64 = 1_000_000.0;
/// Metres per kilometre — the ephemeris decoder reports altitude in
/// metres and speed in m/s; the log line shows km and km/s.
pub(crate) const METERS_PER_KM: f64 = 1_000.0;

/// Format one decoded [`OrbcommEvent`] as a log entry.
///
/// `Packet` events render as a single line (repaired packets get a
/// `~` prefix); `MessageComplete` events render as a header line
/// plus a multi-line [`format_hexdump`] block, with a `[partial]`
/// marker on the header when the message was flushed early.
#[must_use]
pub fn format_packet_row(event: &OrbcommEvent) -> String {
    match &event.kind {
        OrbcommEventKind::Packet { packet, repaired } => {
            let prefix = if *repaired { "~" } else { "" };
            format!("{prefix}{}", format_packet_line(packet, event.channel_hz))
        }
        OrbcommEventKind::MessageComplete { bytes, partial } => {
            format_message_complete(event.channel_hz, bytes, *partial)
        }
    }
}

/// One-line rendering of a parsed [`OrbcommPacket`].
fn format_packet_line(packet: &OrbcommPacket, channel_hz: f64) -> String {
    let mhz = channel_hz / HZ_PER_MHZ;
    match packet {
        OrbcommPacket::Ephemeris(eph) => format_ephemeris_line(eph),
        OrbcommPacket::Sync { code, sat_id } => {
            format!(
                "Sync · {} · code {code:06X} · {mhz:.4} MHz",
                sat_label(*sat_id)
            )
        }
        OrbcommPacket::Other { packet_type, bytes } => {
            format!(
                "{} · {mhz:.4} MHz · {}",
                packet_type_name(*packet_type),
                format_hex_inline(bytes)
            )
        }
    }
}

/// Ephemeris row: `Sat 0x2C · 51.2°N 7.4°E · 715 km · 7.45 km/s · 19:42:11Z`.
fn format_ephemeris_line(eph: &Ephemeris) -> String {
    format!(
        "{} · {} {} · {:.0} km · {:.2} km/s · {}",
        sat_label(eph.sat_id),
        format_lat(eph.lat_deg),
        format_lon(eph.lon_deg),
        eph.alt_m / METERS_PER_KM,
        eph.vel_ms / METERS_PER_KM,
        format_utc_hms(eph.sat_time_unix),
    )
}

/// Latitude with an `N`/`S` hemisphere letter derived from sign, one
/// decimal place, e.g. `51.2°N` / `33.9°S`. `pub(crate)` — also used
/// by the "Heard via Orbcomm" panel group's subtitle formatter
/// (`window/satellites/heard.rs`, issue #865 Task 12) so the two
/// coordinate renderings can't drift apart.
pub(crate) fn format_lat(lat_deg: f64) -> String {
    let hemi = if lat_deg < 0.0 { 'S' } else { 'N' };
    format!("{:.1}°{hemi}", lat_deg.abs())
}

/// Longitude with an `E`/`W` hemisphere letter derived from sign, one
/// decimal place, e.g. `7.4°E` / `18.4°W`. `pub(crate)` — see
/// [`format_lat`]'s doc for why.
pub(crate) fn format_lon(lon_deg: f64) -> String {
    let hemi = if lon_deg < 0.0 { 'W' } else { 'E' };
    format!("{:.1}°{hemi}", lon_deg.abs())
}

/// `HH:MM:SSZ` UTC clock time from a Unix timestamp. `sdr-ui` already
/// depends on `chrono` (used throughout the ACARS/satellite viewers),
/// so this leans on `DateTime::from_timestamp` rather than hand-
/// rolling a mod-86400 clock. Falls back to a sentinel string on the
/// (practically unreachable — the ephemeris decoder's own altitude
/// plausibility gate rejects garbage payloads long before this runs)
/// out-of-range case.
pub(crate) fn format_utc_hms(unix_secs: i64) -> String {
    chrono::DateTime::from_timestamp(unix_secs, 0).map_or_else(
        || "??:??:??Z".to_string(),
        |dt| dt.format("%H:%M:%SZ").to_string(),
    )
}

/// `PacketType` variant name for the `Other` packet row. A plain
/// match rather than `{:?}` so the mapping stays under our control
/// (Debug output would silently follow whatever the enum's derive
/// happens to produce).
fn packet_type_name(ty: PacketType) -> &'static str {
    match ty {
        PacketType::Sync => "Sync",
        PacketType::Message => "Message",
        PacketType::UplinkInfo => "UplinkInfo",
        PacketType::DownlinkInfo => "DownlinkInfo",
        PacketType::Network => "Network",
        PacketType::Fill => "Fill",
        PacketType::Ephemeris => "Ephemeris",
        PacketType::Orbital => "Orbital",
    }
}

/// Uppercase hex, no separators — used inline in the `Other` packet
/// row (as opposed to [`format_hexdump`]'s multi-line block for
/// `MessageComplete`).
fn format_hex_inline(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02X}");
    }
    out
}

/// Header line + [`format_hexdump`] block for a `MessageComplete`
/// event.
fn format_message_complete(channel_hz: f64, bytes: &[u8], partial: bool) -> String {
    let mhz = channel_hz / HZ_PER_MHZ;
    let marker = if partial { " [partial]" } else { "" };
    format!(
        "Message complete · {mhz:.4} MHz{marker}\n{}",
        format_hexdump(bytes)
    )
}

/// Render `bytes` as a classic hexdump: 16 bytes per row, an
/// 8-digit hex offset, space-separated hex byte pairs padded to a
/// fixed column width so short trailing rows still line up, and a
/// `|...|` printable-ASCII gutter (`0x20..=0x7E` verbatim, anything
/// else as `.`).
#[must_use]
pub fn format_hexdump(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut lines = Vec::with_capacity(bytes.len().div_ceil(HEXDUMP_BYTES_PER_ROW));
    for (row_idx, chunk) in bytes.chunks(HEXDUMP_BYTES_PER_ROW).enumerate() {
        let offset = row_idx * HEXDUMP_BYTES_PER_ROW;
        let mut hex = String::with_capacity(HEXDUMP_BYTES_PER_ROW * 3);
        for b in chunk {
            let _ = write!(hex, "{b:02X} ");
        }
        for _ in chunk.len()..HEXDUMP_BYTES_PER_ROW {
            hex.push_str("   ");
        }
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..=0x7E).contains(&b) {
                    char::from(b)
                } else {
                    '.'
                }
            })
            .collect();
        lines.push(format!("{offset:08X}  {hex}|{ascii}|"));
    }
    lines.join("\n")
}

/// Channel-strip label text: bare frequency when no stats exist yet
/// (fresh viewer open, first tick not landed), frequency + running
/// ok/err counters once they do.
pub(crate) fn channel_label_text(freq_hz: f64, stats: Option<&ChannelStats>) -> String {
    let mhz = freq_hz / HZ_PER_MHZ;
    match stats {
        Some(s) => format!(
            "{mhz:.4} MHz\n{} ok / {} err",
            s.packets_ok, s.checksum_fail
        ),
        None => format!("{mhz:.4} MHz"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
