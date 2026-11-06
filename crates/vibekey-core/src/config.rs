//! Configuration model.
//!
//! The configuration is a single TOML file. It is intentionally plain text so
//! that it can be edited by hand, versioned and synchronised through WebDAV.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::keymap::Keymap;

/// Name of the directory below APPDATA (or XDG_CONFIG_HOME).
pub const APP_DIR_NAME: &str = "Vibekey";
/// File name of the configuration inside the application directory.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// How a snippet hotkey is triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    /// Alt+1 immediately expands the snippet.
    #[default]
    Immediate,
    /// Alt has to be held for Config::hold_ms milliseconds first.
    Hold,
}

/// How the snippet text is delivered to the target application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputMethod {
    /// Put the text on the clipboard, send Ctrl+V, then restore the clipboard.
    /// This is the most reliable method for CJK text.
    #[default]
    Paste,
    /// Type the text with SendInput(KEYEVENTF_UNICODE).
    Type,
}

/// Global hotkeys of the two built-in menus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    /// Opens the selectable snippet list. Default: Alt + backtick.
    pub list: String,
    /// Opens the configuration menu. Default: Alt + Backspace.
    pub config: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            list: "alt+\x60".to_string(),
            config: "alt+backspace".to_string(),
        }
    }
}

/// WebDAV synchronisation settings.
///
/// The password itself is *never* stored here: it lives in the platform secret
/// store (DPAPI on Windows) so that the synchronised file does not contain
/// secrets and can be shared between machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub enabled: bool,
    /// Full URL of the remote configuration file, for example
    /// https://dav.example.com/remote.php/dav/files/me/vibekey/config.toml
    pub url: String,
    pub username: String,
    /// Synchronise every N minutes. 0 disables the periodic sync.
    pub interval_minutes: u32,
    /// Synchronise after the configuration was saved.
    pub sync_on_change: bool,
    /// Keep a copy of the remote file when both sides changed.
    pub backup_on_conflict: bool,
    pub timeout_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            username: String::new(),
            interval_minutes: 15,
            sync_on_change: true,
            backup_on_conflict: true,
            timeout_secs: 20,
        }
    }
}

/// One snippet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Snippet {
    /// Slot key: "1".."9", "0", "-", "=".
    /// An empty string means the snippet only appears in the list and has no
    /// direct hotkey.
    pub key: String,
    /// Short name shown in the list.
    pub name: String,
    /// Text to insert. Supports {date}, {time}, {datetime}, {clipboard},
    /// {user}, {host} and doubled-brace escapes.
    pub text: String,
    /// Send Enter after inserting the text.
    pub send_enter: bool,
    /// Disabled snippets are ignored completely.
    pub enabled: bool,
}

impl Default for Snippet {
    fn default() -> Self {
        Self {
            key: String::new(),
            name: String::new(),
            text: String::new(),
            send_enter: false,
            enabled: true,
        }
    }
}

impl Snippet {
    pub fn new(key: &str, name: &str, text: &str) -> Self {
        Self {
            key: key.to_string(),
            name: name.to_string(),
            text: text.to_string(),
            ..Self::default()
        }
    }
}

/// The whole configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub activation: ActivationMode,
    /// Milliseconds Alt has to be held in ActivationMode::Hold.
    pub hold_ms: u32,
    pub input_method: InputMethod,
    pub restore_clipboard: bool,
    pub start_with_windows: bool,
    pub ignore_apps: Vec<String>,
    pub hotkeys: Hotkeys,
    pub sync: SyncConfig,
    pub snippet: Vec<Snippet>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            activation: ActivationMode::default(),
            hold_ms: 350,
            input_method: InputMethod::default(),
            restore_clipboard: true,
            start_with_windows: false,
            ignore_apps: Vec::new(),
            hotkeys: Hotkeys::default(),
            sync: SyncConfig::default(),
            snippet: Vec::new(),
        }
    }
}

impl Config {
    /// A small configuration that is written on first start.
    pub fn example() -> Self {
        let mut config = Config::default();
        config.snippet = vec![
            Snippet::new("1", "打招呼", "你好，我是 {user}，很高兴认识你。"),
            Snippet::new("2", "邮箱", "me@example.com"),
            Snippet::new("3", "当前时间", "现在是 {datetime}"),
            Snippet::new("", "剪贴板", "{clipboard}"),
        ];
        config
    }

    /// Directory that holds the configuration, synchronisation state and log.
    pub fn config_dir() -> PathBuf {
        #[cfg(windows)]
        {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                return PathBuf::from(appdata).join(APP_DIR_NAME);
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
                return PathBuf::from(xdg).join("vibekey");
            }
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(".config").join("vibekey");
            }
        }
        PathBuf::from(APP_DIR_NAME)
    }

    pub fn default_path() -> PathBuf {
        Self::config_dir().join(CONFIG_FILE_NAME)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => ConfigError::NotFound(path.to_path_buf()),
            _ => ConfigError::Io {
                path: path.to_path_buf(),
                source,
            },
        })?;
        Self::parse(&text).map_err(|err| ConfigError::Parse {
            path: path.to_path_buf(),
            source: err,
        })
    }

    /// Parse a configuration from a TOML string.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Load the configuration, creating it from Config::example if missing.
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        match Self::load(path) {
            Ok(config) => Ok(config),
            Err(ConfigError::NotFound(_)) => {
                let config = Config::example();
                config.save(path)?;
                Ok(config)
            }
            Err(err) => Err(err),
        }
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|source| ConfigError::Serialize {
            message: source.to_string(),
        })
    }

    /// Persist the configuration atomically (write to a temporary file, then
    /// rename it over the destination).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = self.to_toml()?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    /// Validate the configuration. Errors are fatal for the hotkey layer,
    /// warnings are informational.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();

        if !(100..=3000).contains(&self.hold_ms) {
            issues.push(Issue::warning("hold_ms should be between 100 and 3000"));
        }
        if let Err(errors) = Keymap::build(self) {
            for error in errors {
                issues.push(Issue::error(error.to_string()));
            }
        }
        if self.sync.enabled {
            if self.sync.url.trim().is_empty() {
                issues.push(Issue::error("sync.url must be set when sync is enabled"));
            } else if !(self.sync.url.starts_with("http://")
                || self.sync.url.starts_with("https://"))
            {
                issues.push(Issue::error("sync.url must start with http:// or https://"));
            }
            if self.sync.interval_minutes == 0 {
                issues.push(Issue::warning(
                    "sync.interval_minutes is 0: periodic sync is disabled",
                ));
            }
            if self.sync.timeout_secs == 0 {
                issues.push(Issue::error("sync.timeout_secs must be greater than 0"));
            }
        }
        if self.hotkeys.list.trim().is_empty() {
            issues.push(Issue::error("hotkeys.list must not be empty"));
        }
        if self.hotkeys.config.trim().is_empty() {
            issues.push(Issue::error("hotkeys.config must not be empty"));
        }
        if self.ignore_apps.iter().any(|name| name.trim().is_empty()) {
            issues.push(Issue::warning("ignore_apps contains an empty entry"));
        }
        issues
    }

    /// Snippet bound to a slot key ("1", "0", "-", "=").
    pub fn snippet_for_key(&self, key: &str) -> Option<&Snippet> {
        self.snippet
            .iter()
            .find(|snippet| snippet.enabled && snippet.key == key)
    }

    /// Snippets shown in the picker, in configuration order.
    pub fn visible_snippets(&self) -> impl Iterator<Item = &Snippet> {
        self.snippet.iter().filter(|snippet| snippet.enabled)
    }
}

/// A validation finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub level: IssueLevel,
    pub message: String,
}

impl Issue {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: IssueLevel::Error,
            message: message.into(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: IssueLevel::Warning,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueLevel {
    Error,
    Warning,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.level {
            IssueLevel::Error => write!(f, "error: {}", self.message),
            IssueLevel::Warning => write!(f, "warning: {}", self.message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file not found: {0}")]
    NotFound(PathBuf),
    #[error("failed to read or write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialise configuration: {message}")]
    Serialize { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let issues = Config::default().validate();
        assert!(
            issues.iter().all(|i| i.level == IssueLevel::Warning),
            "unexpected errors: {issues:?}"
        );
    }

    #[test]
    fn example_round_trips_through_toml() {
        let config = Config::example();
        let text = config.to_toml().unwrap();
        let parsed = Config::parse(&text).unwrap();
        assert_eq!(parsed.snippet.len(), config.snippet.len());
        assert_eq!(parsed.snippet[0].text, config.snippet[0].text);
        assert_eq!(parsed.hotkeys.list, Hotkeys::default().list);
        assert_eq!(parsed.activation, ActivationMode::Immediate);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let parsed = Config::parse("[[snippet]]\nkey = \"1\"\ntext = \"hi\"\n").unwrap();
        assert_eq!(parsed.hold_ms, 350);
        assert!(parsed.restore_clipboard);
        assert_eq!(parsed.snippet[0].name, "");
        assert!(parsed.snippet[0].enabled);
        assert!(!parsed.snippet[0].send_enter);
    }

    #[test]
    fn invalid_slot_keys_are_reported() {
        let mut config = Config::default();
        config.snippet = vec![
            Snippet::new("1", "a", "a"),
            Snippet::new("1", "b", "b"),
            Snippet::new("x", "c", "c"),
        ];
        let issues = config.validate();
        assert!(issues.iter().any(|issue| issue.level == IssueLevel::Error));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::example();
        config.sync.enabled = true;
        config.sync.url = "https://dav.example.com/vibekey/config.toml".into();
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.sync.url, config.sync.url);
        assert_eq!(loaded.snippet.len(), config.snippet.len());
    }

    #[test]
    fn snippet_lookup_skips_disabled() {
        let mut config = Config::default();
        let mut disabled = Snippet::new("1", "off", "off");
        disabled.enabled = false;
        config.snippet = vec![disabled, Snippet::new("2", "on", "on")];
        assert!(config.snippet_for_key("1").is_none());
        assert!(config.snippet_for_key("2").is_some());
        assert_eq!(config.visible_snippets().count(), 1);
    }
}
