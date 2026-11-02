//! TCP transport for replication.
//!
//! * **Framing:** a 4-byte big-endian length, then a JSON message. Frames before login are
//!   capped at 4 KiB; after login at 16 MiB, and the buffer grows only as bytes arrive.
//! * **Login:** mutual challenge-response with the enrolled Ed25519 keys. Both sides
//!   contribute a random nonce and the signatures cover both, so a recorded login cannot
//!   be replayed. The client pins the server's device id; a device must be enrolled on the
//!   server or it is refused.
//! * **Sync:** the client drives. It asks for the server's version vector, pushes what the
//!   server lacks, then pulls what it lacks. Every event is individually signed, so the
//!   server can relay events between devices without being able to alter or invent them.
//! * **Retry:** [`sync_with_retry`] backs off exponentially on connection failures. Retrying
//!   is safe because applying events is idempotent.
//!
//! ## Not here
//! * **No encryption.** After login the traffic is plaintext. Event signatures stop
//!   tampering and forgery, but not eavesdropping, and nothing binds the login to the
//!   connection, so a man in the middle could drop or delay messages. Run this over a VPN
//!   (for example WireGuard) or a TLS tunnel until native TLS is added.
//! * No rate limiting beyond a connection cap, and no revocation of enrolled devices.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::identity::random_hex;
use crate::{valid_device_id, ApplyReport, Event, Role, SyncError, SyncNode};

pub const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_PRELOGIN_FRAME: usize = 4096;
/// Serialized events per message stay well below `MAX_FRAME`.
const BATCH_BYTES: usize = 8 * 1024 * 1024;
const BATCH_EVENTS: usize = 200;
const MAX_PUSH_EVENTS: usize = 1000;
const MAX_CONNECTIONS: usize = 64;
const IDLE_SECONDS: u32 = 120;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum NetError {
    Io(io::Error),
    Protocol(String),
    /// Login failed: unknown device, wrong key, or an impostor server.
    Auth(String),
    Sync(SyncError),
    /// The node must have `require_signatures(true)` before it may talk to the network.
    SignaturesNotRequired,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Io(e) => write!(f, "network i/o: {e}"),
            NetError::Protocol(m) => write!(f, "protocol error: {m}"),
            NetError::Auth(m) => write!(f, "authentication failed: {m}"),
            NetError::Sync(e) => write!(f, "{e}"),
            NetError::SignaturesNotRequired => write!(f, "call require_signatures(true) before using the network"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> Self {
        NetError::Io(e)
    }
}

impl From<SyncError> for NetError {
    fn from(e: SyncError) -> Self {
        NetError::Sync(e)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    Hello { device: String, nonce: String },
    Challenge { device: String, nonce: String, sig: String },
    Auth { sig: String },
    Ready,
    GetVv,
    Vv { vv: BTreeMap<String, u64> },
    Pull { vv: BTreeMap<String, u64>, limit: usize },
    Events { events: Vec<Event> },
    Push { events: Vec<Event> },
    Applied { report: ApplyReport },
    Error { message: String },
}

pub fn write_frame<W: Write>(w: &mut W, m: &Msg) -> io::Result<()> {
    let body = serde_json::to_vec(m).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_frame<R: Read>(r: &mut R, max: usize) -> Result<Msg, NetError> {
    let mut head = [0u8; 4];
    r.read_exact(&mut head)?;
    let n = u32::from_be_bytes(head) as usize;
    if n > max {
        return Err(NetError::Protocol(format!("frame of {n} bytes exceeds the limit of {max}")));
    }
    let mut body = Vec::new();
    r.take(n as u64).read_to_end(&mut body)?;
    if body.len() != n {
        return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    serde_json::from_slice(&body).map_err(|e| NetError::Protocol(format!("bad message: {e}")))
}

fn auth_bytes(who: &[u8], nonce_c: &str, nonce_s: &str) -> Vec<u8> {
    let mut b = b"bo-auth-v1".to_vec();
    b.extend_from_slice(who);
    b.extend_from_slice(nonce_c.as_bytes());
    b.extend_from_slice(nonce_s.as_bytes());
    b
}

fn valid_nonce(n: &str) -> bool {
    n.len() == 64 && n.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Cap a batch by serialized size, keeping at least one event.
fn trim_batch(mut events: Vec<Event>) -> Vec<Event> {
    let mut total = 0;
    let mut keep = 0;
    for e in &events {
        total += serde_json::to_vec(e).map(|b| b.len()).unwrap_or(0);
        if total > BATCH_BYTES && keep > 0 {
            break;
        }
        keep += 1;
    }
    events.truncate(keep);
    events
}

// ---------------------------------------------------------------------- server

pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    /// Listen on `bind` (for example `"0.0.0.0:7400"`; use port 0 in tests). Only a
    /// `Role::Server` node with signatures required may serve.
    pub fn start(node: Arc<SyncNode>, bind: &str) -> Result<Server, NetError> {
        if !node.signatures_required() {
            return Err(NetError::SignaturesNotRequired);
        }
        if node.role() != Role::Server {
            return Err(NetError::Protocol("only a node created with Role::Server can serve".into()));
        }
        let listener = TcpListener::bind(bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let flag = stop.clone();
        let accept = thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS || sock.set_nonblocking(false).is_err() {
                            continue; // dropping the socket refuses the connection
                        }
                        active.fetch_add(1, Ordering::SeqCst);
                        let (node, active, flag) = (node.clone(), active.clone(), flag.clone());
                        thread::spawn(move || {
                            let _ = serve_connection(&node, sock, &flag);
                            active.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Ok(Server { addr, stop, accept: Some(accept) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting; open connections end within about a second.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

fn serve_connection(node: &SyncNode, mut sock: TcpStream, stop: &AtomicBool) -> Result<(), NetError> {
    sock.set_write_timeout(Some(IO_TIMEOUT))?;
    sock.set_read_timeout(Some(IO_TIMEOUT))?;
    let peer = match handshake_server(node, &mut sock) {
        Ok(p) => p,
        Err(e) => {
            let _ = write_frame(&mut sock, &Msg::Error { message: "login refused".into() });
            return Err(e);
        }
    };
    let _ = peer;
    let mut idle = 0;
    while !stop.load(Ordering::SeqCst) {
        // Wait for the next request without a half-read frame ever timing out.
        sock.set_read_timeout(Some(Duration::from_secs(1)))?;
        match sock.peek(&mut [0u8; 1]) {
            Ok(0) => return Ok(()),
            Ok(_) => idle = 0,
            Err(e) if is_timeout(&e) => {
                idle += 1;
                if idle > IDLE_SECONDS {
                    return Ok(());
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        }
        sock.set_read_timeout(Some(IO_TIMEOUT))?;
        let reply = match read_frame(&mut sock, MAX_FRAME)? {
            Msg::GetVv => node.version_vector().map(|vv| Msg::Vv { vv }),
            Msg::Pull { vv, limit } => node.events_since(&vv, limit.clamp(1, BATCH_EVENTS)).map(|e| Msg::Events { events: trim_batch(e) }),
            Msg::Push { events } if events.len() <= MAX_PUSH_EVENTS => node.apply_remote(&events).map(|report| Msg::Applied { report }),
            Msg::Push { .. } => {
                write_frame(&mut sock, &Msg::Error { message: "too many events in one push".into() })?;
                return Ok(());
            }
            _ => {
                write_frame(&mut sock, &Msg::Error { message: "unexpected message".into() })?;
                return Ok(());
            }
        };
        match reply {
            Ok(m) => write_frame(&mut sock, &m)?,
            Err(_) => {
                // Details stay in the server; the peer only learns that it failed.
                write_frame(&mut sock, &Msg::Error { message: "server error".into() })?;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn handshake_server(node: &SyncNode, sock: &mut TcpStream) -> Result<String, NetError> {
    let Msg::Hello { device, nonce: nonce_c } = read_frame(sock, MAX_PRELOGIN_FRAME)? else {
        return Err(NetError::Protocol("expected hello".into()));
    };
    if !valid_device_id(&device) || !valid_nonce(&nonce_c) || node.trusted(&device).is_none() {
        return Err(NetError::Auth("device is not enrolled".into()));
    }
    let nonce_s = random_hex(32);
    let sig = node.sign(&auth_bytes(b"server", &nonce_c, &nonce_s));
    write_frame(sock, &Msg::Challenge { device: node.device_id().to_string(), nonce: nonce_s.clone(), sig })?;
    let Msg::Auth { sig } = read_frame(sock, MAX_PRELOGIN_FRAME)? else {
        return Err(NetError::Protocol("expected auth".into()));
    };
    if node.verify_from(&device, &auth_bytes(b"client", &nonce_c, &nonce_s), &sig).is_none() {
        return Err(NetError::Auth("bad signature".into()));
    }
    write_frame(sock, &Msg::Ready)?;
    Ok(device)
}

// ---------------------------------------------------------------------- client

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncStats {
    pub pushed: usize,
    pub pulled: usize,
    pub rounds: usize,
}

fn expect_vv(m: Msg) -> Result<BTreeMap<String, u64>, NetError> {
    match m {
        Msg::Vv { vv } => Ok(vv),
        other => Err(unexpected(other)),
    }
}

fn unexpected(m: Msg) -> NetError {
    match m {
        Msg::Error { message } => NetError::Protocol(format!("peer reported: {message}")),
        _ => NetError::Protocol("unexpected reply".into()),
    }
}

fn connect(addr: &str) -> Result<TcpStream, NetError> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "address did not resolve");
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, IO_TIMEOUT) {
            Ok(s) => {
                s.set_read_timeout(Some(IO_TIMEOUT))?;
                s.set_write_timeout(Some(IO_TIMEOUT))?;
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last.into())
}

/// One full synchronisation with the server at `addr` whose device id is
/// `server_device` (which must be enrolled here with the server role).
pub fn sync_once(node: &SyncNode, addr: &str, server_device: &str) -> Result<SyncStats, NetError> {
    if !node.signatures_required() {
        return Err(NetError::SignaturesNotRequired);
    }
    match node.trusted(server_device) {
        Some(t) if t.role == Role::Server => {}
        _ => return Err(NetError::Auth("the server is not enrolled on this device".into())),
    }
    let mut sock = connect(addr)?;

    let nonce_c = random_hex(32);
    write_frame(&mut sock, &Msg::Hello { device: node.device_id().to_string(), nonce: nonce_c.clone() })?;
    let (device, nonce_s, sig) = match read_frame(&mut sock, MAX_PRELOGIN_FRAME)? {
        Msg::Challenge { device, nonce, sig } => (device, nonce, sig),
        Msg::Error { .. } => return Err(NetError::Auth("the server refused this device".into())),
        _ => return Err(NetError::Protocol("expected challenge".into())),
    };
    if device != server_device || !valid_nonce(&nonce_s) || node.verify_from(&device, &auth_bytes(b"server", &nonce_c, &nonce_s), &sig) != Some(Role::Server) {
        return Err(NetError::Auth("the server could not prove its identity".into()));
    }
    write_frame(&mut sock, &Msg::Auth { sig: node.sign(&auth_bytes(b"client", &nonce_c, &nonce_s)) })?;
    match read_frame(&mut sock, MAX_PRELOGIN_FRAME)? {
        Msg::Ready => {}
        Msg::Error { .. } => return Err(NetError::Auth("the server refused this device".into())),
        _ => return Err(NetError::Protocol("expected ready".into())),
    }

    let mut stats = SyncStats::default();
    // Push what the server lacks.
    loop {
        write_frame(&mut sock, &Msg::GetVv)?;
        let server_vv = expect_vv(read_frame(&mut sock, MAX_FRAME)?)?;
        let events = trim_batch(node.events_since(&server_vv, BATCH_EVENTS)?);
        if events.is_empty() {
            break;
        }
        let n = events.len();
        write_frame(&mut sock, &Msg::Push { events })?;
        let Msg::Applied { report } = read_frame(&mut sock, MAX_FRAME)? else {
            return Err(NetError::Protocol("expected applied".into()));
        };
        stats.pushed += n;
        stats.rounds += 1;
        if !report.progressed() {
            break; // the server will not take these (for example, not yet enrolled): stop, don't spin
        }
    }
    // Pull what we lack.
    loop {
        write_frame(&mut sock, &Msg::Pull { vv: node.version_vector()?, limit: BATCH_EVENTS })?;
        let Msg::Events { events } = read_frame(&mut sock, MAX_FRAME)? else {
            return Err(NetError::Protocol("expected events".into()));
        };
        if events.is_empty() {
            break;
        }
        let n = events.len();
        let report = node.apply_remote(&events)?;
        stats.pulled += n;
        stats.rounds += 1;
        if !report.progressed() {
            break;
        }
    }
    Ok(stats)
}

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_ms: u64,
    pub max_ms: u64,
}

impl RetryPolicy {
    /// Wait before retry number `attempt` (0-based): doubles each time, capped.
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        self.base_ms.saturating_mul(1u64 << attempt.min(32)).min(self.max_ms)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_attempts: 5, base_ms: 500, max_ms: 30_000 }
    }
}

/// `sync_once` with exponential backoff. Only connection-level failures are retried;
/// authentication and protocol errors will not fix themselves, so they return immediately.
pub fn sync_with_retry(node: &SyncNode, addr: &str, server_device: &str, policy: RetryPolicy, sleep: &dyn Fn(Duration)) -> Result<SyncStats, NetError> {
    let mut last = None;
    for attempt in 0..policy.max_attempts {
        match sync_once(node, addr, server_device) {
            Ok(s) => return Ok(s),
            Err(NetError::Io(e)) => {
                last = Some(NetError::Io(e));
                if attempt + 1 < policy.max_attempts {
                    sleep(Duration::from_millis(policy.delay_ms(attempt)));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| NetError::Protocol("no attempts allowed".into())))
}
