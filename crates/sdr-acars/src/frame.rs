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
    /// How many bits remain to fill `out_bits`.
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
    /// Bytes finished by `BitSink::put_bit` but not yet handed
    /// to the state machine. `drain(on_message)` walks this.
    pending_bytes: Vec<u8>,
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
            pending_bytes: Vec::new(),
            channel_idx,
            channel_freq_hz,
        }
    }

    /// Reset to look for the next frame's preamble. Called
    /// internally on completion or on a hard sync loss
    /// (parity-error overrun, frame-too-long, malformed sync,
    /// etc.). Mirrors `acars.c::resetAcars` (L239-244) plus
    /// our own buf/parity-errors clear.
    fn reset_to_idle(&mut self) {
        self.state = State::WaitingSyn;
        // C `resetAcars` sets nbits=1 (per-bit re-sync).
        self.n_bits = 1;
        self.out_bits = 0;
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

    /// Drain completed bytes accumulated by `BitSink::put_bit`,
    /// running each through `consume_byte`. Production callers
    /// (`ChannelBank::process`) invoke this after every demod
    /// block. Tests use `feed_bytes()` instead.
    pub fn drain<F: FnMut(AcarsMessage)>(&mut self, mut on_message: F) {
        let bytes = std::mem::take(&mut self.pending_bytes);
        for b in bytes {
            self.consume_byte(b, &mut on_message);
        }
    }

    /// Consume one fully-assembled byte. Drives the state
    /// machine; emits an `AcarsMessage` via `on_message` when
    /// CRC2 closes a successful frame. Mirrors the byte-level
    /// switch in `acars.c::decodeAcars` (L246-388).
    fn consume_byte<F: FnMut(AcarsMessage)>(&mut self, byte: u8, on_message: &mut F) {
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
                    // Jump straight to the CRC-verify / putmsg path.
                    self.finalize_frame(on_message);
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
                self.finalize_frame(on_message);
            }
        }
    }

    /// CRC-verify, optionally FEC-recover, build the
    /// `AcarsMessage`, hand it to the callback, and reset.
    /// Shared between the normal CRC2 path and the DLE-escape
    /// recovery (`acars.c::putmsg_lbl`).
    fn finalize_frame<F: FnMut(AcarsMessage)>(&mut self, on_message: &mut F) {
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
        let ack = self.buf[8] & 0x7F;
        let mut label = [self.buf[9] & 0x7F, self.buf[10] & 0x7F];
        // DEL (0x7F) in second label byte → 'd' (output.c:520).
        if label[1] == 0x7F {
            label[1] = b'd';
        }
        let block_id = self.buf[11] & 0x7F;
        // self.buf[12] is STX (0x02 with parity → 0x82); skipped.
        // Text body runs from buf[13] up to (but not including)
        // the trailing ETX/ETB.
        let text_end = self.buf.len() - 1;
        let mut text = String::with_capacity(text_end.saturating_sub(13));
        if text_end > 13 {
            for &b in &self.buf[13..text_end] {
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
            flight_id: None,  // v1 — see #577.
            message_no: None, // v1 — see #577.
            text,
            end_of_message,
        };
        on_message(msg);
        self.reset_to_idle();
    }

    /// Convenience: drive the parser with a sequence of fully-
    /// formed bytes — used by unit tests that bypass MSK demod
    /// and feed hand-crafted byte sequences directly.
    pub fn feed_bytes<F: FnMut(AcarsMessage)>(&mut self, bytes: &[u8], mut on_message: F) {
        for &b in bytes {
            self.consume_byte(b, &mut on_message);
        }
    }
}

impl BitSink for FrameParser {
    fn put_bit(&mut self, value: f32) {
        // Shift the bit into the byte register LSB-first
        // (matches acarsdec putbit, msk.c:53-63). When the byte
        // fills, push to `pending_bytes` for the caller's later
        // `drain(on_message)`. Splitting bit accumulation from
        // state-machine driving keeps `BitSink::put_bit`
        // callback-free and lets unit tests bypass the bit path.
        self.out_bits >>= 1;
        if value > 0.0 {
            self.out_bits |= 0x80;
        }
        self.n_bits = self.n_bits.saturating_sub(1);
        if self.n_bits == 0 {
            self.n_bits = 8;
            let byte = self.out_bits;
            self.out_bits = 0;
            self.pending_bytes.push(byte);
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
    /// block "0", text "TEST".
    ///
    /// Layout: `[SYN][SYN][SOH][Mode][Addr×7][ACK][Label×2]
    ///          [BlockID][STX][text...][ETX][CRC_lo][CRC_hi]`.
    fn synthesize_minimal_frame() -> Vec<u8> {
        let mut buf = vec![0x16, 0x16, 0x01];
        buf.push(b'2'); // Mode
        buf.extend_from_slice(b".N12345"); // Address (7 bytes)
        buf.push(b'!'); // ACK = 0x21
        buf.extend_from_slice(b"H1"); // Label
        buf.push(b'0'); // Block ID
        buf.push(0x02); // STX
        buf.extend_from_slice(b"TEST"); // text
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

    #[test]
    fn parses_a_known_good_frame() {
        let bytes = synthesize_minimal_frame();
        let mut parser = FrameParser::new(0, 0.0);
        let mut decoded = Vec::new();
        parser.feed_bytes(&bytes, |msg| decoded.push(msg));

        assert_eq!(decoded.len(), 1, "expected exactly one frame");
        let msg = &decoded[0];
        assert_eq!(msg.mode, b'2');
        assert_eq!(&msg.aircraft[..], ".N12345");
        assert_eq!(msg.label, *b"H1");
        assert_eq!(msg.block_id, b'0');
        assert_eq!(msg.ack, b'!');
        assert_eq!(msg.text, "TEST");
        assert!(msg.end_of_message);
        assert_eq!(msg.channel_idx, 0);
        assert!(msg.flight_id.is_none());
        assert!(msg.message_no.is_none());
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
}
