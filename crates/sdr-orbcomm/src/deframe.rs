//! Streaming deframer: raw channel bits in, aligned packets out.
//!
//! The demodulator hands over a continuous, unframed bit stream at 4800 bps
//! per channel with no byte or packet alignment. This module recovers both,
//! using the packet header table and the Fletcher-16 checksum as the only
//! sync markers the protocol gives us.
//!
//! Bits arrive **LSB-first per byte**: the first bit received is bit 0 of the
//! byte being assembled.
//!
//! # State machine
//!
//! Every bit is processed synchronously as it is pushed — nothing is
//! batch-scanned out of band. (Lesson from the ACARS port: per-bit re-sync
//! only works when the machine advances in lockstep with the bit clock.)
//!
//! * **`Searching`** — bits append to the pending buffer. On each bit, the
//!   only start offsets that can *newly* complete a packet are the two that
//!   end exactly on the bit just pushed: the 24-byte candidate
//!   [`MAX_PACKET_BITS`] back, and the 12-byte candidate [`MIN_PACKET_BITS`]
//!   back. Each is assembled LSB-first, its header must map to a
//!   [`PacketType`] whose [`PacketType::packet_len`] matches the span tried,
//!   and its Fletcher-16 must fold to zero. The older (24-byte) candidate is
//!   tried first so the earliest packet in the stream wins. On a hit the
//!   packet is emitted, the consumed bits are drained, and the machine moves
//!   to `Locked` at that phase.
//!
//!   This is equivalent to re-scanning the whole `[len - 192, len - 96]`
//!   offset window on every bit, but without the redundant work: the pending
//!   buffer is append-only, so the bits at a given start offset never change,
//!   and an offset that fails its length-appropriate test can never later
//!   pass it. Each offset is therefore tested exactly once, at the bit where
//!   its packet would complete.
//!
//! * **`Locked { consecutive_bad }`** — bits accumulate until a full stride
//!   is pending. The stride comes from peeking the header byte at the locked
//!   phase ([`PacketType::packet_len`], defaulting to [`MIN_PACKET_BYTES`]
//!   for an unrecognized header), so an ephemeris packet consumes 24 bytes
//!   and everything else 12. The stride is always consumed, valid or not, to
//!   keep the phase. A packet with a good header and a zero checksum is
//!   emitted as-is; otherwise single-bit repair flips each of the stride's
//!   bits once and re-checks (emitted with `repaired: true` on success).
//!   [`MAX_CONSECUTIVE_BAD`] failures in a row drop back to `Searching`,
//!   keeping whatever bits are still pending.
//!
//! The pending buffer is capped at [`MAX_PENDING_BITS`]; the oldest bit is
//! dropped to make room. Only `Searching` can ever reach the cap — `Locked`
//! drains every stride, so it never holds more than [`MAX_PACKET_BITS`].

use std::collections::VecDeque;

use crate::packet::{PacketType, fletcher16};

/// Bits in a byte.
const BITS_PER_BYTE: usize = 8;
/// Shortest packet on the downlink — every type except ephemeris.
const MIN_PACKET_BYTES: usize = 12;
/// Longest packet on the downlink — ephemeris.
const MAX_PACKET_BYTES: usize = 24;
/// [`MIN_PACKET_BYTES`] as a bit count.
const MIN_PACKET_BITS: usize = MIN_PACKET_BYTES * BITS_PER_BYTE;
/// [`MAX_PACKET_BYTES`] as a bit count.
const MAX_PACKET_BITS: usize = MAX_PACKET_BYTES * BITS_PER_BYTE;
/// Consecutive locked-state failures tolerated before re-acquiring.
const MAX_CONSECUTIVE_BAD: u8 = 4;
/// Pending-bit ceiling: four maximum-length packets.
const MAX_PENDING_BITS: usize = 4 * MAX_PACKET_BITS;

/// A deframed packet: header byte, payload and the two Fletcher-16 check
/// bytes, verified to fold to zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeframedPacket {
    /// Full packet bytes, header and check bytes included.
    pub bytes: Vec<u8>,
    /// `true` when the packet only passed after single-bit repair.
    pub repaired: bool,
}

/// Deframer state. See the module docs for the transition rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No alignment yet; probing every bit offset for a valid packet.
    Searching,
    /// Aligned; consuming whole packets at the acquired phase.
    Locked {
        /// Strides that failed both checksum and repair since the last
        /// good packet.
        consecutive_bad: u8,
    },
}

/// Streaming bit-to-packet deframer for one Orbcomm channel.
#[derive(Debug)]
pub struct Deframer {
    /// Bits received but not yet consumed by an emitted packet, oldest first.
    pending: VecDeque<bool>,
    /// Current acquisition state.
    state: State,
    /// Locked strides that failed both the checksum and single-bit repair.
    bad_strides: u64,
}

impl Default for Deframer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deframer {
    /// A deframer with no lock and an empty bit buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(MAX_PENDING_BITS),
            state: State::Searching,
            bad_strides: 0,
        }
    }

    /// Number of locked strides rejected so far: whole packet-length spans
    /// consumed at the acquired phase whose Fletcher-16 failed and which
    /// single-bit repair could not rescue.
    ///
    /// This is the only place a checksum failure is observable from outside
    /// the deframer — [`Self::push_bit`] emits nothing for a rejected stride,
    /// and `Searching` deliberately does not count its probes (it tests two
    /// speculative bit offsets per bit, so nearly all of them fail by
    /// construction and counting them would measure the search, not the
    /// link). Monotonic; saturating.
    #[must_use]
    pub fn bad_strides(&self) -> u64 {
        self.bad_strides
    }

    /// Push one demodulated bit (LSB-first within its byte) and return the
    /// packet that completes on it, if any.
    pub fn push_bit(&mut self, bit: bool) -> Option<DeframedPacket> {
        if self.pending.len() >= MAX_PENDING_BITS {
            self.pending.pop_front();
        }
        self.pending.push_back(bit);
        match self.state {
            State::Searching => self.search_step(),
            State::Locked { consecutive_bad } => self.locked_step(consecutive_bad),
        }
    }

    /// `Searching`: probe the two start offsets whose packet would end on
    /// the bit just pushed, oldest first.
    fn search_step(&mut self) -> Option<DeframedPacket> {
        let len = self.pending.len();
        for span_bits in [MAX_PACKET_BITS, MIN_PACKET_BITS] {
            let Some(start) = len.checked_sub(span_bits) else {
                continue;
            };
            if let Some(bytes) = self.packet_at(start, span_bits / BITS_PER_BYTE) {
                self.pending.drain(..start + span_bits);
                self.state = State::Locked { consecutive_bad: 0 };
                return Some(DeframedPacket {
                    bytes,
                    repaired: false,
                });
            }
        }
        None
    }

    /// `Locked`: consume one stride once it is fully pending, verifying and
    /// optionally repairing it. O(1) per bit until the stride completes.
    fn locked_step(&mut self, consecutive_bad: u8) -> Option<DeframedPacket> {
        if self.pending.len() < BITS_PER_BYTE {
            return None;
        }
        let stride_bytes = PacketType::from_header(self.byte_at(0))
            .map_or(MIN_PACKET_BYTES, PacketType::packet_len);
        let stride_bits = stride_bytes * BITS_PER_BYTE;
        if self.pending.len() < stride_bits {
            return None;
        }

        let mut bytes: Vec<u8> = (0..stride_bytes)
            .map(|i| self.byte_at(i * BITS_PER_BYTE))
            .collect();
        // Consumed either way: holding a bad stride would slip the phase.
        self.pending.drain(..stride_bits);

        if is_valid_packet(&bytes) {
            self.state = State::Locked { consecutive_bad: 0 };
            return Some(DeframedPacket {
                bytes,
                repaired: false,
            });
        }
        if repair_single_bit(&mut bytes) {
            self.state = State::Locked { consecutive_bad: 0 };
            return Some(DeframedPacket {
                bytes,
                repaired: true,
            });
        }

        self.bad_strides = self.bad_strides.saturating_add(1);
        let bad = consecutive_bad.saturating_add(1);
        self.state = if bad >= MAX_CONSECUTIVE_BAD {
            State::Searching
        } else {
            State::Locked {
                consecutive_bad: bad,
            }
        };
        None
    }

    /// Assemble the byte whose first (LSB) bit sits at `bit_index`. Bits
    /// past the end of the buffer read as zero; callers check the length
    /// first, so that only guards against a panic.
    fn byte_at(&self, bit_index: usize) -> u8 {
        let mut byte = 0u8;
        for k in 0..BITS_PER_BYTE {
            if self.pending.get(bit_index + k).copied().unwrap_or(false) {
                byte |= 1u8 << k;
            }
        }
        byte
    }

    /// Assemble `byte_len` bytes starting at bit `start` and return them
    /// only if they form a well-formed packet of exactly that length.
    fn packet_at(&self, start: usize, byte_len: usize) -> Option<Vec<u8>> {
        if PacketType::from_header(self.byte_at(start))?.packet_len() != byte_len {
            return None;
        }
        let bytes: Vec<u8> = (0..byte_len)
            .map(|i| self.byte_at(start + i * BITS_PER_BYTE))
            .collect();
        is_valid_packet(&bytes).then_some(bytes)
    }
}

/// A packet is acceptable when its header is in the spec table, its length
/// matches that header's type, and its Fletcher-16 folds to zero.
fn is_valid_packet(bytes: &[u8]) -> bool {
    PacketType::from_header(bytes.first().copied().unwrap_or(0))
        .is_some_and(|ty| ty.packet_len() == bytes.len())
        && fletcher16(bytes) == 0
}

/// Flip each bit of `bytes` in turn, keeping the first flip that makes the
/// packet valid. Returns `false` (and leaves `bytes` untouched) if none does.
fn repair_single_bit(bytes: &mut [u8]) -> bool {
    for bit in 0..bytes.len() * BITS_PER_BYTE {
        let index = bit / BITS_PER_BYTE;
        let mask = 1u8 << (bit % BITS_PER_BYTE);
        // `index` is in range by construction; `get_mut` keeps the loop
        // panic-free without an indexing assertion.
        let Some(byte) = bytes.get_mut(index) else {
            continue;
        };
        *byte ^= mask;
        if is_valid_packet(bytes) {
            return true;
        }
        // Restore before trying the next candidate bit. The re-borrow is
        // needed because `is_valid_packet` borrows the whole slice.
        if let Some(byte) = bytes.get_mut(index) {
            *byte ^= mask;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{PacketType, encode_ephemeris_for_test, fletcher16_check_bytes};

    /// A valid 12-byte Sync packet: header + payload + Fletcher check bytes.
    fn valid_sync_packet() -> Vec<u8> {
        let mut p = vec![
            PacketType::Sync.header_byte(),
            0xAA,
            0xBB,
            0x2C,
            0x01,
            0x02,
            0x03,
            0x04,
            0x05,
            0x06,
        ];
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        p
    }

    /// Bits in wire order: bit 0 (LSB) of each byte first — the same
    /// convention [`Deframer`] assembles bytes with.
    fn bits_lsb_first(bytes: &[u8]) -> impl Iterator<Item = bool> + '_ {
        bytes
            .iter()
            .flat_map(|b| (0..8).map(move |k| (b >> k) & 1 == 1))
    }

    #[test]
    fn locks_and_emits_at_every_bit_offset() {
        let pkt = valid_sync_packet();
        for offset in 0..96 {
            let mut d = Deframer::new();
            let mut got = Vec::new();
            for _ in 0..offset {
                d.push_bit(false);
            }
            for _ in 0..3 {
                for bit in bits_lsb_first(&pkt) {
                    if let Some(p) = d.push_bit(bit) {
                        got.push(p);
                    }
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
        for bit in bits_lsb_first(&pkt) {
            d.push_bit(bit);
        }
        let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
        corrupted[40] = !corrupted[40];
        let mut got = Vec::new();
        for bit in corrupted {
            if let Some(p) = d.push_bit(bit) {
                got.push(p);
            }
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].repaired);
        assert_eq!(got[0].bytes, pkt);
    }

    #[test]
    fn resyncs_after_consecutive_garbage() {
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        for bit in bits_lsb_first(&pkt) {
            d.push_bit(bit);
        }
        for i in 0..(5 * 96) {
            d.push_bit(i % 3 == 0);
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            for bit in bits_lsb_first(&pkt) {
                if let Some(p) = d.push_bit(bit) {
                    got.push(p);
                }
            }
        }
        assert!(!got.is_empty(), "must reacquire after garbage");
    }

    #[test]
    fn ephemeris_stride_is_24_bytes() {
        // A valid 24-byte ephemeris packet followed immediately by a sync
        // packet: both must emit. Acquisition has to probe the 24-byte
        // alignment, and the locked stride has to honour `packet_len` of
        // the *peeked* header rather than a fixed 12 bytes.
        let eph = encode_ephemeris_for_test(
            0x2C,
            2434,
            400_000,
            [3_000_000.0, 4_000_000.0, 4_900_000.0],
            [-5_000.0, 3_000.0, 2_000.0],
        );
        assert_eq!(eph.len(), 24);
        let sync = valid_sync_packet();

        let mut d = Deframer::new();
        let mut got = Vec::new();
        for bit in bits_lsb_first(&eph).chain(bits_lsb_first(&sync)) {
            if let Some(p) = d.push_bit(bit) {
                got.push(p);
            }
        }
        assert_eq!(got.len(), 2, "got {got:?}");
        assert_eq!(got[0].bytes, eph);
        assert_eq!(got[1].bytes, sync);
        assert!(got.iter().all(|p| !p.repaired));
    }

    #[test]
    fn stride_constants_match_the_packet_table() {
        assert_eq!(MIN_PACKET_BYTES, PacketType::Sync.packet_len());
        assert_eq!(MAX_PACKET_BYTES, PacketType::Ephemeris.packet_len());
        assert_eq!(MIN_PACKET_BITS, MIN_PACKET_BYTES * BITS_PER_BYTE);
        assert_eq!(MAX_PACKET_BITS, MAX_PACKET_BYTES * BITS_PER_BYTE);
    }

    #[test]
    fn two_bit_errors_are_not_fabricated_into_packets() {
        // Single-bit repair must not invent a packet out of a doubly
        // corrupted stride.
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        for bit in bits_lsb_first(&pkt) {
            d.push_bit(bit);
        }
        let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
        corrupted[40] = !corrupted[40];
        corrupted[75] = !corrupted[75];
        let got: Vec<_> = corrupted
            .into_iter()
            .filter_map(|b| d.push_bit(b))
            .collect();
        assert!(got.is_empty(), "got {got:?}");
        assert_eq!(d.state, State::Locked { consecutive_bad: 1 });
    }

    #[test]
    fn a_good_packet_resets_the_bad_stride_counter() {
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        for bit in bits_lsb_first(&pkt) {
            d.push_bit(bit);
        }
        // Three bad strides — one short of re-acquiring.
        for _ in 0..3 {
            for _ in 0..MIN_PACKET_BITS {
                d.push_bit(true);
            }
        }
        assert_eq!(d.state, State::Locked { consecutive_bad: 3 });
        for bit in bits_lsb_first(&pkt) {
            d.push_bit(bit);
        }
        assert_eq!(d.state, State::Locked { consecutive_bad: 0 });
    }

    #[test]
    fn bad_strides_counts_only_rejected_locked_strides() {
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        // Acquisition probes many offsets that fail — none of them count.
        for _ in 0..(3 * MIN_PACKET_BITS) {
            d.push_bit(false);
        }
        for bit in bits_lsb_first(&pkt) {
            d.push_bit(bit);
        }
        assert_eq!(d.bad_strides(), 0);

        // A locked stride that neither checksums nor repairs counts once.
        let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
        corrupted[40] = !corrupted[40];
        corrupted[75] = !corrupted[75];
        for bit in corrupted {
            d.push_bit(bit);
        }
        assert_eq!(d.bad_strides(), 1);

        // A clean stride after it does not.
        for bit in bits_lsb_first(&pkt) {
            d.push_bit(bit);
        }
        assert_eq!(d.bad_strides(), 1);
    }

    #[test]
    fn pending_bits_stay_capped_while_searching() {
        let mut d = Deframer::new();
        for i in 0..(4 * MAX_PENDING_BITS) {
            d.push_bit(i % 7 == 0);
            assert!(d.pending.len() <= MAX_PENDING_BITS, "at bit {i}");
        }
        assert_eq!(d.state, State::Searching);
    }
}
