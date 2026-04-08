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
    #[error("credential not found")]
    NotFound,
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
        entry
            .set_password(value)
            .map_err(|e| KeyringError::Platform(e.to_string()))
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, KeyringError> {
        let entry = self.entry(key)?;
        match entry.get_password() {
            Ok(val) => Ok(Some(val)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(KeyringError::Platform(e.to_string())),
        }
    }

    pub fn delete(&self, key: &str) -> Result<(), KeyringError> {
        let entry = self.entry(key)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(KeyringError::Platform(e.to_string())),
        }
    }

    /// Check whether a credential exists for the given key.
    ///
    /// # Errors
    ///
    /// Returns a [`KeyringError`] if the keyring backend is unavailable.
    pub fn has(&self, key: &str) -> Result<bool, KeyringError> {
        self.get(key).map(|val| val.is_some())
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, KeyringError> {
        keyring::Entry::new(&self.service, key).map_err(|e| match e {
            keyring::Error::NoStorageAccess(_) => KeyringError::NoBackend,
            other => KeyringError::Platform(other.to_string()),
        })
    }
}
