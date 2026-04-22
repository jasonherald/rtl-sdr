//! Stream codecs for the extended rtl_tcp protocol (#307).
//!
//! The classic rtl_tcp protocol streams raw 8-bit I/Q samples over
//! TCP — roughly 4.8 MB/s at 2.4 Msps. On a good LAN that's free;
//! over Wi-Fi near the edge of range, congested networks, or
//! travel/hotel routers, it's unusable. Extended clients can
//! negotiate an optional codec at handshake time to compress the
//! stream in flight.
//!
//! This module defines:
//!
//! - [`Codec`] — the scalar code sent in the server's extension
//!   response, and the trait-object handle used by both client
//!   and server to wrap their TCP streams.
//! - [`CodecMask`] — the bitmask a client sends in its hello
//!   advertising which codecs it understands; the server intersects
//!   this with its own configured codec set and picks the best
//!   mutual option.
//! - The [`Encoder`] / [`Decoder`] trait pair — streaming framing
//!   that both server and client implementations call per chunk.
//!
//! # Compatibility
//!
//! Codec negotiation happens entirely inside the [`"RTLX"`]
//! extension block; legacy clients (GQRX, SDR++, vanilla rtl_tcp)
//! that don't send a hello time out on the server's 100 ms sniff
//! window and fall through to the unchanged legacy path at
//! [`Codec::None`]. No codec-related bytes ever reach a legacy
//! client.
//!
//! [`"RTLX"`]: crate::extension

use std::io::{Read, Write};

use lz4_flex::frame::{FrameDecoder, FrameEncoder};

/// Over-the-wire codec identifier sent by the server in the 8-byte
/// extension response. Stable scalar values — any future codec
/// addition MUST append (existing values never change) so that
/// client libraries compiled against older versions of this crate
/// don't silently misinterpret the byte as an unrelated codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec {
    /// No compression — raw 8-bit I/Q bytes on the wire, same as
    /// the legacy rtl_tcp protocol. Every compliant server MUST
    /// support this as the fallback codec.
    None = 0,
    /// LZ4 frame format — `lz4_flex` streaming encoder/decoder.
    /// Good throughput (~500 MB/s on a modern CPU core), modest
    /// ratio (1.1× to 1.5× on typical I/Q traffic — the signal is
    /// close to white noise, so entropy coding has limited headroom,
    /// but voice-rate structure at high SNR does yield some wins).
    Lz4 = 1,
    // Reserved for #307 follow-up: Zstd = 2. Additive; current
    // wire format does not allocate code 2 so legacy clients that
    // don't understand `Zstd` never receive it (server advertises
    // only mutually-supported codecs).
}

impl Codec {
    /// Human-readable label for UI / log messages.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Lz4 => "LZ4",
        }
    }

    /// Decode the 1-byte on-wire value into a [`Codec`]. Returns
    /// [`None`] for any value the current build doesn't recognize —
    /// a newer server advertising a future codec code won't crash
    /// an older client; the client just falls back to legacy.
    #[must_use]
    pub fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::None),
            1 => Some(Self::Lz4),
            _ => None,
        }
    }

    /// The 1-byte on-wire value. Inverse of [`Self::from_wire`].
    #[must_use]
    pub fn to_wire(self) -> u8 {
        self as u8
    }

    /// Bit position in the [`CodecMask`] advertised by clients.
    /// `Codec::None` is bit 0; every compliant client must set it
    /// so the server can always fall back. `Codec::Lz4` is bit 1.
    #[must_use]
    pub fn mask_bit(self) -> u8 {
        match self {
            Self::None => 1 << 0,
            Self::Lz4 => 1 << 1,
        }
    }
}

/// Bitmask of supported codecs, sent by the client in its hello
/// and stored by the server as its local configuration ("which
/// codecs am I willing to offer"). Negotiation is the intersection
/// — see [`Self::pick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecMask(u8);

impl CodecMask {
    /// Mask advertising *only* `Codec::None`. Safe default for a
    /// compliant server that doesn't want to offer compression.
    pub const NONE_ONLY: Self = Self(1 << 0);

    /// Mask advertising `Codec::None` and `Codec::Lz4`. Typical
    /// default for a client build that has LZ4 available.
    pub const NONE_AND_LZ4: Self = Self((1 << 0) | (1 << 1));

    /// Wrap a raw wire byte.
    #[must_use]
    pub fn from_wire(byte: u8) -> Self {
        Self(byte)
    }

    /// Unwrap the raw wire byte.
    #[must_use]
    pub fn to_wire(self) -> u8 {
        self.0
    }

    /// Does this mask advertise the given codec?
    #[must_use]
    pub fn supports(self, codec: Codec) -> bool {
        self.0 & codec.mask_bit() != 0
    }

    /// Pick the best mutual codec between this mask (the server's
    /// configuration, "what am I willing to offer") and the client's
    /// advertised support. Preference order: `Lz4` > `None`. Always
    /// returns at least `Codec::None` because the `None` bit is
    /// considered implicit — a client that forgets to set it is
    /// still assumed to accept the legacy uncompressed fallback.
    #[must_use]
    pub fn pick(self, client: Self) -> Codec {
        if self.supports(Codec::Lz4) && client.supports(Codec::Lz4) {
            Codec::Lz4
        } else {
            Codec::None
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Streaming encoder wrapping an underlying `Write`. The server
/// writes uncompressed I/Q chunks to this handle; the wrapper
/// encodes + frames them and forwards the compressed bytes to the
/// TCP socket.
pub enum Encoder<W: Write + Send> {
    /// Pass-through — no framing overhead, identical to writing
    /// directly to the socket.
    None(W),
    /// LZ4 frame format. `FrameEncoder` writes a short frame
    /// header the first time it's written to; after that every
    /// flush emits a self-contained LZ4 block. The consumer side
    /// uses a matching `FrameDecoder` that tolerates any chunk
    /// boundary (frame blocks are self-delimiting in the LZ4
    /// frame spec).
    Lz4(FrameEncoder<W>),
}

impl<W: Write + Send> Encoder<W> {
    /// Wrap `inner` using the negotiated codec.
    pub fn new(codec: Codec, inner: W) -> Self {
        match codec {
            Codec::None => Self::None(inner),
            Codec::Lz4 => Self::Lz4(FrameEncoder::new(inner)),
        }
    }
}

impl<W: Write + Send> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::None(w) => w.write(buf),
            Self::Lz4(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::None(w) => w.flush(),
            Self::Lz4(w) => w.flush(),
        }
    }
}

/// Streaming decoder wrapping an underlying `Read`. The client
/// reads what looks like a plain I/Q byte stream; the decoder
/// transparently de-frames LZ4 (or passes bytes through, for
/// `Codec::None`).
pub enum Decoder<R: Read> {
    None(R),
    Lz4(FrameDecoder<R>),
}

impl<R: Read> Decoder<R> {
    /// Wrap `inner` using the negotiated codec.
    pub fn new(codec: Codec, inner: R) -> Self {
        match codec {
            Codec::None => Self::None(inner),
            Codec::Lz4 => Self::Lz4(FrameDecoder::new(inner)),
        }
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::None(r) => r.read(buf),
            Self::Lz4(r) => r.read(buf),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn codec_wire_round_trip() {
        // Every currently-defined codec round-trips through
        // `to_wire` / `from_wire`. Pins the stable scalar
        // mapping — a future refactor that renumbers variants
        // fails here first.
        assert_eq!(Codec::from_wire(Codec::None.to_wire()), Some(Codec::None));
        assert_eq!(Codec::from_wire(Codec::Lz4.to_wire()), Some(Codec::Lz4));
    }

    #[test]
    fn codec_from_wire_rejects_unknown() {
        // Future codec values (e.g. Zstd=2 when #307 gets its
        // follow-up) are `None` under an older client build. Keeps
        // forward-compat behavior explicit — the client falls back
        // to legacy rather than treating `2` as garbage.
        assert!(Codec::from_wire(2).is_none());
        assert!(Codec::from_wire(99).is_none());
        assert!(Codec::from_wire(255).is_none());
    }

    #[test]
    fn mask_pick_prefers_lz4_when_mutual() {
        let server = CodecMask::NONE_AND_LZ4;
        let client = CodecMask::NONE_AND_LZ4;
        assert_eq!(server.pick(client), Codec::Lz4);
    }

    #[test]
    fn mask_pick_falls_back_to_none_when_server_disables_lz4() {
        // Server config says "I only offer None"; client
        // advertises Lz4 support. Negotiation lands at None.
        // This is the scenario from the PR description: sdr-rs
        // client ↔ LZ4-disabled sdr-rs server → None.
        let server = CodecMask::NONE_ONLY;
        let client = CodecMask::NONE_AND_LZ4;
        assert_eq!(server.pick(client), Codec::None);
    }

    #[test]
    fn mask_pick_falls_back_to_none_when_client_lacks_lz4() {
        // Server offers both; client only supports None.
        let server = CodecMask::NONE_AND_LZ4;
        let client = CodecMask::NONE_ONLY;
        assert_eq!(server.pick(client), Codec::None);
    }

    #[test]
    fn lz4_round_trip_preserves_bytes_exactly() {
        // End-to-end: encode a chunk with the Lz4 encoder, decode
        // with the matching decoder, assert byte-exact recovery.
        // Critical — compression is lossless; any divergence is a
        // framing or library-version bug.
        let plaintext: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();

        let mut buf: Vec<u8> = Vec::new();
        {
            let mut encoder = Encoder::Lz4(FrameEncoder::new(&mut buf));
            encoder.write_all(&plaintext).unwrap();
            encoder.flush().unwrap();
            // `FrameEncoder` requires `finish()` for its terminal
            // end-of-frame block; `Drop` also finalizes but an
            // explicit finish is clearer in tests.
            if let Encoder::Lz4(enc) = encoder {
                enc.finish().unwrap();
            }
        }
        assert!(!buf.is_empty(), "encoder should have emitted bytes");
        assert!(
            buf.len() < plaintext.len(),
            "constant-stride input should compress smaller ({} vs {})",
            buf.len(),
            plaintext.len()
        );

        let mut decoder = Decoder::Lz4(FrameDecoder::new(buf.as_slice()));
        let mut recovered: Vec<u8> = Vec::new();
        std::io::copy(&mut decoder, &mut recovered).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn none_codec_is_transparent_passthrough() {
        // None codec must not alter bytes in either direction.
        // Confirms the enum's pass-through arm doesn't accidentally
        // double-buffer or mutate.
        let plaintext: &[u8] = b"arbitrary \x00\xff binary payload";

        let mut sink: Vec<u8> = Vec::new();
        {
            let mut encoder = Encoder::None(&mut sink);
            encoder.write_all(plaintext).unwrap();
            encoder.flush().unwrap();
        }
        assert_eq!(sink, plaintext);

        let mut decoder = Decoder::None(sink.as_slice());
        let mut recovered: Vec<u8> = Vec::new();
        std::io::copy(&mut decoder, &mut recovered).unwrap();
        assert_eq!(recovered, plaintext);
    }
}
