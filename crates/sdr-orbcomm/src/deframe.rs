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
//!   tried first so the earliest packet in the stream wins. A hit does **not**
//!   emit: the consumed bits are drained and the candidate is held for
//!   confirmation.
//!
//!   This is equivalent to re-scanning the whole `[len - 192, len - 96]`
//!   offset window on every bit, but without the redundant work: the pending
//!   buffer is append-only, so the bits at a given start offset never change,
//!   and an offset that fails its length-appropriate test can never later
//!   pass it. Each offset is therefore tested exactly once, at the bit where
//!   its packet would complete.
//!
//! * **`Confirming { candidate }`** — the held candidate is only believed
//!   once the *next* stride at the same phase also has a table header and a
//!   zero checksum. On success both packets are emitted in stream order
//!   (candidate first) and the machine enters `Locked`. On failure the
//!   candidate is dropped silently — no emission, and no
//!   [`Self::bad_strides`] tick, because a phase that was never confirmed was
//!   never a lock, and counting it would report the search as link quality.
//!   The confirm window's bits are *kept* rather than drained: they were
//!   never probed at any other offset, so they go back to `Searching`.
//!   Single-bit repair is deliberately unavailable here — see below.
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
//! # Why one checksum is not enough
//!
//! Fletcher-16 is a 16-bit fold, and 8 of the 256 possible header bytes are in
//! the spec table, so a *random* 12-byte span passes both tests with
//! probability `(8 / 256) × 2⁻¹⁶ ≈ 4.8e-7`. `Searching` runs two probes per
//! bit at 4800 bps — 9600 probes per second per channel, 86 400 across the
//! nine-channel bank — so emitting on a single fold yields
//! `86_400 × 4.8e-7 ≈ 0.04` false packets per second: **two to three ghost
//! packets a minute with no satellite overhead**, spread evenly over all eight
//! header types. That is exactly what the first live smoke test produced, and
//! it is worse than log noise: a ghost `Sync` invents a spacecraft in the
//! Heard-via-Orbcomm panel.
//!
//! Requiring a second, independent stride to pass squares the per-probe odds
//! to `≈ 2.3e-13`, or one false lock per bank-decade rather than per minute,
//! at a cost of one packet of acquisition latency. Real traffic is
//! back-to-back, so the confirming stride is already on the wire.
//!
//! For the same reason single-bit repair is confined to `Locked`: it hands
//! the checksum up to 192 extra chances to fold, which is a fine trade once a
//! phase is trusted and a terrible one while guessing at it.
//!
//! The pending buffer is capped at [`MAX_PENDING_BITS`]; the oldest bit is
//! dropped to make room. Only `Searching` can ever reach the cap — `Confirming`
//! acts within one stride and `Locked` drains every stride, so neither holds
//! more than [`MAX_PACKET_BITS`].

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
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// No alignment yet; probing every bit offset for a valid packet.
    Searching,
    /// A checksum-valid candidate is held, awaiting a second good stride at
    /// the same phase before either is believed.
    Confirming {
        /// The unconfirmed packet, emitted first if confirmation succeeds.
        candidate: Vec<u8>,
    },
    /// Aligned and confirmed; consuming whole packets at the acquired phase.
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
    /// and neither `Searching` nor `Confirming` counts its failures
    /// (`Searching` tests two speculative bit offsets per bit, so nearly all
    /// of them fail by construction, and a failed confirmation means the
    /// phase was never a lock). Counting either would measure the search
    /// rather than the link. Monotonic; saturating.
    #[must_use]
    pub fn bad_strides(&self) -> u64 {
        self.bad_strides
    }

    /// Push one demodulated bit (LSB-first within its byte), appending any
    /// packets it completes to `out`.
    ///
    /// Usually appends nothing. It appends *two* packets on the bit that
    /// confirms an acquisition — the candidate found in `Searching` followed
    /// by the stride that vouched for it — which is why this takes a sink
    /// rather than returning an `Option`. `out` is never cleared, so a caller
    /// may accumulate across a whole block of bits.
    pub fn push_bit(&mut self, bit: bool, out: &mut Vec<DeframedPacket>) {
        if self.pending.len() >= MAX_PENDING_BITS {
            self.pending.pop_front();
        }
        self.pending.push_back(bit);
        match self.state {
            State::Searching => self.search_step(),
            State::Confirming { .. } => self.confirm_step(out),
            State::Locked { consecutive_bad } => self.locked_step(consecutive_bad, out),
        }
    }

    /// `Searching`: probe the two start offsets whose packet would end on
    /// the bit just pushed, oldest first. A hit is held for confirmation,
    /// not emitted.
    fn search_step(&mut self) {
        let len = self.pending.len();
        for span_bits in [MAX_PACKET_BITS, MIN_PACKET_BITS] {
            let Some(start) = len.checked_sub(span_bits) else {
                continue;
            };
            if let Some(bytes) = self.packet_at(start, span_bits / BITS_PER_BYTE) {
                self.pending.drain(..start + span_bits);
                self.state = State::Confirming { candidate: bytes };
                return;
            }
        }
    }

    /// `Confirming`: the next whole stride at the candidate's phase has to
    /// stand on its own — table header, zero checksum, **no repair**. Both
    /// packets emit together, or the candidate is dropped without a trace.
    fn confirm_step(&mut self, out: &mut Vec<DeframedPacket>) {
        let Some(stride_bits) = self.pending_stride_bits() else {
            return;
        };
        let bytes: Vec<u8> = (0..stride_bits / BITS_PER_BYTE)
            .map(|i| self.byte_at(i * BITS_PER_BYTE))
            .collect();

        if !is_valid_packet(&bytes) {
            // Keep the bits rather than draining them: this window was only
            // ever tested at the candidate's phase, so hand it back to the
            // search instead of swallowing a stride of possibly real signal.
            self.state = State::Searching;
            return;
        }

        self.pending.drain(..stride_bits);
        let confirmed = std::mem::replace(&mut self.state, State::Locked { consecutive_bad: 0 });
        if let State::Confirming { candidate } = confirmed {
            out.push(DeframedPacket {
                bytes: candidate,
                repaired: false,
            });
        }
        out.push(DeframedPacket {
            bytes,
            repaired: false,
        });
    }

    /// `Locked`: consume one stride once it is fully pending, verifying and
    /// optionally repairing it. O(1) per bit until the stride completes.
    fn locked_step(&mut self, consecutive_bad: u8, out: &mut Vec<DeframedPacket>) {
        let Some(stride_bits) = self.pending_stride_bits() else {
            return;
        };
        let mut bytes: Vec<u8> = (0..stride_bits / BITS_PER_BYTE)
            .map(|i| self.byte_at(i * BITS_PER_BYTE))
            .collect();
        // Consumed either way: holding a bad stride would slip the phase.
        self.pending.drain(..stride_bits);

        if is_valid_packet(&bytes) {
            self.state = State::Locked { consecutive_bad: 0 };
            out.push(DeframedPacket {
                bytes,
                repaired: false,
            });
            return;
        }
        if repair_single_bit(&mut bytes) {
            self.state = State::Locked { consecutive_bad: 0 };
            out.push(DeframedPacket {
                bytes,
                repaired: true,
            });
            return;
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
    }

    /// Stride implied by the header byte at the current phase, but only once
    /// the whole stride is pending. `None` while more bits are still needed,
    /// which keeps both `Confirming` and `Locked` O(1) per bit until a stride
    /// completes. An unrecognized header falls back to [`MIN_PACKET_BYTES`]
    /// so the phase still advances by a plausible packet.
    fn pending_stride_bits(&self) -> Option<usize> {
        if self.pending.len() < BITS_PER_BYTE {
            return None;
        }
        let stride_bytes = PacketType::from_header(self.byte_at(0))
            .map_or(MIN_PACKET_BYTES, PacketType::packet_len);
        let stride_bits = stride_bytes * BITS_PER_BYTE;
        (self.pending.len() >= stride_bits).then_some(stride_bits)
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

    /// `count` back-to-back copies of `pkt` as a bit stream.
    fn repeat_packet(pkt: &[u8], count: usize) -> Vec<bool> {
        let one: Vec<bool> = bits_lsb_first(pkt).collect();
        one.iter()
            .copied()
            .cycle()
            .take(one.len() * count)
            .collect()
    }

    /// Push every bit and collect everything emitted, in order.
    fn feed(d: &mut Deframer, bits: impl IntoIterator<Item = bool>) -> Vec<DeframedPacket> {
        let mut got = Vec::new();
        for bit in bits {
            d.push_bit(bit, &mut got);
        }
        got
    }

    /// Drive the deframer to a confirmed lock on `pkt` and return the two
    /// packets that confirmation emits. Acquisition costs two packets now,
    /// so every locked-state test starts here.
    fn lock_on(d: &mut Deframer, pkt: &[u8]) -> Vec<DeframedPacket> {
        let got = feed(d, bits_lsb_first(pkt).chain(bits_lsb_first(pkt)));
        assert_eq!(got.len(), 2, "expected a confirmed lock, got {got:?}");
        assert_eq!(d.state, State::Locked { consecutive_bad: 0 });
        got
    }

    #[test]
    fn locks_and_emits_at_every_bit_offset() {
        let pkt = valid_sync_packet();
        for offset in 0..96 {
            let mut d = Deframer::new();
            let mut got = Vec::new();
            for _ in 0..offset {
                d.push_bit(false, &mut got);
            }
            // Three packets: the first two acquire-and-confirm (emitting
            // both together on the second one's last bit), the third arrives
            // already locked. Confirmation costs latency, not packets, so all
            // three still come out — comfortably above the >= 2 floor.
            got.extend(feed(&mut d, repeat_packet(&pkt, 3)));
            assert_eq!(got.len(), 3, "offset {offset}: got {}", got.len());
            assert!(got.iter().all(|p| p.bytes == pkt && !p.repaired));
        }
    }

    #[test]
    fn two_back_to_back_packets_emit_both_in_order() {
        // The whole point of acquire-confirm: latency is one packet, but
        // nothing is lost and stream order is preserved.
        let sync = valid_sync_packet();
        let mut fill = vec![
            PacketType::Fill.header_byte(),
            0x11,
            0x22,
            0x33,
            0x44,
            0x55,
            0x66,
            0x77,
            0x88,
            0x99,
        ];
        let (c0, c1) = fletcher16_check_bytes(&fill);
        fill.push(c0);
        fill.push(c1);

        let mut d = Deframer::new();
        let got = feed(&mut d, bits_lsb_first(&sync).chain(bits_lsb_first(&fill)));
        assert_eq!(got.len(), 2, "got {got:?}");
        assert_eq!(got[0].bytes, sync);
        assert_eq!(got[1].bytes, fill);
        assert!(got.iter().all(|p| !p.repaired));
    }

    #[test]
    fn unconfirmed_candidate_is_discarded_silently() {
        // A checksum-valid packet followed by garbage must emit nothing at
        // all — not the candidate, not a bad-stride tick — and must still
        // reacquire once real traffic resumes.
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();

        let got = feed(&mut d, bits_lsb_first(&pkt));
        assert!(got.is_empty(), "candidate must not emit unconfirmed");
        assert!(matches!(d.state, State::Confirming { .. }));

        // One stride of garbage: confirmation fails, candidate evaporates.
        let got = feed(&mut d, (0..MIN_PACKET_BITS).map(|i| i % 3 == 0));
        assert!(got.is_empty(), "got {got:?}");
        assert_eq!(d.state, State::Searching);
        assert_eq!(d.bad_strides(), 0, "a failed confirm is not a bad stride");

        // Real traffic afterwards still acquires.
        let got = feed(&mut d, bits_lsb_first(&pkt).chain(bits_lsb_first(&pkt)));
        assert_eq!(got.len(), 2, "must reacquire after a failed confirm");
        assert!(got.iter().all(|p| p.bytes == pkt));
    }

    #[test]
    fn single_bit_error_is_repaired_when_locked() {
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        lock_on(&mut d, &pkt);
        let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
        corrupted[40] = !corrupted[40];
        let got = feed(&mut d, corrupted);
        assert_eq!(got.len(), 1);
        assert!(got[0].repaired);
        assert_eq!(got[0].bytes, pkt);
    }

    #[test]
    fn repair_is_unavailable_before_the_lock_is_confirmed() {
        // The same single-bit error that `Locked` repairs must sink the
        // confirmation instead — repair would hand a guessed phase 192 extra
        // chances to fold the checksum.
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        let got = feed(&mut d, bits_lsb_first(&pkt));
        assert!(got.is_empty());

        let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
        corrupted[40] = !corrupted[40];
        let got = feed(&mut d, corrupted);
        assert!(got.is_empty(), "got {got:?}");
        assert_eq!(d.state, State::Searching);
    }

    #[test]
    fn resyncs_after_consecutive_garbage() {
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        lock_on(&mut d, &pkt);
        let got = feed(&mut d, (0..(5 * 96)).map(|i| i % 3 == 0));
        assert!(got.is_empty(), "garbage must not emit: {got:?}");
        let got = feed(&mut d, repeat_packet(&pkt, 3));
        assert!(!got.is_empty(), "must reacquire after garbage");
        assert!(got.iter().all(|p| p.bytes == pkt));
    }

    #[test]
    fn ephemeris_stride_is_24_bytes() {
        // A valid 24-byte ephemeris packet, a second one to confirm it, then
        // a sync packet. Acquisition has to probe the 24-byte alignment, and
        // both the confirming and locked strides have to honour `packet_len`
        // of the *peeked* header rather than a fixed 12 bytes.
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
        let got = feed(
            &mut d,
            bits_lsb_first(&eph)
                .chain(bits_lsb_first(&eph))
                .chain(bits_lsb_first(&sync)),
        );
        assert_eq!(got.len(), 3, "got {got:?}");
        assert_eq!(got[0].bytes, eph);
        assert_eq!(got[1].bytes, eph);
        assert_eq!(got[2].bytes, sync);
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
        lock_on(&mut d, &pkt);
        let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
        corrupted[40] = !corrupted[40];
        corrupted[75] = !corrupted[75];
        let got = feed(&mut d, corrupted);
        assert!(got.is_empty(), "got {got:?}");
        assert_eq!(d.state, State::Locked { consecutive_bad: 1 });
    }

    #[test]
    fn a_good_packet_resets_the_bad_stride_counter() {
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        lock_on(&mut d, &pkt);
        // Three bad strides — one short of re-acquiring.
        for _ in 0..3 {
            feed(&mut d, std::iter::repeat_n(true, MIN_PACKET_BITS));
        }
        assert_eq!(d.state, State::Locked { consecutive_bad: 3 });
        feed(&mut d, bits_lsb_first(&pkt));
        assert_eq!(d.state, State::Locked { consecutive_bad: 0 });
    }

    #[test]
    fn bad_strides_counts_only_rejected_locked_strides() {
        let pkt = valid_sync_packet();
        let mut d = Deframer::new();
        // Acquisition probes many offsets that fail — none of them count.
        feed(&mut d, std::iter::repeat_n(false, 3 * MIN_PACKET_BITS));
        lock_on(&mut d, &pkt);
        assert_eq!(d.bad_strides(), 0);

        // A locked stride that neither checksums nor repairs counts once.
        let mut corrupted: Vec<bool> = bits_lsb_first(&pkt).collect();
        corrupted[40] = !corrupted[40];
        corrupted[75] = !corrupted[75];
        feed(&mut d, corrupted);
        assert_eq!(d.bad_strides(), 1);

        // A clean stride after it does not.
        feed(&mut d, bits_lsb_first(&pkt));
        assert_eq!(d.bad_strides(), 1);
    }

    /// Deterministic xorshift64 — a seeded, dependency-free bit source so
    /// the noise guard below is reproducible rather than flaky.
    struct XorShift64(u64);

    impl XorShift64 {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    /// Bits of pure noise fed to the false-lock guard. At 4800 bps this is
    /// about 14 minutes of one dead channel.
    const NOISE_BITS: usize = 4_000_000;

    /// Push `count` seeded-random bits and collect everything emitted.
    fn feed_noise(d: &mut Deframer, seed: u64, count: usize) -> Vec<DeframedPacket> {
        let mut rng = XorShift64(seed);
        let mut got = Vec::new();
        let mut pushed = 0usize;
        while pushed < count {
            let word = rng.next_u64();
            for k in 0..64 {
                if pushed >= count {
                    break;
                }
                d.push_bit((word >> k) & 1 == 1, &mut got);
                pushed += 1;
            }
        }
        got
    }

    #[test]
    fn pure_noise_never_emits_a_packet() {
        // The regression this module's acquire-confirm design exists for. A
        // single Fletcher-16 fold is far too weak to be a lock criterion:
        // P(table header) x P(fold) = (8/256) x 2^-16 = 4.8e-7 per probe, and
        // Searching runs two probes per bit. Before confirmation was added
        // this exact stream produced three ghost packets (headers 1E, 1E, 1D).
        let mut d = Deframer::new();
        let ghosts = feed_noise(&mut d, 0x2545_F491_4F6C_DD1D, NOISE_BITS);
        assert!(
            ghosts.is_empty(),
            "{} ghost packet(s) from {NOISE_BITS} bits of noise: {:02X?}",
            ghosts.len(),
            ghosts.iter().map(|p| p.bytes.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pure_noise_never_emits_a_packet_across_seeds() {
        // Nine channels' worth of independent noise, so the guard does not
        // rest on one lucky stream.
        for seed in 1..=9u64 {
            let mut d = Deframer::new();
            let ghosts = feed_noise(&mut d, seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 500_000);
            assert!(ghosts.is_empty(), "seed {seed}: {ghosts:?}");
        }
    }

    #[test]
    fn pending_bits_stay_capped_while_searching() {
        let mut d = Deframer::new();
        let mut out = Vec::new();
        for i in 0..(4 * MAX_PENDING_BITS) {
            d.push_bit(i % 7 == 0, &mut out);
            assert!(d.pending.len() <= MAX_PENDING_BITS, "at bit {i}");
        }
        assert_eq!(d.state, State::Searching);
    }
}
