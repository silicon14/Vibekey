//! WebDAV synchronisation.
//!
//! * [engine] contains the conflict resolution logic and is platform
//!   independent.
//! * [webdav] contains the ureq based WebDAV client used by the app.

pub mod engine;
pub mod webdav;

pub use engine::{
    content_hash, PutResult, RemoteConfig, SyncEngine, SyncError, SyncOutcome, WebDav,
};
pub use webdav::{parent_url, UreqWebDav};
