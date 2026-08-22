//! Secure credential storage via the OS keyring.
//!
//! Uses `keyring` crate which delegates to:
//! - **Linux**: Secret Service D-Bus API (GNOME Keyring, `KeePassXC`)
//! - **macOS**: Keychain

/// Error type for keyring operations.
#[derive(Debug, thiserror::Error)]
pub enum KeyringError {
    #[error("no secure storage available — install GNOME Keyring or KeePassXC")]
    NoBackend,
    #[error("keyring error: {0}")]
    Platform(String),
}

/// Thin wrapper around the OS keyring for storing secrets.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), KeyringError> {
        let entry = self.entry(key)?;
        entry.set_password(value).map_err(map_keyring_error)
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, KeyringError> {
        let entry = self.entry(key)?;
        match entry.get_password() {
            Ok(val) => Ok(Some(val)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_keyring_error(e)),
        }
    }

    pub fn delete(&self, key: &str) -> Result<(), KeyringError> {
        let entry = self.entry(key)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_error(e)),
        }
    }

    /// Check whether a credential exists for the given key.
    ///
    /// The Secret Service API has no metadata-only existence query, so
    /// this reads the secret (and on a locked GNOME Keyring triggers the
    /// unlock prompt). Call it off the main thread, or cache the result.
    ///
    /// # Errors
    ///
    /// Returns a [`KeyringError`] if the keyring backend is unavailable.
    pub fn has(&self, key: &str) -> Result<bool, KeyringError> {
        self.get(key).map(|val| val.is_some())
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, KeyringError> {
        keyring::Entry::new(&self.service, key).map_err(map_keyring_error)
    }
}

/// Collapse a `keyring` error into our error type.
///
/// The "no usable backend" shapes map to [`KeyringError::NoBackend`] so
/// callers fall back to the in-memory / JSON path instead of hard-failing.
/// Both can surface from `Entry::new` **and** from the get/set/delete
/// operations — the Secret Service / D-Bus connection is lazy, so a backend
/// that exists at entry-creation time can still be locked or unreachable
/// when an operation actually runs:
///   * `NoStorageAccess` — a backend is registered but locked or unreachable
///     (a locked GNOME Keyring, a headless box with no Secret Service
///     session, a missing `DBUS_SESSION_BUS_ADDRESS`, …).
///   * `NoDefaultStore` — keyring 4's `v1` auto-registration couldn't wire
///     any platform store (e.g. an unsupported target). Per #681.
///
/// Everything else is an opaque `Platform` error.
fn map_keyring_error(e: keyring::Error) -> KeyringError {
    match e {
        keyring::Error::NoStorageAccess(_) | keyring::Error::NoDefaultStore => {
            KeyringError::NoBackend
        }
        other => KeyringError::Platform(other.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Real round-trip against the OS keyring — proves the keyring 4 `v1`
    /// backend actually persists, the direct guard against the silent
    /// no-op-store trap from PR #346 that `v1` auto-registration is meant
    /// to prevent. `#[ignore]`d because CI runners have no Secret Service /
    /// Keychain session; run locally with
    /// `cargo test -p sdr-config -- --ignored`.
    #[test]
    #[ignore = "requires a real OS keyring (Secret Service / Keychain)"]
    fn keyring_round_trip_against_real_backend() {
        let store = KeyringStore::new("sdr-rs-keyring-selftest");
        let key = "round-trip-probe-681";
        let secret = "s3cr3t-value";

        store.set(key, secret).expect("set on a real backend");
        assert_eq!(
            store.get(key).expect("get on a real backend").as_deref(),
            Some(secret),
            "value must persist and round-trip through the keyring",
        );

        store.delete(key).expect("delete on a real backend");
        assert_eq!(
            store.get(key).expect("get after delete"),
            None,
            "entry must be gone after delete",
        );
    }
}
