//! Multi-packet message reassembly.
//!
//! A subscriber message that doesn't fit in one 12-byte Message packet
//! (header `0x1A`) is split across several packets. Byte 1 of each packet
//! carries the fragment's sequence number and the sequence's total fragment
//! count as two nibbles; [`Reassembler`] collects fragments for a single
//! in-flight sequence and emits the concatenated payload once every
//! fragment has arrived, or as a `partial` flush if the sequence stalls or
//! is superseded.
//!
//! Orbcomm interleaves multi-packet messages on a channel only rarely, so
//! this tracks exactly one in-flight sequence — not one per originating
//! subscriber or channel. A fragment whose `total` doesn't match the
//! in-flight sequence is treated as the start of a new sequence.

use std::collections::BTreeMap;

use crate::packet::PacketType;

/// Number of payload bytes in a Message packet: 12 total, minus the header
/// byte (0), the length/sequence byte (1), and the two trailing Fletcher-16
/// check bytes (10, 11) — `bytes[2..10]`.
pub const MESSAGE_PAYLOAD_BYTES: usize = 8;

/// Default staleness bound: an in-flight sequence missing a fragment for
/// this many further pushes (of any packet) is flushed as partial.
pub const DEFAULT_MAX_AGE_PACKETS: u32 = 50;

/// Sequence's total fragment count: low nibble of byte 1.
///
/// Nibble order is provisionally per the reference layout summary; the
/// real-capture fixture (Task 9) is the arbiter — flip HERE if captures
/// disagree.
#[must_use]
pub fn msg_total_len(bytes: &[u8]) -> u8 {
    bytes[1] & 0x0F
}

/// Fragment's 1-based sequence number within its sequence: high nibble of
/// byte 1.
///
/// Nibble order is provisionally per the reference layout summary; the
/// real-capture fixture (Task 9) is the arbiter — flip HERE if captures
/// disagree.
#[must_use]
pub fn msg_seq_num(bytes: &[u8]) -> u8 {
    bytes[1] >> 4
}

/// A fragment's payload bytes, `bytes[2..10]`.
#[must_use]
pub fn msg_payload(bytes: &[u8]) -> &[u8] {
    &bytes[2..2 + MESSAGE_PAYLOAD_BYTES]
}

/// A reassembled (or stale-flushed) subscriber message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedMessage {
    /// Concatenated fragment payloads, in sequence order.
    pub bytes: Vec<u8>,
    /// `true` when the sequence was flushed early (missing fragments or
    /// superseded by a new sequence) rather than completed in full.
    pub partial: bool,
}

/// State for the one sequence currently being assembled.
struct InFlight {
    /// This sequence's total fragment count, from the fragment that opened
    /// it. Every joining fragment must report the same total.
    total: u8,
    /// Fragments seen so far, keyed by their 1-based sequence number.
    /// A `BTreeMap` keeps concatenation order free (sorted iteration) even
    /// though fragments may arrive out of order.
    fragments: BTreeMap<u8, [u8; MESSAGE_PAYLOAD_BYTES]>,
    /// Number of `push` calls (of any Message packet) since this sequence
    /// started, incremented on every push while it's in flight. Compared
    /// against `Reassembler::max_age_packets` to detect staleness.
    age: u32,
}

impl InFlight {
    /// Sequence numbers `1..=total` are all present.
    fn is_complete(&self) -> bool {
        self.total > 0 && (1..=self.total).all(|seq| self.fragments.contains_key(&seq))
    }

    /// Concatenate fragment payloads in ascending sequence order. Missing
    /// fragments (a partial flush) simply contribute nothing — the gap is
    /// silently skipped rather than zero-filled, since we don't know their
    /// true length.
    fn concat_payloads(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.fragments.len() * MESSAGE_PAYLOAD_BYTES);
        for payload in self.fragments.values() {
            out.extend_from_slice(payload);
        }
        out
    }
}

/// Collects Message-packet fragments into complete subscriber messages.
///
/// Tracks a single in-flight sequence. Feed it every checksum-valid Message
/// packet (`bytes[0] == 0x1A`, `bytes.len() == 12`) via [`Self::push`];
/// non-Message input is the caller's responsibility to filter out —
/// `push` simply ignores (returns `None` for) anything that doesn't match
/// the Message header and length, rather than asserting, since the input
/// is untrusted RF-derived data even after checksum validation.
pub struct Reassembler {
    max_age_packets: u32,
    inflight: Option<InFlight>,
}

impl Reassembler {
    /// Build a reassembler that flushes an in-flight sequence as partial
    /// once it has gone `max_age_packets` pushes without completing.
    #[must_use]
    pub fn new(max_age_packets: u32) -> Self {
        Self {
            max_age_packets,
            inflight: None,
        }
    }

    /// Feed one checksum-valid packet. Returns a [`CompletedMessage`] when
    /// this call completes the in-flight sequence, or flushes it (stale, or
    /// superseded by a fragment reporting a different `total`) as partial.
    ///
    /// When a push both triggers a stale/superseded flush of the old
    /// sequence *and* the incoming fragment immediately completes the new
    /// one it starts (only possible for a single-fragment, `total == 1`
    /// sequence), the flush is returned and the new completion is left
    /// in-flight — it will complete again, and be returned, on the very
    /// next call if that call doesn't itself restart the sequence. This
    /// keeps `push` to a single `Option` return without silently dropping
    /// the old sequence's data.
    #[must_use]
    pub fn push(&mut self, bytes: &[u8]) -> Option<CompletedMessage> {
        if bytes.len() != PacketType::Message.packet_len()
            || bytes.first() != Some(&PacketType::Message.header_byte())
        {
            return None;
        }

        let total = msg_total_len(bytes);
        let seq = msg_seq_num(bytes);
        let mut payload = [0u8; MESSAGE_PAYLOAD_BYTES];
        payload.copy_from_slice(msg_payload(bytes));

        // Age the in-flight sequence (if any) by this push, and flush it if
        // it's now stale.
        let mut flushed = None;
        if let Some(inflight) = self.inflight.as_mut() {
            inflight.age += 1;
            if inflight.age > self.max_age_packets {
                flushed = self.inflight.take().map(|f| CompletedMessage {
                    bytes: f.concat_payloads(),
                    partial: true,
                });
            }
        }

        // A fragment reporting a different total than the (still-live)
        // in-flight sequence restarts: flush the old sequence as partial.
        if flushed.is_none()
            && self
                .inflight
                .as_ref()
                .is_some_and(|inflight| inflight.total != total)
        {
            flushed = self.inflight.take().map(|f| CompletedMessage {
                bytes: f.concat_payloads(),
                partial: true,
            });
        }

        let inflight = self.inflight.get_or_insert_with(|| InFlight {
            total,
            fragments: BTreeMap::new(),
            age: 0,
        });
        inflight.fragments.insert(seq, payload);

        if flushed.is_some() {
            return flushed;
        }

        if inflight.is_complete() {
            return self.inflight.take().map(|f| CompletedMessage {
                bytes: f.concat_payloads(),
                partial: false,
            });
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::fletcher16_check_bytes;

    /// Build a checksum-valid 12-byte Message packet fragment.
    fn msg_fragment(seq: u8, total: u8, payload: [u8; MESSAGE_PAYLOAD_BYTES]) -> Vec<u8> {
        let mut p = vec![
            PacketType::Message.header_byte(),
            (seq << 4) | (total & 0x0F),
        ];
        p.extend_from_slice(&payload);
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        assert_eq!(p.len(), PacketType::Message.packet_len());
        p
    }

    #[test]
    fn in_order_fragments_complete_and_concatenate() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [1u8; MESSAGE_PAYLOAD_BYTES];
        let p2 = [2u8; MESSAGE_PAYLOAD_BYTES];
        let p3 = [3u8; MESSAGE_PAYLOAD_BYTES];

        assert_eq!(r.push(&msg_fragment(1, 3, p1)), None);
        assert_eq!(r.push(&msg_fragment(2, 3, p2)), None);
        let done = r.push(&msg_fragment(3, 3, p3)).expect("completes");
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&p1);
        expected.extend_from_slice(&p2);
        expected.extend_from_slice(&p3);
        assert_eq!(done.bytes, expected);
    }

    #[test]
    fn out_of_order_fragments_still_complete_in_sequence_order() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [0xAAu8; MESSAGE_PAYLOAD_BYTES];
        let p2 = [0xBBu8; MESSAGE_PAYLOAD_BYTES];
        let p3 = [0xCCu8; MESSAGE_PAYLOAD_BYTES];

        assert_eq!(r.push(&msg_fragment(3, 3, p3)), None);
        assert_eq!(r.push(&msg_fragment(1, 3, p1)), None);
        let done = r.push(&msg_fragment(2, 3, p2)).expect("completes");
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&p1);
        expected.extend_from_slice(&p2);
        expected.extend_from_slice(&p3);
        assert_eq!(done.bytes, expected);
    }

    #[test]
    fn missing_fragment_flushes_partial_once_stale() {
        let max_age = 3u32;
        let mut r = Reassembler::new(max_age);
        let p1 = [0x11u8; MESSAGE_PAYLOAD_BYTES];
        assert_eq!(r.push(&msg_fragment(1, 3, p1)), None);
        // Fragment 2 never arrives. Feed unrelated-looking pushes (still
        // total==3, non-conflicting seq) to age the sequence without
        // restarting it — actually simplest: push non-Message bytes, which
        // `push` ignores and does NOT age (only Message pushes age).
        // So age it with same-seq repeats which just overwrite fragment 1.
        for _ in 0..max_age {
            assert_eq!(r.push(&msg_fragment(1, 3, p1)), None);
        }
        let flushed = r
            .push(&msg_fragment(1, 3, p1))
            .expect("stale flush on the push that exceeds max_age");
        assert!(flushed.partial);
        assert_eq!(flushed.bytes, p1.to_vec());
    }

    #[test]
    fn fresh_sequence_restarts_after_a_flush() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [0x01u8; MESSAGE_PAYLOAD_BYTES];
        assert_eq!(r.push(&msg_fragment(1, 3, p1)), None);

        // A fragment reporting a different total restarts the sequence,
        // flushing the old one (fragment 1 of a total-3 sequence) partial.
        let restart_p1 = [0x02u8; MESSAGE_PAYLOAD_BYTES];
        let flushed = r
            .push(&msg_fragment(1, 2, restart_p1))
            .expect("old sequence flushed on restart");
        assert!(flushed.partial);
        assert_eq!(flushed.bytes, p1.to_vec());

        // The new total-2 sequence completes normally on its next fragment.
        let restart_p2 = [0x03u8; MESSAGE_PAYLOAD_BYTES];
        let done = r
            .push(&msg_fragment(2, 2, restart_p2))
            .expect("new sequence completes");
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&restart_p1);
        expected.extend_from_slice(&restart_p2);
        assert_eq!(done.bytes, expected);
    }

    #[test]
    fn non_message_input_is_ignored() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        assert_eq!(r.push(&[0u8; 12]), None);
        assert_eq!(r.push(&[PacketType::Message.header_byte()]), None);
    }
}
