//! Multi-packet message reassembly.
//!
//! A subscriber message that doesn't fit in one 12-byte Message packet
//! (header `0x1A`) is split across several packets. Byte 1 of each packet
//! carries the sequence's total fragment count in its **high** nibble and the
//! fragment's own **zero-based** sequence number in its low nibble;
//! [`Reassembler`] collects fragments for a single in-flight sequence and
//! emits the concatenated payload once every fragment has arrived, or as a
//! `partial` flush if the sequence stalls or is superseded.
//!
//! Both the nibble order and the zero base were **confirmed against the real
//! captures in Task 9** (`tests/real_capture.rs`); see [`msg_total_len`].
//!
//! Orbcomm interleaves multi-packet messages on a channel only rarely, so
//! this tracks exactly one in-flight sequence — not one per originating
//! subscriber or channel. A fragment whose `total` doesn't match the
//! in-flight sequence is treated as the start of a new sequence.

use std::collections::{BTreeMap, VecDeque};

use crate::packet::PacketType;

/// Number of payload bytes in a Message packet: 12 total, minus the header
/// byte (0), the length/sequence byte (1), and the two trailing Fletcher-16
/// check bytes (10, 11) — `bytes[2..10]`.
pub const MESSAGE_PAYLOAD_BYTES: usize = 8;

/// Default staleness bound: an in-flight sequence missing a fragment for
/// this many further pushes (of any packet) is flushed as partial.
pub const DEFAULT_MAX_AGE_PACKETS: u32 = 50;

/// Sequence's total fragment count: **high** nibble of byte 1. Returns `None`
/// when `bytes` is too short to contain byte 1 — this function takes an
/// unbounded slice (rather than `&[u8; 12]`) specifically so fixtures can
/// probe it with arbitrary/malformed input without panicking.
///
/// # Confirmed against real captures, Task 9
///
/// The nibble order shipped provisionally the other way round. The 41 Message
/// fragments the two off-air recordings decode (`tests/real_capture.rs`) settle
/// it: byte 1 takes values `10`, `20`, `21`, `30`, `31`, `32`, `40` … `43` —
/// a constant high nibble across each burst, with the low nibble counting up
/// from zero to one less than it. The high nibble is therefore the total and
/// the low nibble the sequence number. That also matches the reference
/// decoder's field table, which reads `msg_total_length` from hex character 2
/// (the high nibble) and `msg_packet_num` from character 3
/// (`original/ORBCOMM-receiver/orbcomm_packet.py`).
#[must_use]
pub fn msg_total_len(bytes: &[u8]) -> Option<u8> {
    bytes.get(1).map(|b| b >> 4)
}

/// Fragment's **zero-based** sequence number within its sequence: low nibble of
/// byte 1, valid over `0..total`. Returns `None` when `bytes` is too short to
/// contain byte 1.
///
/// Confirmed against real captures alongside the nibble order — see
/// [`msg_total_len`] for the evidence. The reference decoder prints the same
/// field starting at 0 (`msg_packet_num: 0 … msg_total_length: 2`, then
/// `msg_packet_num: 1`), so the numbering is zero-based, not one-based.
#[must_use]
pub fn msg_seq_num(bytes: &[u8]) -> Option<u8> {
    bytes.get(1).map(|b| b & 0x0F)
}

/// A fragment's payload bytes, `bytes[2..10]`. Returns `None` when `bytes`
/// is shorter than 10 bytes.
#[must_use]
pub fn msg_payload(bytes: &[u8]) -> Option<&[u8]> {
    bytes.get(2..2 + MESSAGE_PAYLOAD_BYTES)
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
    /// Fragments seen so far, keyed by their zero-based sequence number. A
    /// `BTreeMap` keeps concatenation order free (sorted iteration) even
    /// though fragments may arrive out of order. Only sequence numbers in
    /// `0..total` are ever inserted (see [`Reassembler::push`]), so
    /// [`Self::is_complete`] and [`Self::concat_payloads`] never need to
    /// re-check the range themselves. A fragment re-sent for a sequence
    /// number already present overwrites the earlier one — last write
    /// wins, silently.
    fragments: BTreeMap<u8, [u8; MESSAGE_PAYLOAD_BYTES]>,
    /// Number of `push` calls (of any syntactically valid Message packet)
    /// since this sequence started, incremented on every push while it's
    /// in flight. Compared against `Reassembler::max_age_packets` to
    /// detect staleness. Starts at 0 on the push that opens the sequence.
    age: u32,
}

impl InFlight {
    /// Sequence numbers `0..total` are all present.
    fn is_complete(&self) -> bool {
        self.total > 0 && (0..self.total).all(|seq| self.fragments.contains_key(&seq))
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
/// `push` simply ignores (returns whatever is queued, see below, rather
/// than asserting) anything that doesn't match the Message header and
/// length, since the input is untrusted RF-derived data even after
/// checksum validation.
///
/// A fragment whose own sequence number falls outside `0..total` (as
/// reported by that same fragment) is likewise treated as inert: it does
/// not age or otherwise perturb the in-flight sequence, and its payload is
/// never inserted. This keeps a self-inconsistent (but checksum-valid)
/// fragment from corrupting a completed message's byte order.
pub struct Reassembler {
    max_age_packets: u32,
    inflight: Option<InFlight>,
    /// Completed/flushed messages produced but not yet returned. `push`
    /// returns only one `Option<CompletedMessage>` per call, but a single
    /// call can produce two events (a flush of the superseded/stale
    /// sequence *and* an immediate completion of the fragment that
    /// triggered it — only possible for a single-fragment, `total == 1`
    /// sequence). Both are queued in the order they occurred and drained
    /// one-per-call (oldest first) by every subsequent `push`, so no
    /// completed or flushed message is ever silently dropped — it's just
    /// delayed until the next call(s) pop it off.
    pending: VecDeque<CompletedMessage>,
}

impl Reassembler {
    /// Build a reassembler that flushes an in-flight sequence as partial
    /// once it goes stale.
    ///
    /// A sequence's age starts at 0 on the push that opens it and
    /// increments by 1 on every later push of a syntactically valid
    /// Message packet while it's in flight (regardless of which sequence
    /// number that later push carries). Once age exceeds
    /// `max_age_packets`, the sequence is flushed as partial on that push
    /// — i.e. it takes `max_age_packets + 2` total Message pushes touching
    /// this reassembler (the opening push, plus `max_age_packets + 1`
    /// more) before a never-completing sequence flushes.
    #[must_use]
    pub fn new(max_age_packets: u32) -> Self {
        Self {
            max_age_packets,
            inflight: None,
            pending: VecDeque::new(),
        }
    }

    /// Feed one checksum-valid packet. Returns the oldest not-yet-returned
    /// [`CompletedMessage`] this reassembler has produced, if any — see the
    /// [`Reassembler::pending`] doc for why a single call doesn't always
    /// return the event it just produced.
    #[must_use]
    pub fn push(&mut self, bytes: &[u8]) -> Option<CompletedMessage> {
        if bytes.len() != PacketType::Message.packet_len()
            || bytes.first() != Some(&PacketType::Message.header_byte())
        {
            return self.pending.pop_front();
        }

        // `bytes.len() == 12` is already guaranteed above, so these are
        // infallible in practice; handled without panicking regardless.
        let (Some(total), Some(seq), Some(payload_slice)) =
            (msg_total_len(bytes), msg_seq_num(bytes), msg_payload(bytes))
        else {
            return self.pending.pop_front();
        };

        // A fragment whose own seq is outside its own total's valid range
        // is self-inconsistent — ignore it entirely (see struct doc). Sequence
        // numbers are zero-based, so `0..total` is the range and a `total` of
        // zero admits nothing at all.
        if seq >= total {
            return self.pending.pop_front();
        }

        let mut payload = [0u8; MESSAGE_PAYLOAD_BYTES];
        payload.copy_from_slice(payload_slice);

        // Age the in-flight sequence (if any) by this push, and flush it if
        // it's now stale.
        if let Some(inflight) = self.inflight.as_mut() {
            inflight.age += 1;
            if inflight.age > self.max_age_packets {
                self.flush_inflight(true);
            }
        }

        // A fragment reporting a different total than the (still-live)
        // in-flight sequence restarts: flush the old sequence as partial.
        if self
            .inflight
            .as_ref()
            .is_some_and(|inflight| inflight.total != total)
        {
            self.flush_inflight(true);
        }

        let inflight = self.inflight.get_or_insert_with(|| InFlight {
            total,
            fragments: BTreeMap::new(),
            age: 0,
        });
        inflight.fragments.insert(seq, payload);

        if inflight.is_complete() {
            self.flush_inflight(false);
        }

        self.pending.pop_front()
    }

    /// Take the in-flight sequence, if any, concatenate its fragments, and
    /// queue the result onto `pending` with the given `partial` flag.
    /// No-op when nothing is in flight.
    fn flush_inflight(&mut self, partial: bool) {
        if let Some(f) = self.inflight.take() {
            self.pending.push_back(CompletedMessage {
                bytes: f.concat_payloads(),
                partial,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::fletcher16_check_bytes;

    /// Build a checksum-valid 12-byte Message packet fragment. `seq` is
    /// zero-based; the on-air byte 1 is `total` in the high nibble and `seq`
    /// in the low one (see [`msg_total_len`]).
    fn msg_fragment(seq: u8, total: u8, payload: [u8; MESSAGE_PAYLOAD_BYTES]) -> Vec<u8> {
        let mut p = vec![
            PacketType::Message.header_byte(),
            (total << 4) | (seq & 0x0F),
        ];
        p.extend_from_slice(&payload);
        let (c0, c1) = fletcher16_check_bytes(&p);
        p.push(c0);
        p.push(c1);
        assert_eq!(p.len(), PacketType::Message.packet_len());
        p
    }

    #[test]
    fn nibble_layout_matches_the_real_captures() {
        // Verbatim byte-1 values decoded off air by `tests/real_capture.rs`, one
        // burst per row: the high nibble is constant across a burst and the low
        // nibble counts 0, 1, … total − 1. Anchoring the accessors to real wire
        // bytes is what stops the pair silently swapping back — every other test
        // in this module goes through `msg_fragment`, which would swap with them.
        for (byte1, want_total, want_seq) in [
            (0x10u8, 1u8, 0u8),
            (0x20, 2, 0),
            (0x21, 2, 1),
            (0x30, 3, 0),
            (0x31, 3, 1),
            (0x32, 3, 2),
            (0x40, 4, 0),
            (0x43, 4, 3),
        ] {
            let packet = [PacketType::Message.header_byte(), byte1];
            assert_eq!(
                msg_total_len(&packet),
                Some(want_total),
                "byte1 {byte1:02X}"
            );
            assert_eq!(msg_seq_num(&packet), Some(want_seq), "byte1 {byte1:02X}");
            // Every real fragment is self-consistent under this reading, which
            // is the property the reassembler's range check leans on.
            assert!(want_seq < want_total);
        }
    }

    #[test]
    fn in_order_fragments_complete_and_concatenate() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [1u8; MESSAGE_PAYLOAD_BYTES];
        let p2 = [2u8; MESSAGE_PAYLOAD_BYTES];
        let p3 = [3u8; MESSAGE_PAYLOAD_BYTES];

        assert_eq!(r.push(&msg_fragment(0, 3, p1)), None);
        assert_eq!(r.push(&msg_fragment(1, 3, p2)), None);
        let done = r.push(&msg_fragment(2, 3, p3)).expect("completes");
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

        assert_eq!(r.push(&msg_fragment(2, 3, p3)), None);
        assert_eq!(r.push(&msg_fragment(0, 3, p1)), None);
        let done = r.push(&msg_fragment(1, 3, p2)).expect("completes");
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
        assert_eq!(r.push(&msg_fragment(0, 3, p1)), None);
        // Fragments 1 and 2 never arrive. Age the sequence with same-seq repeats
        // (total unchanged, so no restart), which just overwrite fragment 0
        // in place — harmless since we only assert the *fact* of a flush
        // and the surviving payload below.
        for _ in 0..max_age {
            assert_eq!(r.push(&msg_fragment(0, 3, p1)), None);
        }
        let flushed = r
            .push(&msg_fragment(0, 3, p1))
            .expect("stale flush on the push that exceeds max_age");
        assert!(flushed.partial);
        assert_eq!(flushed.bytes, p1.to_vec());
    }

    #[test]
    fn fresh_sequence_restarts_after_a_flush() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [0x01u8; MESSAGE_PAYLOAD_BYTES];
        assert_eq!(r.push(&msg_fragment(0, 3, p1)), None);

        // A fragment reporting a different total restarts the sequence,
        // flushing the old one (fragment 0 of a total-3 sequence) partial.
        let restart_p1 = [0x02u8; MESSAGE_PAYLOAD_BYTES];
        let flushed = r
            .push(&msg_fragment(0, 2, restart_p1))
            .expect("old sequence flushed on restart");
        assert!(flushed.partial);
        assert_eq!(flushed.bytes, p1.to_vec());

        // The new total-2 sequence completes normally on its next fragment.
        let restart_p2 = [0x03u8; MESSAGE_PAYLOAD_BYTES];
        let done = r
            .push(&msg_fragment(1, 2, restart_p2))
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

    /// Regression test for the same-push flush+completion race: a fragment
    /// (B) that both triggers a restart-flush of an old, incomplete
    /// sequence (A) AND itself immediately completes a `total == 1`
    /// sequence must not have its own completed data silently overwritten
    /// by the next fragment (C) that reuses the same sequence number. Both
    /// B's and C's completions must eventually come back out, in order,
    /// and A must come back out partial.
    #[test]
    fn flush_and_completion_in_the_same_push_are_both_eventually_returned() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let a1 = [0xA1u8; MESSAGE_PAYLOAD_BYTES];
        let b = [0xB0u8; MESSAGE_PAYLOAD_BYTES];
        let c = [0xC0u8; MESSAGE_PAYLOAD_BYTES];

        // A: first fragment of a 3-fragment sequence, left incomplete.
        assert_eq!(r.push(&msg_fragment(0, 3, a1)), None);

        // B: a total==1 fragment restarts the sequence. In the same push,
        // A's old sequence flushes partial AND B's new total==1 sequence
        // completes. Only one `Option` can come back from this call; the
        // flush comes back first (it was queued first).
        let out1 = r.push(&msg_fragment(0, 1, b)).expect("A flushes partial");
        assert!(out1.partial, "A must flush partial");
        assert_eq!(out1.bytes, a1.to_vec());

        // C: another total==1, seq==1 fragment arrives. It must build a
        // *fresh* in-flight sequence rather than overwriting B's
        // already-migrated (but not yet returned) completed data. This
        // push returns B's queued completion.
        let out2 = r
            .push(&msg_fragment(0, 1, c))
            .expect("B's completion, queued from the previous push");
        assert!(!out2.partial, "B must come out complete, not partial");
        assert_eq!(out2.bytes, b.to_vec(), "B's data must not be lost");

        // Drain: any further push (even non-Message input, which performs
        // no state mutation) pops the next queued item — C's completion.
        let out3 = r
            .push(&[0u8; 12])
            .expect("C's completion, queued from the previous push");
        assert!(!out3.partial, "C must come out complete too");
        assert_eq!(out3.bytes, c.to_vec());
    }

    #[test]
    fn out_of_range_seq_does_not_corrupt_completed_message() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [0x11u8; MESSAGE_PAYLOAD_BYTES];
        let p2 = [0x22u8; MESSAGE_PAYLOAD_BYTES];
        let bogus = [0xFFu8; MESSAGE_PAYLOAD_BYTES];

        assert_eq!(r.push(&msg_fragment(0, 2, p1)), None);
        // seq==2 is outside 0..2 for this fragment's own total — ignored.
        assert_eq!(r.push(&msg_fragment(2, 2, bogus)), None);
        // seq==3 is likewise outside 0..2 — ignored.
        assert_eq!(r.push(&msg_fragment(3, 2, bogus)), None);

        let done = r.push(&msg_fragment(1, 2, p2)).expect("completes");
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&p1);
        expected.extend_from_slice(&p2);
        assert_eq!(
            done.bytes, expected,
            "bogus out-of-range fragment must not appear in the output"
        );
    }
}
