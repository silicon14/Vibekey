//! DPAPI backed implementation of vibekey_core::SecretStore.
//!
//! The WebDAV password is encrypted with CryptProtectData (user scope) and
//! written to %APPDATA%\Vibekey\secret.bin after base64 encoding. The plain
//! password is never written to the log or to the synchronised configuration.

use std::fs;
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use vibekey_core::{SecretError, SecretStore};
use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// Extra entropy mixed into the DPAPI key so that a generic DPAPI blob from
/// another application cannot be used to read this file.
const ENTROPY: &[u8] = b"Vibekey";

/// DPAPI secret store on top of a single file.
pub struct DpapiSecretStore {
    path: PathBuf,
}

impl DpapiSecretStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn entropy_blob() -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: ENTROPY.len() as u32,
            pbData: ENTROPY.as_ptr() as *mut u8,
        }
    }

    fn protect(&self, plain: &str) -> Result<Vec<u8>, SecretError> {
        let data = plain.as_bytes();
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let entropy = Self::entropy_blob();
        // SAFETY: all blobs point at live buffers; the description, reserved and
        // prompt pointers are optional and passed as null. The output blob is
        // freed with LocalFree below when the call succeeds.
        let ok = unsafe {
            CryptProtectData(
                &mut input,
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
            // SAFETY: the pointer was allocated by CryptProtectData with
            // LocalAlloc and is released exactly once here.
            unsafe {
                LocalFree(output.pbData as HLOCAL);
            }
        }
        Ok(bytes)
    }

    fn unprotect(&self, blob: &[u8]) -> Result<String, SecretError> {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let entropy = Self::entropy_blob();
        // SAFETY: see protect(); the description pointer is optional so null is
        // passed and the output blob is freed below.
        let ok = unsafe {
            CryptUnprotectData(
                &mut input,
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
            // SAFETY: the pointer was allocated by CryptUnprotectData with
            // LocalAlloc and is released exactly once here.
            unsafe {
                LocalFree(output.pbData as HLOCAL);
            }
        }
        String::from_utf8(bytes).map_err(|_| SecretError::Decrypt)
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), SecretError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("bin.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

impl SecretStore for DpapiSecretStore {
    fn store_password(&self, password: &str) -> Result<(), SecretError> {
        let encrypted = self.protect(password)?;
        let encoded = BASE64.encode(encrypted);
        self.write_atomically(encoded.as_bytes())
    }

    fn load_password(&self) -> Result<Option<String>, SecretError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| {
                    SecretError::Unavailable("secret.bin is not valid UTF-8".into())
                })?;
                let decoded = BASE64
                    .decode(text.trim())
                    .map_err(|_| SecretError::Decrypt)?;
                self.unprotect(&decoded).map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(SecretError::Io(error)),
        }
    }

    fn clear_password(&self) -> Result<(), SecretError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SecretError::Io(error)),
        }
    }
}

fn last_error_message() -> String {
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("Windows error {code}")
}
