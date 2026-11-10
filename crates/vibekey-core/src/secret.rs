//! Abstraction over the platform password store.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret storage is not available: {0}")]
    Unavailable(String),
    #[error("failed to decrypt the stored password")]
    Decrypt,
    #[error("failed to encrypt the password: {0}")]
    Encrypt(String),
    #[error(
        "complete the pending configuration encryption password rotation before replacing it again"
    )]
    RotationPending,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Independent credentials whose lifetimes and purposes must not be confused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    /// HTTP Basic authentication for the WebDAV service.
    WebDav,
    /// Password-based encryption of the configuration body.
    ConfigEncryption,
}

/// Stores passwords outside the synchronised configuration. On Windows this is
/// backed by DPAPI. A replacement configuration-encryption password retains the
/// previous value until the caller confirms a complete successful sync; this
/// allows an old encrypted remote to be re-encrypted without a lockout.
pub trait SecretStore: Send + Sync {
    fn store(&self, kind: SecretKind, password: &str) -> Result<(), SecretError>;
    fn load(&self, kind: SecretKind) -> Result<Option<String>, SecretError>;
    fn clear(&self, kind: SecretKind) -> Result<(), SecretError>;

    /// Previous password retained only for configuration encryption rotation.
    fn load_previous(&self, _kind: SecretKind) -> Result<Option<String>, SecretError> {
        Ok(None)
    }

    /// Read one coherent rotation pair. Platforms with a combined blob should
    /// override this so a concurrent settings save cannot split the pair.
    fn load_encryption_passwords(&self) -> Result<(Option<String>, Option<String>), SecretError> {
        let current = self.load(SecretKind::ConfigEncryption)?;
        let previous = self.load_previous(SecretKind::ConfigEncryption)?;
        Ok((current, previous))
    }

    /// Delete the previous password after every remote operation succeeded and
    /// the state was persisted. Other secret kinds have no rotation state.
    fn finish_password_rotation(
        &self,
        _kind: SecretKind,
        _expected_current: &str,
        _expected_previous: Option<&str>,
    ) -> Result<(), SecretError> {
        Ok(())
    }
}

/// In-memory store, useful for tests and as a fallback.
#[derive(Default)]
pub struct MemorySecretStore {
    passwords: Mutex<HashMap<SecretKind, String>>,
    previous_encryption: Mutex<Option<String>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemorySecretStore {
    fn store(&self, kind: SecretKind, password: &str) -> Result<(), SecretError> {
        let mut passwords = self.passwords.lock().unwrap();
        if kind == SecretKind::ConfigEncryption {
            if let Some(existing) = passwords.get(&kind) {
                if existing != password {
                    let mut previous = self.previous_encryption.lock().unwrap();
                    if previous.is_some() {
                        return Err(SecretError::RotationPending);
                    }
                    *previous = Some(existing.clone());
                }
            }
        }
        passwords.insert(kind, password.to_string());
        Ok(())
    }

    fn load(&self, kind: SecretKind) -> Result<Option<String>, SecretError> {
        Ok(self.passwords.lock().unwrap().get(&kind).cloned())
    }

    fn clear(&self, kind: SecretKind) -> Result<(), SecretError> {
        let mut passwords = self.passwords.lock().unwrap();
        passwords.remove(&kind);
        if kind == SecretKind::ConfigEncryption {
            *self.previous_encryption.lock().unwrap() = None;
        }
        Ok(())
    }

    fn load_previous(&self, kind: SecretKind) -> Result<Option<String>, SecretError> {
        if kind == SecretKind::ConfigEncryption {
            Ok(self.previous_encryption.lock().unwrap().clone())
        } else {
            Ok(None)
        }
    }

    fn load_encryption_passwords(&self) -> Result<(Option<String>, Option<String>), SecretError> {
        let passwords = self.passwords.lock().unwrap();
        let previous = self.previous_encryption.lock().unwrap();
        Ok((
            passwords.get(&SecretKind::ConfigEncryption).cloned(),
            previous.clone(),
        ))
    }

    fn finish_password_rotation(
        &self,
        kind: SecretKind,
        expected_current: &str,
        expected_previous: Option<&str>,
    ) -> Result<(), SecretError> {
        if kind == SecretKind::ConfigEncryption {
            let passwords = self.passwords.lock().unwrap();
            if passwords.get(&kind).map(String::as_str) != Some(expected_current) {
                return Ok(());
            }
            let mut previous = self.previous_encryption.lock().unwrap();
            if previous.as_deref() == expected_previous {
                *previous = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_keeps_kinds_separate() {
        let store = MemorySecretStore::new();
        assert_eq!(store.load(SecretKind::WebDav).unwrap(), None);
        store.store(SecretKind::WebDav, "hunter2").unwrap();
        store
            .store(SecretKind::ConfigEncryption, "another secret")
            .unwrap();
        assert_eq!(
            store.load(SecretKind::WebDav).unwrap().as_deref(),
            Some("hunter2")
        );
        assert_eq!(
            store.load(SecretKind::ConfigEncryption).unwrap().as_deref(),
            Some("another secret")
        );
        store.clear(SecretKind::WebDav).unwrap();
        assert_eq!(store.load(SecretKind::WebDav).unwrap(), None);
        assert!(store.load(SecretKind::ConfigEncryption).unwrap().is_some());
    }

    #[test]
    fn encryption_rotation_keeps_original_until_commit() {
        let store = MemorySecretStore::new();
        store
            .store(SecretKind::ConfigEncryption, "old password")
            .unwrap();
        store
            .store(SecretKind::ConfigEncryption, "new password")
            .unwrap();
        assert_eq!(
            store
                .load_previous(SecretKind::ConfigEncryption)
                .unwrap()
                .as_deref(),
            Some("old password")
        );
        // A second pending replacement could orphan a remote that was already
        // re-encrypted just before a crash. It must be rejected atomically.
        assert!(matches!(
            store.store(SecretKind::ConfigEncryption, "third password"),
            Err(SecretError::RotationPending)
        ));
        assert_eq!(
            store.load_encryption_passwords().unwrap(),
            (
                Some("new password".to_string()),
                Some("old password".to_string())
            )
        );
        // A sync that started before the first rotation must not clear the
        // newer pair after it finishes.
        store
            .finish_password_rotation(SecretKind::ConfigEncryption, "old password", None)
            .unwrap();
        assert_eq!(
            store
                .load_previous(SecretKind::ConfigEncryption)
                .unwrap()
                .as_deref(),
            Some("old password")
        );
        store
            .finish_password_rotation(
                SecretKind::ConfigEncryption,
                "new password",
                Some("old password"),
            )
            .unwrap();
        assert_eq!(
            store.load_previous(SecretKind::ConfigEncryption).unwrap(),
            None
        );
        store
            .store(SecretKind::ConfigEncryption, "third password")
            .unwrap();
        assert_eq!(
            store.load_encryption_passwords().unwrap(),
            (
                Some("third password".to_string()),
                Some("new password".to_string())
            )
        );
    }
}
