//! WebDAV client built on top of ureq.
//!
//! Only the small subset needed to synchronise one configuration file is
//! implemented: GET, PUT with If-Match and MKCOL for the parent directories.
//! TLS is handled by rustls with bundled webpki roots, which works on Windows 7
//! regardless of the state of the system certificate store.

use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use ureq::{Agent, AgentBuilder};

use super::engine::{PutResult, RemoteConfig, SyncError, WebDav};

/// Parsed SHA-256 fingerprint of one explicitly trusted self-signed certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertificateFingerprint {
    bytes: [u8; 32],
}

impl CertificateFingerprint {
    pub fn parse(input: &str) -> Result<Self, SyncError> {
        let input = input.trim();
        let input = if input
            .as_bytes()
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"sha256:"))
        {
            &input[7..]
        } else {
            input
        };
        let hex = input
            .chars()
            .filter(|character| !matches!(character, ':' | '-' | ' '))
            .collect::<String>();
        if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SyncError::Invalid(
                "certificate_sha256 must contain exactly 64 hexadecimal characters".to_string(),
            ));
        }
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|_| SyncError::Invalid("invalid certificate_sha256".to_string()))?;
        }
        Ok(Self { bytes })
    }
}

struct PinnedCertificateVerifier {
    fingerprint: CertificateFingerprint,
    supported: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedCertificateVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedCertificateVerifier")
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if actual != self.fingerprint.bytes {
            return Err(rustls::Error::General(
                "server certificate SHA256 fingerprint mismatch".to_string(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // Pinning authenticates the certificate; these checks prove that the
        // peer also owns its private key. Never accept-all handshake signatures.
        verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

pub struct UreqWebDav {
    agent: Agent,
    auth: Option<String>,
}

impl UreqWebDav {
    pub fn new(username: &str, password: &str, timeout: Duration) -> Self {
        Self::build(username, password, timeout, None)
    }

    /// Build a client with normal WebPKI validation, or with a single pinned
    /// self-signed certificate when certificate_sha256 is non-empty.
    pub fn new_with_fingerprint(
        username: &str,
        password: &str,
        timeout: Duration,
        certificate_sha256: &str,
    ) -> Result<Self, SyncError> {
        let fingerprint = if certificate_sha256.trim().is_empty() {
            None
        } else {
            Some(CertificateFingerprint::parse(certificate_sha256)?)
        };
        Ok(Self::build(username, password, timeout, fingerprint))
    }

    fn build(
        username: &str,
        password: &str,
        timeout: Duration,
        fingerprint: Option<CertificateFingerprint>,
    ) -> Self {
        let mut builder = AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .redirects(5);
        if fingerprint.is_some() {
            // A pinned HTTPS endpoint must never silently downgrade its final
            // response to plaintext HTTP. Ordinary sync URLs keep old behavior.
            builder = builder.https_only(true);
        }
        if let Some(fingerprint) = fingerprint {
            let provider = rustls::crypto::ring::default_provider();
            let supported = provider.signature_verification_algorithms;
            let verifier = PinnedCertificateVerifier {
                fingerprint,
                supported,
            };
            let config = ClientConfig::builder_with_provider(provider.into())
                .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
                .expect("ring supports TLS 1.2 and 1.3")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth();
            builder = builder.tls_config(Arc::new(config));
        }
        let agent = builder.build();
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
                Ok(Some(RemoteConfig::new(body, etag)))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(err) => Err(map_ureq_error(err)),
        }
    }

    fn put(&self, url: &str, body: &[u8], if_match: Option<&str>) -> Result<PutResult, SyncError> {
        let content_type = if crate::crypto::has_config_envelope_marker(body) {
            "application/octet-stream"
        } else {
            "text/plain; charset=utf-8"
        };
        let mut request = self.request("PUT", url).set("Content-Type", content_type);
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

    #[test]
    fn fingerprint_parser_accepts_hex_colons_and_prefix() {
        let raw = "ab".repeat(32);
        let parsed = CertificateFingerprint::parse(&raw).unwrap();
        assert_eq!(parsed.bytes, [0xab; 32]);
        let colon = (0..32).map(|_| "AB").collect::<Vec<_>>().join(":");
        let parsed = CertificateFingerprint::parse(&format!("SHA256:{colon}")).unwrap();
        assert_eq!(parsed.bytes, [0xab; 32]);
        let spaced = format!(" {} ", raw);
        assert!(CertificateFingerprint::parse(&spaced).is_ok());
    }

    #[test]
    fn fingerprint_parser_rejects_bad_input_without_panicking() {
        for invalid in ["", "01", "zz", "中文指纹不是十六进制"] {
            assert!(
                CertificateFingerprint::parse(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn pinned_verifier_checks_leaf_hash_not_accept_all() {
        let provider = rustls::crypto::ring::default_provider();
        let certificate = CertificateDer::from_slice(b"self signed certificate bytes");
        let fingerprint = CertificateFingerprint {
            bytes: Sha256::digest(certificate.as_ref()).into(),
        };
        let verifier = PinnedCertificateVerifier {
            fingerprint,
            supported: provider.signature_verification_algorithms,
        };
        let server_name = ServerName::try_from("localhost").unwrap();
        let result =
            verifier.verify_server_cert(&certificate, &[], &server_name, &[], UnixTime::now());
        assert!(result.is_ok());

        let other = CertificateDer::from_slice(b"different certificate");
        let result = verifier.verify_server_cert(&other, &[], &server_name, &[], UnixTime::now());
        assert!(result.is_err());
    }
}
