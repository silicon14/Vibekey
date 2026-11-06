//! Abstraction over the platform password store.

use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret storage is not available: {0}")]
    Unavailable(String),
    #[error("failed to decrypt the stored password")]
    Decrypt,
    #[error("failed to encrypt the password: {0}")]
    Encrypt(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Stores the WebDAV password. On Windows this is backed by DPAPI and a file
/// below the application directory; the password is never part of the
/// synchronised configuration file.
pub trait SecretStore: Send + Sync {
    fn store_password(&self, password: &str) -> Result<(), SecretError>;
    fn load_password(&self) -> Result<Option<String>, SecretError>;
    fn clear_password(&self) -> Result<(), SecretError>;
}

/// In-memory store, useful for tests and as a fallback.
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    password: Mutex<Option<String>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemorySecretStore {
    fn store_password(&self, password: &str) -> Result<(), SecretError> {
        *self.password.lock().unwrap() = Some(password.to_string());
        Ok(())
    }

    fn load_password(&self) -> Result<Option<String>, SecretError> {
        Ok(self.password.lock().unwrap().clone())
    }

    fn clear_password(&self) -> Result<(), SecretError> {
        *self.password.lock().unwrap() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let store = MemorySecretStore::new();
        assert_eq!(store.load_password().unwrap(), None);
        store.store_password("hunter2").unwrap();
        assert_eq!(store.load_password().unwrap().as_deref(), Some("hunter2"));
        store.clear_password().unwrap();
        assert_eq!(store.load_password().unwrap(), None);
    }
}
