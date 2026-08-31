//! The #394 connection auth gate: eager (`has_auth=true` hello
//! follow-up) and lazy (challenge → reply on the same socket)
//! paths, run BEFORE role admission. Split out of `setup.rs` per
//! the 500-NLOC file gate (Codacy round on PR #880); behavior is
//! unchanged.

use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};

use librtlsdr_rs::RtlSdrDevice;

use crate::extension::Status;
use crate::server::accept::handshake::{
    send_denied_response, send_extension_only, sniff_auth_key_message,
};

use super::HelloOutcome;

/// Auth gate (#394). Runs BEFORE role admission — an unauthenticated
/// client shouldn't even be evaluated for role because the wire
/// response would leak server state (role grants or cap status) to
/// an attacker probing the slot. The four outcomes:
///
///   - No auth required: skip entirely, fall through to role
///     admission as-is.
///   - Auth required + vanilla client: bare TCP FIN, no bytes.
///     Vanilla has no wire field to carry a key, so they can't
///     participate. Same signal as "server not there" from the
///     legacy client's POV.
///   - Auth required + RTLX + has_auth (eager): read the follow-up
///     `AuthKeyMessage` with a bounded timeout, constant-time
///     compare against the configured key.
///     - Match: continue to role admission.
///     - Mismatch / malformed / timeout / eof: send dongle_info_t
///       + `ServerExtension(status=AuthFailed)` and close.
///   - Auth required + RTLX + !has_auth (lazy, per #394 spec): see
///     [`run_lazy_auth_challenge`].
///
/// If `has_auth` is set but auth isn't configured, we STILL read
/// the AuthKeyMessage from the stream to keep the post-hello byte
/// position in sync (the client doesn't know our config and sent
/// the key based on its own); we just discard it without
/// validation.
///
/// Returns `Some(dongle_info_sent)` to continue to role admission —
/// the flag threads through so the lazy path's initial dongle_info_t
/// emission is visible to role admission (which must skip the
/// duplicate send in both the granted and denied follow-up
/// branches). `None` = the client was denied/dropped (any response
/// already sent). Split out per the 50-NLOC gate (Codacy round on
/// PR #880).
pub(super) fn run_auth_gate(
    stream: &TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    auth_key: Option<&[u8]>,
    hello: &HelloOutcome,
) -> Option<bool> {
    if hello.seen && hello.has_auth {
        run_eager_auth(stream, peer, device, auth_key, hello.version)
    } else if let Some(expected) = auth_key {
        // Client didn't set has_auth but the server requires auth.
        if hello.seen {
            run_lazy_auth_challenge(stream, peer, device, expected, hello.version)
        } else {
            // Vanilla client: can't authenticate, so there's
            // nothing meaningful to tell them. Bare TCP FIN.
            tracing::info!(
                %peer,
                "rtl_tcp vanilla client denied — auth required"
            );
            None
        }
    } else {
        Some(false)
    }
}

/// Eager auth path: the RTLX client set `has_auth=true`, so an
/// `AuthKeyMessage` follow-up is next on the wire. Read it
/// regardless of whether auth is required — the bytes must be
/// consumed either way so the stream position stays correct — then
/// constant-time compare against the configured key when one is
/// set. Returns `Some(false)` (no dongle_info_t emitted yet) to
/// continue to role admission, `None` when the client was denied.
/// Split out of [`run_auth_gate`] per the 50-NLOC gate (Codacy
/// round on PR #880).
fn run_eager_auth(
    stream: &TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    auth_key: Option<&[u8]>,
    hello_version: u8,
) -> Option<bool> {
    match sniff_auth_key_message(stream) {
        Ok(msg) => {
            if let Some(expected) = auth_key {
                if !crate::auth::validate_auth_key(&msg.key, expected) {
                    tracing::info!(
                        %peer,
                        "rtl_tcp auth key mismatch — denying client"
                    );
                    send_denied_response(stream, peer, device, Status::AuthFailed, hello_version);
                    return None;
                }
                tracing::info!(%peer, "rtl_tcp auth key validated");
            } else {
                // Client sent a key to a server that doesn't
                // require one — fine, just ignore it. Keeps
                // the wire-protocol compat flexible: clients
                // can always send has_auth=true without
                // knowing the server's config.
                tracing::debug!(
                    %peer,
                    "rtl_tcp client sent auth key but server doesn't require one — ignored"
                );
            }
            Some(false)
        }
        Err(e) => {
            tracing::info!(
                %peer,
                %e,
                "rtl_tcp auth key follow-up unreadable — denying client"
            );
            if auth_key.is_some() {
                send_denied_response(stream, peer, device, Status::AuthFailed, hello_version);
            }
            // If auth wasn't required but the client promised
            // a follow-up (has_auth=true) and didn't deliver,
            // the stream position is wrong either way — drop.
            None
        }
    }
}

/// Lazy auth path per the #394 spec: send dongle_info_t +
/// `ServerExtension(AuthRequired)` and keep the socket open so a
/// compliant client can reply with `AuthKeyMessage` on the same
/// connection. The peer has `AUTH_REPLY_TIMEOUT` to deliver the
/// key; [`sniff_auth_key_message`] enforces the bound with an
/// absolute deadline. Per `CodeRabbit` round 3 on PR #405.
///
/// On match, returns `Some(true)` — dongle_info_t is already on the
/// wire, so the granted path skips the duplicate header write and
/// any role-denial switches to `send_extension_only`. On mismatch /
/// timeout / parse error, sends a follow-up
/// `ServerExtension(AuthFailed)` (extension-only, no second
/// dongle_info_t — the client would misread a duplicate header as a
/// second handshake) and returns `None`. Split out per the 50-NLOC
/// gate (Codacy round on PR #880).
fn run_lazy_auth_challenge(
    stream: &TcpStream,
    peer: SocketAddr,
    device: &Arc<Mutex<RtlSdrDevice>>,
    expected: &[u8],
    hello_version: u8,
) -> Option<bool> {
    tracing::info!(
        %peer,
        "rtl_tcp auth required but client didn't send key — sending AuthRequired (lazy path)"
    );
    send_denied_response(stream, peer, device, Status::AuthRequired, hello_version);

    let auth_ok = match sniff_auth_key_message(stream) {
        Ok(msg) => {
            if crate::auth::validate_auth_key(&msg.key, expected) {
                tracing::info!(
                    %peer,
                    "rtl_tcp lazy auth key validated"
                );
                true
            } else {
                tracing::info!(
                    %peer,
                    "rtl_tcp lazy auth key mismatch — denying"
                );
                false
            }
        }
        Err(e) => {
            tracing::info!(
                %peer,
                %e,
                "rtl_tcp lazy auth follow-up unreadable — denying"
            );
            false
        }
    };
    if !auth_ok {
        // dongle_info_t already on the wire — send only the 8-byte
        // `ServerExtension(AuthFailed)` so the client doesn't
        // misread a duplicate header as a second handshake.
        send_extension_only(stream, peer, Status::AuthFailed, hello_version);
        return None;
    }
    // Match → fall through to role admission with
    // `dongle_info_sent = true`.
    Some(true)
}
