//! Client-side `rtl_tcp` auth-key keyring plumbing: per-server
//! entry naming and the load/save/clear OS-keyring round-trip.
//! Split out of `window/source/connect.rs` per the Codacy
//! large-file gate (#846).

/// Keyring-entry prefix for per-server **client** auth keys. The
/// full entry name is `{prefix}-{host}:{port}` — per-server
/// so the user can save distinct keys for distinct servers on
/// the LAN (different owners, different rotation schedules).
/// Kept distinct from `KEYRING_KEY_AUTH_KEY` (which stores the
/// local server's own key, single entry) so neither surface
/// ever reads the other's bytes by accident. Per issue #396.
pub(super) const KEYRING_KEY_CLIENT_AUTH_KEY_PREFIX: &str = "rtl_tcp-client-auth-key-";

/// Build the keyring entry name for a client-side saved key
/// keyed by the server's `host:port` identity. Matches the
/// identity `FavoriteEntry.key` uses, so the keyring entry
/// survives server rename / nickname change. Per issue #396.
pub(super) fn client_auth_key_entry_name(host: &str, port: u16) -> String {
    format!("{KEYRING_KEY_CLIENT_AUTH_KEY_PREFIX}{host}:{port}")
}

/// Load the saved auth key for the given `rtl_tcp` server, if
/// the user previously connected successfully with a key
/// against this `host:port`. Returns `None` for missing /
/// corrupt / keyring-unavailable cases — callers treat that
/// as "ask the user for a key" rather than silently connecting
/// without one. Per issue #396.
pub(super) fn load_client_auth_key_from_keyring(host: &str, port: u16) -> Option<Vec<u8>> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_SERVICE, auth_key_from_hex};

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    match store.get(&entry) {
        Ok(Some(hex)) => {
            let Some(bytes) = auth_key_from_hex(&hex) else {
                tracing::warn!(
                    entry = %entry,
                    "rtl_tcp client auth key in keyring is malformed hex; treating as missing"
                );
                return None;
            };
            Some(bytes)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%e, entry = %entry, "rtl_tcp client auth key keyring read failed");
            None
        }
    }
}

/// Save a successfully-used client auth key for the given
/// server to the OS keyring. Called AFTER a successful
/// auth-required connect so the user doesn't have to re-enter
/// the key on subsequent reconnects to the same server. A
/// keyring write failure is non-fatal — the current session
/// still works; the next launch will just prompt for the key
/// again. Per issue #396.
pub(super) fn save_client_auth_key_to_keyring(
    host: &str,
    port: u16,
    bytes: &[u8],
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::{KEYRING_SERVICE, auth_key_to_hex};

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    store.set(&entry, &auth_key_to_hex(bytes))
}

/// Delete a saved client auth key for the given server. Called
/// from the UI when the user explicitly clears the key (e.g.
/// the server regenerated on the other end and the old key no
/// longer works; clearing avoids auto-sending the dead key on
/// every reconnect attempt). Missing-entry is treated as
/// success — the goal is "there is no saved key after this
/// call," which a missing entry already satisfies. Per #396.
pub(super) fn clear_client_auth_key_from_keyring(
    host: &str,
    port: u16,
) -> Result<(), sdr_config::keyring_store::KeyringError> {
    use sdr_config::KeyringStore;

    use crate::sidebar::server_panel::KEYRING_SERVICE;

    let entry = client_auth_key_entry_name(host, port);
    let store = KeyringStore::new(KEYRING_SERVICE);
    store.delete(&entry)
}
