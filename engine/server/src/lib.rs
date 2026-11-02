//! Shared pieces of `bo-server` and `bo-device`: the deployment definition (which
//! collections exist and how conflicts are resolved), the file a device is provisioned with,
//! and small helpers for key files and command-line flags.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use bo_sync::{AppendOnly, LastWriterWins, Rule, ServerAuthoritative, Versioned};
use serde::{Deserialize, Serialize};

/// Collections created by `bo-server init` unless the config is edited. `orders` is plain
/// last-writer-wins for now: the order state machine is a later phase, and until then two
/// devices editing one order at once will lose one edit.
pub const DEFAULT_COLLECTIONS: [(&str, &str); 5] = [
    ("menu", "versioned"),
    ("orders", "last-writer-wins"),
    ("customers", "last-writer-wins"),
    ("inventory_ledger", "append-only"),
    ("payments", "server-authoritative"),
];

pub fn rule_by_name(name: &str) -> Option<Box<dyn Rule>> {
    match name {
        "last-writer-wins" => Some(Box::new(LastWriterWins)),
        "append-only" => Some(Box::new(AppendOnly)),
        "server-authoritative" => Some(Box::new(ServerAuthoritative)),
        "versioned" => Some(Box::new(Versioned)),
        _ => None,
    }
}

pub fn rules_from(collections: &BTreeMap<String, String>) -> Result<Vec<(String, Box<dyn Rule>)>, String> {
    collections
        .iter()
        .map(|(c, r)| rule_by_name(r).map(|rule| (c.clone(), rule)).ok_or_else(|| format!("collection '{c}': unknown rule '{r}'")))
        .collect()
}

/// Server settings, `<dir>/config.json`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServerConfig {
    /// Address to listen on, e.g. `0.0.0.0:7443`.
    pub listen: String,
    pub collections: BTreeMap<String, String>,
}

/// Public facts a device needs to talk to the server. Not secret, but it must reach the
/// device through a channel you trust (it contains the CA certificate the device will pin).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServerIdentity {
    pub server_device: String,
    pub server_public: String,
    /// `host:port` the device connects to.
    pub address: String,
    /// The name on the server certificate; usually the host part of `address`.
    pub tls_name: String,
    pub ca_cert_pem: String,
    pub collections: BTreeMap<String, String>,
}

pub fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not valid: {e}", path.display()))
}

pub fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Write a secret (private key) readable only by the owner where the platform supports it.
/// On Windows the file inherits the folder's permissions: keep the data directory private.
pub fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    f.write_all(contents.as_bytes()).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// All values given for a repeatable `--name value` flag.
pub fn flag_values(args: &[String], name: &str) -> Vec<String> {
    args.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
}

pub fn flag(args: &[String], name: &str) -> Option<String> {
    flag_values(args, name).into_iter().next()
}

/// Positional arguments: everything that is not a `--flag value` pair.
pub fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += 2;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

/// The port of a `host:port` string, if it has a valid one.
pub fn port_of(addr: &str) -> Option<u16> {
    addr.rsplit_once(':')?.1.parse().ok()
}
