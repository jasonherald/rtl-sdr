//! Pre-shared-key authentication helpers for the rtl_tcp server
//! (#394).
//!
//! Two responsibilities:
//!
//! 1. **Key generation** — [`generate_random_auth_key`] wraps
//!    [`rand::rngs::OsRng`] to produce a 32-byte key. OS-backed
//!    CSPRNG (reads from `/dev/urandom` on unix, `BCryptGenRandom`
//!    on Windows), so no seed management on our side. 32 bytes =
//!    256 bits of entropy, more than sufficient for the threat
//!    model the issue specifies (casual LAN interlopers, not
//!    nation-state actors).
//!
//! 2. **Constant-time compare** — [`validate_auth_key`] wraps
//!    [`subtle::ConstantTimeEq`] so a byte-by-byte mismatch
//!    doesn't leak timing info about where the keys diverge. A
//!    naive `==` on `&[u8]` short-circuits at the first
//!    differing byte, which (on a LAN attacker's scope) is
//!    visible in the round-trip microseconds — measurable over
//!    thousands of attempts.
//!
//! The server holds the configured key as `Vec<u8>` in
//! [`crate::server::ServerConfig`]; the client hello follow-up
//! lands as an [`crate::extension::AuthKeyMessage`]. This module
//! bridges the two with verification + a helper to produce the
//! expected key at server-start time.

use rand::RngCore;
use subtle::ConstantTimeEq;

/// Size in bytes of the server-generated random key. 32 bytes
/// chosen to match the issue's "URL-safe base64 32-byte" spec —
/// base64-encoding 32 raw bytes yields ~43 display chars, fits on
/// one UI line, and provides 256 bits of entropy. User-supplied
/// keys can be any length in `1..=MAX_AUTH_KEY_LEN`; this is only
/// the default when the server auto-generates.
pub const DEFAULT_AUTH_KEY_LEN: usize = 32;

/// Generate a random auth key using the OS CSPRNG. Returns
/// [`DEFAULT_AUTH_KEY_LEN`] bytes. Intended for:
///
/// - Server-start when the operator enables auth for the first
///   time (and hasn't pasted a pre-existing key from the UI).
/// - Tests that need a realistic key shape.
///
/// OS-CSPRNG means there's no seed state on our side — every
/// call is independent. `rand::rngs::OsRng` is infallible in
/// practice on our targets (the `try_fill_bytes` path would only
/// surface an error on exotic platforms where `/dev/urandom` is
/// unreadable; we don't support those).
#[must_use]
pub fn generate_random_auth_key() -> Vec<u8> {
    let mut buf = vec![0u8; DEFAULT_AUTH_KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

/// Compare `provided` (client-sent) against `expected` (server's
/// configured key) in constant time — the compare walks both
/// slices fully even when they diverge early, so an attacker
/// can't derive "where the keys differ" from the round-trip
/// duration.
///
/// `false` on length mismatch. `true` only when the slices are
/// byte-for-byte equal.
///
/// # Why constant time
///
/// A naive `provided == expected` on `&[u8]` short-circuits at
/// the first differing byte. On a LAN an attacker with a scope
/// can measure the round-trip microseconds and derive each
/// leading byte of the expected key by brute-forcing single-byte
/// positions in turn — a timing oracle. `ConstantTimeEq` runs
/// the XOR over every byte regardless of early divergence, so
/// the return duration is a function of `min(lenA, lenB)` only.
#[must_use]
pub fn validate_auth_key(provided: &[u8], expected: &[u8]) -> bool {
    // Empty keys are rejected at every layer of the auth gate
    // — wire format, FFI translator, and here. Defense in depth:
    // if an upstream layer ever regresses and hands us an empty
    // `expected` (misconfigured `ServerConfig { auth_key:
    // Some(vec![]) }` constructed by hand, say), this check
    // makes sure the validator doesn't silently accept every
    // peer with a matching-empty provided. `ct_eq(empty, empty)`
    // would otherwise return `true` and defeat the gate. Per
    // `CodeRabbit` round 1 on PR #405.
    if provided.is_empty() || expected.is_empty() {
        return false;
    }
    // `ConstantTimeEq::ct_eq` returns a `Choice` (wire byte 1 or
    // 0) that we convert to `bool` via `Into::into`. The length-
    // mismatch check is intentionally NOT constant-time — a
    // length difference isn't a secret (the wire format
    // advertises `key_len` in cleartext), and short-circuiting
    // here avoids the edge case where `ct_eq` panics on
    // differently-sized slices.
    if provided.len() != expected.len() {
        return false;
    }
    provided.ct_eq(expected).into()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::extension::MAX_AUTH_KEY_LEN;

    #[test]
    fn generate_random_auth_key_returns_default_length() {
        // Length contract — 32 bytes matches the issue's
        // URL-safe base64 32-byte spec. If a future change
        // bumps the default, this test is the trip-wire that
        // forces re-reading the generator's callers (wire-format
        // `MAX_AUTH_KEY_LEN` still accepts up to 256, so the
        // increase is safe from that angle, but the display
        // layer in #395 would need to resize the key field).
        let key = generate_random_auth_key();
        assert_eq!(key.len(), DEFAULT_AUTH_KEY_LEN);
    }

    #[test]
    fn generate_random_auth_key_fits_wire_bound() {
        // Every generator output must fit the
        // `AuthKeyMessage`-imposed upper bound. Otherwise
        // serialization downstream (`msg.to_bytes()`) would
        // reject our own server-generated key.
        let key = generate_random_auth_key();
        assert!(key.len() <= MAX_AUTH_KEY_LEN);
        assert!(!key.is_empty(), "generator must not produce empty keys");
    }

    #[test]
    fn generate_random_auth_key_entropy_smoke_check() {
        // Cheap sanity check that the OsRng is actually doing
        // something — two consecutive calls should produce
        // different bytes. A broken rand impl (e.g., a stub
        // returning zeros) would fail this.
        let a = generate_random_auth_key();
        let b = generate_random_auth_key();
        assert_ne!(
            a, b,
            "two successive OsRng-based key generations must differ"
        );
    }

    #[test]
    fn validate_auth_key_accepts_matching_keys() {
        let key = vec![0xAA, 0xBB, 0xCC, 0xDD];
        assert!(validate_auth_key(&key, &key));
    }

    #[test]
    fn validate_auth_key_rejects_mismatch() {
        let provided = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let expected = vec![0xAA, 0xBB, 0xCC, 0xDE]; // last byte differs
        assert!(!validate_auth_key(&provided, &expected));
    }

    #[test]
    fn validate_auth_key_rejects_length_mismatch() {
        let shorter = vec![0xAA, 0xBB];
        let longer = vec![0xAA, 0xBB, 0xCC];
        assert!(!validate_auth_key(&shorter, &longer));
        assert!(!validate_auth_key(&longer, &shorter));
    }

    #[test]
    fn validate_auth_key_rejects_empty_vs_empty() {
        // Defense-in-depth: empty keys are rejected at every
        // layer of the auth gate. The wire format rejects
        // zero-length `AuthKeyMessage`s at parse time and the
        // FFI translator rejects `auth_key_len == 0` with
        // non-null pointer — but a direct `ServerConfig {
        // auth_key: Some(vec![]) }` construction could slip a
        // naked empty Vec through. The validator's own empty
        // guard means that even in that pathological case, no
        // peer authenticates. Per `CodeRabbit` round 1 on
        // PR #405 — the previous test documented the
        // `ct_eq(empty, empty) == true` invariant as an
        // assumption-chain note; that's now a real guard.
        let empty_a: Vec<u8> = Vec::new();
        let empty_b: Vec<u8> = Vec::new();
        assert!(
            !validate_auth_key(&empty_a, &empty_b),
            "validator must reject empty-vs-empty (defense in depth)"
        );
    }

    #[test]
    fn validate_auth_key_rejects_empty_provided() {
        // Client sends zero-length key against a non-empty
        // expected — should be rejected. Covers the wire-path
        // regression where a bad parser hands us Vec::new() as
        // the provided key.
        let provided: Vec<u8> = Vec::new();
        let expected = vec![0xAA, 0xBB];
        assert!(!validate_auth_key(&provided, &expected));
    }

    #[test]
    fn validate_auth_key_rejects_empty_expected() {
        // Server config mistakenly carries an empty key (should
        // have been caught upstream, but defense in depth).
        let provided = vec![0xAA, 0xBB];
        let expected: Vec<u8> = Vec::new();
        assert!(!validate_auth_key(&provided, &expected));
    }

    #[test]
    fn validate_auth_key_with_generated_key_round_trips() {
        // End-to-end smoke: generator output + validator should
        // accept itself.
        let key = generate_random_auth_key();
        assert!(validate_auth_key(&key, &key));
    }
}
