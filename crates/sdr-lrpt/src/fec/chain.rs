//! End-to-end FEC chain — soft i8 symbol pairs in, decoded VCDU
//! bytes out.
//!
//! Stitches the per-stage primitives ([`ViterbiDecoder`],
//! [`SyncCorrelator`], [`Derandomizer`], [`ReedSolomon`]) into a
//! single streaming state machine matching medet's `try_frame`
//! flow:
//!
//! ```text
//!   soft i8 pair ─▶ Viterbi ─bit▶ Sync correlator ─bit▶ {hunting | capturing}
//!                                                                   │
//!                          ASM hit                                  │
//!                                                                   ▼
//!                                                       1020-byte CADU buffer
//!                                                                   │
//!                                                                   ▼
//!                                          derandomize  → de-interleave (×4)
//!                                                                   │
//!                                                                   ▼
//!                                       RS-decode each codeword (255 → 223 bytes)
//!                                                                   │
//!                                                                   ▼
//!                                          re-interleave → 892-byte VCDU
//! ```
//!
//! Layered like this so [`LrptPipeline::push_symbol`] can be a
//! thin wrapper that drives the chain and feeds emitted VCDUs
//! into the existing demux + image stages, while the chain
//! itself stays unit-testable on synthetic byte streams.

use crate::fec::{Derandomizer, ReedSolomon, SyncCorrelator, ViterbiDecoder};

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

/// Per-bit chain state. Hunting until ASM, then capturing 1020
/// bytes worth of CADU payload.
#[derive(Debug)]
enum State {
    /// Looking for the ASM in the post-Viterbi bit stream.
    Hunting,
    /// ASM matched; capturing the next [`CADU_PAYLOAD_LEN`]
    /// bytes of payload, packing 8 bits at a time. `partial` /
    /// `partial_count` accumulate the in-flight byte; `bytes`
    /// holds the completed bytes.
    Capturing {
        bytes: Vec<u8>,
        partial: u8,
        partial_count: u8,
    },
}

/// Streaming FEC chain — push one soft i8 symbol pair per call,
/// receive a decoded VCDU when one becomes available.
pub struct FecChain {
    viterbi: ViterbiDecoder,
    sync: SyncCorrelator,
    derand: Derandomizer,
    rs: ReedSolomon,
    state: State,
}

impl Default for FecChain {
    fn default() -> Self {
        Self::new()
    }
}

impl FecChain {
    #[must_use]
    pub fn new() -> Self {
        Self {
            viterbi: ViterbiDecoder::new(),
            sync: SyncCorrelator::new(),
            derand: Derandomizer::new(),
            rs: ReedSolomon::new(),
            state: State::Hunting,
        }
    }

    /// Push one soft i8 symbol pair (one Viterbi-encoded bit's
    /// worth from the demod). Returns `Some(VCDU bytes)` on the
    /// call that completes a successful CADU decode; otherwise
    /// `None`. Failed RS decodes are silently dropped — the
    /// chain returns to hunting for the next ASM.
    pub fn push_symbol(&mut self, soft: [i8; 2]) -> Option<Vec<u8>> {
        let bit = self.viterbi.step(soft)?;
        self.process_bit(bit)
    }

    /// Reset the entire chain to a fresh state. Called between
    /// passes. Per-stage internals (Viterbi traceback, sync
    /// window, derand position) all flush; in-flight CADU
    /// capture is dropped.
    pub fn reset(&mut self) {
        self.viterbi = ViterbiDecoder::new();
        self.sync = SyncCorrelator::new();
        self.derand.reset();
        self.state = State::Hunting;
    }

    fn process_bit(&mut self, bit: u8) -> Option<Vec<u8>> {
        match &mut self.state {
            State::Hunting => {
                if self.sync.push(bit).is_some() {
                    self.state = State::Capturing {
                        bytes: Vec::with_capacity(CADU_PAYLOAD_LEN),
                        partial: 0,
                        partial_count: 0,
                    };
                }
                None
            }
            State::Capturing {
                bytes,
                partial,
                partial_count,
            } => {
                *partial = (*partial << 1) | (bit & 1);
                *partial_count += 1;
                if *partial_count == 8 {
                    bytes.push(*partial);
                    *partial = 0;
                    *partial_count = 0;
                }
                if bytes.len() == CADU_PAYLOAD_LEN {
                    let cadu = std::mem::take(bytes);
                    self.state = State::Hunting;
                    return self.decode_cadu(cadu);
                }
                None
            }
        }
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
            let (corrected, _errors) = self.rs.decode(&codeword).ok()?;
            for i in 0..RS_MESSAGE_LEN {
                vcdu[i * RS_INTERLEAVE + col] = corrected[i];
            }
        }
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
        assert!(matches!(c.state, State::Hunting));
        c.reset();
        assert!(matches!(c.state, State::Hunting));
    }

    #[test]
    fn fec_chain_returns_none_during_warmup() {
        // Viterbi needs TRACEBACK_DEPTH symbol pairs before it
        // emits its first bit. Until then, push_symbol must
        // return None on every call.
        let mut c = FecChain::new();
        for _ in 0..10 {
            let result = c.push_symbol([0, 0]);
            assert!(result.is_none());
        }
    }

    /// Pump a full clean ASM bit pattern through the chain's
    /// `process_bit` (skipping Viterbi). After ASM the next
    /// 1020 bytes are zeros (which derand will XOR to PN, then
    /// RS will fail on because zeros aren't a valid codeword).
    /// We're not testing the VCDU contents — just that the
    /// state machine cleanly transitions Hunting → Capturing
    /// → Hunting without a panic.
    #[test]
    fn process_bit_state_machine_transitions_after_asm() {
        let mut c = FecChain::new();
        // Push the ASM bits one at a time, MSB-first.
        let asm = crate::fec::ASM;
        for i in 0..32 {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "ASM is u32; shift index 0..32 is safe"
            )]
            let bit = ((asm >> (31 - i)) & 1) as u8;
            let _ = c.process_bit(bit);
        }
        // After ASM the chain must be capturing.
        assert!(matches!(c.state, State::Capturing { .. }));
        // Push 1020 × 8 zero bits to fill the CADU buffer.
        for _ in 0..(CADU_PAYLOAD_LEN * 8) {
            let _ = c.process_bit(0);
        }
        // CADU complete → decode_cadu attempted → returned to
        // Hunting (whether RS succeeded or not).
        assert!(matches!(c.state, State::Hunting));
    }

    #[test]
    fn decode_cadu_returns_none_on_invalid_codeword() {
        // Hand-construct a CADU payload that's all zeros. After
        // derand it becomes the PN sequence, which is not a
        // valid RS codeword — every column should fail to decode
        // and the chain should return None.
        let mut c = FecChain::new();
        let cadu = vec![0_u8; CADU_PAYLOAD_LEN];
        assert!(c.decode_cadu(cadu).is_none());
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
}
