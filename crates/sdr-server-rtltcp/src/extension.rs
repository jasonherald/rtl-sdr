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
//!                               3=auth_failed, 4=listener_cap_reached
//!                               (0/1/4 used by #392, 2/3 reserved for #394)
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

/// Status code in the server's response block. Variants 0, 1, 4
/// are live with #392 (role gate + listener cap); 2 and 3 are
/// reserved for #394 (auth) and will be emitted by a future
/// handshake layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// Negotiation succeeded. Client proceeds to the IQ stream.
    Ok = 0,
    /// Controller slot is busy and the client didn't request
    /// takeover. #392.
    ControllerBusy = 1,
    /// Server requires an auth key and the client didn't provide
    /// one. Reserved for #394.
    AuthRequired = 2,
    /// Client-provided auth key didn't match. Reserved for #394.
    AuthFailed = 3,
    /// Server's listener slot count (`ServerConfig::listener_cap`)
    /// is already fully allocated; the client asked for
    /// `Role::Listen` and there's no room. #392. Additive wire
    /// code — new variants don't version-bump as long as the
    /// `PROTOCOL_VERSION` gate catches peers reading layouts
    /// they don't understand.
    ListenerCapReached = 4,
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
            4 => Some(Self::ListenerCapReached),
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
/// controller if the slot is occupied. #393.
pub const FLAG_REQUEST_TAKEOVER: u8 = 1 << 0;

/// Flag bit indicating the client is sending an
/// [`AuthKeyMessage`] immediately after the hello. Servers that
/// require auth (#394) parse the key from the hello stream
/// without waiting for an `AuthRequired` round-trip. Clients
/// that don't have an auth key leave this clear — the server
/// will reply with `status=AuthRequired` (for auth-enabled
/// servers) and the client follows up with the key then.
pub const FLAG_HAS_AUTH: u8 = 1 << 1;

/// Full [`ClientHello::flags`] value for the "no flags set" case —
/// the #307 common path, where the client isn't requesting a
/// takeover, not carrying auth, and has no other bits to assert.
/// Named constant so callers don't litter the codebase with bare
/// `0` literals that silently mean "don't set any flag bit".
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
    /// if the magic doesn't match, the schema version doesn't
    /// match [`PROTOCOL_VERSION`], or the role byte is unknown.
    /// Callers surface `None` as a protocol error and drop the
    /// client — letting a peer built for a future wire layout
    /// through would cause silent mis-negotiation rather than a
    /// clean version break. Per CodeRabbit round 3 on PR #399.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; CLIENT_HELLO_LEN]) -> Option<Self> {
        if bytes[..EXTENSION_MAGIC.len()] != EXTENSION_MAGIC {
            return None;
        }
        // Version gate is strict: peers built against a newer
        // layout must be rejected, not silently parsed as v1.
        // If we ever bump PROTOCOL_VERSION we'll add a wider
        // accept-set here (e.g., `matches!(bytes[7], 1 | 2)`)
        // once both sides of the upgrade have shipped.
        if bytes[7] != PROTOCOL_VERSION {
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

    /// Convenience: does the caller's flags byte announce that an
    /// [`AuthKeyMessage`] is being sent immediately after this
    /// hello? #394.
    #[must_use]
    pub fn has_auth(self) -> bool {
        self.flags & FLAG_HAS_AUTH != 0
    }
}

/// 4-byte magic identifying an [`AuthKeyMessage`] on the wire.
/// Chosen to be distinct from [`EXTENSION_MAGIC`] (`RTLX`) so the
/// server can unambiguously tell whether an incoming message is
/// a hello follow-up or stray protocol garbage. First byte is
/// `'R' = 0x52` (same as RTLX) — doesn't matter here since
/// auth-key messages are only read AFTER a hello, never on a
/// fresh connection. #394.
pub const AUTH_KEY_MAGIC: [u8; 4] = *b"RTKA";

/// Maximum length in bytes of an auth key. The issue spec caps it
/// at 256 and the wire format uses `u16` BE for the length, so
/// values beyond this would overflow the signaled size. 32-byte
/// URL-safe base64 (the server-generated default) encodes to
/// ~43 chars; 256 leaves plenty of headroom for user-chosen
/// phrases, while staying small enough that the message fits in
/// one TCP segment on every path MTU we care about. #394.
pub const MAX_AUTH_KEY_LEN: usize = 256;

/// Serialized size of the fixed [`AuthKeyMessage`] header prefix
/// (magic + length field). The total on-wire size is this plus
/// the key bytes themselves. Named so `sniff_auth_key` can
/// `read_exact(AUTH_KEY_HEADER_LEN)` without a magic number.
pub const AUTH_KEY_HEADER_LEN: usize = 4 + 2;

/// Maximum total on-wire size of an [`AuthKeyMessage`] — header
/// plus a [`MAX_AUTH_KEY_LEN`]-byte key. Buffer size hint for
/// server-side reads; values beyond this are a protocol error.
pub const MAX_AUTH_KEY_MESSAGE_LEN: usize = AUTH_KEY_HEADER_LEN + MAX_AUTH_KEY_LEN;

/// Client → server auth-key follow-up. Sent immediately after a
/// [`ClientHello`] that set the [`FLAG_HAS_AUTH`] bit, or in
/// response to a [`Status::AuthRequired`] server message. The
/// server validates the key using a constant-time compare and
/// either proceeds to the normal role-admission flow (on match)
/// or closes the connection with [`Status::AuthFailed`] (on
/// mismatch). #394.
///
/// # Wire layout
///
/// ```text
/// off  size    field
/// 0    4       magic = "RTKA"
/// 4    2       key_len (u16 BE)  — in range 1..=MAX_AUTH_KEY_LEN
/// 6    key_len key bytes         — raw, no encoding
/// ```
///
/// Length is big-endian for consistency with
/// `sdr_server_rtltcp::protocol` (upstream `rtl_tcp.c` uses BE
/// for the 4-byte param in its 5-byte command frames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthKeyMessage {
    /// Raw key bytes. Length is signaled by the on-wire `key_len`
    /// field; the crate enforces `1..=MAX_AUTH_KEY_LEN` at parse
    /// time. Not a `String` because auth keys aren't required to
    /// be valid UTF-8 — the canonical server-generated form is
    /// URL-safe base64, but user-supplied keys might be hex,
    /// ASCII passphrases, or arbitrary bytes.
    pub key: Vec<u8>,
}

impl AuthKeyMessage {
    /// Serialize to its `6 + key.len()` byte wire representation.
    /// Returns `None` when `key.is_empty()` (auth keys must carry
    /// at least one byte; zero-length keys would be trivially
    /// matched by the empty-string-matches-empty-string case and
    /// defeat the auth gate) or `key.len() > MAX_AUTH_KEY_LEN`.
    #[must_use]
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        if self.key.is_empty() || self.key.len() > MAX_AUTH_KEY_LEN {
            return None;
        }
        let mut out = Vec::with_capacity(AUTH_KEY_HEADER_LEN + self.key.len());
        out.extend_from_slice(&AUTH_KEY_MAGIC);
        // `self.key.len() <= MAX_AUTH_KEY_LEN (256) < u16::MAX` so
        // the `as u16` cast is lossless. Guarded by the bounds
        // check above.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "key length bounded by MAX_AUTH_KEY_LEN (256)"
        )]
        let len = self.key.len() as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&self.key);
        Some(out)
    }

    /// Parse from a byte slice. Returns `None` on:
    /// - slice shorter than `AUTH_KEY_HEADER_LEN`
    /// - bad magic (not `"RTKA"`)
    /// - `key_len == 0` (empty keys rejected per `to_bytes`)
    /// - `key_len > MAX_AUTH_KEY_LEN`
    /// - slice length doesn't match `header + key_len`
    ///
    /// The caller is responsible for having read exactly the
    /// right number of bytes — servers should first `read_exact`
    /// the 6-byte header to decode `key_len`, then `read_exact`
    /// that many more bytes.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < AUTH_KEY_HEADER_LEN {
            return None;
        }
        if bytes[..AUTH_KEY_MAGIC.len()] != AUTH_KEY_MAGIC {
            return None;
        }
        let key_len = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
        if key_len == 0 || key_len > MAX_AUTH_KEY_LEN {
            return None;
        }
        if bytes.len() != AUTH_KEY_HEADER_LEN + key_len {
            return None;
        }
        Some(Self {
            key: bytes[AUTH_KEY_HEADER_LEN..].to_vec(),
        })
    }

    /// Decode just the `key_len` field from the header bytes.
    /// Returns `None` on bad magic or out-of-range length.
    /// Servers call this after `read_exact(6)` to know how many
    /// more bytes to read before calling [`Self::from_bytes`] on
    /// the full buffer.
    #[must_use]
    pub fn parse_header_len(header: &[u8; AUTH_KEY_HEADER_LEN]) -> Option<u16> {
        if header[..AUTH_KEY_MAGIC.len()] != AUTH_KEY_MAGIC {
            return None;
        }
        let len = u16::from_be_bytes([header[4], header[5]]);
        let len_usize = len as usize;
        if len_usize == 0 || len_usize > MAX_AUTH_KEY_LEN {
            return None;
        }
        Some(len)
    }
}

/// How long the server waits for an [`AuthKeyMessage`] follow-up
/// after sending [`Status::AuthRequired`] to a client that didn't
/// set [`FLAG_HAS_AUTH`] on its hello. 5 seconds matches the
/// issue spec and is long enough that a UI-driven "paste the
/// key" flow can land within the window, but short enough that a
/// silent-client DOS can't wedge the accept thread. #394.
pub const AUTH_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// when the magic doesn't match, the schema version doesn't
    /// match [`PROTOCOL_VERSION`], or any enum-typed byte is
    /// unknown. Callers surface `None` as a protocol error and
    /// drop the connection — a peer built for a future wire
    /// layout should trigger a clean version break rather than
    /// silent mis-negotiation as v1. Per CodeRabbit round 3 on
    /// PR #399.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; SERVER_EXTENSION_LEN]) -> Option<Self> {
        if bytes[..EXTENSION_MAGIC.len()] != EXTENSION_MAGIC {
            return None;
        }
        // Strict version gate — see the matching comment in
        // `ClientHello::from_bytes`.
        if bytes[7] != PROTOCOL_VERSION {
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
    fn client_hello_rejects_unknown_version() {
        // Regression test for CodeRabbit round 3 on PR #399: a
        // future peer that bumps the wire layout must be rejected
        // so we don't silently mis-negotiate it as v1. Otherwise
        // `PROTOCOL_VERSION` is dead metadata and a clean protocol
        // break turns into a subtle runtime bug.
        let mut bytes = [0u8; CLIENT_HELLO_LEN];
        bytes[..4].copy_from_slice(&EXTENSION_MAGIC);
        bytes[4] = CodecMask::NONE_ONLY.to_wire();
        bytes[5] = Role::Control.to_wire();
        bytes[6] = 0;
        bytes[7] = PROTOCOL_VERSION.wrapping_add(1);
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
    fn server_extension_rejects_unknown_version() {
        // Same rationale as `client_hello_rejects_unknown_version`
        // — a newer server's response with an unknown schema
        // version must cause a clean protocol error at parse time
        // rather than silently coercing forward-compat fields into
        // v1 semantics. Per CodeRabbit round 3 on PR #399.
        let mut bytes = [0u8; SERVER_EXTENSION_LEN];
        bytes[..4].copy_from_slice(&EXTENSION_MAGIC);
        bytes[4] = Codec::None.to_wire();
        bytes[5] = Role::Control.to_wire();
        bytes[6] = Status::Ok.to_wire();
        bytes[7] = PROTOCOL_VERSION.wrapping_add(1);
        assert!(ServerExtension::from_bytes(&bytes).is_none());
    }

    #[test]
    fn server_extension_listener_cap_reached_round_trips() {
        // #392 path: server denies a Listen request because the
        // cap is already filled. Encoded with `granted_role =
        // denied (0xFF)` + `status = ListenerCapReached (4)`.
        // Additive status code — no PROTOCOL_VERSION bump needed
        // because the schema gate already catches peers that
        // read a value they don't know.
        let ext = ServerExtension {
            codec: Codec::None,
            granted_role: None,
            status: Status::ListenerCapReached,
            version: PROTOCOL_VERSION,
        };
        let bytes = ext.to_bytes();
        assert_eq!(bytes[5], GRANTED_ROLE_DENIED);
        assert_eq!(bytes[6], 4);
        assert_eq!(ServerExtension::from_bytes(&bytes), Some(ext));
    }

    #[test]
    fn status_from_wire_covers_all_documented_variants() {
        // Pin the 0/1/2/3/4 → enum mapping. A future addition
        // that reshuffles the discriminants would break over-the-
        // wire compat with already-shipped clients; this test is
        // the trip-wire.
        assert_eq!(Status::from_wire(0), Some(Status::Ok));
        assert_eq!(Status::from_wire(1), Some(Status::ControllerBusy));
        assert_eq!(Status::from_wire(2), Some(Status::AuthRequired));
        assert_eq!(Status::from_wire(3), Some(Status::AuthFailed));
        assert_eq!(Status::from_wire(4), Some(Status::ListenerCapReached));
        assert_eq!(Status::from_wire(5), None);
        assert_eq!(Status::from_wire(255), None);
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

    // ============================================================
    // AuthKeyMessage (#394) wire-format tests.
    // ============================================================

    #[test]
    fn auth_key_message_round_trip() {
        // Minimum viable auth key — a single byte. Exercises the
        // length-field encoding + the header + key concatenation.
        let msg = AuthKeyMessage { key: vec![0x42] };
        let bytes = msg.to_bytes().expect("single-byte key serializes");
        assert_eq!(bytes.len(), AUTH_KEY_HEADER_LEN + 1);
        assert_eq!(&bytes[..4], &AUTH_KEY_MAGIC);
        assert_eq!(&bytes[4..6], &1u16.to_be_bytes());
        assert_eq!(bytes[6], 0x42);
        assert_eq!(AuthKeyMessage::from_bytes(&bytes), Some(msg));
    }

    #[test]
    fn auth_key_message_round_trip_full_length() {
        // 256-byte key (the max) — pins that the u16 length field
        // encodes correctly at the upper bound and `from_bytes`
        // accepts it. Regression guard against an off-by-one that
        // rejects exactly-MAX keys.
        let key: Vec<u8> = (0..MAX_AUTH_KEY_LEN).map(|i| i as u8).collect();
        let msg = AuthKeyMessage { key: key.clone() };
        let bytes = msg.to_bytes().expect("max-length key serializes");
        assert_eq!(bytes.len(), AUTH_KEY_HEADER_LEN + MAX_AUTH_KEY_LEN);
        let len_field = u16::from_be_bytes([bytes[4], bytes[5]]);
        assert_eq!(len_field as usize, MAX_AUTH_KEY_LEN);
        assert_eq!(AuthKeyMessage::from_bytes(&bytes), Some(msg));
    }

    #[test]
    fn auth_key_message_empty_key_rejected_on_encode() {
        // Zero-length key would be trivially matched by an empty
        // expected key on the server side — defeats the auth gate.
        // Reject at serialize time.
        let msg = AuthKeyMessage { key: vec![] };
        assert!(msg.to_bytes().is_none());
    }

    #[test]
    fn auth_key_message_over_max_rejected_on_encode() {
        // Anything > MAX_AUTH_KEY_LEN can't be expressed in the
        // u16 length field's valid range (we cap below u16::MAX so
        // a malicious server can't allocate ~64 KiB per handshake).
        let msg = AuthKeyMessage {
            key: vec![0u8; MAX_AUTH_KEY_LEN + 1],
        };
        assert!(msg.to_bytes().is_none());
    }

    #[test]
    fn auth_key_message_rejects_bad_magic() {
        let mut bytes = vec![0x00, 0x01, 0x02, 0x03]; // not RTKA
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.push(0x42);
        assert!(AuthKeyMessage::from_bytes(&bytes).is_none());
    }

    #[test]
    fn auth_key_message_rejects_zero_length() {
        let mut bytes = AUTH_KEY_MAGIC.to_vec();
        bytes.extend_from_slice(&0u16.to_be_bytes());
        // No trailing bytes — slice length matches header but the
        // length field is 0.
        assert!(AuthKeyMessage::from_bytes(&bytes).is_none());
    }

    #[test]
    fn auth_key_message_rejects_length_mismatch() {
        // Header says 4 bytes but only 2 follow — truncated on
        // the wire. `from_bytes` requires the full message to
        // have been read; servers that decode incrementally must
        // `read_exact(header.key_len)` after parsing the header.
        let mut bytes = AUTH_KEY_MAGIC.to_vec();
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&[0x01, 0x02]);
        assert!(AuthKeyMessage::from_bytes(&bytes).is_none());
    }

    #[test]
    fn auth_key_message_parse_header_len_decodes_valid_range() {
        let mut header = [0u8; AUTH_KEY_HEADER_LEN];
        header[..4].copy_from_slice(&AUTH_KEY_MAGIC);
        header[4..6].copy_from_slice(&42u16.to_be_bytes());
        assert_eq!(AuthKeyMessage::parse_header_len(&header), Some(42));
    }

    #[test]
    fn auth_key_message_parse_header_len_rejects_zero_and_overlong() {
        let mut header = [0u8; AUTH_KEY_HEADER_LEN];
        header[..4].copy_from_slice(&AUTH_KEY_MAGIC);
        // Zero length.
        header[4..6].copy_from_slice(&0u16.to_be_bytes());
        assert!(AuthKeyMessage::parse_header_len(&header).is_none());
        // Length exceeds MAX.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test-only overflow synthesis, caller check is the point"
        )]
        let overlong = (MAX_AUTH_KEY_LEN + 1) as u16;
        header[4..6].copy_from_slice(&overlong.to_be_bytes());
        assert!(AuthKeyMessage::parse_header_len(&header).is_none());
    }

    #[test]
    fn client_hello_has_auth_flag_helper() {
        let with_auth = ClientHello {
            codec_mask: CodecMask::NONE_ONLY,
            role: Role::Control,
            flags: FLAG_HAS_AUTH,
            version: PROTOCOL_VERSION,
        };
        assert!(with_auth.has_auth());
        assert!(!with_auth.request_takeover());

        // Flags are additive — takeover + auth together.
        let both = ClientHello {
            flags: FLAG_HAS_AUTH | FLAG_REQUEST_TAKEOVER,
            ..with_auth
        };
        assert!(both.has_auth());
        assert!(both.request_takeover());

        let without_auth = ClientHello {
            flags: 0,
            ..with_auth
        };
        assert!(!without_auth.has_auth());
    }

    #[test]
    fn flag_bits_are_distinct() {
        // Defense-in-depth: if someone ever adds a third flag bit
        // and accidentally collides with an existing one, this
        // test trips. Each bit must be uniquely assigned.
        assert_eq!(FLAG_REQUEST_TAKEOVER & FLAG_HAS_AUTH, 0);
        assert_ne!(FLAG_REQUEST_TAKEOVER, 0);
        assert_ne!(FLAG_HAS_AUTH, 0);
    }

    #[test]
    fn auth_key_magic_first_byte_distinct_from_legacy_opcodes() {
        // `RTKA`'s first byte is 'R' (0x52), same as
        // EXTENSION_MAGIC. That's fine here because an
        // AuthKeyMessage is only ever read AFTER a hello, never
        // as the first bytes on a fresh connection, so there's
        // no legacy-opcode collision path. Documenting this as
        // a test so a future refactor that re-reads AUTH_KEY_MAGIC
        // at connection-start would trip the >0x0E assertion
        // and the reviewer knows to re-examine the flow.
        assert_eq!(AUTH_KEY_MAGIC[0], b'R');
        assert!(AUTH_KEY_MAGIC[0] > 0x0E);
    }
}
