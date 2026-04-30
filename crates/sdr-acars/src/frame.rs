//! ACARS frame parser. Bit-by-bit streaming state machine that
//! consumes the output of [`crate::msk::MskDemod`] and emits
//! [`AcarsMessage`]s when complete frames pass parity + CRC
//! (with optional FEC recovery via [`crate::syndrom`]).
//!
//! Faithful port of `original/acarsdec/acars.c::decodeAcars`,
//! restructured into a single-threaded sync emitter (the C
//! version uses a worker thread + condition variable; we
//! pass messages out via a callback to keep the API simple
//! and avoid threading constraints inside the library crate).

use std::time::SystemTime;

use arrayvec::ArrayString;

use crate::msk::BitSink;

// ACARS framing constants. These match `original/acarsdec/acars.c`
// L22-27 verbatim; note that ETX and ETB include the high parity
// bit (`0x03 | 0x80 = 0x83` and `0x17 | 0x80 = 0x97`) because the
// MSK demod hands bytes to the parser **with** parity intact.
const SYN: u8 = 0x16;
const SYN_INV: u8 = !SYN; // 0xE9
const SOH: u8 = 0x01;
const ETX: u8 = 0x83; // 0x03 + odd parity
const ETB: u8 = 0x97; // 0x17 + odd parity
const DLE: u8 = 0x7F;

/// Maximum frame body length (Mode through ETX/ETB inclusive)
/// before the parser gives up and resets. Mirrors `acars.c:334`.
const MAX_FRAME_LEN: usize = 240;

/// Minimum buffer length before the DLE-escape recovery path is
/// considered. Mirrors `acars.c:324`.
const DLE_ESCAPE_MIN_LEN: usize = 20;

/// One decoded ACARS message.
#[derive(Clone, Debug)]
pub struct AcarsMessage {
    /// Wall-clock time when the closing bit arrived.
    pub timestamp: SystemTime,
    /// Channel index this message came from. `0` for the
    /// single-channel WAV-input path; `0..N` for `ChannelBank`.
    pub channel_idx: u8,
    /// Channel center frequency (Hz). `0.0` if unknown
    /// (e.g. WAV input where no center is supplied).
    pub freq_hz: f64,
    /// Matched-filter output magnitude in dB. Volatile —
    /// stripped from e2e diff. Filled in by `ChannelBank`; the
    /// parser leaves it at `0.0`.
    pub level_db: f32,
    /// Number of bytes corrected by parity FEC. Volatile —
    /// stripped from e2e diff.
    pub error_count: u8,
    /// Mode character (acarsdec field).
    pub mode: u8,
    /// 2-byte label code (e.g. b"H1").
    pub label: [u8; 2],
    /// Block ID (acarsdec field).
    pub block_id: u8,
    /// ACK character (acarsdec field).
    pub ack: u8,
    /// Aircraft registration including leading dot, e.g.
    /// ".N12345". 7 chars + leading dot = up to 8 chars.
    pub aircraft: ArrayString<8>,
    /// Optional flight ID (downlink only). 6 chars max.
    pub flight_id: Option<ArrayString<7>>,
    /// Optional message number. 4 chars max.
    pub message_no: Option<ArrayString<5>>,
    /// Variable-length text body. Up to ~220 bytes.
    pub text: String,
    /// `true` if the closing byte was `ETX` (final block);
    /// `false` if `ETB` (multi-block, more to come — see #580).
    pub end_of_message: bool,
    /// Number of frames that were reassembled into this
    /// message by [`crate::reassembly::MessageAssembler`]. `1`
    /// for a single-block message (the parser's default — no
    /// reassembly took place); `≥ 2` when an ETB chain was
    /// merged into a single logical message. Surfaced for the
    /// viewer's "[N blocks]" indicator. Issue #580.
    pub reassembled_block_count: u8,
}

/// Internal state of the byte-level state machine. Mirrors
/// the enum in acars.c:88 (we collapse the trivial `END` state
/// into "go directly back to `WaitingSyn`" since `Crc2` success
/// already does that and the C only used END as a one-byte
/// holdover before resetting).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    WaitingSyn,
    Syn2,
    SeekingSoh,
    Text,
    Crc1,
    Crc2,
}

/// Frame parser. One per channel.
pub struct FrameParser {
    state: State,
    /// Bits accumulated for the current byte (LSB-first).
    out_bits: u8,
    /// How many bits remain to fill `out_bits`. **Critical**:
    /// the state machine sets this to 1 in `reset_to_idle` so
    /// `BitSink::put_bit` per-bit re-syncs (each new bit
    /// produces a shifted byte candidate the state machine
    /// re-evaluates). `put_bit` MUST drive `consume_byte`
    /// synchronously — buffering bytes between MSK demod and
    /// state machine breaks the re-sync (we lose 7 of every 8
    /// bit-shift candidates). Mirrors C `acars.c::putbit` +
    /// `decodeAcars` running per-bit interleaved.
    n_bits: u8,
    /// Bytes accumulated for the current frame: Mode through
    /// the trailing ETX/ETB inclusive. NOT including the
    /// 2-byte BCS — those land in `crc_bytes`.
    buf: Vec<u8>,
    /// Per-character parity error positions in `buf`. Used by
    /// `fix_parity_errors` at CRC2 verify time.
    parity_errors: Vec<usize>,
    /// Running parity-error count (acarsdec `blk->err`). Used
    /// for the `> MAXPERR + 1` abort check during TXT.
    parity_err_count: u8,
    /// The two BCS bytes captured during CRC1 + CRC2 states.
    /// `[crc_low, crc_high]` matching ACARS wire order.
    crc_bytes: [u8; 2],
    /// Polarity-flip flag set when WSYN/SYN2 sees `~SYN` (0xE9).
    /// `ChannelBank::process` polls and clears via
    /// `take_polarity_flip()` after each demod block.
    polarity_flip_pending: bool,
    /// Decoded messages awaiting `drain()`. `BitSink::put_bit`
    /// drives `consume_byte` synchronously (so per-bit re-sync
    /// works); decoded messages buffer here until the caller
    /// pulls them out.
    pending_messages: std::collections::VecDeque<AcarsMessage>,
    /// Channel index to stamp into emitted messages.
    channel_idx: u8,
    /// Channel center frequency to stamp into emitted messages.
    channel_freq_hz: f64,
}

impl FrameParser {
    /// Create a parser stamping the given channel index + freq
    /// onto every emitted message.
    #[must_use]
    pub fn new(channel_idx: u8, channel_freq_hz: f64) -> Self {
        Self {
            state: State::WaitingSyn,
            out_bits: 0,
            n_bits: 8,
            buf: Vec::with_capacity(256),
            parity_errors: Vec::new(),
            parity_err_count: 0,
            crc_bytes: [0, 0],
            polarity_flip_pending: false,
            pending_messages: std::collections::VecDeque::new(),
            channel_idx,
            channel_freq_hz,
        }
    }

    /// Reset to look for the next frame's preamble. Called
    /// internally on completion or on a hard sync loss
    /// (parity-error overrun, frame-too-long, malformed sync,
    /// etc.). Mirrors `acars.c::resetAcars` (L239-244) plus
    /// our own buf/parity-errors clear.
    ///
    /// **Critical: does NOT clear `out_bits`.** acarsdec's
    /// `resetAcars` only touches state + nbits — leaving the
    /// byte register intact is what makes per-bit re-sync
    /// work: a new single bit shifts the existing register one
    /// position, producing a fresh 8-bit candidate the state
    /// machine evaluates against SYN. Clearing here would
    /// prevent re-sync from a false-positive SYN.
    fn reset_to_idle(&mut self) {
        self.state = State::WaitingSyn;
        // C `resetAcars` sets nbits=1 (per-bit re-sync).
        self.n_bits = 1;
        self.buf.clear();
        self.parity_errors.clear();
        self.parity_err_count = 0;
        self.crc_bytes = [0, 0];
    }

    /// Polarity-flip handshake. `ChannelBank` reads + clears this
    /// after each `MskDemod::process` round; if true, it calls
    /// `MskDemod::toggle_polarity()` to recover from 180° phase
    /// slip detected via the inverted-SYN preamble.
    pub fn take_polarity_flip(&mut self) -> bool {
        std::mem::replace(&mut self.polarity_flip_pending, false)
    }

    /// Drain decoded messages buffered by synchronous
    /// `BitSink::put_bit` → `consume_byte` runs. Production
    /// callers (`ChannelBank::process`) invoke this after each
    /// demod block. Tests use `feed_bytes()` instead.
    pub fn drain<F: FnMut(AcarsMessage)>(&mut self, mut on_message: F) {
        while let Some(msg) = self.pending_messages.pop_front() {
            on_message(msg);
        }
    }

    /// Consume one fully-assembled byte. Drives the state
    /// machine; pushes an `AcarsMessage` onto `pending_messages`
    /// when CRC2 closes a successful frame. Mirrors the byte-
    /// level switch in `acars.c::decodeAcars` (L246-388). The C
    /// `decodeAcars` runs SYNCHRONOUSLY per byte from `putbit` —
    /// our Rust port does the same via this method being called
    /// from `BitSink::put_bit` (NOT buffered for later) so the
    /// `n_bits = 1` per-bit re-sync semantic in `reset_to_idle`
    /// works correctly.
    fn consume_byte(&mut self, byte: u8) {
        match self.state {
            // acars.c:252-265
            State::WaitingSyn => {
                if byte == SYN {
                    self.state = State::Syn2;
                    self.n_bits = 8;
                } else if byte == SYN_INV {
                    // Inverted SYN: 180° phase slip. Signal upper
                    // layer to flip polarity; advance state.
                    self.polarity_flip_pending = true;
                    self.state = State::Syn2;
                    self.n_bits = 8;
                } else {
                    // No sync — keep advancing one bit at a time.
                    self.n_bits = 1;
                }
            }
            // acars.c:267-279
            State::Syn2 => {
                if byte == SYN {
                    self.state = State::SeekingSoh;
                    self.n_bits = 8;
                } else if byte == SYN_INV {
                    // Inverted SYN at SYN2: still polarity slip,
                    // stay in SYN2 (matches the C — no state
                    // transition here, only the polarity flip).
                    self.polarity_flip_pending = true;
                    self.n_bits = 8;
                } else {
                    self.reset_to_idle();
                }
            }
            // acars.c:281-301
            State::SeekingSoh => {
                if byte == SOH {
                    // Frame start: reset accumulators and enter TXT.
                    self.buf.clear();
                    self.parity_errors.clear();
                    self.parity_err_count = 0;
                    self.crc_bytes = [0, 0];
                    self.state = State::Text;
                    self.n_bits = 8;
                } else {
                    self.reset_to_idle();
                }
            }
            // acars.c:303-341
            State::Text => {
                self.buf.push(byte);
                let pos = self.buf.len() - 1;
                if !has_odd_parity(byte) {
                    self.parity_err_count = self.parity_err_count.saturating_add(1);
                    self.parity_errors.push(pos);
                    if usize::from(self.parity_err_count) > crate::syndrom::MAX_PARITY_ERRORS + 1 {
                        // Too many parity errors — bail.
                        self.reset_to_idle();
                        return;
                    }
                }
                if byte == ETX || byte == ETB {
                    self.state = State::Crc1;
                    self.n_bits = 8;
                    return;
                }
                // DLE escape recovery (acars.c:324-332): if we've
                // accumulated more than 20 bytes and see a DLE, we
                // treat the previous 3 bytes as `padding | crc[0] |
                // crc[1]` (the C truncates len by 3 and copies
                // txt[len] / txt[len+1] into crc[0] / crc[1] — note
                // that means `padding` is whatever was at the new
                // `txt[len-1]` and is left in place — implementer
                // matches the C even though it looks odd).
                if self.buf.len() > DLE_ESCAPE_MIN_LEN && byte == DLE {
                    let new_len = self.buf.len() - 3;
                    // Capture crc[0] and crc[1] from the now-trimmed
                    // tail. C: crc[0] = txt[len], crc[1] = txt[len+1]
                    // where `len` is the post-truncation length.
                    self.crc_bytes[0] = self.buf[new_len];
                    self.crc_bytes[1] = self.buf[new_len + 1];
                    self.buf.truncate(new_len);
                    // Drop parity-error offsets that pointed into the
                    // 3 bytes we just removed; otherwise
                    // fix_parity_errors would index past frame.len()
                    // in finalize_frame (panic in debug, wrong-bit
                    // flip / syndrome OOB in release). Sync the
                    // running count so the AcarsMessage error_count
                    // stays accurate.
                    self.parity_errors.retain(|&pos| pos < new_len);
                    self.parity_err_count =
                        u8::try_from(self.parity_errors.len()).unwrap_or(u8::MAX);
                    // Jump straight to the CRC-verify / putmsg path.
                    self.finalize_frame();
                    return;
                }
                if self.buf.len() > MAX_FRAME_LEN {
                    self.reset_to_idle();
                    return;
                }
                self.n_bits = 8;
            }
            // acars.c:343-347
            State::Crc1 => {
                self.crc_bytes[0] = byte;
                self.state = State::Crc2;
                self.n_bits = 8;
            }
            // acars.c:348-373 (putmsg_lbl), then END→reset
            State::Crc2 => {
                self.crc_bytes[1] = byte;
                self.finalize_frame();
            }
        }
    }

    /// CRC-verify, optionally FEC-recover, build the
    /// `AcarsMessage`, push it onto `pending_messages`, and
    /// reset. Shared between the normal CRC2 path and the
    /// DLE-escape recovery (`acars.c::putmsg_lbl`).
    fn finalize_frame(&mut self) {
        // Compute the CRC over buf + crc_bytes. acars.c:160-165
        // does this one-shot: fold every byte in `txt` then both
        // BCS bytes; expect 0.
        let mut crc = crate::crc::compute(&self.buf);
        crc = crate::crc::update(crc, self.crc_bytes[0]);
        crc = crate::crc::update(crc, self.crc_bytes[1]);

        // Try FEC if non-zero. acars.c:170-192:
        //   if (pn) {
        //       fixprerr(...) — try parity-error correction
        //   } else if (crc) {
        //       fixdberr(...) — try double-bit-flip recovery
        //   }
        if crc != 0 {
            let recovered = if self.parity_errors.is_empty() {
                crate::syndrom::fix_double_error(&mut self.buf, crc)
            } else {
                crate::syndrom::fix_parity_errors(&mut self.buf, crc, &self.parity_errors)
            };
            if !recovered {
                self.reset_to_idle();
                return;
            }
        }

        // Frame must be at least Mode + Address(7) + ACK + Label(2)
        // + BlockID + STX + ETX = 13 bytes (acars.c:124).
        if self.buf.len() < 13 {
            self.reset_to_idle();
            return;
        }

        // Field extraction. Strip parity (& 0x7F) on every byte
        // that becomes user-facing text. Mirrors output.c:494-525.
        let mode = self.buf[0] & 0x7F;
        let mut aircraft = ArrayString::<8>::new();
        // C output.c:503-508 skips '.' chars; we keep them so the
        // caller sees the leading dot the wire actually carries.
        for &b in &self.buf[1..8] {
            // Push silently ignores overflow — the slice is exactly
            // 7 chars and the buffer holds 8, so this is safe by
            // construction.
            let _ = aircraft.try_push((b & 0x7F) as char);
        }
        // NAK character (0x15) is non-printable — normalize to
        // '!' (0x21) here so consumers can compare against the
        // printable sentinel. Mirrors `output.c::buildmsg:513-514`.
        let mut ack = self.buf[8] & 0x7F;
        if ack == 0x15 {
            ack = b'!';
        }
        let mut label = [self.buf[9] & 0x7F, self.buf[10] & 0x7F];
        // DEL (0x7F) in second label byte → 'd' (output.c:520).
        if label[1] == 0x7F {
            label[1] = b'd';
        }
        let block_id = self.buf[11] & 0x7F;
        // self.buf[12] is STX (0x02 with parity → 0x82); skipped.
        // Downlink frames (block_id ∈ '0'..='9' per
        // `output.c::IS_DOWNLINK_BLK`) carry a 4-char message
        // number then a 6-char flight ID immediately after STX,
        // before the visible text. Uplinks have no such prefix —
        // text starts at buf[13]. We extract these here so the
        // e2e diff against acarsdec's text printer matches.
        let is_downlink = block_id.is_ascii_digit();
        let text_end = self.buf.len() - 1;
        let mut message_no: Option<ArrayString<5>> = None;
        let mut flight_id: Option<ArrayString<7>> = None;
        // Downlink prefix is up to 4 msgno bytes then up to 6
        // flight-id bytes. Each field is independently
        // bounds-checked against `text_end`, so a partial
        // msgno (text_end < 17) still extracts what's there
        // — mirrors the C's per-byte `i < N && k < blk->len
        // - 1` guards in `output.c:548, 561`. Per CR round 1
        // on PR #583 — was previously gated on `text_end >=
        // 17` which dropped partial-prefix downlink frames.
        let text_start: usize = if is_downlink && text_end > 13 {
            let msgno_finish = 17.min(text_end);
            if msgno_finish > 13 {
                let mut no = ArrayString::<5>::new();
                for &b in &self.buf[13..msgno_finish] {
                    let _ = no.try_push((b & 0x7F) as char);
                }
                if !no.is_empty() {
                    message_no = Some(no);
                }
            }
            let flight_start = msgno_finish;
            let flight_finish = 23.min(text_end);
            if flight_start < flight_finish {
                let mut fid = ArrayString::<7>::new();
                for &b in &self.buf[flight_start..flight_finish] {
                    let _ = fid.try_push((b & 0x7F) as char);
                }
                if !fid.is_empty() {
                    flight_id = Some(fid);
                }
            }
            flight_finish
        } else {
            13
        };
        let mut text = String::with_capacity(text_end.saturating_sub(text_start));
        if text_end > text_start {
            for &b in &self.buf[text_start..text_end] {
                text.push((b & 0x7F) as char);
            }
        }
        let end_of_message = (self.buf[text_end] & 0x7F) == 0x03;

        let msg = AcarsMessage {
            timestamp: SystemTime::now(),
            channel_idx: self.channel_idx,
            freq_hz: self.channel_freq_hz,
            level_db: 0.0, // filled in by ChannelBank in T7.
            error_count: self.parity_err_count,
            mode,
            label,
            block_id,
            ack,
            aircraft,
            flight_id,
            message_no,
            text,
            end_of_message,
            // The parser produces single-block messages by
            // construction; reassembly into multi-block
            // logical messages happens later, in
            // `crate::reassembly::MessageAssembler`. Issue #580.
            reassembled_block_count: 1,
        };
        self.pending_messages.push_back(msg);
        self.reset_to_idle();
    }

    /// Convenience: drive the parser with a sequence of fully-
    /// formed bytes — used by unit tests that bypass MSK demod
    /// and feed hand-crafted byte sequences directly. Also
    /// drains the resulting messages into `on_message` for test
    /// ergonomics.
    pub fn feed_bytes<F: FnMut(AcarsMessage)>(&mut self, bytes: &[u8], mut on_message: F) {
        for &b in bytes {
            self.consume_byte(b);
        }
        self.drain(&mut on_message);
    }
}

impl BitSink for FrameParser {
    fn take_polarity_flip(&mut self) -> bool {
        // Delegate to the inherent method (also kept public so
        // ChannelBank's pre-existing per-block poll keeps
        // working — though per-block polling is now redundant
        // for ACARS since MskDemod polls per-bit).
        FrameParser::take_polarity_flip(self)
    }

    fn put_bit(&mut self, value: f32) {
        // LSB-first byte accumulator (acarsdec putbit, msk.c:53-63):
        // shift right, set bit 7 on a positive sample. When the
        // count hits 0, hand the assembled byte to consume_byte
        // SYNCHRONOUSLY — the C does this from inside putbit, and
        // crucially the state machine sets nbits=1 (per-bit re-sync)
        // when the candidate doesn't match SYN. Buffering bytes for
        // a later drain breaks that re-sync (we'd lose 7 of every 8
        // bit-shift candidates).
        self.out_bits >>= 1;
        if value > 0.0 {
            self.out_bits |= 0x80;
        }
        self.n_bits = self.n_bits.saturating_sub(1);
        if self.n_bits == 0 {
            // n_bits is set to 8 (or 1 for re-sync) by consume_byte
            // via the state-machine transitions; do NOT pre-set it
            // here.
            let byte = self.out_bits;
            self.consume_byte(byte);
        }
    }
}

/// Odd-parity check: returns `true` if the byte has an odd
/// number of 1-bits (ACARS valid byte). Mirrors `numbits[byte]
/// & 1 == 1` in `acars.c:138`.
fn has_odd_parity(b: u8) -> bool {
    b.count_ones() & 1 == 1
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Apply odd parity (set bit 7 if needed) to every byte in
    /// `bytes`. ACARS uses 7-bit ASCII with the high bit chosen
    /// so the total bit count is odd.
    fn add_odd_parity(bytes: &mut [u8]) {
        for b in bytes.iter_mut() {
            if (b.count_ones() & 1) == 0 {
                *b |= 0x80;
            }
        }
    }

    /// Build a known-good ACARS frame as a byte sequence ready
    /// to feed into `FrameParser`. Address ".N12345", label "H1",
    /// block `block_id`, text `text`.
    ///
    /// Layout: `[SYN][SYN][SOH][Mode][Addr×7][ACK][Label×2]
    ///          [BlockID][STX][text...][ETX][CRC_lo][CRC_hi]`.
    fn synthesize_frame(block_id: u8, text: &[u8]) -> Vec<u8> {
        let mut buf = vec![0x16, 0x16, 0x01];
        buf.push(b'2'); // Mode
        buf.extend_from_slice(b".N12345"); // Address (7 bytes)
        buf.push(b'!'); // ACK = 0x21
        buf.extend_from_slice(b"H1"); // Label
        buf.push(block_id);
        buf.push(0x02); // STX
        buf.extend_from_slice(text);
        buf.push(0x03); // ETX (will get parity bit added below)
        // Apply odd parity over Mode through ETX (the CRC payload).
        let payload_start = 3;
        let payload_end = buf.len();
        add_odd_parity(&mut buf[payload_start..payload_end]);
        // Compute CRC over the parity-applied payload (the buffer
        // the receiver folds through update_crc).
        let crc = crate::crc::compute(&buf[payload_start..payload_end]);
        buf.push((crc & 0xFF) as u8); // BCS low
        buf.push((crc >> 8) as u8); // BCS high
        buf
    }

    /// Backwards-compatible default: uplink frame (block 'A')
    /// with a short body. Uplink avoids the
    /// `msgno`/`flight_id` field-extraction so callers checking
    /// raw `text` see exactly what they passed in.
    fn synthesize_minimal_frame() -> Vec<u8> {
        synthesize_frame(b'A', b"TEST")
    }

    #[test]
    fn parses_a_known_good_uplink_frame() {
        // Uplink (block 'A' is not '0'..='9' so IS_DOWNLINK_BLK
        // is false): no msgno/flight_id extraction; text body is
        // the entire payload between STX and ETX.
        let bytes = synthesize_minimal_frame();
        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(&bytes, |msg| decoded.push(msg));

        assert_eq!(decoded.len(), 1, "expected exactly one frame");
        let msg = &decoded[0];
        assert_eq!(msg.mode, b'2');
        assert_eq!(&msg.aircraft[..], ".N12345");
        assert_eq!(msg.label, *b"H1");
        assert_eq!(msg.block_id, b'A');
        assert_eq!(msg.ack, b'!');
        assert_eq!(msg.text, "TEST");
        assert!(msg.end_of_message);
        assert_eq!(msg.channel_idx, 0);
        assert!(msg.flight_id.is_none(), "uplink has no flight_id");
        assert!(msg.message_no.is_none(), "uplink has no message_no");
    }

    #[test]
    fn parses_a_known_good_downlink_frame() {
        // Downlink (block '0' ∈ '0'..='9' triggers
        // IS_DOWNLINK_BLK): text payload starts with 4-char
        // msgno + 6-char flight_id, then the visible body.
        // We pass a 14-char payload: "S64A" + "BA031T" + "BODY"
        // → msgno=S64A, flight=BA031T, text=BODY.
        let bytes = synthesize_frame(b'0', b"S64ABA031TBODY");
        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(&bytes, |msg| decoded.push(msg));

        assert_eq!(decoded.len(), 1, "expected exactly one frame");
        let msg = &decoded[0];
        assert_eq!(msg.block_id, b'0');
        assert_eq!(msg.message_no.as_deref(), Some("S64A"));
        assert_eq!(msg.flight_id.as_deref(), Some("BA031T"));
        assert_eq!(msg.text, "BODY");
    }

    #[test]
    fn rejects_a_corrupted_frame_when_fec_cant_recover() {
        let mut bytes = synthesize_minimal_frame();
        // Wreck the CRC bytes so neither parity-error correction
        // nor double-bit-flip recovery can salvage it.
        let n = bytes.len();
        bytes[n - 2] = 0x00;
        bytes[n - 1] = 0x00;

        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(&bytes, |msg| decoded.push(msg));

        assert!(decoded.is_empty(), "corrupted frame must not decode");
    }

    #[test]
    fn ignores_bytes_outside_a_frame() {
        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(b"\x00\xFF\x00\xFF\x00", |msg| decoded.push(msg));
        assert!(decoded.is_empty());
    }

    #[test]
    fn dle_recovery_drops_stale_parity_offsets() {
        // Regression: the DLE recovery branch trims `self.buf` by
        // 3 bytes but used to leave `self.parity_errors` holding
        // offsets pointing into the now-removed tail. The next
        // `fix_parity_errors` call would then index `frame[stale]`
        // past `frame.len()` (panic in debug, wrong-bit-flip /
        // syndrome OOB in release).
        //
        // Construction: drop into Text state, accumulate 22 bytes
        // with valid odd parity, then 3 even-parity bytes (recorded
        // at positions 22, 23, 24), then send DLE. The parser
        // truncates buf to len 22 and goes to finalize_frame; the
        // CRC is non-zero, so finalize_frame routes through
        // fix_parity_errors which would panic without the fix.
        let mut bytes = vec![SYN, SYN, SOH];
        // 22 odd-parity bytes (0x80 has one 1-bit). Body bytes —
        // the parser doesn't care about content during Text.
        bytes.extend(std::iter::repeat_n(0x80, 22));
        // 3 even-parity bytes that go on parity_errors at
        // positions 22, 23, 24. NUL is even-parity (0 ones).
        bytes.extend_from_slice(&[0x00, 0x00, 0x00]);
        // DLE at position 25 — buf.len()=25 > DLE_ESCAPE_MIN_LEN=20,
        // triggers the recovery branch.
        bytes.push(DLE);

        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        // The frame must NOT decode (CRC garbage), and the parser
        // must NOT panic — the only assertion that matters here is
        // "we got past feed_bytes alive".
        parser.feed_bytes(&bytes, |msg| decoded.push(msg));
        assert!(
            decoded.is_empty(),
            "synthetic DLE-recovery frame must not decode"
        );
    }
}
