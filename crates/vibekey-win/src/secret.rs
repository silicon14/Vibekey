//! DPAPI backed implementation of vibekey_core::SecretStore.
//!
//! WebDAV and configuration-encryption passwords use different files and DPAPI
//! entropy. The latter stores current + previous in one atomic encrypted blob,
//! so a crash cannot persist half of a password-rotation pair.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use vibekey_core::{SecretError, SecretKind, SecretStore};
use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use zeroize::Zeroizing;

/// Existing WebDAV blobs keep their original entropy for backward compatibility.
const WEBDAV_ENTROPY: &[u8] = b"Vibekey";
const CONFIG_ENCRYPTION_ENTROPY: &[u8] = b"Vibekey/ConfigEncryption";

#[derive(Default, Serialize, Deserialize)]
struct EncryptionPasswords {
    current: String,
    previous: Option<String>,
}

/// DPAPI secret store. `path` remains the legacy WebDAV secret.bin path; the
/// independent configuration-encryption blob lives next to it.
pub struct DpapiSecretStore {
    path: PathBuf,
    /// Serialises complete read/check/write transactions shared by the settings
    /// UI and sync worker. Atomic rename alone cannot prevent lost updates.
    transaction: Mutex<()>,
}

impl DpapiSecretStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            transaction: Mutex::new(()),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, SecretError> {
        self.transaction
            .lock()
            .map_err(|_| SecretError::Unavailable("secret store lock is poisoned".into()))
    }

    fn path_for(&self, kind: SecretKind) -> PathBuf {
        match kind {
            SecretKind::WebDav => self.path.clone(),
            SecretKind::ConfigEncryption => self.path.with_file_name("sync-encryption.bin"),
        }
    }

    fn entropy(kind: SecretKind) -> &'static [u8] {
        match kind {
            SecretKind::WebDav => WEBDAV_ENTROPY,
            SecretKind::ConfigEncryption => CONFIG_ENCRYPTION_ENTROPY,
        }
    }

    fn entropy_blob(kind: SecretKind) -> CRYPT_INTEGER_BLOB {
        let entropy = Self::entropy(kind);
        CRYPT_INTEGER_BLOB {
            cbData: entropy.len() as u32,
            pbData: entropy.as_ptr() as *mut u8,
        }
    }

    fn protect(&self, kind: SecretKind, data: &[u8]) -> Result<Vec<u8>, SecretError> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let entropy = Self::entropy_blob(kind);
        // SAFETY: all blobs point at live buffers; optional description/prompt
        // pointers are null. Successful output is freed with LocalFree below.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(SecretError::Encrypt(last_error_message()));
        }
        let bytes = if output.pbData.is_null() {
            Vec::new()
        } else {
            // SAFETY: on success DPAPI allocated cbData bytes at pbData.
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
        };
        if !output.pbData.is_null() {
            // SAFETY: the pointer was allocated by CryptProtectData and is
            // released exactly once here.
            unsafe {
                LocalFree(output.pbData as HLOCAL);
            }
        }
        Ok(bytes)
    }

    fn unprotect(&self, kind: SecretKind, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let entropy = Self::entropy_blob(kind);
        // SAFETY: see protect(); output is copied then freed exactly once.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(SecretError::Decrypt);
        }
        let bytes = if output.pbData.is_null() || output.cbData == 0 {
            Vec::new()
        } else {
            // SAFETY: on success DPAPI allocated cbData bytes at pbData.
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
        };
        if !output.pbData.is_null() {
            // SAFETY: the pointer was allocated by CryptUnprotectData.
            unsafe {
                LocalFree(output.pbData as HLOCAL);
            }
        }
        Ok(Zeroizing::new(bytes))
    }

    fn read_protected(&self, kind: SecretKind) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
        match fs::read(self.path_for(kind)) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| {
                    SecretError::Unavailable("secret file is not valid UTF-8".into())
                })?;
                let decoded = BASE64
                    .decode(text.trim())
                    .map_err(|_| SecretError::Decrypt)?;
                self.unprotect(kind, &decoded).map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(SecretError::Io(error)),
        }
    }

    fn write_protected(&self, kind: SecretKind, plain: &[u8]) -> Result<(), SecretError> {
        let encrypted = self.protect(kind, plain)?;
        let encoded = BASE64.encode(encrypted);
        write_atomically(&self.path_for(kind), encoded.as_bytes())
    }

    fn load_pair(&self) -> Result<EncryptionPasswords, SecretError> {
        match self.read_protected(SecretKind::ConfigEncryption)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| SecretError::Unavailable("invalid encryption secret data".into())),
            None => Ok(EncryptionPasswords::default()),
        }
    }

    fn save_pair(&self, pair: &EncryptionPasswords) -> Result<(), SecretError> {
        let bytes = Zeroizing::new(
            serde_json::to_vec(pair)
                .map_err(|error| SecretError::Unavailable(error.to_string()))?,
        );
        self.write_protected(SecretKind::ConfigEncryption, &bytes)
    }
}

impl SecretStore for DpapiSecretStore {
    fn store(&self, kind: SecretKind, password: &str) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        match kind {
            SecretKind::WebDav => self.write_protected(kind, password.as_bytes()),
            SecretKind::ConfigEncryption => {
                let mut pair = self.load_pair()?;
                if pair.current == password {
                    return Ok(());
                }
                if pair.previous.is_some() {
                    return Err(SecretError::RotationPending);
                }
                if !pair.current.is_empty() {
                    pair.previous = Some(std::mem::take(&mut pair.current));
                }
                pair.current = password.to_string();
                self.save_pair(&pair)
            }
        }
    }

    fn load(&self, kind: SecretKind) -> Result<Option<String>, SecretError> {
        let _guard = self.lock()?;
        match kind {
            SecretKind::WebDav => match self.read_protected(kind)? {
                Some(bytes) => String::from_utf8(bytes.to_vec())
                    .map(Some)
                    .map_err(|_| SecretError::Decrypt),
                None => Ok(None),
            },
            SecretKind::ConfigEncryption => {
                let pair = self.load_pair()?;
                Ok((!pair.current.is_empty()).then_some(pair.current))
            }
        }
    }

    fn clear(&self, kind: SecretKind) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        match fs::remove_file(self.path_for(kind)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SecretError::Io(error)),
        }
    }

    fn load_previous(&self, kind: SecretKind) -> Result<Option<String>, SecretError> {
        if kind != SecretKind::ConfigEncryption {
            return Ok(None);
        }
        let _guard = self.lock()?;
        Ok(self.load_pair()?.previous)
    }

    fn load_encryption_passwords(&self) -> Result<(Option<String>, Option<String>), SecretError> {
        let _guard = self.lock()?;
        let mut pair = self.load_pair()?;
        let current = (!pair.current.is_empty()).then_some(std::mem::take(&mut pair.current));
        Ok((current, pair.previous))
    }

    fn finish_password_rotation(
        &self,
        kind: SecretKind,
        expected_current: &str,
        expected_previous: Option<&str>,
    ) -> Result<(), SecretError> {
        if kind != SecretKind::ConfigEncryption {
            return Ok(());
        }
        let _guard = self.lock()?;
        let mut pair = self.load_pair()?;
        // A settings save may have installed a newer rotation while this sync
        // was waiting on the network. Never commit over that newer pair.
        if pair.current != expected_current || pair.previous.as_deref() != expected_previous {
            return Ok(());
        }
        if pair.previous.is_some() {
            pair.previous = None;
            self.save_pair(&pair)?;
        }
        Ok(())
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("bin.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn last_error_message() -> String {
    // SAFETY: GetLastError has no preconditions.
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("Windows error {code}")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "vibekey-secret-test-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn concurrent_rotation_has_one_winner_and_a_consistent_pair() {
        let directory = temporary_directory();
        fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(DpapiSecretStore::new(directory.join("secret.bin")));
        store
            .store(SecretKind::ConfigEncryption, "initial-password")
            .unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for index in 0..8 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store.store(
                    SecretKind::ConfigEncryption,
                    &format!("replacement-password-{index}"),
                )
            }));
        }
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(SecretError::RotationPending)))
                .count(),
            7
        );

        let (current, previous) = store.load_encryption_passwords().unwrap();
        let current = current.unwrap();
        assert!(current.starts_with("replacement-password-"));
        assert_eq!(previous.as_deref(), Some("initial-password"));
        store
            .finish_password_rotation(SecretKind::ConfigEncryption, &current, previous.as_deref())
            .unwrap();
        assert_eq!(
            store.load_encryption_passwords().unwrap(),
            (Some(current), None)
        );
        store.clear(SecretKind::ConfigEncryption).unwrap();
        assert!(!directory.join("sync-encryption.bin").exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
