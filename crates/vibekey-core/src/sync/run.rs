//! High level entry points that run one synchronisation step.
//!
//! [`sync_once`] glues together the WebDAV client, the conflict resolution
//! engine and the persisted [`SyncState`] so that the platform front end only
//! has to call a single function.

use std::path::Path;
use std::time::Duration;

use crate::config::SyncConfig;
use crate::state::SyncState;

use super::engine::{SyncEngine, SyncError, SyncOutcome, SyncProtection};
use super::webdav::UreqWebDav;

/// Synchronise the configuration file once.
///
/// * `config_path` - local configuration file; the state file and local backup
///   live next to it.
/// * `sync` - WebDAV settings from the configuration.
/// * `password` - password from the platform secret store. It is intentionally
///   a separate argument: the password is never part of `config.toml`.
/// * `timestamp` - `YYYYmmdd-HHMMSS` used to name the remote backup file when
///   both sides changed.
///
/// The updated [`SyncState`] is written to disk even when the synchronisation
/// fails, so the last error survives a restart. A failure to persist the state
/// never masks the original synchronisation error.
pub fn sync_once(
    config_path: &Path,
    sync: &SyncConfig,
    password: Option<&str>,
    timestamp: &str,
) -> Result<(SyncOutcome, SyncState), SyncError> {
    sync_once_protected(
        config_path,
        sync,
        password,
        SyncProtection::disabled(),
        timestamp,
    )
}

/// Protected variant of [`sync_once`]. Password rotation cleanup deliberately
/// remains with the platform secret store: callers must save the returned state
/// successfully before committing the previous-password deletion.
pub fn sync_once_protected(
    config_path: &Path,
    sync: &SyncConfig,
    password: Option<&str>,
    protection: SyncProtection,
    timestamp: &str,
) -> Result<(SyncOutcome, SyncState), SyncError> {
    let state_path = SyncState::path_for(config_path);
    let mut state = SyncState::load(&state_path);

    let dav = UreqWebDav::new_with_fingerprint(
        &sync.username,
        password.unwrap_or(""),
        Duration::from_secs(sync.timeout_secs),
        &sync.certificate_sha256,
    )?;
    let engine = SyncEngine::new(&dav, &sync.url, config_path, sync.backup_on_conflict)
        .with_protection(protection);

    match engine.sync(&mut state, timestamp) {
        Ok(outcome) => {
            state.save(&state_path)?;
            Ok((outcome, state))
        }
        Err(error) => {
            state.record_error(error.to_string());
            let _ = state.save(&state_path);
            Err(error)
        }
    }
}

/// The synchronisation state stored next to `config_path`.
///
/// Missing or corrupt state files are reported as [`SyncState::default`], which
/// means "never synchronised".
pub fn last_state(config_path: &Path) -> SyncState {
    SyncState::load(&SyncState::path_for(config_path))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;
    use crate::config::Config;

    fn unused_local_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        port
    }

    #[test]
    fn last_state_is_default_when_no_state_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state = last_state(&config_path);
        assert!(state.local_hash.is_none());
        assert!(state.last_sync_unix.is_none());
    }

    #[test]
    fn last_state_reads_the_state_file_next_to_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_path = SyncState::path_for(&config_path);

        let mut state = SyncState::default();
        state.local_hash = Some("abc123".to_string());
        state.last_outcome = Some("pulled".to_string());
        state.save(&state_path).unwrap();

        let loaded = last_state(&config_path);
        assert_eq!(loaded.local_hash.as_deref(), Some("abc123"));
        assert_eq!(loaded.last_outcome.as_deref(), Some("pulled"));
    }

    #[test]
    fn network_failure_is_recorded_in_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        Config::example().save(&config_path).unwrap();

        let sync = SyncConfig {
            enabled: true,
            url: format!(
                "http://127.0.0.1:{}/vibekey/config.toml",
                unused_local_port()
            ),
            username: "me".to_string(),
            timeout_secs: 2,
            ..SyncConfig::default()
        };

        let error = sync_once(&config_path, &sync, Some("secret"), "2026-11-06-11-00-00")
            .expect_err("connecting to a closed port must fail");
        assert!(matches!(error, SyncError::Network(_)), "got {error:?}");

        let state = last_state(&config_path);
        let recorded = state.last_error.expect("error recorded in state");
        assert!(
            recorded.contains("network error"),
            "unexpected recorded error: {recorded}"
        );
    }
}
