//! Session packet-type tally for the Orbcomm panel's "what am I
//! getting" breakdown. Pure UI-side classification from the decoded
//! `OrbcommEvent` stream — the decoder does not emit per-type counts.
//! Checksum-fail / repaired totals live in `ChannelStats` and are passed
//! in at format time rather than tracked here (rejects occur before a
//! packet is parsed to a type).

use sdr_orbcomm::packet::{OrbcommPacket, PacketType};
use sdr_orbcomm::{OrbcommEvent, OrbcommEventKind};

/// Fixed display order of the eight packet types (index = tally slot).
const TYPE_ORDER: [(PacketType, &str); 8] = [
    (PacketType::Sync, "Sync"),
    (PacketType::Message, "Message"),
    (PacketType::UplinkInfo, "Uplink"),
    (PacketType::DownlinkInfo, "Downlink"),
    (PacketType::Network, "Network"),
    (PacketType::Fill, "Fill"),
    (PacketType::Ephemeris, "Ephemeris"),
    (PacketType::Orbital, "Orbital"),
];

fn type_index(ty: PacketType) -> usize {
    // Small linear match; TYPE_ORDER is the single source of order.
    TYPE_ORDER.iter().position(|&(t, _)| t == ty).unwrap_or(0)
}

#[derive(Default)]
pub struct OrbcommTally {
    counts: [u64; 8],
}

impl OrbcommTally {
    /// Tally one decoded event. Only `Packet` events carry a type;
    /// `MessageComplete` (a reassembly of already-counted Message
    /// packets) is ignored.
    pub fn record(&mut self, event: &OrbcommEvent) {
        let OrbcommEventKind::Packet { packet, .. } = &event.kind else {
            return;
        };
        let ty = match packet {
            OrbcommPacket::Sync { .. } => PacketType::Sync,
            OrbcommPacket::Ephemeris(_) => PacketType::Ephemeris,
            OrbcommPacket::Other { packet_type, .. } => *packet_type,
        };
        self.counts[type_index(ty)] = self.counts[type_index(ty)].saturating_add(1);
    }

    /// Zero every count (called on decoder disable).
    pub fn reset(&mut self) {
        self.counts = [0; 8];
    }

    /// Multi-line breakdown for the panel label. `checksum_fail` and
    /// `repaired` are summed from the latest `ChannelStats` slice by the
    /// caller.
    #[must_use]
    pub fn format_breakdown(&self, checksum_fail: u64, repaired: u64) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (i, (_, name)) in TYPE_ORDER.iter().enumerate() {
            let _ = writeln!(out, "{name:<9} {}", self.counts[i]);
        }
        let _ = write!(out, "checksum-fail {checksum_fail}  ·  repaired {repaired}");
        out
    }
}

#[cfg(test)]
mod tests;
