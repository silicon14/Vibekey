//! Locally persisted synchronisation bookkeeping.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What the last successful synchronisation looked like. This file is local to
/// the machine and is never uploaded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    /// SHA-256 of the local configuration after the last sync.
    pub local_hash: Option<String>,
    /// SHA-256 of the remote configuration after the last sync.
    pub remote_hash: Option<String>,
    /// ETag returned by the server, used for If-Match writes.
    pub remote_etag: Option<String>,
    pub last_sync_unix: Option<i64>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
}

impl SyncState {
    /// Path of the state file next to the configuration file.
    pub fn path_for(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("sync-state.json")
    }

    /// Load the state, falling back to a default when the file is missing or
    /// unreadable.
    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    }

    pub fn record_error(&mut self, message: impl Into<String>) {
        self.last_error = Some(message.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = SyncState::path_for(&dir.path().join("config.toml"));
        assert!(path.ends_with("sync-state.json"));

        let mut state = SyncState::default();
        state.local_hash = Some("abc".into());
        state.remote_etag = Some("etag-1".into());
        state.last_sync_unix = Some(1234);
        state.save(&path).unwrap();

        let loaded = SyncState::load(&path);
        assert_eq!(loaded.local_hash.as_deref(), Some("abc"));
        assert_eq!(loaded.remote_etag.as_deref(), Some("etag-1"));
        assert_eq!(loaded.last_sync_unix, Some(1234));
    }

    #[test]
    fn corrupt_state_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync-state.json");
        fs::write(&path, "not json").unwrap();
        assert_eq!(SyncState::load(&path).local_hash, None);
    }
}
