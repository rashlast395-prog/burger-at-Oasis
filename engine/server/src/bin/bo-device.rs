//! `bo-device`: a device-side command line tool for provisioning, syncing and smoke tests.
//!
//!   bo-device init   <dir> --server-identity <file>
//!   bo-device show   <dir>
//!   bo-device sync   <dir>
//!   bo-device put    <dir> <collection> '<json document with an _id>'
//!   bo-device get    <dir> <collection> <id>
//!   bo-device list   <dir> <collection>
//!   bo-device delete <dir> <collection> <id>

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use bo_server::*;
use bo_sync::net::{sync_with_retry_tls, RetryPolicy};
use bo_sync::tls::{cert_fingerprint, TlsClient};
use bo_sync::{Role, SyncNode, SystemClock};

const USAGE: &str = "usage:
  bo-device init   <dir> --server-identity <file>
  bo-device show   <dir>
  bo-device sync   <dir>
  bo-device put    <dir> <collection> '<json document with an _id>'
  bo-device get    <dir> <collection> <id>
  bo-device list   <dir> <collection>
  bo-device delete <dir> <collection> <id>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pos = positionals(&args[args.len().min(1)..]);
    let result = match args.first().map(String::as_str) {
        Some("init") => init(&args[1..]),
        Some("show") => with_node(&pos, |n, id| show(n, id)),
        Some("sync") => with_node(&pos, |n, id| sync(n, id)),
        Some("put") => with_node(&pos, |n, _| {
            let doc: serde_json::Value = serde_json::from_str(pos.get(2).ok_or(USAGE)?).map_err(|e| format!("not valid JSON: {e}"))?;
            let ev = n.put(pos.get(1).ok_or(USAGE)?, doc).map_err(|e| e.to_string())?;
            println!("{}", if ev.is_some() { "saved (queued for sync)" } else { "no change" });
            Ok(())
        }),
        Some("get") => with_node(&pos, |n, _| {
            match n.get(pos.get(1).ok_or(USAGE)?, pos.get(2).ok_or(USAGE)?).map_err(|e| e.to_string())? {
                Some(d) => println!("{d}"),
                None => println!("not found"),
            }
            Ok(())
        }),
        Some("list") => with_node(&pos, |n, _| {
            for d in n.find(pos.get(1).ok_or(USAGE)?, &serde_json::json!({})).map_err(|e| e.to_string())? {
                println!("{d}");
            }
            Ok(())
        }),
        Some("delete") => with_node(&pos, |n, _| {
            let ev = n.delete(pos.get(1).ok_or(USAGE)?, pos.get(2).ok_or(USAGE)?).map_err(|e| e.to_string())?;
            println!("{}", if ev.is_some() { "deleted (queued for sync)" } else { "no change" });
            Ok(())
        }),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn with_node(pos: &[String], f: impl FnOnce(&SyncNode, &ServerIdentity) -> Result<(), String>) -> Result<(), String> {
    let dir = PathBuf::from(pos.first().ok_or(USAGE)?);
    let (node, id) = open_node(&dir)?;
    f(&node, &id)
}

fn open_node(dir: &Path) -> Result<(SyncNode, ServerIdentity), String> {
    let id: ServerIdentity = read_json(&dir.join("server-identity.json"))?;
    let rules = rules_from(&id.collections)?;
    let node = SyncNode::open(dir.join("node.db"), rules, Arc::new(SystemClock)).map_err(|e| e.to_string())?;
    node.require_signatures(true);
    Ok((node, id))
}

fn init(args: &[String]) -> Result<(), String> {
    let dir = PathBuf::from(positionals(args).first().ok_or(USAGE)?);
    let file = flag(args, "--server-identity").ok_or("--server-identity <file> is required")?;
    let id: ServerIdentity = read_json(Path::new(&file))?;
    if dir.join("node.db").exists() {
        return Err(format!("{} is already initialised", dir.display()));
    }
    // Validate everything before creating anything.
    TlsClient::from_pem(&id.ca_cert_pem, &id.tls_name).map_err(|e| e.to_string())?;
    let rules = rules_from(&id.collections)?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let node = SyncNode::create(dir.join("node.db"), Role::Device, rules, Arc::new(SystemClock)).map_err(|e| e.to_string())?;
    node.trust_device(&id.server_device, &id.server_public, Role::Server).map_err(|e| e.to_string())?;
    write_json(&dir.join("server-identity.json"), &id)?;
    println!("Device initialised in {}", dir.display());
    println!("  CA fingerprint: {}   <- must match the one printed by `bo-server init`", cert_fingerprint(&id.ca_cert_pem).map_err(|e| e.to_string())?);
    println!();
    println!("Ask the server administrator to run:");
    println!("  bo-server enroll <server-dir> --device {} --public {}", node.device_id(), node.public_key());
    Ok(())
}

fn show(node: &SyncNode, id: &ServerIdentity) -> Result<(), String> {
    println!("device id       : {}", node.device_id());
    println!("device public   : {}", node.public_key());
    println!("server          : {} ({})", id.address, id.server_device);
    println!("CA fingerprint  : {}", cert_fingerprint(&id.ca_cert_pem).map_err(|e| e.to_string())?);
    println!("pending vector  : {:?}", node.version_vector().map_err(|e| e.to_string())?);
    Ok(())
}

fn sync(node: &SyncNode, id: &ServerIdentity) -> Result<(), String> {
    let tls = TlsClient::from_pem(&id.ca_cert_pem, &id.tls_name).map_err(|e| e.to_string())?;
    let stats = sync_with_retry_tls(node, &id.address, &id.server_device, &tls, RetryPolicy::default(), &|d| {
        eprintln!("server unreachable, retrying in {:.1}s", d.as_secs_f32());
        std::thread::sleep(d);
    })
    .map_err(|e| e.to_string())?;
    println!("synced: sent {} events, received {} ({} rounds)", stats.pushed, stats.pulled, stats.rounds);
    Ok(())
}
