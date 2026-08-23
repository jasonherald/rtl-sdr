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

/// Protocol schema version **1** — the original (#307) shape.
/// Callers that only opt into compression or role (#392) / takeover
/// (#393) emit this version: the features are additive within the
/// same 8-byte hello layout and pre-auth servers handle them
/// without knowing about `FLAG_HAS_AUTH` or the
/// `AuthKeyMessage` follow-up.
pub const PROTOCOL_VERSION_V1: u8 = 1;

/// Protocol schema version **2** — introduced in #394 for
/// pre-shared-key auth. Clients that set [`FLAG_HAS_AUTH`] MUST
/// emit this version so pre-v2 servers reject the hello cleanly
/// at parse time rather than accepting it and then reading the
/// queued `AuthKeyMessage` bytes as garbage legacy commands. The
/// wire layout is byte-for-byte identical to v1; the version
/// field is the gate, not a different shape. Per `CodeRabbit`
/// round 1 on PR #405.
pub const PROTOCOL_VERSION_V2: u8 = 2;

/// Newest protocol schema version this crate emits. Clients that
/// need the latest features (auth) write this. Clients that don't
/// — compression-only, takeover-only, or plain — write
/// [`PROTOCOL_VERSION_V1`] to stay backward-compatible with
/// older servers. Bumping this is a breaking wire change, so it
/// happens at well-known sub-issue boundaries.
pub const PROTOCOL_VERSION: u8 = PROTOCOL_VERSION_V2;

/// Versions this crate accepts on parse paths
/// ([`ClientHello::from_bytes`], [`ServerExtension::from_bytes`]).
/// Widens as new versions are introduced so old peers continue to
/// interoperate with new peers for the feature subset they share.
/// A v1 client talking to a v2 server sends a v1 hello, server
/// accepts (this array includes v1), server echoes v1 on the
/// response. Per `CodeRabbit` round 1 on PR #405.
pub const SUPPORTED_VERSIONS: &[u8] = &[PROTOCOL_VERSION_V1, PROTOCOL_VERSION_V2];

/// Pick the minimum-viable protocol version for a hello that
/// carries the given flags. Clients use this helper so they only
/// bump the version when a feature actually requires it —
/// compression-only / takeover-only hellos stay v1 (backward-
/// compat with pre-#394 servers); auth-bearing hellos go v2
/// (pre-#394 servers reject cleanly at parse time). Per
/// `CodeRabbit` round 1 on PR #405.
#[must_use]
pub fn required_protocol_version(flags: u8) -> u8 {
    if flags & FLAG_HAS_AUTH != 0 {
        PROTOCOL_VERSION_V2
    } else {
        PROTOCOL_VERSION_V1
    }
}

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
    /// if the magic doesn't match, the schema version isn't in
    /// [`SUPPORTED_VERSIONS`], or the role byte is unknown.
    /// Callers surface `None` as a protocol error and drop the
    /// client — letting a peer built for a future wire layout
    /// through would cause silent mis-negotiation rather than a
    /// clean version break.
    ///
    /// Version gate widened from the #399-era strict `==
    /// PROTOCOL_VERSION` check to accept any member of
    /// `SUPPORTED_VERSIONS`. Ensures pre-#394 clients emitting
    /// v1 hellos interoperate with this crate's v2-era servers:
    /// v1 is still a first-class parse result, only the wire
    /// layout stayed identical and the flag byte's
    /// `FLAG_HAS_AUTH` is only meaningful when `version ==
    /// PROTOCOL_VERSION_V2`. Per `CodeRabbit` round 3 on PR #399
    /// (initial strict gate) + round 1 on PR #405 (widen for
    /// multi-version support).
    #[must_use]
    pub fn from_bytes(bytes: &[u8; CLIENT_HELLO_LEN]) -> Option<Self> {
        if bytes[..EXTENSION_MAGIC.len()] != EXTENSION_MAGIC {
            return None;
        }
        if !SUPPORTED_VERSIONS.contains(&bytes[7]) {
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
    /// when the magic doesn't match, the schema version isn't in
    /// [`SUPPORTED_VERSIONS`], or any enum-typed byte is unknown.
    /// Callers surface `None` as a protocol error and drop the
    /// connection — a peer built for a future wire layout should
    /// trigger a clean version break rather than silent mis-
    /// negotiation. Per `CodeRabbit` round 3 on PR #399 (initial
    /// strict gate) + round 1 on PR #405 (widen for
    /// multi-version support).
    #[must_use]
    pub fn from_bytes(bytes: &[u8; SERVER_EXTENSION_LEN]) -> Option<Self> {
        if bytes[..EXTENSION_MAGIC.len()] != EXTENSION_MAGIC {
            return None;
        }
        if !SUPPORTED_VERSIONS.contains(&bytes[7]) {
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
mod tests;
