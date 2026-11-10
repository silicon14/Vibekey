//! Password-based authenticated encryption for synchronised configuration.
//!
//! Version 1 stores `MAGIC || iterations || salt || nonce || ciphertext || tag`.
//! AES-256-GCM authenticates both the configuration and the header. PBKDF2 with
//! HMAC-SHA256 derives a fresh key from the user-supplied sync password and a
//! random salt for every upload. The password is never part of the envelope.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::Sha256;
use zeroize::Zeroizing;

/// Eight-byte magic including the format version (`VIBEKEY` + `0x01`).
pub const ENCRYPTED_CONFIG_MAGIC: &[u8; 8] = b"VIBEKEY\x01";
/// OWASP's PBKDF2-HMAC-SHA256 recommendation at the time this format was added.
pub const PBKDF2_ITERATIONS: u32 = 600_000;
/// Minimum accepted password length for the settings UI and sync worker.
pub const MIN_SYNC_PASSWORD_CHARS: usize = 8;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const HEADER_LEN: usize = ENCRYPTED_CONFIG_MAGIC.len() + 4 + SALT_LEN + NONCE_LEN;
const MIN_ENVELOPE_LEN: usize = HEADER_LEN + TAG_LEN;
// The iteration count is attacker-controlled input. Do not let a corrupt remote
// file keep the sync worker busy for an unbounded amount of time.
const MAX_PBKDF2_ITERATIONS: u32 = 2_000_000;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CryptoError {
    #[error("a configuration encryption password is required")]
    PasswordRequired,
    #[error(
        "the configuration encryption password must contain at least {MIN_SYNC_PASSWORD_CHARS} characters"
    )]
    PasswordTooShort,
    #[error("the encrypted configuration has an invalid or unsupported format")]
    InvalidFormat,
    #[error("the configuration encryption password is wrong, or the encrypted file was modified")]
    Decrypt,
    #[error("the operating system random number generator failed")]
    Random,
    #[error("configuration encryption failed")]
    Encrypt,
}

/// Whether a remote body uses Vibekey's encrypted envelope rather than TOML.
pub fn is_encrypted_config(body: &[u8]) -> bool {
    body.starts_with(ENCRYPTED_CONFIG_MAGIC)
}

/// Detect the envelope family, including an unsupported future version. Such a
/// body must fail closed instead of falling back to plaintext TOML parsing.
pub fn has_config_envelope_marker(body: &[u8]) -> bool {
    body.starts_with(b"VIBEKEY")
        && body.get(7).is_some_and(|byte| {
            matches!(*byte, 0x00..=0x1F | 0x7F..=0xFF) && !matches!(*byte, b'\t' | b'\n' | b'\r')
        })
}

/// Encrypt a complete UTF-8 TOML configuration for WebDAV transport.
pub fn encrypt_config(plain: &[u8], password: &str) -> Result<Vec<u8>, CryptoError> {
    validate_password(password)?;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut salt).map_err(|_| CryptoError::Random)?;
    getrandom::getrandom(&mut nonce).map_err(|_| CryptoError::Random)?;
    encrypt_with_parts(plain, password, PBKDF2_ITERATIONS, &salt, &nonce)
}

/// Decrypt and authenticate a complete encrypted configuration envelope.
pub fn decrypt_config(envelope: &[u8], password: &str) -> Result<Vec<u8>, CryptoError> {
    if envelope.len() < MIN_ENVELOPE_LEN || !is_encrypted_config(envelope) {
        return Err(CryptoError::InvalidFormat);
    }
    let iterations = u32::from_be_bytes(
        envelope[8..12]
            .try_into()
            .map_err(|_| CryptoError::InvalidFormat)?,
    );
    if iterations == 0 || iterations > MAX_PBKDF2_ITERATIONS {
        return Err(CryptoError::InvalidFormat);
    }

    let salt = &envelope[12..12 + SALT_LEN];
    let nonce = &envelope[12 + SALT_LEN..HEADER_LEN];
    let key = derive_key(password, salt, iterations);
    let cipher = Aes256Gcm::new_from_slice(&*key).map_err(|_| CryptoError::InvalidFormat)?;
    let mut plain = envelope[HEADER_LEN..].to_vec();
    cipher
        .decrypt_in_place(
            Nonce::from_slice(nonce),
            &envelope[..HEADER_LEN],
            &mut plain,
        )
        .map_err(|_| CryptoError::Decrypt)?;
    Ok(plain)
}

/// Password policy shared by core validation, the UI, and non-UI callers.
pub fn validate_password(password: &str) -> Result<(), CryptoError> {
    if password.is_empty() {
        return Err(CryptoError::PasswordRequired);
    }
    if password.chars().count() < MIN_SYNC_PASSWORD_CHARS {
        return Err(CryptoError::PasswordTooShort);
    }
    Ok(())
}

fn encrypt_with_parts(
    plain: &[u8],
    password: &str,
    iterations: u32,
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, CryptoError> {
    validate_password(password)?;
    let mut envelope = Vec::with_capacity(HEADER_LEN + plain.len() + TAG_LEN);
    envelope.extend_from_slice(ENCRYPTED_CONFIG_MAGIC);
    envelope.extend_from_slice(&iterations.to_be_bytes());
    envelope.extend_from_slice(salt);
    envelope.extend_from_slice(nonce);

    let key = derive_key(password, salt, iterations);
    let cipher = Aes256Gcm::new_from_slice(&*key).map_err(|_| CryptoError::Encrypt)?;
    let mut ciphertext = plain.to_vec();
    cipher
        .encrypt_in_place(Nonce::from_slice(nonce), &envelope, &mut ciphertext)
        .map_err(|_| CryptoError::Encrypt)?;
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

fn derive_key(password: &str, salt: &[u8], iterations: u32) -> Zeroizing<[u8; KEY_LEN]> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    // pbkdf2_hmac writes the entire output and has no failure return for a
    // fixed 32-byte HMAC-SHA256 derivation.
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut *key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "correct horse battery staple";
    const PLAIN: &[u8] = "name = \"秘密片段\"\ntext = \"机密内容\"\n".as_bytes();

    #[test]
    fn encrypted_round_trip_and_random_envelopes() {
        let first = encrypt_config(PLAIN, PASSWORD).unwrap();
        let second = encrypt_config(PLAIN, PASSWORD).unwrap();
        assert_ne!(first, second, "salt and nonce must be fresh");
        assert!(is_encrypted_config(&first));
        assert_eq!(decrypt_config(&first, PASSWORD).unwrap(), PLAIN);
        assert_eq!(decrypt_config(&second, PASSWORD).unwrap(), PLAIN);
        assert!(!first
            .windows("机密内容".len())
            .any(|window| window == "机密内容".as_bytes()));
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let encrypted = encrypt_config(b"", PASSWORD).unwrap();
        assert_eq!(encrypted.len(), MIN_ENVELOPE_LEN);
        assert_eq!(decrypt_config(&encrypted, PASSWORD).unwrap(), b"");
    }

    #[test]
    fn wrong_password_and_tampering_are_rejected() {
        let encrypted = encrypt_config(PLAIN, PASSWORD).unwrap();
        assert_eq!(
            decrypt_config(&encrypted, "another password").unwrap_err(),
            CryptoError::Decrypt
        );

        let mut wrong_magic = encrypted.clone();
        wrong_magic[4] ^= 0x80;
        assert_eq!(
            decrypt_config(&wrong_magic, PASSWORD).unwrap_err(),
            CryptoError::InvalidFormat
        );
        let mut excessive_iterations = encrypted.clone();
        excessive_iterations[8] ^= 0x80;
        assert_eq!(
            decrypt_config(&excessive_iterations, PASSWORD).unwrap_err(),
            CryptoError::InvalidFormat
        );

        for index in [11, 12, 20, HEADER_LEN - 1, HEADER_LEN, encrypted.len() - 1] {
            let mut changed = encrypted.clone();
            changed[index] ^= 0x01;
            assert_eq!(
                decrypt_config(&changed, PASSWORD).unwrap_err(),
                CryptoError::Decrypt,
                "tampered byte {index} was accepted"
            );
        }
    }

    #[test]
    fn malformed_envelopes_are_rejected_before_decryption() {
        assert_eq!(
            decrypt_config(b"not toml", PASSWORD).unwrap_err(),
            CryptoError::InvalidFormat
        );
        let mut encrypted = encrypt_config(PLAIN, PASSWORD).unwrap();
        encrypted.truncate(MIN_ENVELOPE_LEN - 1);
        assert_eq!(
            decrypt_config(&encrypted, PASSWORD).unwrap_err(),
            CryptoError::InvalidFormat
        );

        let mut encrypted = encrypt_config(PLAIN, PASSWORD).unwrap();
        encrypted[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            decrypt_config(&encrypted, PASSWORD).unwrap_err(),
            CryptoError::InvalidFormat
        );
    }

    #[test]
    fn envelope_marker_does_not_capture_valid_toml_keys() {
        assert!(has_config_envelope_marker(ENCRYPTED_CONFIG_MAGIC));
        assert!(has_config_envelope_marker(b"VIBEKEY\x02future"));
        assert!(!has_config_envelope_marker(
            b"VIBEKEY = \"ordinary TOML key\""
        ));
        assert!(!has_config_envelope_marker(
            b"VIBEKEY=\"ordinary TOML key\""
        ));
        assert!(!has_config_envelope_marker(b"VIBEKEY2 = 1"));
        assert!(!has_config_envelope_marker(b"VIBEKEY.foo = 1"));
        assert!(!has_config_envelope_marker(b"title = \"VIBEKEY\""));
    }

    #[test]
    fn password_policy_counts_unicode_characters() {
        assert_eq!(validate_password(""), Err(CryptoError::PasswordRequired));
        assert_eq!(
            validate_password("1234567"),
            Err(CryptoError::PasswordTooShort)
        );
        assert!(validate_password("密码密码密码密码").is_ok());
        assert!(validate_password("12345678").is_ok());
    }

    #[test]
    fn deterministic_parts_make_format_regression_testable() {
        let salt = [0x11; SALT_LEN];
        let nonce = [0x22; NONCE_LEN];
        let encrypted = encrypt_with_parts(b"vibekey", PASSWORD, 100_000, &salt, &nonce).unwrap();
        assert_eq!(decrypt_config(&encrypted, PASSWORD).unwrap(), b"vibekey");
        assert_eq!(encrypted.len(), HEADER_LEN + 7 + TAG_LEN);
        assert_eq!(&encrypted[..8], ENCRYPTED_CONFIG_MAGIC);
        assert_eq!(&encrypted[8..12], &100_000u32.to_be_bytes());
        // Independently generated with .NET Rfc2898DeriveBytes + AesGcm, not
        // RustCrypto. This pins the v1 on-disk interoperability format.
        let expected = "564942454b455901000186a011111111111111111111111111111111222222222222222222222222a220743bd182d3ad04df12a4d0c9261b2356d062ee9bc8";
        let actual = encrypted
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(actual, expected);
    }
}
