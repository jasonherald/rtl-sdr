//! End-to-end FEC chain — soft i8 symbol pairs in, decoded VCDU
//! bytes out.
//!
//! Stitches the per-stage primitives ([`SoftSyncDetector`],
//! [`ViterbiDecoder`], [`SyncCorrelator`], [`Derandomizer`],
//! [`ReedSolomon`]) into a single streaming state machine
//! matching medet's `try_frame` flow:
//!
//! ```text
//!   soft i8 pair ─▶ SoftSyncDetector (8 rotated patterns)
//!                            │
//!                            ▼ rotation locked
//!                   Rotation::apply (un-rotate to canonical)
//!                            │
//!                            ▼
//!                         Viterbi ─bit▶ SyncCorrelator (per-CADU re-sync)
//!                                                  │
//!                                                  ▼
//!                                       1020-byte CADU buffer
//!                                                  │
//!                                                  ▼
//!                          derandomize  → de-interleave (×4)
//!                                                  │
//!                                                  ▼
//!                       RS-decode each codeword (255 → 223 bytes)
//!                                                  │
//!                                                  ▼
//!                          re-interleave → 892-byte VCDU
//! ```
//!
//! [`SoftSyncDetector`] resolves the QPSK 4-fold phase ambiguity
//! (plus optional I/Q axis swap) by pre-Viterbi soft correlation
//! against 8 rotated forms of the encoded ASM. Without it,
//! ~75 % of Costas acquisitions silently drop the entire pass
//! (issue #605). Once locked, every subsequent soft pair is
//! un-rotated by [`Rotation::apply`] before reaching Viterbi,
//! so Viterbi always sees the canonical orientation.
//!
//! Layered like this so [`LrptPipeline::push_symbol`] can be a
//! thin wrapper that drives the chain and feeds emitted VCDUs
//! into the existing demux + image stages, while the chain
//! itself stays unit-testable on synthetic byte streams.

use crate::fec::{
    Derandomizer, DiffDecoder, ReedSolomon, Rotation, SoftSyncDetector, SyncCorrelator,
    ViterbiDecoder,
};

/// Bytes captured per CADU after the ASM. Per CCSDS §10:
/// 1024-byte CADU = 4-byte ASM + 1020-byte payload. The ASM is
/// already consumed by the [`SyncCorrelator`] so the payload
/// length is what we capture.
const CADU_PAYLOAD_LEN: usize = 1020;

/// CCSDS RS interleaving depth for Meteor LRPT. Each 1020-byte
/// CADU payload is 4 byte-byte-interleaved Reed-Solomon
/// codewords. Per medet's `ecc_deinterleave(... n=4)`.
const RS_INTERLEAVE: usize = 4;

/// Reed-Solomon codeword length (bytes). 4 × 255 = 1020 = full
/// CADU payload.
const RS_CODEWORD_LEN: usize = 255;

/// Reed-Solomon message length (bytes). 4 × 223 = 892 = VCDU
/// length.
const RS_MESSAGE_LEN: usize = 223;

/// Output VCDU length in bytes — the value the
/// [`super::super::ccsds::Demux`] expects per VCDU.
const VCDU_LEN: usize = RS_INTERLEAVE * RS_MESSAGE_LEN; // 892

/// Per-symbol chain state. Two-phase lock:
///
/// 1. [`State::HuntingRotation`]: feeding raw soft pairs to the
///    pre-Viterbi [`SoftSyncDetector`] until one of 8 rotated
///    ASM patterns matches. On match we know which orientation
///    Costas locked at.
/// 2. [`State::Locked`]: every subsequent soft pair is un-rotated
///    by [`Rotation::apply`] before reaching Viterbi. The
///    post-Viterbi [`SyncCorrelator`] then runs per-CADU sync
///    on the decoded bit stream — same logic as the original
///    chain, but now operating on a known-canonical bit stream
///    that actually produces matches.
///
/// Once Locked, we stay Locked for the whole pass. Costas can
/// drift through quarter-turns during a low-SNR pass, but
/// re-detecting per-CADU would only re-lock at the same
/// (correct) rotation in the steady state. If we ever observe
/// pathological mid-pass rotation drift, a "if last K CADUs all
/// failed RS, re-hunt rotation" fallback can be added later.
#[derive(Debug)]
enum State {
    /// No rotation lock yet. Soft pairs feed
    /// [`SoftSyncDetector`] only — Viterbi is not stepped, so it
    /// stays in its initial state ready for fresh warmup once
    /// rotation locks.
    HuntingRotation,
    /// Rotation locked. Apply [`Rotation::apply`] to every soft
    /// pair, push through Viterbi, run per-CADU sync on the
    /// emitted bits. `cadu` holds the in-flight CADU capture
    /// state (same fields as the original `Capturing` variant);
    /// `is_capturing` distinguishes "looking for next CADU's ASM
    /// in the bit stream" from "actively capturing CADU bytes".
    /// `inverted` is `true` when the per-CADU [`SyncCorrelator`]
    /// matched the bitwise-inverted ASM rather than the upright
    /// form, signalling a 180° residual after rotation lock —
    /// captured bytes get `XOR`ed with `0xFF` before derand to
    /// undo the inversion.
    Locked {
        rotation: Rotation,
        is_capturing: bool,
        inverted: bool,
        bytes: Vec<u8>,
        partial: u8,
        partial_count: u8,
    },
}

/// Streaming FEC chain — push one soft i8 symbol pair per call,
/// receive a decoded VCDU when one becomes available.
pub struct FecChain {
    /// Differential pre-decoder, present only for differentially
    /// precoded downlinks (legacy Meteor-M2). Applied to the raw
    /// soft pair before sync/Viterbi, per dbdexter `decode.c`.
    diff: Option<DiffDecoder>,
    detector: SoftSyncDetector,
    viterbi: ViterbiDecoder,
    sync: SyncCorrelator,
    derand: Derandomizer,
    rs: ReedSolomon,
    state: State,
    /// Decode statistics for diagnostics / status reporting.
    stats: FecStats,
}

/// Running decode statistics for a [`FecChain`]. Cheap counters
/// surfaced for diagnostics and for the sync-robustness work
/// (re-acquisition tuning).
#[derive(Debug, Clone, Copy, Default)]
pub struct FecStats {
    /// Number of times the chain acquired rotation lock (entered
    /// [`State::Locked`] from [`State::HuntingRotation`]).
    pub rotation_locks: u64,
    /// CADUs that decoded successfully (all 4 RS codewords ok).
    pub cadus_decoded: u64,
    /// CADUs whose RS decode failed (dropped).
    pub cadus_failed: u64,
}

impl Default for FecChain {
    fn default() -> Self {
        Self::new()
    }
}

impl FecChain {
    /// New chain for a non-differential downlink (current
    /// Meteor-M2-3 / M2-4 — concatenated coding, no differential
    /// precoding).
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_differential(false)
    }

    /// New chain, choosing whether the downlink is differentially
    /// precoded. Legacy Meteor-M2 (NORAD 40069) uses `true`; the
    /// current birds use `false`.
    #[must_use]
    pub fn new_with_differential(differential: bool) -> Self {
        Self {
            diff: differential.then(DiffDecoder::new),
            detector: SoftSyncDetector::new(),
            viterbi: ViterbiDecoder::new(),
            sync: SyncCorrelator::new(),
            derand: Derandomizer::new(),
            rs: ReedSolomon::new(),
            state: State::HuntingRotation,
            stats: FecStats::default(),
        }
    }

    /// Snapshot the running decode statistics.
    #[must_use]
    pub fn stats(&self) -> FecStats {
        self.stats
    }

    /// Push one soft i8 symbol pair (one Viterbi-encoded bit's
    /// worth from the demod). Returns `Some(VCDU bytes)` on the
    /// call that completes a successful CADU decode; otherwise
    /// `None`. Failed RS decodes are silently dropped — the
    /// chain returns to hunting for the next ASM (without losing
    /// rotation lock).
    pub fn push_symbol(&mut self, soft: [i8; 2]) -> Option<Vec<u8>> {
        // Differential pre-decode on the raw soft stream, before
        // sync correlation and Viterbi (dbdexter `decode.c`: the
        // `diff_decode` call precedes `correlate`). No-op for
        // non-differential downlinks.
        let soft = match &mut self.diff {
            Some(diff) => diff.decode_pair(soft),
            None => soft,
        };
        match &self.state {
            State::HuntingRotation => {
                if let Some(rotation) = self.detector.push_symbol(soft) {
                    // Rotation acquired — transition to Locked.
                    //
                    // **Critical**: the ASM-containing soft samples
                    // were just consumed by `SoftSyncDetector` and
                    // never reached Viterbi. If we don't replay
                    // them, the post-Viterbi bit stream starts
                    // with CADU payload bits, the per-CADU
                    // `SyncCorrelator` never sees the ASM, and we
                    // miss the entire first CADU. Drain the
                    // detector's window and step Viterbi on the
                    // un-rotated samples so the ASM is properly
                    // queued for emission once Viterbi's
                    // TRACEBACK_DEPTH-symbol warmup completes.
                    let window = self.detector.drain_window();
                    self.stats.rotation_locks += 1;
                    self.state = State::Locked {
                        rotation,
                        is_capturing: false,
                        inverted: false,
                        bytes: Vec::with_capacity(CADU_PAYLOAD_LEN),
                        partial: 0,
                        partial_count: 0,
                    };
                    let mut emitted: Option<Vec<u8>> = None;
                    for pair_chunk in window.as_chunks::<2>().0 {
                        let pair = [pair_chunk[0], pair_chunk[1]];
                        let rotated = rotation.apply(pair);
                        if let Some(bit) = self.viterbi.step(rotated) {
                            // Replay during a 32-symbol drain
                            // can't possibly emit a bit (Viterbi
                            // needs TRACEBACK_DEPTH=224 symbols),
                            // but defensively route any bit
                            // through `process_bit` so a future
                            // Viterbi tweak doesn't regress.
                            if let Some(vcdu) = self.process_bit(bit) {
                                // Multiple CADUs in one push are
                                // impossible at our chunk size,
                                // but if it ever happened we'd
                                // drop the second one — flag it.
                                debug_assert!(emitted.is_none(), "drain emitted multiple VCDUs");
                                emitted = Some(vcdu);
                            }
                        }
                    }
                    return emitted;
                }
                None
            }
            State::Locked { rotation, .. } => {
                let rotated = rotation.apply(soft);
                let bit = self.viterbi.step(rotated)?;
                self.process_bit(bit)
            }
        }
    }

    /// Reset the entire chain to a fresh state. Called between
    /// passes. Per-stage internals (Viterbi traceback, sync
    /// window, derand position, rotation detector) all flush;
    /// in-flight CADU capture is dropped.
    pub fn reset(&mut self) {
        if let Some(diff) = &mut self.diff {
            diff.reset();
        }
        self.detector.reset();
        self.viterbi = ViterbiDecoder::new();
        self.sync = SyncCorrelator::new();
        self.derand.reset();
        self.state = State::HuntingRotation;
        self.stats = FecStats::default();
    }

    /// Currently-locked rotation, or `None` while the chain is
    /// still hunting. Exposed for diagnostics / future status-bar
    /// readouts; the FEC chain itself routes the rotation
    /// internally.
    #[must_use]
    pub fn locked_rotation(&self) -> Option<Rotation> {
        match self.state {
            State::Locked { rotation, .. } => Some(rotation),
            State::HuntingRotation => None,
        }
    }

    fn process_bit(&mut self, bit: u8) -> Option<Vec<u8>> {
        let State::Locked {
            is_capturing,
            inverted,
            bytes,
            partial,
            partial_count,
            ..
        } = &mut self.state
        else {
            // Unreachable: process_bit is only called from the
            // Locked arm of push_symbol. Match-irrefutable would
            // require pulling the fields apart further; leave
            // the early-return as a defensive guard.
            return None;
        };
        if !*is_capturing {
            // Per-CADU ASM hunt on the post-Viterbi bit stream.
            // This re-syncs every CADU regardless of bit-level
            // jitter; rotation is already locked. The hit's
            // `inverted` flag tells us whether the rotation
            // detector picked the right of two 180°-symmetric
            // patterns — if not, captured payload bytes get
            // XORed with 0xFF below.
            if let Some(hit) = self.sync.push(bit) {
                *is_capturing = true;
                *inverted = hit.inverted;
            }
            return None;
        }
        // Capturing CADU payload: 8 bits → 1 byte, accumulate
        // until CADU_PAYLOAD_LEN bytes are in hand.
        *partial = (*partial << 1) | (bit & 1);
        *partial_count += 1;
        if *partial_count == 8 {
            // If the pre-Viterbi rotation lock was 180° off,
            // every emitted byte is the bit-inverted form of
            // what derand expects. Flip them at capture time so
            // derand → RS see the canonical bytes. Per medet's
            // `try_frame` residual-flip safety net.
            let byte = if *inverted { *partial ^ 0xFF } else { *partial };
            bytes.push(byte);
            *partial = 0;
            *partial_count = 0;
        }
        if bytes.len() == CADU_PAYLOAD_LEN {
            // `mem::replace` (vs `mem::take`) preserves the
            // pre-allocated capacity for the next CADU's bytes
            // buffer — `mem::take` would leave `bytes` as a
            // zero-capacity Vec and force a fresh allocation
            // on every CADU in the locked steady-state path.
            // Per CR round 1 on PR #606.
            let cadu = std::mem::replace(bytes, Vec::with_capacity(CADU_PAYLOAD_LEN));
            // Reset to "hunting for the next ASM in the same
            // rotation" — keep rotation lock, fresh sync state,
            // clear the inversion flag (next ASM hunt re-decides).
            *is_capturing = false;
            *inverted = false;
            *partial = 0;
            *partial_count = 0;
            self.sync = SyncCorrelator::new();
            return self.decode_cadu(cadu);
        }
        None
    }

    /// Decode one captured CADU payload: derandomize, de-interleave
    /// the 4 Reed-Solomon codewords, decode each, re-interleave the
    /// corrected message portions into a 892-byte VCDU. Returns
    /// `None` if any of the 4 codewords fails to decode (matches
    /// medet's all-or-nothing acceptance per `try_frame`).
    fn decode_cadu(&mut self, mut cadu: Vec<u8>) -> Option<Vec<u8>> {
        debug_assert_eq!(cadu.len(), CADU_PAYLOAD_LEN);
        // Step 1: derandomize. PN sequence restarts at every
        // CADU boundary per spec; reset before consuming.
        self.derand.reset();
        for byte in &mut cadu {
            *byte = self.derand.process(*byte);
        }
        // Step 2 + 3 + 4: per RS interleave column, extract one
        // codeword, decode, write the corrected message bytes
        // back into the VCDU at the same interleave column.
        let mut vcdu = vec![0_u8; VCDU_LEN];
        for col in 0..RS_INTERLEAVE {
            let mut codeword = [0_u8; RS_CODEWORD_LEN];
            for i in 0..RS_CODEWORD_LEN {
                codeword[i] = cadu[i * RS_INTERLEAVE + col];
            }
            let Ok((corrected, _errors)) = self.rs.decode(&codeword) else {
                self.stats.cadus_failed += 1;
                return None;
            };
            for i in 0..RS_MESSAGE_LEN {
                vcdu[i * RS_INTERLEAVE + col] = corrected[i];
            }
        }
        self.stats.cadus_decoded += 1;
        Some(vcdu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Total bits in a CADU including the 32-bit ASM. Used by
    /// the tests below to size the synthetic bit streams.
    const CADU_TOTAL_BITS: usize = 32 + CADU_PAYLOAD_LEN * 8;

    #[test]
    fn fec_chain_constructible_and_resets() {
        let mut c = FecChain::new();
        assert!(matches!(c.state, State::HuntingRotation));
        assert_eq!(c.locked_rotation(), None);
        c.reset();
        assert!(matches!(c.state, State::HuntingRotation));
        assert_eq!(c.locked_rotation(), None);
    }

    #[test]
    fn fec_chain_returns_none_during_warmup() {
        // Until rotation locks (which requires a clean ASM in the
        // soft stream), push_symbol returns None on every call.
        let mut c = FecChain::new();
        for _ in 0..10 {
            let result = c.push_symbol([0, 0]);
            assert!(result.is_none());
        }
    }

    #[test]
    fn decode_cadu_returns_none_on_invalid_codeword() {
        // Build a VALID RS-encoded, derandomised CADU (so the
        // chain's derand recovers clean codewords), then corrupt
        // one interleave column far beyond the RS correction
        // capacity (t = 16). That column can't decode, so the
        // all-or-nothing `decode_cadu` must return None.
        //
        // Note: we deliberately do NOT rely on "all-zeros → derand
        // → PN is not a codeword" — that was an accident of an
        // earlier corrupted PN table. This construction guarantees
        // an uncorrectable column for any (correct) PN sequence.
        let rs = ReedSolomon::new();
        let mut vcdu = vec![0_u8; VCDU_LEN];
        for (i, b) in vcdu.iter_mut().enumerate() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "modulo 256 fits in u8 by definition"
            )]
            let byte = ((i * 7 + 11) % 256) as u8;
            *b = byte;
        }
        let mut cadu = vec![0_u8; CADU_PAYLOAD_LEN];
        for col in 0..RS_INTERLEAVE {
            let mut message = [0_u8; RS_MESSAGE_LEN];
            for i in 0..RS_MESSAGE_LEN {
                message[i] = vcdu[i * RS_INTERLEAVE + col];
            }
            let codeword = rs.encode(&message);
            for i in 0..RS_CODEWORD_LEN {
                cadu[i * RS_INTERLEAVE + col] = codeword[i];
            }
        }
        // Corrupt column 0 with the specific T+1 = 17-error
        // pattern that `reed_solomon::tests::rejects_this_t_plus_
        // one_pattern` pins as detectably uncorrectable (positions
        // 0, 11, 22, … XOR 0x3C). RS beyond-T detection is NOT
        // guaranteed for arbitrary patterns — far-over-capacity
        // inputs can miscorrect to a different valid codeword and
        // return Ok — so we reuse the known-caught pattern here.
        // XOR is linear and survives the chain's later derand, so
        // post-derand column 0 carries exactly these 17 errors.
        for k in 0..=16 {
            cadu[(k * 11) * RS_INTERLEAVE] ^= 0x3C;
        }
        // Apply derand so the chain's derand recovers our (corrupted)
        // codewords.
        let mut derand = Derandomizer::new();
        derand.reset();
        for byte in &mut cadu {
            *byte = derand.process(*byte);
        }
        let mut c = FecChain::new();
        assert!(
            c.decode_cadu(cadu).is_none(),
            "a column with 255 byte-errors must exceed RS capacity and yield None",
        );
    }

    #[test]
    fn decode_cadu_round_trips_clean_rs_encoded_data() {
        // Build a synthetic CADU from scratch:
        //   1. Pick 892 bytes of "VCDU" content
        //   2. De-interleave (split into 4 × 223-byte messages)
        //   3. RS-encode each message → 4 × 255-byte codewords
        //   4. Interleave into a 1020-byte CADU payload
        //   5. Apply derand (XOR with PN) so when the chain's
        //      derand undoes it, we recover the encoded form
        //
        // Then push that synthetic CADU through `decode_cadu`
        // and assert we recover the original 892-byte VCDU.
        let rs = ReedSolomon::new();
        // Distinct, non-uniform VCDU content so any byte-order
        // bug surfaces visibly.
        let mut original_vcdu = vec![0_u8; VCDU_LEN];
        for (i, b) in original_vcdu.iter_mut().enumerate() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "modulo 256 fits in u8 by definition"
            )]
            let byte = ((i * 7 + 11) % 256) as u8;
            *b = byte;
        }
        // De-interleave VCDU into 4 message buffers.
        let mut messages = [[0_u8; RS_MESSAGE_LEN]; RS_INTERLEAVE];
        for col in 0..RS_INTERLEAVE {
            for i in 0..RS_MESSAGE_LEN {
                messages[col][i] = original_vcdu[i * RS_INTERLEAVE + col];
            }
        }
        // RS-encode each message.
        let codewords: Vec<[u8; RS_CODEWORD_LEN]> = messages.iter().map(|m| rs.encode(m)).collect();
        // Interleave back into 1020-byte CADU payload.
        let mut cadu_payload = vec![0_u8; CADU_PAYLOAD_LEN];
        for col in 0..RS_INTERLEAVE {
            for i in 0..RS_CODEWORD_LEN {
                cadu_payload[i * RS_INTERLEAVE + col] = codewords[col][i];
            }
        }
        // Apply derand (XOR with PN) so the chain's derand
        // (which XORs again with PN) recovers `cadu_payload`.
        let mut derand = Derandomizer::new();
        derand.reset();
        for byte in &mut cadu_payload {
            *byte = derand.process(*byte);
        }
        // Now feed into the chain's decode path and confirm we
        // get the original VCDU back.
        let mut c = FecChain::new();
        let recovered = c
            .decode_cadu(cadu_payload)
            .expect("clean RS-encoded CADU must decode");
        assert_eq!(recovered, original_vcdu);
    }

    #[test]
    fn cadu_total_bits_matches_protocol_constant() {
        // 1024-byte CADU = 32-bit ASM + 1020 bytes payload =
        // 32 + 8160 = 8192 bits. Pin so a future constant tweak
        // that breaks the protocol layout fails loudly.
        assert_eq!(CADU_TOTAL_BITS, 8192);
    }

    /// Forward QPSK rotation transform that builds pattern N
    /// in [`super::super::soft_sync::build_patterns`]. Test-only
    /// — the production path only ever applies the *inverse*
    /// (`Rotation::ALL[N].apply`) to received samples.
    ///
    /// Pinned in `chain.rs` so the round-trip test below
    /// catches any `soft_sync.rs` refactor that changes the
    /// forward table without simultaneously updating `apply()`.
    fn forward_rotation(idx: usize, p: [i8; 2]) -> [i8; 2] {
        let neg = |x: i8| x.saturating_neg();
        let [i, q] = p;
        match idx {
            0 => [i, q],
            1 => [q, neg(i)],
            2 => [neg(i), neg(q)],
            3 => [neg(q), i],
            4 => [q, i],
            5 => [i, neg(q)],
            6 => [neg(q), neg(i)],
            7 => [neg(i), q],
            _ => unreachable!(),
        }
    }

    /// Build a clean encoded soft stream for one full CADU
    /// (ASM + RS-encoded + derandomised payload that decodes
    /// back to `original_vcdu`), padded with enough trailing
    /// zero input bits to flush Viterbi's traceback so the
    /// chain emits the VCDU before the soft buffer runs out.
    fn synthesise_cadu_soft(original_vcdu: &[u8]) -> Vec<i8> {
        // RS-encode: de-interleave VCDU → 4 messages, encode
        // each, re-interleave into 1020-byte CADU payload.
        let rs = ReedSolomon::new();
        let mut messages = [[0_u8; RS_MESSAGE_LEN]; RS_INTERLEAVE];
        for col in 0..RS_INTERLEAVE {
            for i in 0..RS_MESSAGE_LEN {
                messages[col][i] = original_vcdu[i * RS_INTERLEAVE + col];
            }
        }
        let codewords: Vec<[u8; RS_CODEWORD_LEN]> = messages.iter().map(|m| rs.encode(m)).collect();
        let mut cadu_payload = vec![0_u8; CADU_PAYLOAD_LEN];
        for col in 0..RS_INTERLEAVE {
            for i in 0..RS_CODEWORD_LEN {
                cadu_payload[i * RS_INTERLEAVE + col] = codewords[col][i];
            }
        }
        // Apply derand so the chain's derand re-XOR recovers the
        // RS-encoded form.
        let mut derand = Derandomizer::new();
        derand.reset();
        for byte in &mut cadu_payload {
            *byte = derand.process(*byte);
        }
        // Build the ASM + payload bitstream (MSB first).
        let mut bits: Vec<u8> = Vec::with_capacity(32 + CADU_PAYLOAD_LEN * 8);
        for i in 0..32 {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "ASM is u32, shift index 0..32 is safe"
            )]
            let bit = ((crate::fec::ASM >> (31 - i)) & 1) as u8;
            bits.push(bit);
        }
        for &byte in &cadu_payload {
            for j in 0..8 {
                bits.push((byte >> (7 - j)) & 1);
            }
        }
        // Trailing zero bits: enough to push every CADU bit out
        // of Viterbi's TRACEBACK_DEPTH window. 32 ASM + 8160
        // payload + slack = 8500 input bits for a comfortable
        // margin (Viterbi's traceback is 224 symbols).
        bits.extend(std::iter::repeat_n(0_u8, 500));
        crate::fec::viterbi::ccsds_encode(&bits)
    }

    /// **Gold-standard test for issue #605.** Build a clean
    /// CADU, convolutionally encode it, apply each of 8 forward
    /// rotation transforms (one per QPSK phase + I/Q-swap
    /// orientation Costas can lock at), push through
    /// `FecChain`, and assert the chain recovers the original
    /// VCDU at every rotation. Before the `SoftSyncDetector`
    /// fix, this test would have failed for 7 of 8 rotations —
    /// the chain only decoded at the upright phase.
    #[test]
    fn fec_chain_decodes_through_each_of_eight_rotations() {
        // Distinct, non-uniform VCDU content so any byte-order
        // bug surfaces visibly.
        let mut original_vcdu = vec![0_u8; VCDU_LEN];
        for (i, b) in original_vcdu.iter_mut().enumerate() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "modulo 256 fits in u8 by definition"
            )]
            let byte = ((i * 7 + 11) % 256) as u8;
            *b = byte;
        }
        let soft = synthesise_cadu_soft(&original_vcdu);
        for (idx, rot) in Rotation::ALL.iter().enumerate() {
            let mut chain = FecChain::new();
            let mut decoded: Option<Vec<u8>> = None;
            for pair_chunk in soft.as_chunks::<2>().0 {
                let pair = [pair_chunk[0], pair_chunk[1]];
                let rotated = forward_rotation(idx, pair);
                if let Some(vcdu) = chain.push_symbol(rotated)
                    && decoded.is_none()
                {
                    decoded = Some(vcdu);
                }
            }
            assert_eq!(
                chain.locked_rotation(),
                Some(*rot),
                "rotation {idx} ({rot:?}): chain should report the matching rotation",
            );
            assert_eq!(
                decoded.as_ref(),
                Some(&original_vcdu),
                "rotation {idx} ({rot:?}): chain failed to decode VCDU",
            );
        }
    }
}
