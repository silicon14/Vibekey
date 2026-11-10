//! End-to-end tests for the WebDAV synchronisation pipeline.
//!
//! A small in-process WebDAV server built on tiny_http is used instead of a
//! real remote: the tests only ever talk to 127.0.0.1 and therefore never need
//! network access. The mock implements the subset used by UreqWebDav:
//!
//! * GET with ETag and 404 Not Found;
//! * PUT with If-Match: <etag> (412 on mismatch);
//! * MKCOL with 201 (created), 405 (already exists) and 409 (missing parent);
//! * HTTP Basic authentication (401 + WWW-Authenticate).
//!
//! Every test starts its own server on an ephemeral port and a pair of
//! temporary directories when it needs to model two machines.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use tempfile::TempDir;
use tiny_http::{Header, Request, Response, Server, StatusCode};

use vibekey_core::crypto::{decrypt_config, encrypt_config, is_encrypted_config};
use vibekey_core::sync::{
    last_state, sync_once, sync_once_protected, SyncError, SyncOutcome, SyncProtection,
};
use vibekey_core::{Config, CryptoError, SyncConfig};

/// Timestamp passed to sync_once; also part of the remote backup file name.
const TS: &str = "2026-11-06-11-00-00";
const REMOTE_PATH: &str = "/vibekey/config.toml";

// ---------------------------------------------------------------------------
// Mock WebDAV server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StoredFile {
    body: Vec<u8>,
    etag: String,
}

struct MockState {
    files: HashMap<String, StoredFile>,
    collections: HashSet<String>,
    etag_counter: u32,
    /// None disables authentication (the client may send any header).
    credentials: Option<(String, String)>,
    /// Fail exactly one PUT after the request reached the server.
    reject_next_put: bool,
    /// "METHOD /path" of every request, in order.
    requests: Vec<String>,
}

struct MockDav {
    addr: SocketAddr,
    server: Arc<Server>,
    state: Arc<Mutex<MockState>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockDav {
    fn start(credentials: Option<(&str, &str)>) -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("start mock WebDAV server"));
        let addr = server.server_addr().to_ip().expect("TCP listen address");
        let state = Arc::new(Mutex::new(MockState {
            files: HashMap::new(),
            collections: HashSet::from(["/".to_string()]),
            etag_counter: 0,
            credentials: credentials
                .map(|(user, password)| (user.to_string(), password.to_string())),
            reject_next_put: false,
            requests: Vec::new(),
        }));

        let thread_server = Arc::clone(&server);
        let thread_state = Arc::clone(&state);
        let handle = thread::spawn(move || {
            for request in thread_server.incoming_requests() {
                if let Err(error) = handle_request(request, &thread_state) {
                    eprintln!("mock WebDAV server error: {error}");
                }
            }
        });

        Self {
            addr,
            server,
            state,
            handle: Some(handle),
        }
    }

    /// Absolute URL of a resource on the mock server.
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn body(&self, path: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .files
            .get(path)
            .map(|file| file.body.clone())
    }

    fn etag(&self, path: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .files
            .get(path)
            .map(|file| file.etag.clone())
    }

    fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().files.keys().cloned().collect()
    }

    fn collections(&self) -> HashSet<String> {
        self.state.lock().unwrap().collections.clone()
    }

    fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    fn put_count(&self, path: &str) -> usize {
        let expected = format!("PUT {path}");
        self.requests()
            .iter()
            .filter(|request| *request == &expected)
            .count()
    }

    fn reject_next_put(&self) {
        self.state.lock().unwrap().reject_next_put = true;
    }

    /// Simulate a raw external writer, including tampering/downgrade attacks.
    fn store(&self, path: &str, body: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        let etag = next_etag(&mut state);
        state
            .files
            .insert(path.to_string(), StoredFile { body, etag });
    }

    /// Simulate another writer that updated the file's metadata but not its
    /// content, i.e. the ETag stored in the local sync state becomes stale.
    fn bump_etag(&self, path: &str) {
        let mut state = self.state.lock().unwrap();
        let body = state
            .files
            .get(path)
            .expect("remote file exists")
            .body
            .clone();
        let etag = next_etag(&mut state);
        state
            .files
            .insert(path.to_string(), StoredFile { body, etag });
    }
}

impl Drop for MockDav {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_request(mut request: Request, state: &Mutex<MockState>) -> std::io::Result<()> {
    let method = request.method().as_str().to_string();
    let path = request_path(request.url());
    let authorization = header_value(&request, "Authorization");
    let if_match = header_value(&request, "If-Match");

    let mut state = state.lock().unwrap();
    state.requests.push(format!("{method} {path}"));

    if let Some((user, password)) = state.credentials.clone() {
        let expected = format!("Basic {}", BASE64.encode(format!("{user}:{password}")));
        if authorization.as_deref() != Some(expected.as_str()) {
            let response = empty_response(401).with_header(
                Header::from_bytes(&b"WWW-Authenticate"[..], &b"Basic realm=\"vibekey\""[..])
                    .expect("valid header"),
            );
            return request.respond(response);
        }
    }

    match method.as_str() {
        "GET" => match state.files.get(&path) {
            Some(file) => {
                let etag = file.etag.clone();
                let response = Response::from_data(file.body.clone())
                    .with_header(
                        Header::from_bytes(&b"ETag"[..], etag.as_bytes()).expect("valid header"),
                    )
                    .with_header(
                        Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
                            .expect("valid header"),
                    );
                request.respond(response)
            }
            None => request.respond(empty_response(404)),
        },
        "PUT" => {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            if !state.collections.contains(parent_path(&path)) {
                return request.respond(empty_response(409));
            }
            if std::mem::take(&mut state.reject_next_put) {
                return request.respond(empty_response(412));
            }
            let etag_matches = match if_match.as_deref() {
                None => true,
                Some(expected) => state
                    .files
                    .get(&path)
                    .map(|file| file.etag == expected)
                    .unwrap_or(false),
            };
            if !etag_matches {
                return request.respond(empty_response(412));
            }
            let etag = next_etag(&mut state);
            state.files.insert(
                path,
                StoredFile {
                    body,
                    etag: etag.clone(),
                },
            );
            let response = empty_response(201).with_header(
                Header::from_bytes(&b"ETag"[..], etag.as_bytes()).expect("valid header"),
            );
            request.respond(response)
        }
        "MKCOL" => {
            let collection = request_path(&path);
            if state.collections.contains(&collection) {
                return request.respond(empty_response(405));
            }
            let parent = parent_path(&collection).to_string();
            if !state.collections.contains(&parent) {
                return request.respond(empty_response(409));
            }
            state.collections.insert(collection);
            request.respond(empty_response(201))
        }
        _ => request.respond(empty_response(405)),
    }
}

fn empty_response(status: u16) -> Response<std::io::Empty> {
    Response::empty(StatusCode(status))
}

fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

/// Request target without query string or trailing slash.
fn request_path(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_string()
}

/// Parent collection path: "/a/b/c" -> "/a/b", "/a" -> "/".
fn parent_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &trimmed[..index],
    }
}

fn next_etag(state: &mut MockState) -> String {
    state.etag_counter += 1;
    format!("\"etag-{}\"", state.etag_counter)
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

struct Machine {
    dir: TempDir,
    config_path: PathBuf,
}

impl Machine {
    fn new() -> Self {
        Self::with_config(&Config::example())
    }

    fn with_config(config: &Config) -> Self {
        let dir = tempfile::tempdir().expect("create machine directory");
        let config_path = dir.path().join("config.toml");
        config.save(&config_path).expect("write config");
        Self { dir, config_path }
    }

    fn path(&self) -> &Path {
        &self.config_path
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn read_bytes(&self) -> Vec<u8> {
        fs::read(&self.config_path).expect("read config")
    }

    fn load(&self) -> Config {
        Config::load(&self.config_path).expect("load config")
    }

    fn save(&self, config: &Config) {
        config.save(&self.config_path).expect("save config");
    }
}

fn sync_config(dav: &MockDav, remote_path: &str) -> SyncConfig {
    SyncConfig {
        enabled: true,
        url: dav.url(remote_path),
        username: "me".to_string(),
        interval_minutes: 0,
        sync_on_change: true,
        backup_on_conflict: true,
        encrypt: false,
        timeout_secs: 5,
    }
}

/// Send a raw HTTP/1.1 request (used to inspect the 401 challenge that the
/// high level client intentionally hides behind SyncError::Http).
fn raw_http(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to mock server");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8_lossy(&response).into_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn first_upload_pushes_the_config_and_persists_state() {
    let password = "s3cr3t-pässword";
    let dav = MockDav::start(Some(("me", password)));
    let machine = Machine::new();
    let sync = sync_config(&dav, REMOTE_PATH);

    let (outcome, state) =
        sync_once(machine.path(), &sync, Some(password), TS).expect("first sync");

    assert_eq!(outcome, SyncOutcome::Pushed { first_upload: true });
    assert_eq!(state.last_outcome.as_deref(), Some("pushed"));
    assert!(state.last_sync_unix.is_some());
    assert!(state.last_error.is_none());
    assert!(state.remote_etag.is_some());
    assert_eq!(
        state.local_hash, state.remote_hash,
        "after a push both hashes must be identical"
    );

    let remote = dav.body(REMOTE_PATH).expect("remote config");
    assert_eq!(remote, machine.read_bytes());
    assert!(
        !String::from_utf8_lossy(&remote).contains(password),
        "the WebDAV password must never be part of the synced config"
    );

    assert!(dav.collections().contains("/vibekey"));
    assert_eq!(dav.etag(REMOTE_PATH), state.remote_etag);

    let requests = dav.requests();
    assert!(requests.iter().any(|r| r == "MKCOL /vibekey"));
    assert!(requests.iter().any(|r| r == "GET /vibekey/config.toml"));
    assert!(requests.iter().any(|r| r == "PUT /vibekey/config.toml"));

    // The state was written next to the config and can be read back without a
    // network round trip.
    let persisted = last_state(machine.path());
    assert_eq!(persisted.local_hash, state.local_hash);
    assert_eq!(persisted.last_outcome.as_deref(), Some("pushed"));
}

#[test]
fn second_sync_from_the_same_machine_is_up_to_date() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let sync = sync_config(&dav, REMOTE_PATH);
    sync_once(machine.path(), &sync, None, TS).expect("first sync");

    let (outcome, state) = sync_once(machine.path(), &sync, None, TS).expect("second sync");
    assert_eq!(outcome, SyncOutcome::UpToDate);
    assert_eq!(state.last_outcome.as_deref(), Some("up-to-date"));
}

#[test]
fn new_machine_pulls_the_remote_config_and_backs_up_the_local_one() {
    let dav = MockDav::start(None);
    let source = Machine::new();
    let mut source_config = Config::example();
    source_config.snippet[0].text = "来自第一台机器".to_string();
    source.save(&source_config);
    let sync = sync_config(&dav, REMOTE_PATH);
    sync_once(source.path(), &sync, None, TS).expect("source push");

    let target = Machine::new();
    let (outcome, state) = sync_once(target.path(), &sync, None, TS).expect("target pull");

    assert_eq!(outcome, SyncOutcome::Pulled);
    assert_eq!(state.last_outcome.as_deref(), Some("pulled"));
    assert_eq!(target.load().snippet[0].text, "来自第一台机器");
    assert_eq!(
        target.read_bytes(),
        dav.body(REMOTE_PATH).expect("remote config")
    );
    assert!(
        target.dir().join("config.toml.bak").exists(),
        "the previous local config must be backed up before it is overwritten"
    );
}

#[test]
fn two_machines_end_to_end_with_conflict_backup() {
    let dav = MockDav::start(None);
    let machine1 = Machine::new();
    let machine2 = Machine::new();
    let sync = sync_config(&dav, REMOTE_PATH);

    // 1. machine1 uploads the first version.
    let mut config1 = Config::example();
    config1.snippet[0].text = "第一台机器初始".to_string();
    machine1.save(&config1);
    let (outcome, _) = sync_once(machine1.path(), &sync, None, TS).expect("machine1 push");
    assert_eq!(outcome, SyncOutcome::Pushed { first_upload: true });

    // 2. machine2 pulls it (a second machine that has never synced).
    let (outcome, _) = sync_once(machine2.path(), &sync, None, TS).expect("machine2 pull");
    assert_eq!(outcome, SyncOutcome::Pulled);
    assert_eq!(machine2.load().snippet[0].text, "第一台机器初始");

    // 3. machine1 edits and uploads.
    let mut config1_edit = machine1.load();
    config1_edit.snippet[0].text = "第一台机器修改".to_string();
    machine1.save(&config1_edit);
    let (outcome, _) = sync_once(machine1.path(), &sync, None, TS).expect("machine1 push 2");
    assert_eq!(
        outcome,
        SyncOutcome::Pushed {
            first_upload: false
        }
    );

    // 4. machine2 edits its copy too, then syncs: both sides changed.
    let mut config2_edit = machine2.load();
    config2_edit.snippet[0].text = "第二台机器修改".to_string();
    machine2.save(&config2_edit);
    let (outcome, state) =
        sync_once(machine2.path(), &sync, None, TS).expect("machine2 conflict sync");

    let backup_url = match outcome {
        SyncOutcome::ConflictLocalWins { remote_backup } => {
            remote_backup.expect("a remote backup must be created on conflict")
        }
        other => panic!("expected a conflict, got {other:?}"),
    };
    assert!(
        backup_url.contains(".remote-backup-"),
        "unexpected backup URL: {backup_url}"
    );
    assert!(
        backup_url.ends_with(".toml"),
        "unexpected backup URL: {backup_url}"
    );
    assert!(
        backup_url.contains(TS),
        "the backup name must contain the timestamp: {backup_url}"
    );

    let backup_path = backup_url
        .strip_prefix(&format!("http://{}", dav.addr))
        .expect("backup URL must point at the mock server");
    let backed_up = String::from_utf8(dav.body(backup_path).expect("backup body")).unwrap();
    assert!(backed_up.contains("第一台机器修改"));
    assert!(!backed_up.contains("第二台机器修改"));

    // The local (machine2) version won and is now the remote file.
    assert_eq!(
        dav.body(REMOTE_PATH).expect("remote config"),
        machine2.read_bytes()
    );
    assert_eq!(state.last_outcome.as_deref(), Some("conflict-local-wins"));
    assert_eq!(
        last_state(machine2.path()).last_outcome.as_deref(),
        Some("conflict-local-wins")
    );

    // 5. machine1 sees the remote change and pulls machine2's version.
    let (outcome, _) = sync_once(machine1.path(), &sync, None, TS).expect("machine1 pull");
    assert_eq!(outcome, SyncOutcome::Pulled);
    assert_eq!(machine1.load().snippet[0].text, "第二台机器修改");
}

#[test]
fn stale_etag_is_recovered_by_retrying_the_conditional_put() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let sync = sync_config(&dav, REMOTE_PATH);
    sync_once(machine.path(), &sync, None, TS).expect("first sync");

    // Another client rewrites the same content, so only the ETag changes and
    // the ETag remembered in sync-state.json is now stale.
    dav.bump_etag(REMOTE_PATH);

    let mut config = machine.load();
    config.snippet[0].text = "本地修改".to_string();
    machine.save(&config);

    let (outcome, state) = sync_once(machine.path(), &sync, None, TS).expect("sync after 412");
    assert_eq!(
        outcome,
        SyncOutcome::Pushed {
            first_upload: false
        }
    );
    assert_eq!(
        dav.body(REMOTE_PATH).expect("remote config"),
        machine.read_bytes()
    );
    assert_eq!(state.remote_etag, dav.etag(REMOTE_PATH));
    assert!(
        dav.keys().iter().all(|key| !key.contains("remote-backup")),
        "a stale ETag with unchanged content is not a real conflict: {:?}",
        dav.keys()
    );

    let puts = dav
        .requests()
        .iter()
        .filter(|request| *request == "PUT /vibekey/config.toml")
        .count();
    assert_eq!(
        puts, 2,
        "the first conditional PUT must fail and be retried"
    );
}

#[test]
fn wrong_password_is_mapped_to_http_401() {
    let dav = MockDav::start(Some(("me", "correct horse battery staple")));
    let machine = Machine::new();
    let sync = sync_config(&dav, REMOTE_PATH);

    let error = sync_once(machine.path(), &sync, Some("wrong password"), TS)
        .expect_err("a wrong password must fail");
    match error {
        SyncError::Http { status, message } => {
            assert_eq!(status, 401);
            assert!(
                message.contains("Unauthorized"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected SyncError::Http, got {other:?}"),
    }

    // The failure is persisted for the settings dialog / tray.
    let state = last_state(machine.path());
    let recorded = state.last_error.expect("error recorded in state");
    assert!(
        recorded.contains("401"),
        "unexpected recorded error: {recorded}"
    );
    assert!(dav.body(REMOTE_PATH).is_none(), "nothing may be uploaded");

    // The right password works against the same server.
    let (outcome, _) = sync_once(
        machine.path(),
        &sync,
        Some("correct horse battery staple"),
        TS,
    )
    .expect("sync with the correct password");
    assert_eq!(outcome, SyncOutcome::Pushed { first_upload: true });
}

#[test]
fn unauthorized_requests_are_challenged_with_basic_auth() {
    let dav = MockDav::start(Some(("me", "pass")));
    let response = raw_http(
        dav.addr,
        "GET /vibekey/config.toml HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "unexpected response: {response}"
    );
    assert!(
        response.contains("WWW-Authenticate: Basic realm=\"vibekey\""),
        "missing Basic challenge: {response}"
    );

    let authorized = raw_http(
        dav.addr,
        &format!(
            "GET /vibekey/config.toml HTTP/1.1\r\n\
             Host: localhost\r\n\
             Authorization: Basic {}\r\n\
             Connection: close\r\n\r\n",
            BASE64.encode("me:pass")
        ),
    );
    assert!(
        authorized.starts_with("HTTP/1.1 404"),
        "an authenticated missing file must be a 404: {authorized}"
    );
}

#[test]
fn parent_collections_are_created_recursively() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let sync = sync_config(&dav, "/vibekey/sub/dir/config.toml");

    let (outcome, _) = sync_once(machine.path(), &sync, None, TS).expect("first sync");
    assert_eq!(outcome, SyncOutcome::Pushed { first_upload: true });

    let collections = dav.collections();
    for collection in ["/vibekey", "/vibekey/sub", "/vibekey/sub/dir"] {
        assert!(
            collections.contains(collection),
            "missing collection {collection}: {collections:?}"
        );
    }

    // 409 (missing parent) is retried bottom-up, 201 is success.
    let requests = dav.requests();
    let mkcols: Vec<&str> = requests
        .iter()
        .filter_map(|request| request.strip_prefix("MKCOL "))
        .collect();
    assert_eq!(
        mkcols,
        vec![
            "/vibekey/sub/dir",
            "/vibekey/sub",
            "/vibekey",
            "/vibekey/sub",
            "/vibekey/sub/dir",
        ]
    );

    // A second sync hits an existing collection (405) and must still succeed.
    let (outcome, _) = sync_once(machine.path(), &sync, None, TS).expect("second sync");
    assert_eq!(outcome, SyncOutcome::UpToDate);
}

#[test]
fn large_chinese_body_round_trips_byte_for_byte() {
    // ~277 KiB of UTF-8: larger than a typical snippet config, exercises the
    // request body handling on both the client and the mock server.
    let payload = "这是一段用于验证较大正文完整性的中文文本。".repeat(4500);
    assert!(payload.len() > 250 * 1024);

    let dav = MockDav::start(None);
    let source = Machine::new();
    let mut config = Config::example();
    config.snippet[0].text = payload.clone();
    source.save(&config);
    let sync = sync_config(&dav, REMOTE_PATH);

    let (outcome, _) = sync_once(source.path(), &sync, None, TS).expect("source push");
    assert_eq!(outcome, SyncOutcome::Pushed { first_upload: true });

    let remote = dav.body(REMOTE_PATH).expect("remote config");
    assert!(
        remote.len() > 250 * 1024,
        "remote body is too small: {}",
        remote.len()
    );
    assert_eq!(remote, source.read_bytes());

    let target = Machine::new();
    let (outcome, _) = sync_once(target.path(), &sync, None, TS).expect("target pull");
    assert_eq!(outcome, SyncOutcome::Pulled);
    assert_eq!(target.load().snippet[0].text, payload);
    assert_eq!(target.read_bytes(), remote);
}

// ---------------------------------------------------------------------------
// Authenticated configuration encryption over intentionally plaintext HTTP
// ---------------------------------------------------------------------------

const SYNC_PASSWORD: &str = "a separate configuration password";

fn encryption(current: &str) -> SyncProtection {
    SyncProtection::new(true, Some(current.to_string()), None).expect("valid encryption settings")
}

fn encrypted_sync(dav: &MockDav) -> SyncConfig {
    SyncConfig {
        encrypt: true,
        ..sync_config(dav, REMOTE_PATH)
    }
}

#[test]
fn encrypted_http_round_trip_hides_secrets_and_second_sync_is_a_noop() {
    let dav = MockDav::start(None);
    let source = Machine::new();
    let mut config = Config::example();
    config.sync.encrypt = true;
    config.snippet[0].text = "绝密片段-不得出现在HTTP正文中".to_string();
    source.save(&config);
    let sync = encrypted_sync(&dav);

    let (outcome, state) =
        sync_once_protected(source.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
            .expect("encrypted first upload");
    assert_eq!(outcome, SyncOutcome::Pushed { first_upload: true });
    assert_eq!(state.remote_encrypted, Some(true));
    assert_eq!(state.local_hash, state.remote_hash);

    let remote = dav.body(REMOTE_PATH).expect("encrypted remote");
    assert!(is_encrypted_config(&remote));
    assert!(!remote
        .windows("绝密片段".len())
        .any(|window| window == "绝密片段".as_bytes()));
    assert_ne!(remote, source.read_bytes());

    let (outcome, _) =
        sync_once_protected(source.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
            .expect("encrypted no-op");
    assert_eq!(outcome, SyncOutcome::UpToDate);
    assert_eq!(dav.put_count(REMOTE_PATH), 1, "no-op must not re-encrypt");

    let target = Machine::new();
    let mut target_config = target.load();
    target_config.sync.encrypt = true;
    target.save(&target_config);
    let (outcome, _) =
        sync_once_protected(target.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
            .expect("new machine decrypts");
    assert_eq!(outcome, SyncOutcome::Pulled);
    assert_eq!(
        target.load().snippet[0].text,
        "绝密片段-不得出现在HTTP正文中"
    );
    assert!(
        target.load().sync.encrypt,
        "pull must preserve local policy"
    );
    assert_ne!(target.read_bytes(), remote, "local TOML remains plaintext");
    assert!(target.dir().join("config.toml.bak").exists());
}

#[test]
fn wrong_password_tamper_and_plaintext_downgrade_fail_closed() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let mut config = machine.load();
    config.sync.encrypt = true;
    config.snippet[0].text = "authenticated secret".to_string();
    machine.save(&config);
    let sync = encrypted_sync(&dav);
    sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
        .expect("initial encrypted upload");
    let local_before = machine.read_bytes();
    let remote_before = dav.body(REMOTE_PATH).unwrap();

    let error = sync_once_protected(
        machine.path(),
        &sync,
        None,
        encryption("definitely wrong password"),
        TS,
    )
    .expect_err("wrong encryption password must fail");
    assert!(matches!(error, SyncError::Crypto(CryptoError::Decrypt)));
    assert_eq!(machine.read_bytes(), local_before);
    assert_eq!(dav.body(REMOTE_PATH).unwrap(), remote_before);
    assert_eq!(dav.put_count(REMOTE_PATH), 1);

    let mut tampered = remote_before.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    dav.store(REMOTE_PATH, tampered);
    let error = sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
        .expect_err("tampering must fail authentication");
    assert!(matches!(error, SyncError::Crypto(CryptoError::Decrypt)));
    assert_eq!(machine.read_bytes(), local_before);
    assert_eq!(dav.put_count(REMOTE_PATH), 1);

    dav.store(
        REMOTE_PATH,
        Config::example().to_toml().unwrap().into_bytes(),
    );
    let error = sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
        .expect_err("encrypted-to-plaintext downgrade must fail");
    assert!(matches!(error, SyncError::Downgrade));
    assert_eq!(machine.read_bytes(), local_before);
    assert_eq!(dav.put_count(REMOTE_PATH), 1);
    assert!(dav.keys().iter().all(|key| !key.contains("remote-backup")));

    let invalid_secret = "text = \"decrypted-secret-that-must-not-enter-logs";
    dav.store(
        REMOTE_PATH,
        encrypt_config(invalid_secret.as_bytes(), SYNC_PASSWORD).unwrap(),
    );
    let error = sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
        .expect_err("invalid decrypted TOML must fail without leaking its source line");
    match error {
        SyncError::RemoteInvalid(message) => {
            assert!(!message.contains("decrypted-secret"), "leaked: {message}");
        }
        other => panic!("expected RemoteInvalid, got {other:?}"),
    }
    let recorded = last_state(machine.path()).last_error.unwrap();
    assert!(
        !recorded.contains("decrypted-secret"),
        "state leaked: {recorded}"
    );
    assert_eq!(machine.read_bytes(), local_before);
    assert_eq!(dav.put_count(REMOTE_PATH), 1);
}

#[test]
fn plaintext_conflict_and_remote_backup_are_migrated_to_ciphertext() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let sync = sync_config(&dav, REMOTE_PATH);
    sync_once(machine.path(), &sync, None, TS).expect("legacy plaintext sync");

    let mut local = machine.load();
    local.sync.encrypt = true;
    local.snippet[0].text = "local encrypted winner".to_string();
    machine.save(&local);
    let mut remote = Config::example();
    remote.snippet[0].text = "remote secret needing encrypted backup".to_string();
    dav.store(REMOTE_PATH, remote.to_toml().unwrap().into_bytes());

    let protected_sync = encrypted_sync(&dav);
    let (outcome, state) = sync_once_protected(
        machine.path(),
        &protected_sync,
        None,
        encryption(SYNC_PASSWORD),
        TS,
    )
    .expect("encrypted conflict resolution");
    let backup_url = match outcome {
        SyncOutcome::ConflictLocalWins { remote_backup } => remote_backup.unwrap(),
        other => panic!("expected conflict, got {other:?}"),
    };
    assert_eq!(state.remote_encrypted, Some(true));
    let main_body = dav.body(REMOTE_PATH).unwrap();
    assert!(is_encrypted_config(&main_body));
    assert_eq!(
        decrypt_config(&main_body, SYNC_PASSWORD).unwrap(),
        machine.read_bytes()
    );

    let backup_path = backup_url
        .strip_prefix(&format!("http://{}", dav.addr))
        .unwrap();
    let backup_body = dav.body(backup_path).unwrap();
    assert!(is_encrypted_config(&backup_body));
    assert!(!String::from_utf8_lossy(&backup_body).contains("remote secret"));
    let backup_plain = decrypt_config(&backup_body, SYNC_PASSWORD).unwrap();
    let backup_config = Config::parse(std::str::from_utf8(&backup_plain).unwrap()).unwrap();
    assert_eq!(
        backup_config.snippet[0].text,
        "remote secret needing encrypted backup"
    );
    assert!(backup_config.sync.encrypt);
}

#[test]
fn password_rotation_reencrypts_without_conflict_then_uses_only_new_password() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let mut config = machine.load();
    config.sync.encrypt = true;
    machine.save(&config);
    let sync = encrypted_sync(&dav);
    sync_once_protected(
        machine.path(),
        &sync,
        None,
        encryption("old encryption password"),
        TS,
    )
    .expect("old password upload");
    let old_remote = dav.body(REMOTE_PATH).unwrap();

    let rotated = SyncProtection::new(
        true,
        Some("new encryption password".to_string()),
        Some("old encryption password".to_string()),
    )
    .unwrap();
    let (outcome, state) = sync_once_protected(machine.path(), &sync, None, rotated, TS)
        .expect("rotation must re-encrypt, not conflict");
    assert_eq!(
        outcome,
        SyncOutcome::Pushed {
            first_upload: false
        }
    );
    assert_eq!(state.remote_encrypted, Some(true));
    let new_remote = dav.body(REMOTE_PATH).unwrap();
    assert_ne!(new_remote, old_remote);
    assert_eq!(
        decrypt_config(&new_remote, "new encryption password").unwrap(),
        machine.read_bytes()
    );
    assert_eq!(
        decrypt_config(&new_remote, "old encryption password").unwrap_err(),
        CryptoError::Decrypt
    );

    let (outcome, _) = sync_once_protected(
        machine.path(),
        &sync,
        None,
        encryption("new encryption password"),
        TS,
    )
    .expect("new password no-op");
    assert_eq!(outcome, SyncOutcome::UpToDate);
    assert_eq!(dav.put_count(REMOTE_PATH), 2);
}

#[test]
fn legacy_plaintext_is_migrated_even_when_snippets_are_identical() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let legacy = machine.read_bytes();
    dav.store(REMOTE_PATH, legacy);

    let mut local = machine.load();
    local.sync.encrypt = true;
    machine.save(&local);
    let sync = encrypted_sync(&dav);
    let (outcome, _) =
        sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
            .expect("legacy migration");
    assert_eq!(outcome, SyncOutcome::Pulled);
    assert!(machine.load().sync.encrypt);
    let remote = dav.body(REMOTE_PATH).unwrap();
    assert!(is_encrypted_config(&remote));
    assert_eq!(
        decrypt_config(&remote, SYNC_PASSWORD).unwrap(),
        machine.read_bytes()
    );

    let (outcome, _) =
        sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
            .expect("post-migration no-op");
    assert_eq!(outcome, SyncOutcome::UpToDate);
    assert_eq!(dav.put_count(REMOTE_PATH), 1);
}

#[test]
fn disabling_encryption_requires_password_and_uploads_plaintext_deliberately() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let mut config = machine.load();
    config.sync.encrypt = true;
    machine.save(&config);
    let encrypted = encrypted_sync(&dav);
    sync_once_protected(
        machine.path(),
        &encrypted,
        None,
        encryption(SYNC_PASSWORD),
        TS,
    )
    .expect("encrypted upload");

    let plain_sync = sync_config(&dav, REMOTE_PATH);
    let error = sync_once_protected(
        machine.path(),
        &plain_sync,
        None,
        SyncProtection::disabled(),
        TS,
    )
    .expect_err("ordinary disabled mode must not strip encryption");
    assert!(matches!(
        error,
        SyncError::Crypto(CryptoError::PasswordRequired)
    ));
    assert!(is_encrypted_config(&dav.body(REMOTE_PATH).unwrap()));

    config.sync.encrypt = false;
    machine.save(&config);
    let disabling = SyncProtection::new(false, Some(SYNC_PASSWORD.to_string()), None).unwrap();
    let (outcome, state) = sync_once_protected(machine.path(), &plain_sync, None, disabling, TS)
        .expect("explicit disable with password");
    assert_eq!(
        outcome,
        SyncOutcome::Pushed {
            first_upload: false
        }
    );
    assert_eq!(state.remote_encrypted, Some(false));
    assert_eq!(dav.body(REMOTE_PATH).unwrap(), machine.read_bytes());
}

#[test]
fn encrypted_remote_requires_a_local_encrypted_baseline_before_disabling() {
    let dav = MockDav::start(None);
    let legacy = Machine::new();
    let plain_sync = sync_config(&dav, REMOTE_PATH);
    sync_once(legacy.path(), &plain_sync, None, TS).expect("legacy plaintext baseline");
    let legacy_before = legacy.read_bytes();

    let source = Machine::new();
    let mut source_config = source.load();
    source_config.sync.encrypt = true;
    source.save(&source_config);
    sync_once_protected(
        source.path(),
        &encrypted_sync(&dav),
        None,
        encryption(SYNC_PASSWORD),
        TS,
    )
    .expect("encrypt the legacy remote");
    let encrypted_remote = dav.body(REMOTE_PATH).unwrap();
    assert!(is_encrypted_config(&encrypted_remote));
    // One legacy plaintext PUT plus one encrypted migration PUT.
    assert_eq!(dav.put_count(REMOTE_PATH), 2);
    let puts_after_encryption = dav.put_count(REMOTE_PATH);

    // A brand-new default-off machine has not deliberately disabled anything.
    let new_machine = Machine::new();
    let new_before = new_machine.read_bytes();
    let with_password = SyncProtection::new(false, Some(SYNC_PASSWORD.to_string()), None).unwrap();
    let error = sync_once_protected(new_machine.path(), &plain_sync, None, with_password, TS)
        .expect_err("default-off must not downgrade a new encrypted remote");
    assert!(matches!(error, SyncError::EncryptionRequired));
    assert_eq!(new_machine.read_bytes(), new_before);
    assert_eq!(dav.body(REMOTE_PATH).unwrap(), encrypted_remote);

    // A machine whose baseline was plaintext must not silently undo another
    // client's encryption during an ordinary periodic sync either.
    let with_password = SyncProtection::new(false, Some(SYNC_PASSWORD.to_string()), None).unwrap();
    let error = sync_once_protected(legacy.path(), &plain_sync, None, with_password, TS)
        .expect_err("plaintext baseline must not authorise downgrade");
    assert!(matches!(error, SyncError::EncryptionRequired));
    assert_eq!(legacy.read_bytes(), legacy_before);
    assert_eq!(dav.body(REMOTE_PATH).unwrap(), encrypted_remote);
    assert_eq!(dav.put_count(REMOTE_PATH), puts_after_encryption);
    assert!(dav.keys().iter().all(|key| !key.contains("remote-backup")));
}

#[test]
fn encrypted_etag_retry_decodes_fresh_remote_and_random_reencryption_is_a_noop() {
    let dav = MockDav::start(None);
    let machine = Machine::new();
    let mut config = machine.load();
    config.sync.encrypt = true;
    machine.save(&config);
    let sync = encrypted_sync(&dav);
    sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
        .expect("initial encrypted upload");

    config.snippet[0].text = "encrypted local edit after stale ETag".to_string();
    machine.save(&config);
    dav.reject_next_put();
    let (outcome, _) =
        sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
            .expect("412 fresh encrypted GET then retry");
    assert_eq!(
        outcome,
        SyncOutcome::Pushed {
            first_upload: false
        }
    );
    // Initial PUT + failed conditional PUT + successful retry.
    assert_eq!(dav.put_count(REMOTE_PATH), 3);
    assert_eq!(
        decrypt_config(&dav.body(REMOTE_PATH).unwrap(), SYNC_PASSWORD).unwrap(),
        machine.read_bytes()
    );
    assert!(dav.keys().iter().all(|key| !key.contains("remote-backup")));

    // Another client may re-encrypt identical plaintext with a fresh salt and
    // nonce. Ciphertext bytes and ETag change, but logical content does not.
    let reencrypted = encrypt_config(&machine.read_bytes(), SYNC_PASSWORD).unwrap();
    dav.store(REMOTE_PATH, reencrypted);
    let (outcome, state) =
        sync_once_protected(machine.path(), &sync, None, encryption(SYNC_PASSWORD), TS)
            .expect("same plaintext under a new envelope is up to date");
    assert_eq!(outcome, SyncOutcome::UpToDate);
    assert_eq!(state.remote_encrypted, Some(true));
    assert_eq!(dav.put_count(REMOTE_PATH), 3);
}
