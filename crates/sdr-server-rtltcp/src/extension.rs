//! Extended rtl_tcp handshake (`"RTLX"` protocol) (#307 + #390).
//!
//! Extends the legacy 12-byte `dongle_info_t` hello with a tiny
//! negotiation framing so sdr-rs-aware clients can opt into
//! compression today (#307) and, later, role-based multi-client
//! access (#390) without a version bump on the wire.
//!
//! # Wire format
//!
//! ## Client → server: 8-byte `ClientHello`
//!
//! Sent immediately on TCP connect, before reading anything from
//! the server. Fixed size so the server's sniff-with-timeout path
//! is deterministic.
//!
//! ```text
//! off  size  field
//! 0    4     magic = "RTLX"
//! 4    1     codec_mask        bitmask of supported codecs (#307)
//! 5    1     role              0=control, 1=listen (reserved for #392)
//! 6    1     flags             bit 0=request_takeover (reserved for #393)
//! 7    1     version           hello schema version, currently 1
//! ```
//!
//! Bytes 5–7 are ignored by the #307 server — it's single-client,
//! always grants `control`, never honors takeover. A future
//! #392/#393 implementation will plug real semantics into the
//! same layout, so the wire format stays stable across the
//! sub-issues of #390.
//!
//! ## Server → client: 8-byte `ServerExtension`
//!
//! Written **after** the legacy 12-byte `dongle_info_t`, only if
//! the server received a valid `ClientHello`. Clients that
//! negotiated the extension peek the next 4 bytes after
//! `dongle_info_t` — if they match `"RTLX"`, consume the full
//! 8-byte block; else treat incoming bytes as the raw I/Q stream
//! (legacy-server case).
//!
//! ```text
//! off  size  field
//! 0    4     magic = "RTLX"
//! 4    1     codec             chosen codec scalar (#307)
//! 5    1     granted_role      0=control, 1=listen, 255=denied (reserved for #392)
//! 6    1     status            0=ok, 1=controller_busy, 2=auth_required,
//!                               3=auth_failed (reserved for #392/#394)
//! 7    1     version           response schema version, currently 1
//! ```
//!
//! # Compatibility with vanilla `rtl_tcp`
//!
//! Vanilla servers receive the 8 bytes and interpret them as one
//! 5-byte command (`opcode='R'=0x52`, which is not a defined
//! opcode in upstream `rtl_tcp` and is silently ignored) plus 3
//! unread bytes that leak into the next command read. Legacy
//! servers continue to stream the plain legacy `dongle_info_t`;
//! our sdr-rs client peeks for the `"RTLX"` magic and, not
//! seeing it, falls back to the legacy uncompressed path without
//! consuming any bytes from the IQ stream.

use crate::codec::{Codec, CodecMask};

/// 4-byte magic that identifies both sides of the extended
/// handshake. Chosen because its first byte `'R'=0x52` is not a
/// defined rtl_tcp command opcode, so vanilla servers treat an
/// inadvertent hello as a no-op.
pub const EXTENSION_MAGIC: [u8; 4] = *b"RTLX";

/// Serialized size of [`ClientHello`] on the wire.
pub const CLIENT_HELLO_LEN: usize = 8;

/// Serialized size of [`ServerExtension`] on the wire.
pub const SERVER_EXTENSION_LEN: usize = 8;

/// Current protocol schema version for both hello and response.
/// Bumped only on breaking layout changes — adding codecs or
/// status codes is additive and doesn't require a version bump
/// because the over-the-wire bytes keep their positions.
pub const PROTOCOL_VERSION: u8 = 1;

/// Role a client is requesting (or that the server granted).
/// Reserved values — #307 only ever uses [`Self::Control`]; #392
/// adds [`Self::Listen`] semantics and the `255` denied sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    /// Full command access — tune, gain, sample rate, etc.
    Control = 0,
    /// Passive — receives the IQ stream; commands are dropped
    /// server-side. Reserved for #392.
    Listen = 1,
}

impl Role {
    /// Decode the 1-byte wire value. Unknown or `255` (denied
    /// sentinel) → `None`. Current #307 server always writes `0`.
    #[must_use]
    pub fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Control),
            1 => Some(Self::Listen),
            _ => None,
        }
    }

    /// The 1-byte wire value.
    #[must_use]
    pub fn to_wire(self) -> u8 {
        self as u8
    }
}

/// Status code in the server's response block. Reserved —
/// #307 only ever emits [`Self::Ok`]; #392 and #394 use the other
/// variants when roles / auth are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// Negotiation succeeded. Client proceeds to the IQ stream.
    Ok = 0,
    /// Controller slot is busy and the client didn't request
    /// takeover. Reserved for #392.
    ControllerBusy = 1,
    /// Server requires an auth key and the client didn't provide
    /// one. Reserved for #394.
    AuthRequired = 2,
    /// Client-provided auth key didn't match. Reserved for #394.
    AuthFailed = 3,
}

impl Status {
    /// Decode the 1-byte wire value. Unknown values → `None` so
    /// the client can log + treat as a protocol error rather
    /// than crash.
    #[must_use]
    pub fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Ok),
            1 => Some(Self::ControllerBusy),
            2 => Some(Self::AuthRequired),
            3 => Some(Self::AuthFailed),
            _ => None,
        }
    }

    /// The 1-byte wire value.
    #[must_use]
    pub fn to_wire(self) -> u8 {
        self as u8
    }
}

/// Client → server hello — 8 bytes on the wire, fixed layout.
/// See the module docs for the byte-by-byte layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientHello {
    /// Codecs the client is willing to negotiate (#307).
    pub codec_mask: CodecMask,
    /// Role the client is requesting (#392 — #307 server ignores
    /// and treats every client as `Control`).
    pub role: Role,
    /// Request flags. Bit 0 = request_takeover (#393).
    pub flags: u8,
    /// Hello schema version. Always [`PROTOCOL_VERSION`] for
    /// clients built against this crate; decoded for future-
    /// compat checks.
    pub version: u8,
}

/// Flag bit indicating the client wants to kick the current
/// controller if the slot is occupied. Reserved for #393.
pub const FLAG_REQUEST_TAKEOVER: u8 = 1 << 0;

/// Full [`ClientHello::flags`] value for the "no flags set" case —
/// the #307 common path, where the client isn't requesting a
/// takeover and has no other bits to assert. Named constant so
/// callers don't litter the codebase with bare `0` literals that
/// silently mean "don't set any flag bit".
pub const CLIENT_HELLO_FLAGS_NONE: u8 = 0;

impl ClientHello {
    /// Serialize to its 8-byte wire representation.
    #[must_use]
    pub fn to_bytes(self) -> [u8; CLIENT_HELLO_LEN] {
        let mut out = [0u8; CLIENT_HELLO_LEN];
        out[..4].copy_from_slice(&EXTENSION_MAGIC);
        out[4] = self.codec_mask.to_wire();
        out[5] = self.role.to_wire();
        out[6] = self.flags;
        out[7] = self.version;
        out
    }

    /// Parse from its 8-byte wire representation. Returns `None`
    /// if the magic doesn't match or the role byte is unknown —
    /// the server falls through to the legacy path in either case.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; CLIENT_HELLO_LEN]) -> Option<Self> {
        if bytes[..4] != EXTENSION_MAGIC {
            return None;
        }
        let role = Role::from_wire(bytes[5])?;
        Some(Self {
            codec_mask: CodecMask::from_wire(bytes[4]),
            role,
            flags: bytes[6],
            version: bytes[7],
        })
    }

    /// Convenience: does the caller's flags byte request takeover?
    #[must_use]
    pub fn request_takeover(self) -> bool {
        self.flags & FLAG_REQUEST_TAKEOVER != 0
    }
}

/// Server → client extension response — 8 bytes on the wire,
/// fixed layout. Written immediately after the legacy
/// `dongle_info_t` when (and only when) the server accepted a
/// valid [`ClientHello`]. See the module docs for the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerExtension {
    /// Chosen codec for this connection (#307).
    pub codec: Codec,
    /// Role the server granted. `None` means denied (wire byte =
    /// 255) — #392 semantics; #307 server always emits
    /// `Some(Role::Control)`.
    pub granted_role: Option<Role>,
    /// Outcome status (#392/#394). `Status::Ok` in #307.
    pub status: Status,
    /// Response schema version.
    pub version: u8,
}

/// Sentinel byte used in the `granted_role` field to signal
/// "request denied" — reserved for #392.
pub const GRANTED_ROLE_DENIED: u8 = 0xFF;

impl ServerExtension {
    /// Serialize to its 8-byte wire representation.
    #[must_use]
    pub fn to_bytes(self) -> [u8; SERVER_EXTENSION_LEN] {
        let mut out = [0u8; SERVER_EXTENSION_LEN];
        out[..4].copy_from_slice(&EXTENSION_MAGIC);
        out[4] = self.codec.to_wire();
        out[5] = self.granted_role.map_or(GRANTED_ROLE_DENIED, Role::to_wire);
        out[6] = self.status.to_wire();
        out[7] = self.version;
        out
    }

    /// Parse from its 8-byte wire representation. Returns `None`
    /// when the magic doesn't match (client peeked random IQ
    /// data) or any enum-typed byte is unknown. Callers fall back
    /// to the legacy uncompressed path on `None`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; SERVER_EXTENSION_LEN]) -> Option<Self> {
        if bytes[..4] != EXTENSION_MAGIC {
            return None;
        }
        let codec = Codec::from_wire(bytes[4])?;
        let granted_role = if bytes[5] == GRANTED_ROLE_DENIED {
            None
        } else {
            Some(Role::from_wire(bytes[5])?)
        };
        let status = Status::from_wire(bytes[6])?;
        Some(Self {
            codec,
            granted_role,
            status,
            version: bytes[7],
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn client_hello_round_trip() {
        let hello = ClientHello {
            codec_mask: CodecMask::NONE_AND_LZ4,
            role: Role::Control,
            flags: FLAG_REQUEST_TAKEOVER,
            version: PROTOCOL_VERSION,
        };
        let bytes = hello.to_bytes();
        assert_eq!(bytes.len(), CLIENT_HELLO_LEN);
        assert_eq!(&bytes[..4], &EXTENSION_MAGIC);
        assert_eq!(ClientHello::from_bytes(&bytes), Some(hello));
    }

    #[test]
    fn client_hello_rejects_bad_magic() {
        // Legacy commands leak into the hello slot when a client
        // talks to a vanilla server that didn't consume them
        // cleanly. Magic mismatch → None, so the server falls
        // through to its legacy path.
        let mut bytes = [0u8; CLIENT_HELLO_LEN];
        bytes[..4].copy_from_slice(b"RTLY"); // wrong
        assert!(ClientHello::from_bytes(&bytes).is_none());
    }

    #[test]
    fn client_hello_rejects_unknown_role() {
        let mut bytes = [0u8; CLIENT_HELLO_LEN];
        bytes[..4].copy_from_slice(&EXTENSION_MAGIC);
        bytes[4] = CodecMask::NONE_ONLY.to_wire();
        bytes[5] = 99; // unknown role byte
        bytes[7] = PROTOCOL_VERSION;
        assert!(ClientHello::from_bytes(&bytes).is_none());
    }

    #[test]
    fn server_extension_round_trip() {
        let ext = ServerExtension {
            codec: Codec::Lz4,
            granted_role: Some(Role::Control),
            status: Status::Ok,
            version: PROTOCOL_VERSION,
        };
        let bytes = ext.to_bytes();
        assert_eq!(bytes.len(), SERVER_EXTENSION_LEN);
        assert_eq!(&bytes[..4], &EXTENSION_MAGIC);
        assert_eq!(ServerExtension::from_bytes(&bytes), Some(ext));
    }

    #[test]
    fn server_extension_denied_role_round_trips() {
        // #392 path: the server encodes "denied" as the 0xFF
        // sentinel. Decoder maps it back to `None`.
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: None,
            status: Status::ControllerBusy,
            version: PROTOCOL_VERSION,
        };
        let bytes = ext.to_bytes();
        assert_eq!(bytes[5], GRANTED_ROLE_DENIED);
        assert_eq!(ServerExtension::from_bytes(&bytes), Some(ext));
    }

    #[test]
    fn server_extension_rejects_bad_magic() {
        // Client peeked random IQ data that happens NOT to match
        // `"RTLX"`. Decoder returns None → client falls back to
        // legacy uncompressed read, and those 4 peeked bytes
        // stay in the TCP read buffer for the next stream read.
        let mut bytes = [0u8; SERVER_EXTENSION_LEN];
        bytes[..4].copy_from_slice(b"\x00\x01\x02\x03"); // unlikely in IQ; arbitrary
        assert!(ServerExtension::from_bytes(&bytes).is_none());
    }

    #[test]
    fn client_hello_takeover_flag_helper() {
        let with_flag = ClientHello {
            codec_mask: CodecMask::NONE_ONLY,
            role: Role::Control,
            flags: FLAG_REQUEST_TAKEOVER,
            version: PROTOCOL_VERSION,
        };
        assert!(with_flag.request_takeover());

        let without_flag = ClientHello {
            flags: 0,
            ..with_flag
        };
        assert!(!without_flag.request_takeover());
    }

    #[test]
    fn magic_first_byte_not_a_legacy_opcode() {
        // Defense-in-depth: if a sdr-rs client accidentally
        // sends a hello to a vanilla rtl_tcp server, the first
        // byte `'R' = 0x52` must NOT collide with a real
        // command opcode, or the server would try to execute it.
        // Documented opcodes are 0x01..=0x0E (per rtl_tcp.c); our
        // magic's first byte sits well above that range.
        assert!(EXTENSION_MAGIC[0] > 0x0E);
    }
}
