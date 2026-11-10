//! WebDAV synchronisation of the configuration file.
//!
//! The engine is deliberately independent from the HTTP client so that the
//! conflict handling can be unit tested against an in-memory WebDAV server.
//! Change detection always hashes plaintext, never a randomized ciphertext.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::crypto::{
    decrypt_config, encrypt_config, has_config_envelope_marker, is_encrypted_config,
    validate_password, CryptoError,
};
use crate::state::SyncState;

/// A configuration file stored on the server, before optional decryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub body: Vec<u8>,
    pub etag: Option<String>,
    /// Derived from the body marker, never from a server-supplied header.
    pub encrypted: bool,
}

impl RemoteConfig {
    pub fn new(body: Vec<u8>, etag: Option<String>) -> Self {
        let encrypted = has_config_envelope_marker(&body);
        Self {
            body,
            etag,
            encrypted,
        }
    }
}

/// Result of a conditional (or unconditional) write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutResult {
    Stored { etag: Option<String> },
    PreconditionFailed,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("the remote file changed while uploading")]
    PreconditionFailed,
    #[error("local file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid synchronisation configuration: {0}")]
    Invalid(String),
    #[error("remote configuration is invalid: {0}")]
    RemoteInvalid(String),
    #[error("configuration encryption failed: {0}")]
    Crypto(#[from] CryptoError),
    #[error(
        "the remote is encrypted; enable encryption locally before allowing any plaintext migration"
    )]
    EncryptionRequired,
    #[error(
        "the previously encrypted remote was replaced by plaintext; refusing an HTTP downgrade"
    )]
    Downgrade,
}

/// Minimal WebDAV operations needed by the sync engine.
pub trait WebDav: Send + Sync {
    /// Create the parent collection if it does not exist yet.
    fn ensure_parent(&self, url: &str) -> Result<(), SyncError>;
    /// Fetch the file, or None when it does not exist.
    fn get(&self, url: &str) -> Result<Option<RemoteConfig>, SyncError>;
    /// Upload the file. When if_match is set the write must fail with
    /// PreconditionFailed if the remote ETag differs.
    fn put(&self, url: &str, body: &[u8], if_match: Option<&str>) -> Result<PutResult, SyncError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Both sides already match in content and transport format.
    UpToDate,
    /// The local file was uploaded (including format/password migration).
    Pushed { first_upload: bool },
    /// The remote file replaced the local file. If a format migration was also
    /// needed, the same plaintext was securely re-uploaded first.
    Pulled,
    /// Both sides changed. The local file won, the remote file was uploaded to
    /// a backup URL using the requested transport encryption.
    ConflictLocalWins { remote_backup: Option<String> },
}

/// Immutable encryption settings for one complete synchronisation step.
/// Uploads always use `current`; `previous` is only a fallback for decrypting
/// the pre-rotation remote. The secret store retains it until success is saved.
#[derive(Clone, Default)]
pub struct SyncProtection {
    encrypt: bool,
    current: Option<String>,
    previous: Option<String>,
}

impl SyncProtection {
    pub fn new(
        encrypt: bool,
        current: Option<String>,
        previous: Option<String>,
    ) -> Result<Self, CryptoError> {
        if encrypt {
            validate_password(current.as_deref().unwrap_or(""))?;
        }
        Ok(Self {
            encrypt,
            current,
            previous,
        })
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn encrypt(&self) -> bool {
        self.encrypt
    }
}

#[derive(Debug)]
struct DecodedRemote {
    body: Vec<u8>,
    etag: Option<String>,
    needs_reupload: bool,
    /// Raw remote matched the last state before sync.encrypt was normalised
    /// to this run's policy. A policy toggle alone is not a remote edit.
    policy_only_change: bool,
}

/// Drives one synchronisation step.
pub struct SyncEngine<'a> {
    dav: &'a dyn WebDav,
    url: String,
    local_path: PathBuf,
    backup_on_conflict: bool,
    protection: SyncProtection,
}

impl<'a> SyncEngine<'a> {
    pub fn new(
        dav: &'a dyn WebDav,
        url: impl Into<String>,
        local_path: impl Into<PathBuf>,
        backup_on_conflict: bool,
    ) -> Self {
        Self {
            dav,
            url: url.into(),
            local_path: local_path.into(),
            backup_on_conflict,
            protection: SyncProtection::disabled(),
        }
    }

    pub fn with_protection(mut self, protection: SyncProtection) -> Self {
        self.protection = protection;
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Run one synchronisation step. The updated state is not persisted here;
    /// the caller is responsible for saving it before committing a password
    /// rotation. Decryption/authentication happens before every content or
    /// conflict decision, including fresh GETs after a failed conditional PUT.
    pub fn sync(&self, state: &mut SyncState, timestamp: &str) -> Result<SyncOutcome, SyncError> {
        if self.url.trim().is_empty() {
            return Err(SyncError::Invalid("sync.url is empty".to_string()));
        }

        let local_bytes = fs::read(&self.local_path)?;
        let local_hash = content_hash(&local_bytes);

        self.dav.ensure_parent(&self.url)?;
        let remote = self
            .dav
            .get(&self.url)?
            .map(|remote| self.decode_remote(remote, state))
            .transpose()?;

        // Identical plaintext and target format: randomized ciphertext is not
        // considered a content change and no new upload is necessary.
        if let Some(remote_cfg) = remote.as_ref() {
            if remote_cfg.body == local_bytes && !remote_cfg.needs_reupload {
                self.finish(
                    state,
                    &local_hash,
                    &local_hash,
                    remote_cfg.etag.clone(),
                    "up-to-date",
                );
                return Ok(SyncOutcome::UpToDate);
            }
        }

        let never_synced = state.local_hash.is_none() && state.remote_hash.is_none();
        if never_synced {
            if let Some(remote_cfg) = remote {
                return self.pull_remote(&remote_cfg, state);
            }
            self.push_local(state, &local_bytes, &local_hash, None)?;
            return Ok(SyncOutcome::Pushed { first_upload: true });
        }

        let local_changed = state.local_hash.as_deref() != Some(local_hash.as_str());

        match (remote, local_changed) {
            // The remote file disappeared; recreate it from the local file.
            (None, _) => {
                self.push_local(state, &local_bytes, &local_hash, None)?;
                Ok(SyncOutcome::Pushed {
                    first_upload: false,
                })
            }
            // Local file unchanged: pull remote edits, or only migrate the
            // format/password when its plaintext has not changed.
            (Some(remote_cfg), false) => {
                let remote_hash = content_hash(&remote_cfg.body);
                if state.remote_hash.as_deref() != Some(remote_hash.as_str())
                    && !remote_cfg.policy_only_change
                {
                    self.pull_remote(&remote_cfg, state)
                } else if remote_cfg.needs_reupload {
                    self.push_or_recover(
                        state,
                        &local_bytes,
                        &local_hash,
                        remote_cfg.etag.as_deref(),
                        timestamp,
                    )
                } else {
                    self.finish(
                        state,
                        &local_hash,
                        &local_hash,
                        remote_cfg.etag.clone(),
                        "up-to-date",
                    );
                    Ok(SyncOutcome::UpToDate)
                }
            }
            // Local file changed.
            (Some(remote_cfg), true) => {
                let remote_hash = content_hash(&remote_cfg.body);
                let remote_changed = state.remote_hash.as_deref() != Some(remote_hash.as_str())
                    && !remote_cfg.policy_only_change;
                if remote_changed {
                    return self.resolve_conflict(
                        state,
                        &local_bytes,
                        &local_hash,
                        &remote_cfg,
                        timestamp,
                    );
                }
                self.push_or_recover(
                    state,
                    &local_bytes,
                    &local_hash,
                    remote_cfg.etag.as_deref(),
                    timestamp,
                )
            }
        }
    }

    fn encode(&self, plaintext: &[u8]) -> Result<Vec<u8>, SyncError> {
        if self.protection.encrypt {
            let password = self
                .protection
                .current
                .as_deref()
                .ok_or(CryptoError::PasswordRequired)?;
            Ok(encrypt_config(plaintext, password)?)
        } else {
            Ok(plaintext.to_vec())
        }
    }

    fn decode_remote(
        &self,
        remote: RemoteConfig,
        state: &SyncState,
    ) -> Result<DecodedRemote, SyncError> {
        let marked = remote.encrypted || has_config_envelope_marker(&remote.body);
        if marked && !is_encrypted_config(&remote.body) {
            return Err(CryptoError::InvalidFormat.into());
        }
        if self.protection.encrypt && state.remote_encrypted == Some(true) && !marked {
            return Err(SyncError::Downgrade);
        }
        if marked && !self.protection.encrypt && state.remote_encrypted != Some(true) {
            // A default-off setting or a previously-plaintext machine must not
            // authorise replacing somebody else's encrypted remote with plain
            // TOML. Only a local encrypted baseline makes disabling deliberate.
            return Err(SyncError::EncryptionRequired);
        }

        let mut used_previous = false;
        let plaintext = if marked {
            let mut last_error = CryptoError::PasswordRequired;
            let mut plaintext = None;
            for (index, password) in [
                self.protection.current.as_ref(),
                self.protection.previous.as_ref(),
            ]
            .into_iter()
            .flatten()
            .enumerate()
            {
                match decrypt_config(&remote.body, password) {
                    Ok(decrypted) => {
                        plaintext = Some(decrypted);
                        used_previous = index > 0;
                        break;
                    }
                    Err(CryptoError::Decrypt) => last_error = CryptoError::Decrypt,
                    Err(error) => return Err(error.into()),
                }
            }
            plaintext.ok_or(last_error)?
        } else {
            remote.body.clone()
        };

        // A pulled file must not silently change the user's local encryption
        // policy. Preserve all other remote fields but force sync.encrypt to
        // the setting captured for this run. Any normalisation also requires a
        // PUT so local and remote plaintext hashes genuinely converge.
        let text = std::str::from_utf8(&plaintext)
            .map_err(|_| SyncError::RemoteInvalid("file is not valid UTF-8".to_string()))?;
        let mut config = Config::parse(text).map_err(|_| {
            // toml's Display includes the offending source line. Never persist
            // decrypted snippet contents through logs or sync-state.json.
            SyncError::RemoteInvalid("configuration is not valid TOML".to_string())
        })?;
        let policy_changed = config.sync.encrypt != self.protection.encrypt;
        let raw_hash = content_hash(&plaintext);
        let policy_only_change =
            policy_changed && state.remote_hash.as_deref() == Some(raw_hash.as_str());
        let body = if policy_changed {
            config.sync.encrypt = self.protection.encrypt;
            config
                .to_toml()
                .map_err(|_| {
                    SyncError::RemoteInvalid(
                        "could not serialise the normalised configuration".to_string(),
                    )
                })?
                .into_bytes()
        } else {
            plaintext
        };
        let needs_reupload = marked != self.protection.encrypt || used_previous || policy_changed;
        Ok(DecodedRemote {
            body,
            etag: remote.etag,
            needs_reupload,
            policy_only_change,
        })
    }

    fn push_local(
        &self,
        state: &mut SyncState,
        bytes: &[u8],
        hash: &str,
        if_match: Option<&str>,
    ) -> Result<(), SyncError> {
        let encoded = self.encode(bytes)?;
        match self.dav.put(&self.url, &encoded, if_match)? {
            PutResult::Stored { etag } => {
                self.finish(state, hash, hash, etag, "pushed");
                Ok(())
            }
            PutResult::PreconditionFailed => Err(SyncError::PreconditionFailed),
        }
    }

    fn push_or_recover(
        &self,
        state: &mut SyncState,
        local_bytes: &[u8],
        local_hash: &str,
        if_match: Option<&str>,
        timestamp: &str,
    ) -> Result<SyncOutcome, SyncError> {
        let encoded = self.encode(local_bytes)?;
        match self.dav.put(&self.url, &encoded, if_match)? {
            PutResult::Stored { etag } => {
                self.finish(state, local_hash, local_hash, etag, "pushed");
                Ok(SyncOutcome::Pushed {
                    first_upload: false,
                })
            }
            PutResult::PreconditionFailed => {
                self.recover_after_precondition(state, local_bytes, local_hash, timestamp)
            }
        }
    }

    /// Download and, when necessary, securely re-upload the remote plaintext
    /// before touching the local file. A failed migration therefore never
    /// leaves an unnoticed local reload behind.
    fn pull_remote(
        &self,
        remote: &DecodedRemote,
        state: &mut SyncState,
    ) -> Result<SyncOutcome, SyncError> {
        let mut etag = remote.etag.clone();
        if remote.needs_reupload {
            let encoded = self.encode(&remote.body)?;
            match self.dav.put(&self.url, &encoded, remote.etag.as_deref())? {
                PutResult::Stored { etag: stored } => etag = stored,
                PutResult::PreconditionFailed => return Err(SyncError::PreconditionFailed),
            }
        }
        self.apply_pull(&remote.body, etag, state)?;
        Ok(SyncOutcome::Pulled)
    }

    /// The conditional PUT lost a race: decode the fresh remote before making
    /// any equality, conflict, backup, or retry decision.
    fn recover_after_precondition(
        &self,
        state: &mut SyncState,
        local_bytes: &[u8],
        local_hash: &str,
        timestamp: &str,
    ) -> Result<SyncOutcome, SyncError> {
        let mut fresh = match self.dav.get(&self.url)? {
            Some(fresh) => self.decode_remote(fresh, state)?,
            None => {
                self.push_local(state, local_bytes, local_hash, None)?;
                return Ok(SyncOutcome::Pushed {
                    first_upload: false,
                });
            }
        };

        if fresh.body == local_bytes && !fresh.needs_reupload {
            self.finish(
                state,
                local_hash,
                local_hash,
                fresh.etag.clone(),
                "up-to-date",
            );
            return Ok(SyncOutcome::UpToDate);
        }

        let fresh_hash = content_hash(&fresh.body);
        let remote_really_changed =
            state.remote_hash.as_deref() != Some(fresh_hash.as_str()) && !fresh.policy_only_change;
        if !remote_really_changed {
            let encoded = self.encode(local_bytes)?;
            match self.dav.put(&self.url, &encoded, fresh.etag.as_deref())? {
                PutResult::Stored { etag } => {
                    self.finish(state, local_hash, local_hash, etag, "pushed");
                    return Ok(SyncOutcome::Pushed {
                        first_upload: false,
                    });
                }
                PutResult::PreconditionFailed => {
                    if let Some(newest) = self.dav.get(&self.url)? {
                        fresh = self.decode_remote(newest, state)?;
                    }
                }
            }
        }

        self.resolve_conflict(state, local_bytes, local_hash, &fresh, timestamp)
    }

    fn resolve_conflict(
        &self,
        state: &mut SyncState,
        local_bytes: &[u8],
        local_hash: &str,
        remote: &DecodedRemote,
        timestamp: &str,
    ) -> Result<SyncOutcome, SyncError> {
        let mut remote_backup = None;
        if self.backup_on_conflict {
            let backup_url = format!(
                "{}.remote-backup-{}.toml",
                self.url.trim_end_matches('/'),
                sanitize_timestamp(timestamp)
            );
            // The backup uses the exact same encryption exit as the main file.
            let backup_body = self.encode(&remote.body)?;
            match self.dav.put(&backup_url, &backup_body, None)? {
                PutResult::Stored { .. } => remote_backup = Some(backup_url),
                PutResult::PreconditionFailed => return Err(SyncError::PreconditionFailed),
            }
        }

        let encoded = self.encode(local_bytes)?;
        match self.dav.put(&self.url, &encoded, None)? {
            PutResult::Stored { etag } => {
                self.finish(state, local_hash, local_hash, etag, "conflict-local-wins");
                Ok(SyncOutcome::ConflictLocalWins { remote_backup })
            }
            PutResult::PreconditionFailed => Err(SyncError::PreconditionFailed),
        }
    }

    fn apply_pull(
        &self,
        body: &[u8],
        etag: Option<String>,
        state: &mut SyncState,
    ) -> Result<(), SyncError> {
        if let Ok(previous) = fs::read(&self.local_path) {
            let _ = fs::write(self.local_backup_path(), previous);
        }
        write_atomic(&self.local_path, body)?;

        let hash = content_hash(body);
        state.local_hash = Some(hash.clone());
        state.remote_hash = Some(hash);
        state.remote_encrypted = Some(self.protection.encrypt);
        state.remote_etag = etag;
        state.last_sync_unix = Some(Utc::now().timestamp());
        state.last_outcome = Some("pulled".to_string());
        state.last_error = None;
        Ok(())
    }

    fn finish(
        &self,
        state: &mut SyncState,
        local_hash: &str,
        remote_hash: &str,
        etag: Option<String>,
        outcome: &str,
    ) {
        state.local_hash = Some(local_hash.to_string());
        state.remote_hash = Some(remote_hash.to_string());
        state.remote_encrypted = Some(self.protection.encrypt);
        state.remote_etag = etag;
        state.last_sync_unix = Some(Utc::now().timestamp());
        state.last_outcome = Some(outcome.to_string());
        state.last_error = None;
    }

    fn local_backup_path(&self) -> PathBuf {
        self.local_path.with_extension("toml.bak")
    }
}

pub fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sanitize_timestamp(timestamp: &str) -> String {
    let sanitized: String = timestamp
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.is_empty() {
        "backup".to_string()
    } else {
        sanitized
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;

    use super::*;

    const URL: &str = "https://dav.example.com/vibekey/config.toml";
    const TS: &str = "2026-11-06-11-07-00";

    #[derive(Default)]
    struct MemoryDav {
        files: Mutex<HashMap<String, (Vec<u8>, Option<String>)>>,
        etag_counter: AtomicU32,
        reject_next_put: AtomicBool,
        race_on_next_put: Mutex<Option<Vec<u8>>>,
    }

    impl MemoryDav {
        fn new() -> Self {
            Self::default()
        }

        fn stored(&self, url: &str) -> Option<(Vec<u8>, Option<String>)> {
            self.files.lock().unwrap().get(url).cloned()
        }

        fn body(&self, url: &str) -> Option<Vec<u8>> {
            self.stored(url).map(|(body, _)| body)
        }

        fn new_etag(&self) -> String {
            format!(
                "etag-{}",
                self.etag_counter.fetch_add(1, Ordering::SeqCst) + 1
            )
        }

        fn put_direct(&self, url: &str, body: &[u8]) -> Option<String> {
            let etag = self.new_etag();
            self.files
                .lock()
                .unwrap()
                .insert(url.to_string(), (body.to_vec(), Some(etag.clone())));
            Some(etag)
        }

        fn reject_next_put(&self) {
            self.reject_next_put.store(true, Ordering::SeqCst);
        }

        fn race_next_put(&self, body: Vec<u8>) {
            *self.race_on_next_put.lock().unwrap() = Some(body);
        }

        fn keys(&self) -> Vec<String> {
            self.files.lock().unwrap().keys().cloned().collect()
        }
    }

    impl WebDav for MemoryDav {
        fn ensure_parent(&self, _url: &str) -> Result<(), SyncError> {
            Ok(())
        }

        fn get(&self, url: &str) -> Result<Option<RemoteConfig>, SyncError> {
            Ok(self
                .stored(url)
                .map(|(body, etag)| RemoteConfig::new(body, etag)))
        }

        fn put(
            &self,
            url: &str,
            body: &[u8],
            if_match: Option<&str>,
        ) -> Result<PutResult, SyncError> {
            if let Some(raced_body) = self.race_on_next_put.lock().unwrap().take() {
                let etag = self.new_etag();
                self.files
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), (raced_body, Some(etag)));
                return Ok(PutResult::PreconditionFailed);
            }
            if self.reject_next_put.swap(false, Ordering::SeqCst) {
                return Ok(PutResult::PreconditionFailed);
            }
            let mut files = self.files.lock().unwrap();
            if let Some(expected) = if_match {
                match files.get(url) {
                    Some((_, Some(current))) if current == expected => {}
                    _ => return Ok(PutResult::PreconditionFailed),
                }
            }
            let etag = self.new_etag();
            files.insert(url.to_string(), (body.to_vec(), Some(etag.clone())));
            Ok(PutResult::Stored { etag: Some(etag) })
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, Config, MemoryDav) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::example();
        config.save(&path).unwrap();
        (dir, path, config, MemoryDav::new())
    }

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    #[test]
    fn first_sync_uploads() {
        let (_dir, path, config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(outcome, SyncOutcome::Pushed { first_upload: true });
        assert_eq!(
            dav.body(URL).unwrap(),
            config.to_toml().unwrap().into_bytes()
        );
        assert!(state.local_hash.is_some());
        assert_eq!(state.remote_encrypted, Some(false));
        assert!(state.remote_etag.is_some());
    }

    #[test]
    fn second_sync_is_a_noop() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();
        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(outcome, SyncOutcome::UpToDate);
    }

    #[test]
    fn local_change_is_uploaded() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();

        let mut changed = Config::example();
        changed
            .snippet
            .push(crate::config::Snippet::new("4", "新", "新内容"));
        changed.save(&path).unwrap();

        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(
            outcome,
            SyncOutcome::Pushed {
                first_upload: false
            }
        );
        assert_eq!(dav.body(URL).unwrap(), read(&path));
    }

    #[test]
    fn remote_change_is_pulled_and_local_is_backed_up() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();

        let mut changed = Config::example();
        changed.snippet[0].text = "remote edit".to_string();
        dav.put_direct(URL, changed.to_toml().unwrap().as_bytes());

        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(outcome, SyncOutcome::Pulled);
        assert_eq!(read(&path), changed.to_toml().unwrap().into_bytes());
        assert!(path.with_extension("toml.bak").exists());
    }

    #[test]
    fn both_sides_changed_keeps_local_and_backs_up_remote() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();

        let mut local = Config::example();
        local.snippet[0].text = "local edit".to_string();
        local.save(&path).unwrap();
        let mut remote = Config::example();
        remote.snippet[0].text = "remote edit".to_string();
        dav.put_direct(URL, remote.to_toml().unwrap().as_bytes());

        let outcome = engine.sync(&mut state, TS).unwrap();
        match outcome {
            SyncOutcome::ConflictLocalWins { remote_backup } => {
                let backup = remote_backup.expect("backup url");
                let backed_up = String::from_utf8(dav.body(&backup).unwrap()).unwrap();
                assert!(backed_up.contains("remote edit"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(read(&path), local.to_toml().unwrap().into_bytes());
        assert_eq!(dav.body(URL).unwrap(), read(&path));
    }

    #[test]
    fn never_synced_machine_pulls_existing_remote() {
        let (_dir, path, _config, dav) = setup();
        let mut remote = Config::example();
        remote.snippet[0].text = "from another machine".to_string();
        dav.put_direct(URL, remote.to_toml().unwrap().as_bytes());

        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(outcome, SyncOutcome::Pulled);
        assert_eq!(read(&path), remote.to_toml().unwrap().into_bytes());
    }

    #[test]
    fn invalid_remote_file_is_not_applied_locally() {
        let (_dir, path, _config, dav) = setup();
        let before = read(&path);
        dav.put_direct(URL, b"this is not toml [[[");

        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        let error = engine.sync(&mut state, TS).unwrap_err();
        assert!(matches!(error, SyncError::RemoteInvalid(_)));
        assert_eq!(read(&path), before);
    }

    #[test]
    fn precondition_failure_is_retried_with_the_fresh_etag() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();

        let mut local = Config::example();
        local.snippet[0].text = "local edit".to_string();
        local.save(&path).unwrap();
        dav.reject_next_put();

        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(
            outcome,
            SyncOutcome::Pushed {
                first_upload: false
            }
        );
        assert_eq!(dav.body(URL).unwrap(), read(&path));
        assert!(dav.keys().iter().all(|key| !key.contains("remote-backup")));
    }

    #[test]
    fn precondition_failure_with_a_real_remote_change_resolves_conflict() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();

        let mut local = Config::example();
        local.snippet[0].text = "local edit".to_string();
        local.save(&path).unwrap();
        let mut raced = Config::example();
        raced.snippet[0].text = "remote raced".to_string();
        dav.race_next_put(raced.to_toml().unwrap().into_bytes());

        let outcome = engine.sync(&mut state, TS).unwrap();
        match outcome {
            SyncOutcome::ConflictLocalWins { remote_backup } => {
                let backup = remote_backup.expect("backup url");
                let backed_up = String::from_utf8(dav.body(&backup).unwrap()).unwrap();
                assert!(backed_up.contains("remote raced"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(dav.body(URL).unwrap(), read(&path));
    }

    #[test]
    fn removed_remote_file_is_recreated() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();

        dav.files.lock().unwrap().remove(URL);
        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(
            outcome,
            SyncOutcome::Pushed {
                first_upload: false
            }
        );
        assert!(dav.body(URL).is_some());
    }

    #[test]
    fn conflict_backup_can_be_disabled() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, false);
        engine.sync(&mut state, TS).unwrap();

        let mut local = Config::example();
        local.snippet[0].text = "local".to_string();
        local.save(&path).unwrap();
        let mut remote = Config::example();
        remote.snippet[0].text = "remote".to_string();
        dav.put_direct(URL, remote.to_toml().unwrap().as_bytes());

        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(
            outcome,
            SyncOutcome::ConflictLocalWins {
                remote_backup: None
            }
        );
        assert!(dav.keys().iter().all(|key| !key.contains("remote-backup")));
    }
}
