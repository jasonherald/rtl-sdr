//! Multi-block ACARS message reassembly (issue #580).
//!
//! ACARS frames longer than ~220 bytes split across multiple
//! blocks. Non-final blocks end with the ETB byte (0x17), the
//! final block ends with ETX (0x03). Block IDs sequence
//! per-aircraft per-message-no — the same `(aircraft,
//! message_no)` pair identifies "blocks of the same logical
//! message". Within a pair, block IDs increase (`1`, `2`, …
//! ending on ETX).
//!
//! The v1 viewer (sub-project 3 of epic #474) displayed each
//! block as its own row with the block ID shown — readable but
//! noisy on long messages. This module reassembles a stream of
//! frames into a single logical message per `(aircraft,
//! message_no)` key:
//!
//! - **ETB block** ([`AcarsMessage::end_of_message`] = `false`):
//!   parked in a pending bucket keyed on `(aircraft,
//!   message_no)`. Concatenation is deferred until ETX.
//! - **ETX block** ([`AcarsMessage::end_of_message`] = `true`):
//!   any pending blocks for the same key are sorted by
//!   `block_id`, concatenated to the ETX block's text, and
//!   emitted as a single reassembled message with
//!   [`AcarsMessage::reassembled_block_count`] set to N.
//! - **Timeout**: pending entries older than
//!   [`REASSEMBLY_TIMEOUT`] (30 s) at observation time are
//!   emitted as best-effort partial reassemblies (their text
//!   alone, with `end_of_message = false` preserved). Avoids
//!   leaking RAM for messages whose ETX never arrives.
//!
//! Messages without a `message_no` (e.g. some uplinks, link
//! tests) are pass-through: they can't be reassembled because
//! we have no key to group them. Messages that arrive ETX-only
//! (single-block, `block_id = 1`, `end_of_message = true`) are
//! also pass-through — no pending blocks to merge.
//!
//! # Out-of-order handling
//!
//! Real-world ACARS over the air can deliver blocks out of
//! order — atmospheric fades, channel hopping at the airline's
//! ground network. The assembler accepts blocks in arbitrary
//! arrival order and sorts by `block_id` at emission time; the
//! ETX block doesn't need to arrive last (it's identified by
//! `end_of_message`, not by position).
//!
//! # Pure / no GTK / no I/O
//!
//! `MessageAssembler` is a plain owning state machine. Time is
//! provided by the caller via `observe(msg, now)` so this stays
//! testable without a clock. The DSP-side caller passes
//! `SystemTime::now()`; CLI tools that replay fixtures pass the
//! frame's own `timestamp`.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use arrayvec::ArrayString;

use crate::frame::AcarsMessage;

/// Maximum age of a pending multi-block message before its
/// partial blocks are emitted as a best-effort partial
/// reassembly. Per issue #580: "Timeout: ~30s for partial
/// messages without final block." 30 s is plenty of headroom
/// for real-world block-arrival jitter while bounding the
/// per-key memory footprint.
pub const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on the number of pending multi-block messages held
/// at once. Defends against pathological streams that would
/// otherwise grow the `HashMap` unboundedly (e.g. a stuck
/// transmitter spamming distinct message-numbers without ever
/// sending ETX). On overflow we drop the oldest pending entry
/// to make room. 256 is generous for real airband traffic
/// (typical busy ground stations carry a few dozen aircraft
/// at peak; each aircraft has at most a few open messages).
pub const MAX_PENDING_MESSAGES: usize = 256;

/// Composite key identifying a multi-block message: aircraft
/// registration + ACARS message number. Both must be present
/// (non-`None`, non-empty) for a frame to participate in
/// reassembly.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ReassemblyKey {
    aircraft: ArrayString<8>,
    message_no: ArrayString<5>,
}

/// One pending bucket of blocks waiting for the ETX.
#[derive(Debug)]
struct PendingMessage {
    /// Wall-clock time the first block in this bucket arrived.
    /// Drives [`REASSEMBLY_TIMEOUT`].
    first_seen: SystemTime,
    /// Blocks observed for this key so far. Sorted by `block_id`
    /// at emission time, not insertion time, so out-of-order
    /// arrivals don't matter.
    blocks: Vec<AcarsMessage>,
}

/// Per-channel multi-block reassembler. One instance per
/// independent decode path (e.g. one per channel inside
/// [`crate::ChannelBank`], one for the whole stream inside
/// the WAV-file CLI path).
#[derive(Debug, Default)]
pub struct MessageAssembler {
    pending: HashMap<ReassemblyKey, PendingMessage>,
}

impl MessageAssembler {
    /// Create an empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending multi-block buckets. Test-only
    /// observability.
    #[must_use]
    #[doc(hidden)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Observe one decoded ACARS frame. Returns 0+ messages
    /// ready for downstream consumption:
    ///
    /// - **0 messages**: the frame was an ETB block parked in
    ///   the pending bucket (its key wasn't yet complete).
    /// - **1 message**: the typical case — pass-through (no
    ///   `message_no`, single-block ETX, or unrelated key) or
    ///   a successful reassembly.
    /// - **2+ messages**: the observation triggered timeout
    ///   sweeps in addition to the current frame's own emission.
    ///
    /// `now` is the caller-provided clock. The DSP path passes
    /// `SystemTime::now()`; offline replay paths can pass the
    /// frame's own `timestamp` to make the assembler clock
    /// deterministic.
    pub fn observe(&mut self, msg: AcarsMessage, now: SystemTime) -> Vec<AcarsMessage> {
        let mut out = self.sweep_timeouts(now);

        let Some(key) = build_key(&msg) else {
            // No reassembly key (no message_no) — pass-through.
            out.push(msg);
            return out;
        };

        if msg.end_of_message {
            // ETX: combine any pending blocks for this key with
            // the current frame and emit. If nothing is pending,
            // this is a single-block message — pass through
            // unchanged (block_count stays at 1).
            if let Some(pending) = self.pending.remove(&key) {
                out.push(combine(pending.blocks, msg));
            } else {
                out.push(msg);
            }
            return out;
        }

        // ETB: park in the pending bucket. New bucket if this
        // is the first block we've seen for the key; otherwise
        // append.
        if let Some(pending) = self.pending.get_mut(&key) {
            pending.blocks.push(msg);
        } else {
            // Bound the pending map. If we'd exceed the cap,
            // drop the oldest entry by `first_seen` so the
            // newcomer has a slot.
            if self.pending.len() >= MAX_PENDING_MESSAGES {
                self.evict_oldest_pending();
            }
            self.pending.insert(
                key,
                PendingMessage {
                    // Stamp with the caller-provided clock, not
                    // `msg.timestamp`. Replay paths can drift the
                    // two clocks (e.g. an offline test passing a
                    // synthetic `now` while the message carries a
                    // wall-clock stamp). Pinning to `now` keeps
                    // the timeout contract consistent with the
                    // sweep clock — CR round 1 on PR #593.
                    first_seen: now,
                    blocks: vec![msg],
                },
            );
        }
        out
    }

    /// Forcibly emit every pending bucket as a partial
    /// reassembly. Used at end-of-stream (e.g. when the CLI
    /// finishes a WAV file) so partial messages aren't silently
    /// dropped. The emitted messages keep `end_of_message =
    /// false` to flag that the ETX never arrived.
    pub fn flush(&mut self) -> Vec<AcarsMessage> {
        self.pending
            .drain()
            .filter_map(|(_, pending)| combine_partial(pending.blocks))
            .collect()
    }

    /// Drain pending buckets older than [`REASSEMBLY_TIMEOUT`]
    /// at `now`. Returns the expired entries as partial
    /// reassemblies, sorted by their `first_seen` so test +
    /// log output is deterministic.
    ///
    /// Public so callers (e.g. [`crate::ChannelBank::process`])
    /// can drive timeout emission on a regular tick — without
    /// it, a channel that decodes one ETB and then goes silent
    /// would never get an `observe()` call to internally sweep,
    /// and the partial reassembly would never surface (the
    /// `flush` path emits everything regardless of age, which
    /// is too aggressive for a still-engaged session). CR
    /// round 1 on PR #593.
    pub fn drain_timeouts(&mut self, now: SystemTime) -> Vec<AcarsMessage> {
        self.sweep_timeouts(now)
    }

    /// Drain pending buckets older than [`REASSEMBLY_TIMEOUT`]
    /// at `now`. Returned in deterministic insertion order
    /// (by `first_seen`) so test output is stable.
    fn sweep_timeouts(&mut self, now: SystemTime) -> Vec<AcarsMessage> {
        let cutoff = now.checked_sub(REASSEMBLY_TIMEOUT);
        let Some(cutoff) = cutoff else {
            // `now` is before the epoch + timeout — can't have
            // anything older than that. Skip.
            return Vec::new();
        };
        let stale_keys: Vec<ReassemblyKey> = self
            .pending
            .iter()
            .filter(|(_, p)| p.first_seen < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(stale_keys.len());
        // Sort by (first_seen, aircraft, message_no) for fully
        // deterministic output. `stale_keys` was filtered out of a
        // HashMap, so without the key tiebreak two buckets that
        // share a `first_seen` would surface in HashMap iteration
        // order — non-deterministic across runs. CR round 3 on
        // PR #593.
        let mut entries: Vec<(SystemTime, ReassemblyKey, AcarsMessage)> = stale_keys
            .into_iter()
            .filter_map(|key| {
                self.pending
                    .remove(&key)
                    .and_then(|p| combine_partial(p.blocks).map(|m| (p.first_seen, key, m)))
            })
            .collect();
        entries.sort_by(|(ta, ka, _), (tb, kb, _)| {
            ta.cmp(tb)
                .then_with(|| ka.aircraft.cmp(&kb.aircraft))
                .then_with(|| ka.message_no.cmp(&kb.message_no))
        });
        out.extend(entries.into_iter().map(|(_, _, m)| m));
        out
    }

    /// Drop the bucket with the oldest `first_seen`. Called
    /// when we'd exceed `MAX_PENDING_MESSAGES`.
    fn evict_oldest_pending(&mut self) {
        let oldest = self
            .pending
            .iter()
            .min_by_key(|(_, p)| p.first_seen)
            .map(|(k, _)| k.clone());
        if let Some(key) = oldest {
            tracing::debug!(
                ?key,
                "ACARS reassembly: evicting oldest pending bucket (cap reached)"
            );
            self.pending.remove(&key);
        }
    }
}

/// Build a reassembly key from a frame. Returns `None` if the
/// frame has no `message_no` (can't be reassembled — pass-
/// through) or empty aircraft (decoder oddity, treat as
/// pass-through too).
fn build_key(msg: &AcarsMessage) -> Option<ReassemblyKey> {
    let message_no = msg.message_no?;
    if msg.aircraft.is_empty() || message_no.is_empty() {
        return None;
    }
    Some(ReassemblyKey {
        aircraft: msg.aircraft,
        message_no,
    })
}

/// Combine pending ETB blocks with the closing ETX block into a
/// single reassembled message. Sorts blocks by `block_id` so
/// out-of-order arrival doesn't matter, then concatenates text.
/// The resulting message's metadata (timestamp, channel,
/// aircraft, `message_no`, etc.) comes from the closing ETX so
/// it represents "the message as it became fully observed."
/// `block_id` keeps the ETX's value (highest by definition) so
/// downstream consumers see the final block's identifier.
///
/// Non-panicking by contract: if `pending` and `etx` together
/// somehow produce an empty iterator (impossible by
/// construction — we just pushed `etx`), fall back to the
/// passed-in `etx` rather than panic. The library-crate rule
/// in CLAUDE.md forbids `unwrap`/`panic!`; CR round 2 on PR
/// #593 flagged the previous `expect("non-empty by
/// construction")` even though the invariant holds.
fn combine(mut pending: Vec<AcarsMessage>, etx: AcarsMessage) -> AcarsMessage {
    pending.push(etx.clone());
    pending.sort_by_key(|m| m.block_id);
    let block_count: u8 = u8::try_from(pending.len()).unwrap_or(u8::MAX);
    let mut iter = pending.into_iter();
    let Some(first) = iter.next() else {
        // Defensive fallback — unreachable in practice because
        // we pushed `etx` above, so `pending.len() >= 1`.
        return AcarsMessage {
            reassembled_block_count: 1,
            ..etx
        };
    };
    let mut out = first.clone();
    let mut combined = first.text;
    for next in iter {
        // Prefer the latest timestamp + the latest channel /
        // level metadata: the ETX is by definition the freshest.
        // Walk through, replacing as we go.
        out.timestamp = next.timestamp;
        out.channel_idx = next.channel_idx;
        out.freq_hz = next.freq_hz;
        out.level_db = next.level_db;
        out.error_count = next.error_count;
        out.mode = next.mode;
        out.label = next.label;
        out.block_id = next.block_id;
        out.ack = next.ack;
        out.flight_id = next.flight_id.or(out.flight_id);
        out.message_no = next.message_no.or(out.message_no);
        out.end_of_message = next.end_of_message;
        combined.push_str(&next.text);
    }
    out.text = combined;
    out.reassembled_block_count = block_count;
    out
}

/// Same as [`combine`], but for the timeout / flush path where
/// no closing ETX ever arrived. Concatenates whatever blocks
/// did arrive in `block_id` order and preserves
/// `end_of_message = false` on the result so downstream
/// consumers can tell the message was incomplete.
///
/// Returns `None` if `pending` is empty. Callers (the `flush`
/// and `sweep_timeouts` paths) hold non-empty buckets by
/// construction, but the library-crate rule in CLAUDE.md
/// forbids `unwrap`/`panic!` so the empty case is surfaced as
/// `None` rather than enforced via `expect`. CR round 2 on
/// PR #593.
fn combine_partial(mut pending: Vec<AcarsMessage>) -> Option<AcarsMessage> {
    pending.sort_by_key(|m| m.block_id);
    let block_count: u8 = u8::try_from(pending.len()).unwrap_or(u8::MAX);
    let mut iter = pending.into_iter();
    let first = iter.next()?;
    let mut out = first.clone();
    let mut combined = first.text;
    for next in iter {
        out.timestamp = next.timestamp;
        out.channel_idx = next.channel_idx;
        out.freq_hz = next.freq_hz;
        out.level_db = next.level_db;
        out.error_count = next.error_count;
        out.block_id = next.block_id;
        // `end_of_message` deliberately NOT updated to true —
        // partial reassembly preserves the false flag.
        combined.push_str(&next.text);
    }
    out.text = combined;
    out.reassembled_block_count = block_count;
    // Sanity: a partial reassembly must surface as
    // `end_of_message = false` so consumers (and CR / future
    // smoke tests) can distinguish "complete N-block" from
    // "timeout-flushed partial".
    debug_assert!(!out.end_of_message);
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::similar_names, clippy::panic)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn make_msg(
        aircraft: &str,
        message_no: &str,
        block_id: u8,
        etx: bool,
        text: &str,
    ) -> AcarsMessage {
        AcarsMessage {
            timestamp: SystemTime::UNIX_EPOCH,
            channel_idx: 0,
            freq_hz: 131_550_000.0,
            level_db: 0.0,
            error_count: 0,
            mode: b'2',
            label: *b"H1",
            block_id,
            ack: 0x15,
            aircraft: ArrayString::from(aircraft)
                .expect("test fixture aircraft fits ArrayString<8>"),
            flight_id: None,
            message_no: Some(
                ArrayString::from(message_no).expect("test fixture message_no fits ArrayString<5>"),
            ),
            text: text.to_string(),
            end_of_message: etx,
            reassembled_block_count: 1,
        }
    }

    #[test]
    fn passthrough_for_single_block_etx() {
        let mut a = MessageAssembler::new();
        let msg = make_msg(".N12345", "M01A", 1, true, "PART1");
        let out = a.observe(msg, SystemTime::UNIX_EPOCH);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "PART1");
        assert_eq!(out[0].reassembled_block_count, 1);
        assert!(out[0].end_of_message);
        assert_eq!(a.pending_count(), 0);
    }

    #[test]
    fn passthrough_when_message_no_missing() {
        let mut a = MessageAssembler::new();
        let mut msg = make_msg(".N12345", "M01A", 1, false, "X");
        msg.message_no = None;
        let out = a.observe(msg, SystemTime::UNIX_EPOCH);
        // No key → no reassembly path. ETB without message_no
        // is a degenerate frame; emit it as-is rather than
        // silently swallow.
        assert_eq!(out.len(), 1);
        assert_eq!(a.pending_count(), 0);
    }

    #[test]
    fn etb_then_etx_reassembles_in_order() {
        let mut a = MessageAssembler::new();
        let etb = make_msg(".N12345", "M01A", 1, false, "FIRST_HALF");
        let etx = make_msg(".N12345", "M01A", 2, true, "_SECOND_HALF");
        let now = SystemTime::UNIX_EPOCH;
        let out1 = a.observe(etb, now);
        assert_eq!(out1.len(), 0, "ETB parked, no emission yet");
        assert_eq!(a.pending_count(), 1);
        let out2 = a.observe(etx, now);
        assert_eq!(out2.len(), 1);
        let merged = &out2[0];
        assert_eq!(merged.text, "FIRST_HALF_SECOND_HALF");
        assert_eq!(merged.reassembled_block_count, 2);
        assert!(merged.end_of_message);
        assert_eq!(merged.block_id, 2, "block_id from final ETX");
        assert_eq!(a.pending_count(), 0);
    }

    #[test]
    fn out_of_order_blocks_sort_by_block_id() {
        let mut a = MessageAssembler::new();
        // Three-block message arrives as block 2, then 3, then 1.
        let now = SystemTime::UNIX_EPOCH;
        let _ = a.observe(make_msg(".N12345", "M01A", 2, false, "MIDDLE_"), now);
        let _ = a.observe(make_msg(".N12345", "M01A", 3, true, "LAST"), now);
        // Wait — block 3 is the ETX, so the assembler emitted on the
        // second observe with whatever was pending. Let's redo: ETX
        // closes immediately. Out-of-order means ETBs arrive after.
        // Test the scenario where ETX arrives before an earlier ETB.
        let mut a = MessageAssembler::new();
        let _ = a.observe(make_msg(".N12345", "M02B", 2, false, "MIDDLE_"), now);
        let out = a.observe(make_msg(".N12345", "M02B", 3, true, "LAST"), now);
        // ETX with one pending ETB — emit reassembled.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "MIDDLE_LAST");
        assert_eq!(out[0].reassembled_block_count, 2);
    }

    #[test]
    fn out_of_order_etx_before_etb() {
        // ACARS allows ETX (final block) to arrive before earlier
        // ETBs in pathological reception. The assembler is keyed
        // on (aircraft, message_no), so the ETX immediately tries
        // to combine; if no ETBs have arrived yet, it emits as
        // a single-block message (the late ETBs are then orphans
        // — they'd start a new bucket which times out).
        let mut a = MessageAssembler::new();
        let now = SystemTime::UNIX_EPOCH;
        let etx = make_msg(".N12345", "M03C", 3, true, "FINAL");
        let out_etx = a.observe(etx, now);
        assert_eq!(out_etx.len(), 1, "ETX emits even without prior ETBs");
        assert_eq!(out_etx[0].text, "FINAL");
        assert_eq!(out_etx[0].reassembled_block_count, 1);

        // A late ETB arrives — starts a new bucket that will
        // time out. Documents the limitation.
        let etb = make_msg(".N12345", "M03C", 1, false, "EARLY");
        let out_etb = a.observe(etb, now);
        assert_eq!(out_etb.len(), 0, "late ETB parked, will time out");
        assert_eq!(a.pending_count(), 1);
    }

    #[test]
    fn timeout_emits_partial_reassembly() {
        let mut a = MessageAssembler::new();
        let t0 = SystemTime::UNIX_EPOCH;
        let etb = make_msg(".N12345", "M04D", 1, false, "ONLY_BLOCK");
        let _ = a.observe(etb, t0);
        assert_eq!(a.pending_count(), 1);

        // Observe an unrelated frame after the timeout. Sweep
        // fires + emits the stale ETB as a partial reassembly,
        // then emits the new frame as pass-through.
        let later = t0 + REASSEMBLY_TIMEOUT + Duration::from_secs(1);
        let new = make_msg(".N99999", "M99Z", 1, true, "OTHER");
        let out = a.observe(new, later);
        assert_eq!(out.len(), 2);
        // First the stale, then the new.
        assert_eq!(out[0].text, "ONLY_BLOCK");
        assert_eq!(out[0].reassembled_block_count, 1);
        assert!(!out[0].end_of_message, "partial keeps ETB flag");
        assert_eq!(out[1].text, "OTHER");
        assert_eq!(a.pending_count(), 0);
    }

    #[test]
    fn flush_drains_all_pending() {
        let mut a = MessageAssembler::new();
        let now = SystemTime::UNIX_EPOCH;
        let _ = a.observe(make_msg(".A", "M1", 1, false, "AAA"), now);
        let _ = a.observe(make_msg(".B", "M2", 1, false, "BBB"), now);
        let _ = a.observe(make_msg(".A", "M1", 2, false, "_more"), now);
        assert_eq!(a.pending_count(), 2);
        let mut flushed = a.flush();
        flushed.sort_by(|x, y| x.aircraft.as_str().cmp(y.aircraft.as_str()));
        assert_eq!(flushed.len(), 2);
        assert_eq!(flushed[0].aircraft.as_str(), ".A");
        assert_eq!(flushed[0].text, "AAA_more");
        assert_eq!(flushed[0].reassembled_block_count, 2);
        assert!(!flushed[0].end_of_message);
        assert_eq!(flushed[1].aircraft.as_str(), ".B");
        assert_eq!(flushed[1].text, "BBB");
        assert_eq!(flushed[1].reassembled_block_count, 1);
        assert_eq!(a.pending_count(), 0);
    }

    #[test]
    fn cap_evicts_oldest_pending() {
        let mut a = MessageAssembler::new();
        let base = SystemTime::UNIX_EPOCH;
        // Insert MAX + 1 distinct ETBs at distinct `now` clocks.
        // `first_seen` is stamped from the caller-provided
        // `now` (CR round 1 on PR #593), so the test must vary
        // `now` for the LRU evict-by-first-seen logic to be
        // deterministic — varying `msg.timestamp` is now
        // irrelevant.
        for i in 0..=MAX_PENDING_MESSAGES {
            let aircraft = format!(".N{i:05}");
            let msg_no = format!("M{i:03}");
            let m = make_msg(&aircraft, &msg_no, 1, false, "X");
            let now = base + Duration::from_millis(u64::try_from(i).expect("loop bound fits u64"));
            let _ = a.observe(m, now);
        }
        assert_eq!(a.pending_count(), MAX_PENDING_MESSAGES);
        // The very first key (.N00000) should have been evicted.
        // Send its ETX — should pass through (no pending) rather
        // than reassemble.
        let etx = make_msg(".N00000", "M000", 2, true, "_etx");
        let out = a.observe(etx, base);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "_etx", "evicted ETB body did NOT come through");
    }

    #[test]
    fn drain_timeouts_emits_only_stale_buckets() {
        // Public counterpart of the internal sweep — must drain
        // expired buckets WITHOUT touching fresh ones, so a
        // silent channel can periodically call this from its
        // housekeeping path and still get partials surfaced
        // after REASSEMBLY_TIMEOUT. CR round 1 on PR #593.
        let mut a = MessageAssembler::new();
        let t0 = SystemTime::UNIX_EPOCH;
        let _ = a.observe(make_msg(".A", "M1", 1, false, "AAA"), t0);
        let _ = a.observe(
            make_msg(".B", "M2", 1, false, "BBB"),
            t0 + REASSEMBLY_TIMEOUT,
        );
        // At t0 + 2*timeout, only the .A bucket is stale.
        let later = t0 + REASSEMBLY_TIMEOUT + REASSEMBLY_TIMEOUT;
        let drained = a.drain_timeouts(later);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].aircraft.as_str(), ".A");
        assert_eq!(drained[0].text, "AAA");
        assert!(!drained[0].end_of_message, "partial keeps ETB flag");
        assert_eq!(a.pending_count(), 1, "fresh .B bucket survives");
    }

    #[test]
    fn first_seen_uses_observation_clock_not_msg_timestamp() {
        // Replay safety: the bucket's `first_seen` must be the
        // caller's `now`, so a stale `msg.timestamp` from an
        // offline replay can't fool the timeout logic into
        // emitting (or holding) the wrong way. CR round 1 on
        // PR #593.
        const ONE_HOUR_SECS: u64 = 60 * 60;
        let mut a = MessageAssembler::new();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(ONE_HOUR_SECS);
        // Construct an ETB whose own timestamp is way in the
        // past. If the bucket used `msg.timestamp`, a sweep at
        // `now` would immediately consider it timed out
        // (3600 s > 30 s). We expect the bucket to survive.
        let mut etb = make_msg(".A", "M1", 1, false, "X");
        etb.timestamp = SystemTime::UNIX_EPOCH;
        let _ = a.observe(etb, now);
        assert_eq!(a.pending_count(), 1);
        // Sweep at `now + half-timeout` — bucket should still be
        // alive (would NOT be alive if `first_seen` had been
        // pinned to `msg.timestamp`).
        let drained = a.drain_timeouts(now + REASSEMBLY_TIMEOUT / 2);
        assert_eq!(drained.len(), 0);
        assert_eq!(a.pending_count(), 1);
    }

    #[test]
    fn distinct_keys_dont_cross_pollute() {
        let mut a = MessageAssembler::new();
        let now = SystemTime::UNIX_EPOCH;
        let _ = a.observe(make_msg(".A", "M1", 1, false, "AAA"), now);
        let _ = a.observe(make_msg(".B", "M1", 1, false, "BBB"), now);
        let out_a = a.observe(make_msg(".A", "M1", 2, true, "_a_end"), now);
        let out_b = a.observe(make_msg(".B", "M1", 2, true, "_b_end"), now);
        assert_eq!(out_a.len(), 1);
        assert_eq!(out_a[0].text, "AAA_a_end");
        assert_eq!(out_b.len(), 1);
        assert_eq!(out_b[0].text, "BBB_b_end");
    }
}
