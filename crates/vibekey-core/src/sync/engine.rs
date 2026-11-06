//! WebDAV synchronisation of the configuration file.
//!
//! The engine is deliberately independent from the HTTP client so that the
//! conflict handling can be unit tested against an in-memory WebDAV server.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::state::SyncState;

/// A configuration file stored on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub body: Vec<u8>,
    pub etag: Option<String>,
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
    /// Both sides already match.
    UpToDate,
    /// The local file was uploaded.
    Pushed { first_upload: bool },
    /// The remote file was newer and replaced the local file. The previous
    /// local file is kept as config.toml.bak.
    Pulled,
    /// Both sides changed. The local file won, the remote file was uploaded to
    /// a backup URL.
    ConflictLocalWins { remote_backup: Option<String> },
}

/// Drives one synchronisation step.
pub struct SyncEngine<'a> {
    dav: &'a dyn WebDav,
    url: String,
    local_path: PathBuf,
    backup_on_conflict: bool,
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
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Run one synchronisation step. The updated state is not persisted here;
    /// the caller is responsible for saving it.
    pub fn sync(&self, state: &mut SyncState, timestamp: &str) -> Result<SyncOutcome, SyncError> {
        if self.url.trim().is_empty() {
            return Err(SyncError::Invalid("sync.url is empty".to_string()));
        }

        let local_bytes = fs::read(&self.local_path)?;
        let local_hash = content_hash(&local_bytes);

        self.dav.ensure_parent(&self.url)?;
        let remote = self.dav.get(&self.url)?;

        // Identical content: nothing to transfer, just refresh the bookkeeping.
        if let Some(remote_cfg) = remote.as_ref() {
            if remote_cfg.body == local_bytes {
                self.finish(state, &local_hash, remote_cfg.etag.clone(), "up-to-date");
                return Ok(SyncOutcome::UpToDate);
            }
        }

        let never_synced = state.local_hash.is_none() && state.remote_hash.is_none();
        if never_synced {
            if let Some(remote_cfg) = remote {
                self.apply_pull(&remote_cfg, state)?;
                return Ok(SyncOutcome::Pulled);
            }
            self.push_local(state, &local_bytes, &local_hash, None)?;
            return Ok(SyncOutcome::Pushed { first_upload: true });
        }

        let local_changed = state.local_hash.as_deref() != Some(local_hash.as_str());

        match (remote, local_changed) {
            // The remote file disappeared; recreate it from the local file.
            (None, _) => {
                self.push_local(state, &local_bytes, &local_hash, None)?;
                Ok(SyncOutcome::Pushed { first_upload: false })
            }
            // Local file unchanged: pull when the remote file changed.
            (Some(remote_cfg), false) => {
                let remote_hash = content_hash(&remote_cfg.body);
                if state.remote_hash.as_deref() != Some(remote_hash.as_str()) {
                    self.apply_pull(&remote_cfg, state)?;
                    Ok(SyncOutcome::Pulled)
                } else {
                    self.finish(state, &local_hash, remote_cfg.etag.clone(), "up-to-date");
                    Ok(SyncOutcome::UpToDate)
                }
            }
            // Local file changed.
            (Some(remote_cfg), true) => {
                let remote_hash = content_hash(&remote_cfg.body);
                let remote_changed = state.remote_hash.as_deref() != Some(remote_hash.as_str());
                if remote_changed {
                    return self.resolve_conflict(
                        state,
                        &local_bytes,
                        &local_hash,
                        &remote_cfg,
                        timestamp,
                    );
                }
                match self
                    .dav
                    .put(&self.url, &local_bytes, remote_cfg.etag.as_deref())?
                {
                    PutResult::Stored { etag } => {
                        self.finish(state, &local_hash, etag, "pushed");
                        Ok(SyncOutcome::Pushed { first_upload: false })
                    }
                    PutResult::PreconditionFailed => {
                        // Somebody changed the remote file between GET and PUT.
                        let fresh = self.dav.get(&self.url)?.unwrap_or(remote_cfg);
                        self.resolve_conflict(state, &local_bytes, &local_hash, &fresh, timestamp)
                    }
                }
            }
        }
    }

    fn push_local(
        &self,
        state: &mut SyncState,
        bytes: &[u8],
        hash: &str,
        if_match: Option<&str>,
    ) -> Result<(), SyncError> {
        match self.dav.put(&self.url, bytes, if_match)? {
            PutResult::Stored { etag } => {
                self.finish(state, hash, etag, "pushed");
                Ok(())
            }
            PutResult::PreconditionFailed => Err(SyncError::PreconditionFailed),
        }
    }

    fn resolve_conflict(
        &self,
        state: &mut SyncState,
        local_bytes: &[u8],
        local_hash: &str,
        remote: &RemoteConfig,
        timestamp: &str,
    ) -> Result<SyncOutcome, SyncError> {
        let mut remote_backup = None;
        if self.backup_on_conflict {
            let backup_url = format!(
                "{}.remote-backup-{}.toml",
                self.url.trim_end_matches('/'),
                sanitize_timestamp(timestamp)
            );
            self.dav.put(&backup_url, &remote.body, None)?;
            remote_backup = Some(backup_url);
        }

        match self.dav.put(&self.url, local_bytes, None)? {
            PutResult::Stored { etag } => {
                self.finish(state, local_hash, etag, "conflict-local-wins");
                Ok(SyncOutcome::ConflictLocalWins { remote_backup })
            }
            PutResult::PreconditionFailed => Err(SyncError::PreconditionFailed),
        }
    }

    fn apply_pull(
        &self,
        remote: &RemoteConfig,
        state: &mut SyncState,
    ) -> Result<(), SyncError> {
        let text = std::str::from_utf8(&remote.body)
            .map_err(|_| SyncError::RemoteInvalid("file is not valid UTF-8".to_string()))?;
        Config::parse(text).map_err(|err| SyncError::RemoteInvalid(err.to_string()))?;

        if let Ok(previous) = fs::read(&self.local_path) {
            let _ = fs::write(self.local_backup_path(), previous);
        }
        write_atomic(&self.local_path, &remote.body)?;

        let hash = content_hash(&remote.body);
        state.local_hash = Some(hash.clone());
        state.remote_hash = Some(hash);
        state.remote_etag = remote.etag.clone();
        state.last_sync_unix = Some(Utc::now().timestamp());
        state.last_outcome = Some("pulled".to_string());
        state.last_error = None;
        Ok(())
    }

    fn finish(&self, state: &mut SyncState, hash: &str, etag: Option<String>, outcome: &str) {
        state.local_hash = Some(hash.to_string());
        state.remote_hash = Some(hash.to_string());
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

        fn put_direct(&self, url: &str, body: &[u8]) -> Option<String> {
            let etag = format!("etag-{}", self.etag_counter.fetch_add(1, Ordering::SeqCst) + 1);
            self.files
                .lock()
                .unwrap()
                .insert(url.to_string(), (body.to_vec(), Some(etag.clone())));
            Some(etag)
        }

        fn reject_next_put(&self) {
            self.reject_next_put.store(true, Ordering::SeqCst);
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
            Ok(self.stored(url).map(|(body, etag)| RemoteConfig { body, etag }))
        }

        fn put(&self, url: &str, body: &[u8], if_match: Option<&str>) -> Result<PutResult, SyncError> {
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
            let etag = format!("etag-{}", self.etag_counter.fetch_add(1, Ordering::SeqCst) + 1);
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
        assert_eq!(dav.body(URL).unwrap(), config.to_toml().unwrap().into_bytes());
        assert!(state.local_hash.is_some());
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
        changed.snippet.push(crate::config::Snippet::new("4", "新", "新内容"));
        changed.save(&path).unwrap();

        let outcome = engine.sync(&mut state, TS).unwrap();
        assert_eq!(outcome, SyncOutcome::Pushed { first_upload: false });
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

        let backup = path.with_extension("toml.bak");
        assert!(backup.exists(), "local backup missing");
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
                assert!(backup.contains("remote-backup"));
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
    fn precondition_failure_falls_back_to_conflict_resolution() {
        let (_dir, path, _config, dav) = setup();
        let mut state = SyncState::default();
        let engine = SyncEngine::new(&dav, URL, &path, true);
        engine.sync(&mut state, TS).unwrap();

        let mut local = Config::example();
        local.snippet[0].text = "local edit".to_string();
        local.save(&path).unwrap();
        dav.reject_next_put();

        let outcome = engine.sync(&mut state, TS).unwrap();
        assert!(matches!(outcome, SyncOutcome::ConflictLocalWins { .. }));
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
        assert_eq!(outcome, SyncOutcome::Pushed { first_upload: false });
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
        assert_eq!(outcome, SyncOutcome::ConflictLocalWins { remote_backup: None });
        assert!(dav.keys().iter().all(|key| !key.contains("remote-backup")));
    }
}
