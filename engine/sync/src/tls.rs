//! TLS 1.3 for the sync transport (rustls with the `ring` crypto backend).
//!
//! Trust is **pinned, not delegated to public certificate authorities**: a device is
//! provisioned with the deployment's own private CA certificate and trusts nothing else.
//! That suits a closed system of one server and a known set of devices, and it needs no
//! public CA or domain validation.
//! The Ed25519 login in [`crate::net`] still runs inside the tunnel, so TLS protects
//! confidentiality and integrity of the channel while the login decides *who* may sync.
//!
//! Replacing the server certificate (same CA) needs no change on devices. Replacing the CA
//! means re-provisioning every device; there is no automatic renewal.

use std::fmt;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct TlsError(pub String);

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tls: {}", self.0)
    }
}

impl std::error::Error for TlsError {}

fn err(e: impl fmt::Display) -> TlsError {
    TlsError(e.to_string())
}

/// A private certificate authority and a server certificate it issued, all PEM.
///
/// Devices pin `ca_cert_pem` (public). The server presents `server_cert_pem` with
/// `server_key_pem`. `ca_key_pem` can issue more server certificates, so after `init`
/// move it off the server (or at least keep it out of backups sent anywhere).
///
/// (A self-signed server certificate cannot simply be trusted directly: the verifier only
/// accepts a certificate as a trust anchor for *other* certificates, hence the small CA.)
pub struct Pki {
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
}

/// Create a private CA and a server certificate valid for the given DNS names or IP
/// addresses. Certificates carry no expiry that matters in practice (rcgen's default), so
/// rotation is a deliberate act, not a surprise outage.
pub fn generate_pki(names: &[String]) -> Result<Pki, TlsError> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    if names.is_empty() {
        return Err(TlsError("at least one host name or IP address is required".into()));
    }
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.distinguished_name.push(DnType::CommonName, "Burger Oasis sync CA");
    let ca_key = KeyPair::generate().map_err(err)?;
    let ca = ca_params.self_signed(&ca_key).map_err(err)?;

    let mut leaf_params = CertificateParams::new(names.to_vec()).map_err(err)?;
    leaf_params.distinguished_name.push(DnType::CommonName, names[0].clone());
    let leaf_key = KeyPair::generate().map_err(err)?;
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).map_err(err)?;
    Ok(Pki { ca_cert_pem: ca.pem(), ca_key_pem: ca_key.serialize_pem(), server_cert_pem: leaf.pem(), server_key_pem: leaf_key.serialize_pem() })
}

fn certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let v: Vec<_> = CertificateDer::pem_slice_iter(pem.as_bytes()).collect::<Result<_, _>>().map_err(err)?;
    if v.is_empty() {
        return Err(TlsError("no certificate found in the PEM text".into()));
    }
    Ok(v)
}

/// SHA-256 of the (first) certificate, as hex, for out-of-band comparison.
pub fn cert_fingerprint(cert_pem: &str) -> Result<String, TlsError> {
    let c = certs(cert_pem)?;
    Ok(Sha256::digest(c[0].as_ref()).iter().map(|b| format!("{b:02x}")).collect())
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Run a full TLS handshake and send `payload` from client to server entirely in memory,
/// using exactly the configurations the network code uses. Returns what the server read.
/// For tests and diagnostics: it exercises pinning, name checking and encryption without
/// any socket (a local firewall or antivirus that rewrites TLS on the wire cannot interfere).
#[doc(hidden)]
pub fn loopback_handshake(client: &TlsClient, server: &TlsServer, payload: &[u8]) -> Result<Vec<u8>, TlsError> {
    use rustls::{ClientConnection, ServerConnection};
    use std::io::{Read, Write};
    let mut c = ClientConnection::new(client.config.clone(), client.name.clone()).map_err(err)?;
    let mut s = ServerConnection::new(server.config.clone()).map_err(err)?;
    let mut buf = Vec::new();
    for _ in 0..32 {
        while c.wants_write() {
            buf.clear();
            c.write_tls(&mut buf).map_err(err)?;
            s.read_tls(&mut &buf[..]).map_err(err)?;
            s.process_new_packets().map_err(err)?;
        }
        while s.wants_write() {
            buf.clear();
            s.write_tls(&mut buf).map_err(err)?;
            c.read_tls(&mut &buf[..]).map_err(err)?;
            c.process_new_packets().map_err(err)?;
        }
        if !c.is_handshaking() && !s.is_handshaking() {
            break;
        }
    }
    if c.is_handshaking() || s.is_handshaking() {
        return Err(TlsError("handshake did not complete".into()));
    }
    c.writer().write_all(payload).map_err(err)?;
    while c.wants_write() {
        buf.clear();
        c.write_tls(&mut buf).map_err(err)?;
        // The bytes on the wire must not contain the plaintext.
        if !payload.is_empty() && buf.windows(payload.len()).any(|w| w == payload) {
            return Err(TlsError("payload appeared unencrypted on the wire".into()));
        }
        s.read_tls(&mut &buf[..]).map_err(err)?;
        s.process_new_packets().map_err(err)?;
    }
    let mut out = vec![0u8; payload.len()];
    s.reader().read_exact(&mut out).map_err(err)?;
    Ok(out)
}

/// Server side: the certificate chain and key to present.
#[derive(Clone)]
pub struct TlsServer {
    pub(crate) config: Arc<ServerConfig>,
}

impl TlsServer {
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<TlsServer, TlsError> {
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(err)?;
        let config = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(err)?
            .with_no_client_auth()
            .with_single_cert(certs(cert_pem)?, key)
            .map_err(err)?;
        Ok(TlsServer { config: Arc::new(config) })
    }
}

/// Client side: trusts exactly the pinned server certificate.
#[derive(Clone)]
pub struct TlsClient {
    pub(crate) config: Arc<ClientConfig>,
    pub(crate) name: ServerName<'static>,
}

impl TlsClient {
    /// `ca_cert_pem` (from [`Pki::ca_cert_pem`]) is the only authority trusted;
    /// `server_name` must be one of the names or addresses the server certificate was
    /// issued for.
    pub fn from_pem(ca_cert_pem: &str, server_name: &str) -> Result<TlsClient, TlsError> {
        let mut roots = RootCertStore::empty();
        for c in certs(ca_cert_pem)? {
            roots.add(c).map_err(err)?;
        }
        let config = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(err)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from(server_name.to_string()).map_err(err)?;
        Ok(TlsClient { config: Arc::new(config), name })
    }
}
