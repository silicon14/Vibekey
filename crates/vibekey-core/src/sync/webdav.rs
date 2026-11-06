//! WebDAV client built on top of ureq.
//!
//! Only the small subset needed to synchronise one configuration file is
//! implemented: GET, PUT with If-Match and MKCOL for the parent directories.
//! TLS is handled by rustls with bundled webpki roots, which works on Windows 7
//! regardless of the state of the system certificate store.

use std::io::Read;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ureq::{Agent, AgentBuilder};

use super::engine::{PutResult, RemoteConfig, SyncError, WebDav};

pub struct UreqWebDav {
    agent: Agent,
    auth: Option<String>,
}

impl UreqWebDav {
    pub fn new(username: &str, password: &str, timeout: Duration) -> Self {
        let agent = AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .redirects(5)
            .build();
        let auth = if username.is_empty() {
            None
        } else {
            let raw = format!("{username}:{password}");
            Some(format!("Basic {}", BASE64.encode(raw.as_bytes())))
        };
        Self { agent, auth }
    }

    fn request(&self, method: &str, url: &str) -> ureq::Request {
        let request = self.agent.request(method, url);
        match &self.auth {
            Some(auth) => request.set("Authorization", auth),
            None => request,
        }
    }

    fn mkcol(&self, url: &str, depth: u32) -> Result<(), SyncError> {
        if depth > 8 {
            return Err(SyncError::Invalid(format!(
                "could not create remote collection for {url}"
            )));
        }
        match self.request("MKCOL", url).call() {
            Ok(_) => Ok(()),
            // 405 Not Allowed: the collection already exists.
            Err(ureq::Error::Status(405, _)) => Ok(()),
            // 409 Conflict: a parent is missing, create it first.
            Err(ureq::Error::Status(409, _)) => {
                let parent = parent_url(url).ok_or_else(|| {
                    SyncError::Invalid(format!("cannot determine the parent of {url}"))
                })?;
                self.mkcol(&parent, depth + 1)?;
                self.mkcol(url, depth + 1)
            }
            Err(err) => Err(map_ureq_error(err)),
        }
    }
}

impl WebDav for UreqWebDav {
    fn ensure_parent(&self, url: &str) -> Result<(), SyncError> {
        if let Some(parent) = parent_url(url) {
            self.mkcol(&parent, 0)?;
        }
        Ok(())
    }

    fn get(&self, url: &str) -> Result<Option<RemoteConfig>, SyncError> {
        match self.request("GET", url).call() {
            Ok(response) => {
                let etag = response.header("ETag").map(|value| value.to_string());
                let mut body = Vec::new();
                response
                    .into_reader()
                    .read_to_end(&mut body)
                    .map_err(|err| SyncError::Network(err.to_string()))?;
                Ok(Some(RemoteConfig { body, etag }))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(err) => Err(map_ureq_error(err)),
        }
    }

    fn put(&self, url: &str, body: &[u8], if_match: Option<&str>) -> Result<PutResult, SyncError> {
        let mut request = self
            .request("PUT", url)
            .set("Content-Type", "text/plain; charset=utf-8");
        if let Some(etag) = if_match {
            request = request.set("If-Match", etag);
        }
        match request.send_bytes(body) {
            Ok(response) => Ok(PutResult::Stored {
                etag: response.header("ETag").map(|value| value.to_string()),
            }),
            Err(ureq::Error::Status(412, _)) => Ok(PutResult::PreconditionFailed),
            Err(err) => Err(map_ureq_error(err)),
        }
    }
}

fn map_ureq_error(err: ureq::Error) -> SyncError {
    match err {
        ureq::Error::Status(status, response) => SyncError::Http {
            status,
            message: response.status_text().to_string(),
        },
        ureq::Error::Transport(transport) => SyncError::Network(transport.to_string()),
    }
}

/// Parent collection URL, e.g.
/// "https://dav.example.com/a/b/config.toml" -> "https://dav.example.com/a/b".
pub fn parent_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let trimmed = without_query.trim_end_matches('/');
    let scheme_end = trimmed.find("://").map(|index| index + 3)?;
    let last_slash = trimmed.rfind('/')?;
    if last_slash < scheme_end || last_slash == scheme_end.saturating_sub(1) {
        return None;
    }
    if last_slash <= scheme_end {
        return None;
    }
    Some(trimmed[..last_slash].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_url_of_a_file() {
        assert_eq!(
            parent_url("https://dav.example.com/a/b/config.toml").as_deref(),
            Some("https://dav.example.com/a/b")
        );
        assert_eq!(
            parent_url("https://dav.example.com/config.toml").as_deref(),
            Some("https://dav.example.com")
        );
    }

    #[test]
    fn parent_url_handles_trailing_slash_and_query() {
        assert_eq!(
            parent_url("https://dav.example.com/a/b/").as_deref(),
            Some("https://dav.example.com/a")
        );
        assert_eq!(
            parent_url("https://dav.example.com/a/b/config.toml?x=1").as_deref(),
            Some("https://dav.example.com/a/b")
        );
    }

    #[test]
    fn parent_url_without_scheme_is_none() {
        assert!(parent_url("/a/b/config.toml").is_none());
        assert!(parent_url("config.toml").is_none());
    }
}
