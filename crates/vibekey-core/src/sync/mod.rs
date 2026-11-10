//! WebDAV synchronisation.
//!
//! * [engine] contains the conflict resolution logic and is platform
//!   independent.
//! * [webdav] contains the ureq based WebDAV client used by the app.
//! * [run] is the high level entry point used by the app and by the
//!   end-to-end tests: it loads the state, runs the engine and saves the state.

pub mod engine;
pub mod run;
pub mod webdav;

pub use engine::{
    content_hash, PutResult, RemoteConfig, SyncEngine, SyncError, SyncOutcome, SyncProtection,
    WebDav,
};
pub use run::{last_state, sync_once, sync_once_protected};
pub use webdav::{parent_url, CertificateFingerprint, UreqWebDav};
