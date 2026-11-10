//! Real rustls handshake tests for optional self-signed certificate pinning.
//! All connections stay on loopback; no external server or static private key is used.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use vibekey_core::sync::{content_hash, UreqWebDav, WebDav};

#[test]
fn self_signed_server_requires_matching_fingerprint() {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let certificate = certified.cert.der().clone();
    let private_key =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
    let provider = rustls::crypto::ring::default_provider();
    let server_config = ServerConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key)
        .unwrap();
    let server_config = Arc::new(server_config);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let plain = TcpListener::bind("127.0.0.1:0").unwrap();
    plain.set_nonblocking(true).unwrap();
    let plain_address = plain.local_addr().unwrap();
    let server = std::thread::spawn(move || -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        for attempt in 0..4 {
            let (stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "clients did not make three TLS attempts",
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => return Err(error),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let connection = ServerConnection::new(Arc::clone(&server_config)).unwrap();
            let mut tls = StreamOwned::new(connection, stream);
            let mut request = Vec::new();
            let mut chunk = [0u8; 2048];
            let readable = (|| -> std::io::Result<()> {
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = tls.read(&mut chunk)?;
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    if request.len() > 16 * 1024 {
                        break;
                    }
                }
                Ok(())
            })();
            if readable.is_ok() {
                if attempt == 3 {
                    let redirect = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{plain_address}/config.toml\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = tls.write_all(redirect.as_bytes());
                } else {
                    let _ = tls.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    );
                }
            }
        }
        Ok(())
    });

    let url = format!("https://{address}/config.toml");
    let timeout = Duration::from_secs(3);

    // Default WebPKI must reject this untrusted, IP-mismatched certificate.
    let default_client = UreqWebDav::new("", "", timeout);
    assert!(default_client.get(&url).is_err());

    let wrong = UreqWebDav::new_with_fingerprint("", "", timeout, &"00".repeat(32)).unwrap();
    assert!(wrong.get(&url).is_err());

    // The certificate was issued for localhost, while the client connects to
    // 127.0.0.1. Pinning deliberately replaces both CA and hostname checks,
    // but rustls still verifies that the peer owns the corresponding key.
    let fingerprint = content_hash(certificate.as_ref());
    let pinned = UreqWebDav::new_with_fingerprint("", "", timeout, &fingerprint).unwrap();
    let remote = pinned
        .get(&url)
        .expect("matching fingerprint must allow the TLS handshake")
        .expect("mock config exists");
    assert_eq!(remote.body, b"ok");

    // The fourth response redirects to plaintext HTTP. Pinned mode must reject
    // it before any connection reaches the plain listener.
    assert!(pinned.get(&url).is_err());
    match plain.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("pinned client followed the HTTP downgrade"),
        Err(error) => panic!("unexpected plain listener error: {error}"),
    }

    // Ordinary clients keep supporting direct HTTP URLs when no pin is set.
    plain.set_nonblocking(false).unwrap();
    let plain_server = std::thread::spawn(move || {
        let (mut stream, _) = plain.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });
    let plain_remote = default_client
        .get(&format!("http://{plain_address}/config.toml"))
        .expect("default HTTP remains supported")
        .expect("plain config exists");
    assert_eq!(plain_remote.body, b"ok");
    plain_server.join().unwrap();

    server
        .join()
        .expect("TLS test server thread panicked")
        .expect("TLS test server failed");
}
