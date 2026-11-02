//! `bo-server`: the remote sync node.
//!
//!   bo-server init   <dir> --host <name-or-ip> [--host ...] [--listen 0.0.0.0:7443]
//!   bo-server show   <dir>
//!   bo-server enroll <dir> --device <id> --public <hex>
//!   bo-server run    <dir>
//!
//! `enroll` and `show` open the database, so run them while the server is stopped (a second
//! open is refused by the database lock rather than risking corruption).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bo_server::*;
use bo_sync::net::Server;
use bo_sync::tls::{cert_fingerprint, generate_pki, TlsServer};
use bo_sync::{Role, SyncNode, SystemClock};

const USAGE: &str = "usage:
  bo-server init   <dir> --host <name-or-ip> [--host ...] [--listen 0.0.0.0:7443]
  bo-server show   <dir>
  bo-server enroll <dir> --device <id> --public <hex>
  bo-server run    <dir>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("init") => init(&args[1..]),
        Some("show") => show(&args[1..]),
        Some("enroll") => enroll(&args[1..]),
        Some("run") => run(&args[1..]),
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

fn dir_arg(args: &[String]) -> Result<PathBuf, String> {
    positionals(args).into_iter().next().map(PathBuf::from).ok_or_else(|| USAGE.to_string())
}

fn open_node(dir: &Path) -> Result<(SyncNode, ServerConfig), String> {
    let cfg: ServerConfig = read_json(&dir.join("config.json"))?;
    let rules = rules_from(&cfg.collections)?;
    let node = SyncNode::open(dir.join("node.db"), rules, Arc::new(SystemClock)).map_err(|e| e.to_string())?;
    Ok((node, cfg))
}

fn init(args: &[String]) -> Result<(), String> {
    let dir = dir_arg(args)?;
    let hosts = flag_values(args, "--host");
    if hosts.is_empty() {
        return Err("give at least one --host: the DNS name or IP address devices will use to reach this server".into());
    }
    let listen = flag(args, "--listen").unwrap_or_else(|| "0.0.0.0:7443".into());
    let port = port_of(&listen).ok_or("--listen must look like 0.0.0.0:7443")?;
    if dir.join("config.json").exists() {
        return Err(format!("{} is already initialised", dir.display()));
    }
    fs::create_dir_all(dir.join("tls")).map_err(|e| e.to_string())?;

    let collections: std::collections::BTreeMap<String, String> = DEFAULT_COLLECTIONS.iter().map(|(c, r)| (c.to_string(), r.to_string())).collect();
    let rules = rules_from(&collections)?;
    let node = SyncNode::create(dir.join("node.db"), Role::Server, rules, Arc::new(SystemClock)).map_err(|e| e.to_string())?;

    let pki = generate_pki(&hosts).map_err(|e| e.to_string())?;
    write_private(&dir.join("tls").join("server.key"), &pki.server_key_pem)?;
    write_private(&dir.join("tls").join("ca.key"), &pki.ca_key_pem)?;
    fs::write(dir.join("tls").join("server.crt"), &pki.server_cert_pem).map_err(|e| e.to_string())?;
    fs::write(dir.join("tls").join("ca.crt"), &pki.ca_cert_pem).map_err(|e| e.to_string())?;

    write_json(&dir.join("config.json"), &ServerConfig { listen: listen.clone(), collections: collections.clone() })?;
    let identity = ServerIdentity {
        server_device: node.device_id().to_string(),
        server_public: node.public_key(),
        address: format!("{}:{port}", hosts[0]),
        tls_name: hosts[0].clone(),
        ca_cert_pem: pki.ca_cert_pem.clone(),
        collections,
    };
    write_json(&dir.join("server-identity.json"), &identity)?;

    println!("Initialised {}", dir.display());
    println!("  server device id : {}", identity.server_device);
    println!("  CA fingerprint   : {}", cert_fingerprint(&pki.ca_cert_pem).map_err(|e| e.to_string())?);
    println!("  devices connect to {}", identity.address);
    println!();
    println!("Next:");
    println!("  1. Copy {} to every device (it is public, but verify the CA fingerprint above", dir.join("server-identity.json").display());
    println!("     on the device with `bo-device show`).");
    println!("  2. On each device: bo-device init <dir> --server-identity server-identity.json");
    println!("  3. Enrol each device here: bo-server enroll {} --device <id> --public <key>", dir.display());
    println!("  4. Start: bo-server run {}", dir.display());
    println!();
    println!("SECURITY: {} lets anyone issue certificates for this deployment.", dir.join("tls").join("ca.key").display());
    println!("          Move it somewhere safe and offline now; the server does not need it.");
    println!("          {} holds the server's private keys: keep the whole directory private,", dir.display());
    println!("          and treat node.db and its backups as credentials (they contain the signing key).");
    Ok(())
}

fn show(args: &[String]) -> Result<(), String> {
    let dir = dir_arg(args)?;
    let (node, cfg) = open_node(&dir)?;
    let ca = fs::read_to_string(dir.join("tls").join("ca.crt")).map_err(|e| e.to_string())?;
    println!("listen          : {}", cfg.listen);
    println!("server device id: {}", node.device_id());
    println!("server public   : {}", node.public_key());
    println!("CA fingerprint  : {}", cert_fingerprint(&ca).map_err(|e| e.to_string())?);
    println!("collections     :");
    for (c, r) in &cfg.collections {
        println!("  {c:<20} {r}");
    }
    println!("enrolled devices:");
    for (id, t) in node.trusted_devices() {
        if id != node.device_id() {
            println!("  {id}  {:?}", t.role);
        }
    }
    Ok(())
}

fn enroll(args: &[String]) -> Result<(), String> {
    let dir = dir_arg(args)?;
    let device = flag(args, "--device").ok_or("--device <id> is required")?;
    let public = flag(args, "--public").ok_or("--public <hex> is required")?;
    let (node, _) = open_node(&dir).map_err(|e| format!("{e} (is the server still running? stop it first)"))?;
    node.trust_device(&device, &public, Role::Device).map_err(|e| e.to_string())?;
    println!("enrolled device {device}");
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let dir = dir_arg(args)?;
    let (node, cfg) = open_node(&dir)?;
    node.require_signatures(true);
    let cert = fs::read_to_string(dir.join("tls").join("server.crt")).map_err(|e| format!("tls/server.crt: {e}"))?;
    let key = fs::read_to_string(dir.join("tls").join("server.key")).map_err(|e| format!("tls/server.key: {e}"))?;
    let tls = TlsServer::from_pem(&cert, &key).map_err(|e| e.to_string())?;
    let node = Arc::new(node);
    let server = Server::start_tls(node.clone(), &cfg.listen, tls).map_err(|e| e.to_string())?;
    println!("bo-server {} listening on {} (TLS 1.3, {} devices enrolled)", node.device_id(), server.addr(), node.trusted_devices().len() - 1);

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)).map_err(|e| e.to_string())?;
    // Repair stock effects of offline races (for example, an order accepted on one till and
    // cancelled on another) every 30 seconds. Only meaningful with the standard collections.
    let reconciles = cfg.collections.get(bo_domain::ORDERS).map(String::as_str) == Some("order")
        && cfg.collections.get(bo_domain::LEDGER).map(String::as_str) == Some("inventory-ledger");
    let audits_money = cfg.collections.get(bo_domain::PAYMENTS).map(String::as_str) == Some("payment-ledger");
    let mut ticks = 0u32;
    let mut last_anomalies = 0usize;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
        ticks += 1;
        if ticks % 150 != 0 {
            continue;
        }
        if reconciles {
            match bo_domain::reconcile(&node) {
                Ok(0) => {}
                Ok(n) => println!("reconcile: wrote {n} stock entries"),
                Err(e) => eprintln!("reconcile failed: {e}"),
            }
        }
        // Money is never corrected automatically; problems are reported for a person.
        if reconciles && audits_money {
            match bo_domain::payment_anomalies(&node) {
                Ok(list) if list.len() != last_anomalies => {
                    last_anomalies = list.len();
                    println!("payments: {} order(s) need attention", list.len());
                    for a in &list {
                        println!("  {}: {}", a.order_id, a.problem);
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("payment audit failed: {e}"),
            }
        }
    }
    println!("shutting down");
    server.stop();
    Ok(())
}
