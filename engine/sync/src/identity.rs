//! Ed25519 device identities and signatures.
//!
//! Each node owns a keypair. Every event it creates is signed, and the signature covers all
//! of the event's fields (including its claimed role), so a relay cannot alter an event and
//! a device cannot pretend to be another device or the server. Receivers check signatures
//! against a list of **enrolled** devices (device id -> public key + role).

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

pub struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn generate() -> Identity {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("the operating system's random source is unavailable");
        Identity { key: SigningKey::from_bytes(&seed) }
    }

    pub fn from_secret_hex(s: &str) -> Option<Identity> {
        let b: [u8; 32] = unhex(s)?.try_into().ok()?;
        Some(Identity { key: SigningKey::from_bytes(&b) })
    }

    /// The private key. It is stored inside the node's database file, so database files and
    /// backups must be protected like any other credential.
    pub fn secret_hex(&self) -> String {
        hex(&self.key.to_bytes())
    }

    pub fn public_hex(&self) -> String {
        hex(self.key.verifying_key().as_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> String {
        hex(&self.key.sign(msg).to_bytes())
    }
}

/// Is `public` a well-formed public key?
pub fn valid_public(public: &str) -> bool {
    unhex(public).and_then(|b| <[u8; 32]>::try_from(b).ok()).map_or(false, |b| VerifyingKey::from_bytes(&b).is_ok())
}

/// Verify `sig` over `msg` with the hex public key. Any malformed input is just `false`.
pub fn verify(public: &str, msg: &[u8], sig: &str) -> bool {
    let Some(pk) = unhex(public).and_then(|b| <[u8; 32]>::try_from(b).ok()).and_then(|b| VerifyingKey::from_bytes(&b).ok()) else {
        return false;
    };
    let Some(sg) = unhex(sig).and_then(|b| <[u8; 64]>::try_from(b).ok()) else {
        return false;
    };
    pk.verify_strict(msg, &Signature::from_bytes(&sg)).is_ok()
}

/// `n` random bytes as hex.
pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("the operating system's random source is unavailable");
    hex(&b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}
