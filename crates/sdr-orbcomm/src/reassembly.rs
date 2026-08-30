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
//! subscriber or channel.
//!
//! # Sequence boundaries: strictly increasing `seq`
//!
//! The downlink is an in-order TDM broadcast: a satellite transmits a
//! message's fragments in ascending sequence order, and the next message on
//! that channel starts over at `seq 0`. The real captures show exactly that
//! — the byte-1 values enumerated on [`msg_total_len`] come in runs whose
//! low nibble counts up from zero, with the next same-`total` run restarting
//! at zero rather than continuing.
//!
//! So a fragment joins the in-flight sequence only when its `seq` is
//! **strictly greater** than every `seq` already collected (gaps are fine —
//! a lost fragment simply never arrives). Anything else — an equal `seq`, a
//! lower one, or a different `total` — is the boundary of a NEW sequence:
//! the in-flight one is flushed as `partial` and the arriving fragment opens
//! a fresh sequence. `seq == 0` restarting a sequence falls out of that rule
//! rather than being special-cased, and so do duplicates.
//!
//! An earlier revision instead tolerated out-of-order arrival, keeping any
//! fragment whose `total` matched. That was speculative — nothing in the
//! captures or the reference decoder calls for it — and it actively
//! corrupted messages: a stale fragment left behind by a lost sequence (say
//! `seq 1` of a 3-fragment message) would sit in flight until the NEXT
//! same-`total` sequence's `seq 0` and `seq 2` filled in around it, and the
//! "completed" message would be a hybrid of two unrelated transmissions
//! (`a_new_sequence_never_merges_with_a_stale_same_total_fragment`).

use std::collections::BTreeMap;

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
    /// `BTreeMap` keeps concatenation order free (sorted iteration) and
    /// makes the sequence's high-water mark a `keys().next_back()` away —
    /// which is what the strictly-increasing boundary rule (module docs)
    /// tests each arrival against. Only sequence numbers in `0..total` are
    /// ever inserted (see [`Reassembler::push`]), so [`Self::is_complete`]
    /// and [`Self::concat_payloads`] never need to re-check the range
    /// themselves. A key is never overwritten: an arriving fragment whose
    /// `seq` is already present opens a new sequence instead.
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

    /// Highest sequence number collected so far — the high-water mark an
    /// arriving fragment must strictly exceed to join this sequence rather
    /// than start a new one (module docs). `None` only for the momentarily
    /// empty map inside [`Reassembler::push`]'s `get_or_insert_with`, which
    /// treats it as "nothing to exceed".
    fn highest_seq(&self) -> Option<u8> {
        self.fragments.keys().next_back().copied()
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
/// `push` simply does nothing (appends nothing to `out`, rather than
/// asserting) for anything that doesn't match the Message header and
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
        }
    }

    /// Feed one checksum-valid packet. Sink-style, mirroring
    /// [`crate::deframe::Deframer::push_bit`]: every [`CompletedMessage`]
    /// this call produces is appended to `out`, in the order produced —
    /// `out` is never cleared here, so the caller owns a reused scratch
    /// buffer the same way `Deframer::push_bit`'s callers do.
    ///
    /// A single call can produce **two** events — a flush of a
    /// superseded/stale sequence *and* an immediate completion of the
    /// fragment that triggered it (only possible when the fragment starts
    /// a `total == 1` sequence) — and both land in `out` from that one
    /// call, flush first. Earlier revisions returned at most one
    /// `Option<CompletedMessage>` per call and queued the rest for a later
    /// call to drain; that let a completed message sit unrendered until
    /// the *next* Message packet arrived, which could be minutes away on a
    /// live channel. Appending everything to an out-parameter closes that
    /// gap by construction: nothing is ever deferred past the call that
    /// produced it.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<CompletedMessage>) {
        if bytes.len() != PacketType::Message.packet_len()
            || bytes.first() != Some(&PacketType::Message.header_byte())
        {
            return;
        }

        // `bytes.len() == 12` is already guaranteed above, so these are
        // infallible in practice; handled without panicking regardless.
        let (Some(total), Some(seq), Some(payload_slice)) =
            (msg_total_len(bytes), msg_seq_num(bytes), msg_payload(bytes))
        else {
            return;
        };

        // A fragment whose own seq is outside its own total's valid range
        // is self-inconsistent — ignore it entirely (see struct doc). Sequence
        // numbers are zero-based, so `0..total` is the range and a `total` of
        // zero admits nothing at all.
        if seq >= total {
            return;
        }

        let mut payload = [0u8; MESSAGE_PAYLOAD_BYTES];
        payload.copy_from_slice(payload_slice);

        // Age the in-flight sequence (if any) by this push, and flush it if
        // it's now stale.
        if let Some(inflight) = self.inflight.as_mut() {
            inflight.age += 1;
            if inflight.age > self.max_age_packets {
                self.flush_inflight(true, out);
            }
        }

        // Sequence boundary (module docs): the arriving fragment joins the
        // still-live in-flight sequence only when it reports the same total
        // AND its seq strictly exceeds every seq collected so far. A
        // different total, an equal seq or a lower one all mean a new
        // sequence has started — flush the old one as partial and let the
        // arriving fragment open a fresh one below.
        if self.inflight.as_ref().is_some_and(|inflight| {
            inflight.total != total || inflight.highest_seq().is_some_and(|hi| seq <= hi)
        }) {
            self.flush_inflight(true, out);
        }

        let inflight = self.inflight.get_or_insert_with(|| InFlight {
            total,
            fragments: BTreeMap::new(),
            age: 0,
        });
        inflight.fragments.insert(seq, payload);

        if inflight.is_complete() {
            self.flush_inflight(false, out);
        }
    }

    /// Take the in-flight sequence, if any, concatenate its fragments, and
    /// append the result to `out` with the given `partial` flag. No-op when
    /// nothing is in flight.
    fn flush_inflight(&mut self, partial: bool, out: &mut Vec<CompletedMessage>) {
        if let Some(f) = self.inflight.take() {
            out.push(CompletedMessage {
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

    /// Push one packet and return everything it produced, as a fresh `Vec`
    /// — the sink-style convenience most tests below want, since most
    /// pushes in these tests produce 0 or 1 events. Tests that must observe
    /// two events from a single call use `r.push(..., &mut out)` directly.
    fn push_all(r: &mut Reassembler, bytes: &[u8]) -> Vec<CompletedMessage> {
        let mut out = Vec::new();
        r.push(bytes, &mut out);
        out
    }

    /// Assert `push_all` produced no events.
    fn assert_none(r: &mut Reassembler, bytes: &[u8]) {
        assert_eq!(push_all(r, bytes), Vec::new(), "expected no events");
    }

    /// Assert `push_all` produced exactly one event and return it.
    fn expect_one(r: &mut Reassembler, bytes: &[u8], msg: &str) -> CompletedMessage {
        let mut out = push_all(r, bytes);
        assert_eq!(out.len(), 1, "{msg}: got {out:?}");
        out.remove(0)
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

        assert_none(&mut r, &msg_fragment(0, 3, p1));
        assert_none(&mut r, &msg_fragment(1, 3, p2));
        let done = expect_one(&mut r, &msg_fragment(2, 3, p3), "completes");
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&p1);
        expected.extend_from_slice(&p2);
        expected.extend_from_slice(&p3);
        assert_eq!(done.bytes, expected);
    }

    /// `CodeRabbit` round 1 on PR #871, the defect the monotonic-sequence
    /// boundary rule closes: a stale in-flight fragment left over from an
    /// earlier same-`total` sequence must NOT merge with the fragments of
    /// the next one. Before the rule, the leftover `seq 1` simply stayed
    /// put while `seq 0` and `seq 2` of the *new* sequence filled in
    /// around it, and the "completed" message was a hybrid of two
    /// unrelated transmissions.
    #[test]
    fn a_new_sequence_never_merges_with_a_stale_same_total_fragment() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let stale = [0x99u8; MESSAGE_PAYLOAD_BYTES];
        let p0 = [0xA0u8; MESSAGE_PAYLOAD_BYTES];
        let p1 = [0xA1u8; MESSAGE_PAYLOAD_BYTES];
        let p2 = [0xA2u8; MESSAGE_PAYLOAD_BYTES];

        // Left over from a sequence whose fragments 0 and 2 were never
        // decoded: fragment 1 of a total-3 sequence, still in flight.
        assert_none(&mut r, &msg_fragment(1, 3, stale));

        // A NEW total-3 sequence starts. Its `seq 0` is not greater than
        // the in-flight high-water mark of 1, so it opens a fresh
        // sequence and flushes the leftover as partial — carrying only
        // its own payload.
        let flushed = expect_one(
            &mut r,
            &msg_fragment(0, 3, p0),
            "the stale fragment flushes when the new sequence opens",
        );
        assert!(flushed.partial);
        assert_eq!(flushed.bytes, stale.to_vec());

        // The new sequence then completes from exactly its own three
        // fragments — the stale payload must appear nowhere in it.
        assert_none(&mut r, &msg_fragment(1, 3, p1));
        let done = expect_one(&mut r, &msg_fragment(2, 3, p2), "new sequence completes");
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&p0);
        expected.extend_from_slice(&p1);
        expected.extend_from_slice(&p2);
        assert_eq!(
            done.bytes, expected,
            "the completed message must not carry the stale fragment"
        );
    }

    /// Gaps stay tolerated: a lost middle fragment leaves the sequence in
    /// flight (to be stale-flushed later), it does not complete early.
    /// Strictly increasing seq is the rule; contiguous seq is not.
    #[test]
    fn a_gap_leaves_the_sequence_in_flight() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p0 = [0x10u8; MESSAGE_PAYLOAD_BYTES];
        let p2 = [0x12u8; MESSAGE_PAYLOAD_BYTES];

        assert_none(&mut r, &msg_fragment(0, 3, p0));
        // Fragment 1 was lost; fragment 2 still joins (2 > 0) and the
        // sequence stays incomplete rather than completing on two of three.
        assert_none(&mut r, &msg_fragment(2, 3, p2));
    }

    /// A repeated sequence number is the same boundary signal as a
    /// backwards one: the downlink is an in-order broadcast, so a `seq`
    /// we have already seen can only belong to a new transmission.
    #[test]
    fn a_duplicate_seq_starts_a_new_sequence() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let first = [0x21u8; MESSAGE_PAYLOAD_BYTES];
        let second = [0x22u8; MESSAGE_PAYLOAD_BYTES];
        let tail = [0x23u8; MESSAGE_PAYLOAD_BYTES];

        assert_none(&mut r, &msg_fragment(0, 2, first));
        let flushed = expect_one(
            &mut r,
            &msg_fragment(0, 2, second),
            "the duplicate seq restarts, flushing the first attempt",
        );
        assert!(flushed.partial);
        assert_eq!(flushed.bytes, first.to_vec());

        // The restarted sequence completes from `second` + its own tail.
        let done = expect_one(&mut r, &msg_fragment(1, 2, tail), "restarted completes");
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&second);
        expected.extend_from_slice(&tail);
        assert_eq!(done.bytes, expected);
    }

    #[test]
    fn missing_fragment_flushes_partial_once_stale() {
        /// The largest total a nibble can express. Used so the sequence
        /// can be aged with strictly increasing (hence non-restarting)
        /// fragment numbers without ever completing.
        const TOTAL: u8 = 15;
        let max_age = 3u32;
        let mut r = Reassembler::new(max_age);
        let payload = |seq: u8| [0x10 | seq; MESSAGE_PAYLOAD_BYTES];

        // Fragments 0..=3 arrive; 4..15 never do.
        for seq in 0..=3u8 {
            assert_none(&mut r, &msg_fragment(seq, TOTAL, payload(seq)));
        }
        // The next push takes the age past `max_age`, flushing the stalled
        // sequence as partial before the arriving fragment opens its own.
        let flushed = expect_one(
            &mut r,
            &msg_fragment(4, TOTAL, payload(4)),
            "stale flush on the push that exceeds max_age",
        );
        assert!(flushed.partial);
        let mut expected = Vec::new();
        for seq in 0..=3u8 {
            expected.extend_from_slice(&payload(seq));
        }
        assert_eq!(flushed.bytes, expected);
    }

    #[test]
    fn fresh_sequence_restarts_after_a_flush() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [0x01u8; MESSAGE_PAYLOAD_BYTES];
        assert_none(&mut r, &msg_fragment(0, 3, p1));

        // A fragment reporting a different total restarts the sequence,
        // flushing the old one (fragment 0 of a total-3 sequence) partial.
        let restart_p1 = [0x02u8; MESSAGE_PAYLOAD_BYTES];
        let flushed = expect_one(
            &mut r,
            &msg_fragment(0, 2, restart_p1),
            "old sequence flushed on restart",
        );
        assert!(flushed.partial);
        assert_eq!(flushed.bytes, p1.to_vec());

        // The new total-2 sequence completes normally on its next fragment.
        let restart_p2 = [0x03u8; MESSAGE_PAYLOAD_BYTES];
        let done = expect_one(
            &mut r,
            &msg_fragment(1, 2, restart_p2),
            "new sequence completes",
        );
        assert!(!done.partial);
        let mut expected = Vec::new();
        expected.extend_from_slice(&restart_p1);
        expected.extend_from_slice(&restart_p2);
        assert_eq!(done.bytes, expected);
    }

    #[test]
    fn non_message_input_is_ignored() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        assert_none(&mut r, &[0u8; 12]);
        assert_none(&mut r, &[PacketType::Message.header_byte()]);
    }

    /// Regression test for the same-push flush+completion race: a fragment
    /// (B) that both triggers a restart-flush of an old, incomplete
    /// sequence (A) AND itself immediately completes a `total == 1`
    /// sequence must emit BOTH events from that one `push` call, flush
    /// first — not silently lose B, and not make the caller wait for a
    /// later call to see it. A following fragment (C) that reuses the same
    /// wire bytes as B must build a *fresh* sequence and emit exactly its
    /// own completion, not B's.
    #[test]
    fn flush_and_completion_in_the_same_push_both_come_out_together() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let a1 = [0xA1u8; MESSAGE_PAYLOAD_BYTES];
        let b = [0xB0u8; MESSAGE_PAYLOAD_BYTES];
        let c = [0xC0u8; MESSAGE_PAYLOAD_BYTES];

        // A: first fragment of a 3-fragment sequence, left incomplete.
        assert_none(&mut r, &msg_fragment(0, 3, a1));

        // B: a total==1 fragment restarts the sequence. In the same push,
        // A's old sequence flushes partial AND B's new total==1 sequence
        // completes — both must land in `out` from this one call, flush
        // first (that's the order they're discovered in).
        let mut out = Vec::new();
        r.push(&msg_fragment(0, 1, b), &mut out);
        assert_eq!(out.len(), 2, "expected both events from one call: {out:?}");
        assert!(out[0].partial, "A must flush partial, and come out first");
        assert_eq!(out[0].bytes, a1.to_vec());
        assert!(!out[1].partial, "B must come out complete");
        assert_eq!(out[1].bytes, b.to_vec(), "B's data must not be lost");

        // C: another total==1, seq==0 fragment arrives after B already
        // completed (and was returned). It must build a fresh sequence and
        // emit exactly its own completion — not B's, which is already gone.
        let done = expect_one(&mut r, &msg_fragment(0, 1, c), "C completes on its own");
        assert!(!done.partial, "C must come out complete too");
        assert_eq!(done.bytes, c.to_vec());
    }

    /// Regression test for the overnight live-smoke display-lag defect: an
    /// orphaned second-of-two fragment (no fragment 0 ever arrived) sits
    /// in flight, then a single-fragment message (B) restarts the
    /// sequence. The old API returned only the flush from that push and
    /// queued B's completion for a *later* Message packet — which, on a
    /// live channel, rendered B's bytes under the *next* message's log
    /// entry, potentially minutes later. The fixed sink-style `push` must
    /// emit both the orphan's partial flush and B's completion from the
    /// single push that produced them, and a following message (C) must
    /// emit exactly C.
    #[test]
    fn orphan_flush_and_message_completion_land_in_the_same_push() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let orphan = [0x77u8; MESSAGE_PAYLOAD_BYTES];
        let b = [0xB1u8; MESSAGE_PAYLOAD_BYTES];
        let c = [0xC2u8; MESSAGE_PAYLOAD_BYTES];

        // An orphan: fragment 1 of a two-fragment sequence, with fragment 0
        // never arriving.
        assert_none(&mut r, &msg_fragment(1, 2, orphan));

        // Message B (total==1, seq==0) restarts the sequence: the orphan
        // flushes partial AND B completes, both from this one push.
        let mut out = Vec::new();
        r.push(&msg_fragment(0, 1, b), &mut out);
        assert_eq!(
            out.len(),
            2,
            "orphan flush and B's completion must land together: {out:?}"
        );
        assert!(out[0].partial, "the orphan flushes partial, first");
        assert_eq!(out[0].bytes, orphan.to_vec());
        assert!(!out[1].partial, "B completes");
        assert_eq!(out[1].bytes, b.to_vec());

        // Message C (also total==1, seq==0) must emit exactly C — not B.
        let done = expect_one(&mut r, &msg_fragment(0, 1, c), "C completes on its own");
        assert!(!done.partial);
        assert_eq!(done.bytes, c.to_vec(), "C must not come back as B's bytes");
    }

    #[test]
    fn out_of_range_seq_does_not_corrupt_completed_message() {
        let mut r = Reassembler::new(DEFAULT_MAX_AGE_PACKETS);
        let p1 = [0x11u8; MESSAGE_PAYLOAD_BYTES];
        let p2 = [0x22u8; MESSAGE_PAYLOAD_BYTES];
        let bogus = [0xFFu8; MESSAGE_PAYLOAD_BYTES];

        assert_none(&mut r, &msg_fragment(0, 2, p1));
        // seq==2 is outside 0..2 for this fragment's own total — ignored.
        assert_none(&mut r, &msg_fragment(2, 2, bogus));
        // seq==3 is likewise outside 0..2 — ignored.
        assert_none(&mut r, &msg_fragment(3, 2, bogus));

        let done = expect_one(&mut r, &msg_fragment(1, 2, p2), "completes");
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
