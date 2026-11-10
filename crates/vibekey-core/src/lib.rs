//! Vibekey core library.
//!
//! Everything in this crate is platform independent and can be unit tested on
//! Linux/macOS as well as on Windows:
//!
//! * config - the TOML configuration format and its validation.
//! * keymap - hotkey parsing and the mapping from a hotkey to an action.
//! * expand - snippet template expansion ({date}, {clipboard}, ...).
//! * sync - WebDAV synchronisation of the configuration file.
//! * secret - abstraction over the platform password store (DPAPI on Windows).
//! * state - locally persisted synchronisation bookkeeping.

pub mod config;
pub mod crypto;
pub mod expand;
pub mod keymap;
pub mod secret;
pub mod state;
pub mod sync;

pub use config::{
    ActivationMode, Config, ConfigError, InputMethod, Issue, IssueLevel, Snippet, SyncConfig,
};
pub use crypto::{
    has_config_envelope_marker, is_encrypted_config, CryptoError, MIN_SYNC_PASSWORD_CHARS,
};
pub use expand::{expand, ExpandContext};
pub use keymap::{Binding, Hotkey, Key, KeyError, Keymap, KeymapError, Modifiers};
pub use secret::{SecretError, SecretKind, SecretStore};
pub use state::SyncState;
pub use sync::{last_state, sync_once, sync_once_protected};
