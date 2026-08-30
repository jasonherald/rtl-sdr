//! Unit tests for [`super::auth_key_ready_to_start`] — the hard
//! guard that keeps `Server::start` from ever receiving
//! `auth_key: None` while "Require key" is active (issue #845,
//! `CodeRabbit` round 1 on PR #873, CWE-306).

use super::auth_key_ready_to_start;

/// A placeholder key byte slice for the "key already loaded" cases.
/// Contents don't matter — the guard only cares whether a key is
/// present, not its value.
const FIXTURE_KEY: &[u8] = &[0xAB, 0xCD, 0xEF, 0x01];

#[test]
fn auth_off_without_key_is_ready() {
    // Auth isn't required, so a missing key is irrelevant —
    // `Server::start` will pass `auth_key: None` and that's
    // correct behavior, not the bug this guard exists to catch.
    assert!(auth_key_ready_to_start(false, None));
}

#[test]
fn auth_off_with_key_is_ready() {
    // A stale/cached key sitting around while auth is off is still
    // fine to proceed — `build_server_config_from_panel` drops it
    // when `auth_require_row` isn't active.
    assert!(auth_key_ready_to_start(false, Some(FIXTURE_KEY)));
}

#[test]
fn auth_required_with_key_is_ready() {
    // The normal path: "Require key" is on and the keyring load
    // already landed.
    assert!(auth_key_ready_to_start(true, Some(FIXTURE_KEY)));
}

#[test]
fn auth_required_without_key_is_blocked() {
    // The exact combination that must never reach `Server::start`
    // — "Require key" active but no bytes yet (async keyring load
    // still pending). Letting this through starts an unauthenticated
    // server while the UI claims auth is required.
    assert!(!auth_key_ready_to_start(true, None));
}
