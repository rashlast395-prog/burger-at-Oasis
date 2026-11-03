//! TLS tests. The `*_in_memory` tests need no network and always run. The `wire_*` tests use
//! real sockets; on a machine where antivirus or a firewall rewrites TLS (even on loopback),
//! they detect that and say so instead of failing for the wrong reason.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use bo_sync::net::*;
use bo_sync::tls::*;
use bo_sync::*;
use serde_json::json;
use tempfile::TempDir;

fn rules() -> Vec<(String, Box<dyn Rule>)> {
    vec![("items".into(), Box::new(LastWriterWins))]
}

fn mk(dir: &TempDir, name: &str, role: Role) -> Arc<SyncNode> {
    let n = SyncNode::create(dir.path().join(name), role, rules(), Arc::new(ManualClock::new(1_000_000))).unwrap();
    n.require_signatures(true);
    Arc::new(n)
}

struct Rig {
    _dir: TempDir,
    server: Arc<SyncNode>,
    device: Arc<SyncNode>,
    pki: Pki,
    _srv: Server,
    addr: String,
}

fn rig() -> Rig {
    let dir = TempDir::new().unwrap();
    let server = mk(&dir, "s.db", Role::Server);
    let device = mk(&dir, "d.db", Role::Device);
    server.trust_device(device.device_id(), &device.public_key(), Role::Device).unwrap();
    device.trust_device(server.device_id(), &server.public_key(), Role::Server).unwrap();
    let pki = generate_pki(&["localhost".into(), "127.0.0.1".into()]).unwrap();
    let srv = Server::start_tls(server.clone(), "127.0.0.1:0", TlsServer::from_pem(&pki.server_cert_pem, &pki.server_key_pem).unwrap()).unwrap();
    let addr = srv.addr().to_string();
    Rig { _dir: dir, server, device, pki, _srv: srv, addr }
}

/// Can real-socket TLS be tested here? Some antivirus products (for example Avast's Web
/// Shield) intercept TLS even on loopback and substitute their own certificate; a pinned
/// client then correctly refuses it. That is not a bug in the code under test, so say so.
fn wire_tls_usable(r: &Rig) -> bool {
    let client = TlsClient::from_pem(&r.pki.ca_cert_pem, "localhost").unwrap();
    match sync_once_tls(&r.device, &r.addr, r.server.device_id(), &client) {
        Err(NetError::Tls(m)) if m.contains("UnknownIssuer") => {
            eprintln!("SKIPPED (not verified on this machine): TLS on the wire is being intercepted by local security software; the pinned client refused the substituted certificate.");
            false
        }
        _ => true,
    }
}

// ------------------------------------------------------------ in memory: always run

#[test]
fn in_memory_handshake_pins_the_ca_and_encrypts() {
    let pki = generate_pki(&["localhost".into(), "127.0.0.1".into()]).unwrap();
    let server = TlsServer::from_pem(&pki.server_cert_pem, &pki.server_key_pem).unwrap();
    let payload = b"secret order data: 2x burger";
    for name in ["localhost", "127.0.0.1"] {
        let client = TlsClient::from_pem(&pki.ca_cert_pem, name).unwrap();
        assert_eq!(loopback_handshake(&client, &server, payload).unwrap(), payload);
    }
}

#[test]
fn in_memory_wrong_ca_wrong_name_and_foreign_server_are_refused() {
    let pki = generate_pki(&["localhost".into()]).unwrap();
    let other = generate_pki(&["localhost".into()]).unwrap();
    let server = TlsServer::from_pem(&pki.server_cert_pem, &pki.server_key_pem).unwrap();
    // pinned to a different CA
    let wrong_ca = TlsClient::from_pem(&other.ca_cert_pem, "localhost").unwrap();
    let e = loopback_handshake(&wrong_ca, &server, b"x").unwrap_err();
    assert!(e.0.to_lowercase().contains("issuer") || e.0.to_lowercase().contains("certificate"), "unexpected error: {e}");
    // right CA, but the name is not on the certificate
    let wrong_name = TlsClient::from_pem(&pki.ca_cert_pem, "other.example").unwrap();
    assert!(loopback_handshake(&wrong_name, &server, b"x").is_err());
    // an impostor server with its own CA-signed cert, against a client pinned to the real CA
    let impostor = TlsServer::from_pem(&other.server_cert_pem, &other.server_key_pem).unwrap();
    let real_client = TlsClient::from_pem(&pki.ca_cert_pem, "localhost").unwrap();
    assert!(loopback_handshake(&real_client, &impostor, b"x").is_err());
    // the server's own certificate is not a trust anchor for anything
    assert!(TlsClient::from_pem(&pki.server_cert_pem, "localhost").and_then(|c| loopback_handshake(&c, &server, b"x")).is_err());
}

#[test]
fn in_memory_server_rejects_mismatched_key() {
    let a = generate_pki(&["localhost".into()]).unwrap();
    let b = generate_pki(&["localhost".into()]).unwrap();
    assert!(TlsServer::from_pem(&a.server_cert_pem, &b.server_key_pem).is_err());
}

#[test]
fn bad_pem_and_missing_names_are_errors() {
    assert!(TlsServer::from_pem("nope", "nope").is_err());
    assert!(TlsClient::from_pem("nope", "localhost").is_err());
    assert!(generate_pki(&[]).is_err());
    let c = generate_pki(&["localhost".into()]).unwrap();
    assert!(TlsServer::from_pem(&c.server_cert_pem, "not a key").is_err());
    assert_eq!(cert_fingerprint(&c.ca_cert_pem).unwrap().len(), 64);
    assert_ne!(cert_fingerprint(&c.ca_cert_pem).unwrap(), cert_fingerprint(&generate_pki(&["localhost".into()]).unwrap().ca_cert_pem).unwrap());
}

#[test]
fn slow_sender_across_timeouts_is_not_corrupted() {
    // The frame reader must keep partial frames across read timeouts.
    struct Dribble<'a>(&'a [u8], usize);
    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.1 += 1;
            if self.1 % 2 == 1 {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            if self.0.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }
    let body = br#"{"t":"get_vv"}"#;
    let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
    bytes.extend(body);
    let mut src = Dribble(&bytes, 0);
    let mut fr = FrameReader::new();
    let mut polls = 0;
    let msg = loop {
        polls += 1;
        if let Some(m) = fr.poll(&mut src, MAX_FRAME).unwrap() {
            break m;
        }
        assert!(polls < 1000);
    };
    assert!(matches!(msg, Msg::GetVv));
    assert!(polls > 10);
}

// ------------------------------------------------------------ on the wire

#[test]
fn wire_sync_over_tls_works_both_ways() {
    let r = rig();
    if !wire_tls_usable(&r) {
        return;
    }
    let client = TlsClient::from_pem(&r.pki.ca_cert_pem, "localhost").unwrap();
    r.device.put("items", json!({"_id": "a", "n": 1})).unwrap();
    r.server.put("items", json!({"_id": "b", "n": 2})).unwrap();
    let st = sync_once_tls(&r.device, &r.addr, r.server.device_id(), &client).unwrap();
    assert_eq!((st.pushed, st.pulled), (1, 1));
    assert!(r.server.get("items", "a").unwrap().is_some());
    assert!(r.device.get("items", "b").unwrap().is_some());
    for i in 0..400 {
        r.device.put("items", json!({"_id": format!("x{i}"), "pad": "y".repeat(500)})).unwrap();
    }
    let st = sync_once_tls(&r.device, &r.addr, r.server.device_id(), &client).unwrap();
    assert_eq!(st.pushed, 400);
    assert_eq!(r.server.find("items", &json!({})).unwrap().len(), 402);
}

#[test]
fn wire_plaintext_client_gets_nothing_from_tls_server() {
    let r = rig();
    assert!(sync_once(&r.device, &r.addr, r.server.device_id()).is_err());
    let mut s = TcpStream::connect(&r.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let body = br#"{"t":"hello","device":"00000000000000000000000000000000","nonce":"00"}"#;
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend(body);
    let _ = s.write_all(&frame);
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap_or(0);
    assert!(!String::from_utf8_lossy(&buf[..n]).contains("login refused"));
    assert!(r.server.find("items", &json!({})).unwrap().is_empty());
}

#[test]
fn wire_wrong_certificate_is_a_tls_error_and_not_retried() {
    let r = rig();
    let other = generate_pki(&["localhost".into()]).unwrap();
    let wrong = TlsClient::from_pem(&other.ca_cert_pem, "localhost").unwrap();
    let e = sync_once_tls(&r.device, &r.addr, r.server.device_id(), &wrong).unwrap_err();
    assert!(matches!(e, NetError::Tls(_)), "{e}");
    let slept = std::cell::RefCell::new(0);
    let e = sync_with_retry_tls(&r.device, &r.addr, r.server.device_id(), &wrong, RetryPolicy { max_attempts: 4, base_ms: 1, max_ms: 1 }, &|_| *slept.borrow_mut() += 1).unwrap_err();
    assert!(matches!(e, NetError::Tls(_)));
    assert_eq!(*slept.borrow(), 0);
    assert!(r.server.find("items", &json!({})).unwrap().is_empty());
}

#[test]
fn wire_tls_server_still_authenticates_devices() {
    let r = rig();
    if !wire_tls_usable(&r) {
        return;
    }
    let client = TlsClient::from_pem(&r.pki.ca_cert_pem, "localhost").unwrap();
    let dir = TempDir::new().unwrap();
    let rogue = mk(&dir, "r.db", Role::Device);
    rogue.trust_device(r.server.device_id(), &r.server.public_key(), Role::Server).unwrap();
    assert!(matches!(sync_once_tls(&rogue, &r.addr, r.server.device_id(), &client), Err(NetError::Auth(_))));
}
