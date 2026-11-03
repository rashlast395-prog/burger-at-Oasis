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
//! * **TLS:** `start_tls` / `sync_once_tls` run the same protocol inside TLS 1.3 with a pinned
//!   server certificate (see [`crate::tls`]). The plain variants send everything in the
//!   clear and exist for tests and loopback use only: **never use them across a network.**
//!
//! ## Not here
//! * No rate limiting beyond a connection cap, and no revocation of enrolled devices.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustls::{ClientConnection, ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};

use crate::identity::random_hex;
use crate::tls::{TlsClient, TlsServer};
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
    /// TLS failed: untrusted or wrong certificate, or a protocol violation. Retrying will
    /// not help until the certificate or address is fixed.
    Tls(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Io(e) => write!(f, "network i/o: {e}"),
            NetError::Protocol(m) => write!(f, "protocol error: {m}"),
            NetError::Auth(m) => write!(f, "authentication failed: {m}"),
            NetError::Sync(e) => write!(f, "{e}"),
            NetError::SignaturesNotRequired => write!(f, "call require_signatures(true) before using the network"),
            NetError::Tls(m) => write!(f, "tls: {m}"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> Self {
        // rustls reports TLS failures as io errors wrapping its own error type.
        match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
            Some(t) => NetError::Tls(t.to_string()),
            None => NetError::Io(e),
        }
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

fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Collects bytes into frames. Unlike `read_exact`, a read timeout in the middle of a frame
/// loses nothing: the bytes stay buffered and the next `poll` continues. (TLS streams have
/// no `peek`, so this is how the server waits for the next request without blocking.)
pub struct FrameReader {
    buf: Vec<u8>,
    started: Option<Instant>,
}

impl FrameReader {
    pub fn new() -> FrameReader {
        FrameReader { buf: Vec::new(), started: None }
    }

    fn try_parse(&mut self, max: usize) -> Result<Option<Msg>, NetError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if n > max {
            return Err(NetError::Protocol(format!("frame of {n} bytes exceeds the limit of {max}")));
        }
        if self.buf.len() < 4 + n {
            return Ok(None);
        }
        let msg = serde_json::from_slice(&self.buf[4..4 + n]).map_err(|e| NetError::Protocol(format!("bad message: {e}")));
        self.buf.drain(..4 + n);
        self.started = if self.buf.is_empty() { None } else { Some(Instant::now()) };
        msg.map(Some)
    }

    /// The next complete message, or `None` if the read timed out first. A clean end of
    /// stream between messages is `Io(UnexpectedEof)`; so is one in the middle of a message.
    /// A message that stays incomplete for longer than `IO_TIMEOUT` is an error.
    pub fn poll<R: Read>(&mut self, r: &mut R, max: usize) -> Result<Option<Msg>, NetError> {
        loop {
            if let Some(m) = self.try_parse(max)? {
                return Ok(Some(m));
            }
            let mut chunk = [0u8; 8192];
            match r.read(&mut chunk) {
                Ok(0) => return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into())),
                Ok(n) => {
                    self.started.get_or_insert_with(Instant::now);
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) if is_timeout(&e) => {
                    if self.started.map_or(false, |t| t.elapsed() > IO_TIMEOUT) {
                        return Err(NetError::Io(io::ErrorKind::TimedOut.into()));
                    }
                    return Ok(None);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Default for FrameReader {
    fn default() -> Self {
        FrameReader::new()
    }
}

/// Read one frame, blocking up to the stream's read timeout.
pub fn read_frame<R: Read>(r: &mut R, max: usize) -> Result<Msg, NetError> {
    FrameReader::new().poll(r, max)?.ok_or_else(|| NetError::Io(io::ErrorKind::TimedOut.into()))
}

enum Stream {
    Plain(TcpStream),
    ClientTls(Box<StreamOwned<ClientConnection, TcpStream>>),
    ServerTls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::ClientTls(s) => &s.sock,
            Stream::ServerTls(s) => &s.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::ClientTls(s) => s.read(buf),
            Stream::ServerTls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::ClientTls(s) => s.write(buf),
            Stream::ServerTls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::ClientTls(s) => s.flush(),
            Stream::ServerTls(s) => s.flush(),
        }
    }
}

/// A connection: a (possibly TLS) stream plus its frame buffer.
struct Conn {
    stream: Stream,
    reader: FrameReader,
}

impl Conn {
    fn new(stream: Stream) -> io::Result<Conn> {
        stream.tcp().set_read_timeout(Some(IO_TIMEOUT))?;
        stream.tcp().set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Conn { stream, reader: FrameReader::new() })
    }

    fn send(&mut self, m: &Msg) -> Result<(), NetError> {
        write_frame(&mut self.stream, m).map_err(NetError::from)
    }

    /// Wait for the next message up to the current read timeout.
    fn poll(&mut self, max: usize) -> Result<Option<Msg>, NetError> {
        self.reader.poll(&mut self.stream, max)
    }

    fn recv(&mut self, max: usize) -> Result<Msg, NetError> {
        self.stream.tcp().set_read_timeout(Some(IO_TIMEOUT))?;
        self.poll(max)?.ok_or_else(|| NetError::Io(io::ErrorKind::TimedOut.into()))
    }
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
    /// Listen on `bind` **without encryption**. For tests and loopback only; see the module
    /// docs. Use [`Server::start_tls`] for anything that crosses a network.
    pub fn start(node: Arc<SyncNode>, bind: &str) -> Result<Server, NetError> {
        Server::start_inner(node, bind, None)
    }

    /// Listen on `bind` (for example `"0.0.0.0:7443"`; use port 0 in tests) over TLS 1.3.
    /// Only a `Role::Server` node with signatures required may serve.
    pub fn start_tls(node: Arc<SyncNode>, bind: &str, tls: TlsServer) -> Result<Server, NetError> {
        Server::start_inner(node, bind, Some(tls))
    }

    fn start_inner(node: Arc<SyncNode>, bind: &str, tls: Option<TlsServer>) -> Result<Server, NetError> {
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
                        let stream = match &tls {
                            None => Stream::Plain(sock),
                            Some(t) => match ServerConnection::new(t.config.clone()) {
                                Ok(c) => Stream::ServerTls(Box::new(StreamOwned::new(c, sock))),
                                Err(_) => continue,
                            },
                        };
                        active.fetch_add(1, Ordering::SeqCst);
                        let (node, active, flag) = (node.clone(), active.clone(), flag.clone());
                        thread::spawn(move || {
                            let _ = serve_connection(&node, stream, &flag);
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

fn serve_connection(node: &SyncNode, stream: Stream, stop: &AtomicBool) -> Result<(), NetError> {
    let mut conn = Conn::new(stream)?;
    if let Err(e) = handshake_server(node, &mut conn) {
        let _ = conn.send(&Msg::Error { message: "login refused".into() });
        return Err(e);
    }
    let mut idle = 0;
    while !stop.load(Ordering::SeqCst) {
        conn.stream.tcp().set_read_timeout(Some(Duration::from_secs(1)))?;
        let msg = match conn.poll(MAX_FRAME) {
            Ok(Some(m)) => m,
            Ok(None) => {
                idle += 1;
                if idle > IDLE_SECONDS {
                    return Ok(());
                }
                continue;
            }
            Err(NetError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        idle = 0;
        let reply = match msg {
            Msg::GetVv => node.version_vector().map(|vv| Msg::Vv { vv }),
            Msg::Pull { vv, limit } => node.events_since(&vv, limit.clamp(1, BATCH_EVENTS)).map(|e| Msg::Events { events: trim_batch(e) }),
            Msg::Push { events } if events.len() <= MAX_PUSH_EVENTS => node.apply_remote(&events).map(|report| Msg::Applied { report }),
            Msg::Push { .. } => {
                conn.send(&Msg::Error { message: "too many events in one push".into() })?;
                return Ok(());
            }
            _ => {
                conn.send(&Msg::Error { message: "unexpected message".into() })?;
                return Ok(());
            }
        };
        match reply {
            Ok(m) => conn.send(&m)?,
            Err(_) => {
                // Details stay in the server; the peer only learns that it failed.
                conn.send(&Msg::Error { message: "server error".into() })?;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn handshake_server(node: &SyncNode, conn: &mut Conn) -> Result<(), NetError> {
    let Msg::Hello { device, nonce: nonce_c } = conn.recv(MAX_PRELOGIN_FRAME)? else {
        return Err(NetError::Protocol("expected hello".into()));
    };
    if !valid_device_id(&device) || !valid_nonce(&nonce_c) || node.trusted(&device).is_none() {
        return Err(NetError::Auth("device is not enrolled".into()));
    }
    let nonce_s = random_hex(32);
    let sig = node.sign(&auth_bytes(b"server", &nonce_c, &nonce_s));
    conn.send(&Msg::Challenge { device: node.device_id().to_string(), nonce: nonce_s.clone(), sig })?;
    let Msg::Auth { sig } = conn.recv(MAX_PRELOGIN_FRAME)? else {
        return Err(NetError::Protocol("expected auth".into()));
    };
    if node.verify_from(&device, &auth_bytes(b"client", &nonce_c, &nonce_s), &sig).is_none() {
        return Err(NetError::Auth("bad signature".into()));
    }
    conn.send(&Msg::Ready)
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

fn connect(addr: &str, tls: Option<&TlsClient>) -> Result<Conn, NetError> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "address did not resolve");
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, IO_TIMEOUT) {
            Ok(sock) => {
                let stream = match tls {
                    None => Stream::Plain(sock),
                    Some(t) => {
                        let c = ClientConnection::new(t.config.clone(), t.name.clone()).map_err(|e| NetError::Tls(e.to_string()))?;
                        Stream::ClientTls(Box::new(StreamOwned::new(c, sock)))
                    }
                };
                return Ok(Conn::new(stream)?);
            }
            Err(e) => last = e,
        }
    }
    Err(last.into())
}

/// One full synchronisation with the server at `addr` whose device id is
/// `server_device` (which must be enrolled here with the server role), **without
/// encryption**. For tests and loopback only; see [`sync_once_tls`].
pub fn sync_once(node: &SyncNode, addr: &str, server_device: &str) -> Result<SyncStats, NetError> {
    sync_inner(node, addr, server_device, None)
}

/// [`sync_once`] over TLS 1.3, trusting only the pinned server certificate.
pub fn sync_once_tls(node: &SyncNode, addr: &str, server_device: &str, tls: &TlsClient) -> Result<SyncStats, NetError> {
    sync_inner(node, addr, server_device, Some(tls))
}

fn sync_inner(node: &SyncNode, addr: &str, server_device: &str, tls: Option<&TlsClient>) -> Result<SyncStats, NetError> {
    if !node.signatures_required() {
        return Err(NetError::SignaturesNotRequired);
    }
    match node.trusted(server_device) {
        Some(t) if t.role == Role::Server => {}
        _ => return Err(NetError::Auth("the server is not enrolled on this device".into())),
    }
    let mut conn = connect(addr, tls)?;

    let nonce_c = random_hex(32);
    conn.send(&Msg::Hello { device: node.device_id().to_string(), nonce: nonce_c.clone() })?;
    let (device, nonce_s, sig) = match conn.recv(MAX_PRELOGIN_FRAME)? {
        Msg::Challenge { device, nonce, sig } => (device, nonce, sig),
        Msg::Error { .. } => return Err(NetError::Auth("the server refused this device".into())),
        _ => return Err(NetError::Protocol("expected challenge".into())),
    };
    if device != server_device || !valid_nonce(&nonce_s) || node.verify_from(&device, &auth_bytes(b"server", &nonce_c, &nonce_s), &sig) != Some(Role::Server) {
        return Err(NetError::Auth("the server could not prove its identity".into()));
    }
    conn.send(&Msg::Auth { sig: node.sign(&auth_bytes(b"client", &nonce_c, &nonce_s)) })?;
    match conn.recv(MAX_PRELOGIN_FRAME)? {
        Msg::Ready => {}
        Msg::Error { .. } => return Err(NetError::Auth("the server refused this device".into())),
        _ => return Err(NetError::Protocol("expected ready".into())),
    }

    let mut stats = SyncStats::default();
    // Push what the server lacks.
    loop {
        conn.send(&Msg::GetVv)?;
        let server_vv = expect_vv(conn.recv(MAX_FRAME)?)?;
        let events = trim_batch(node.events_since(&server_vv, BATCH_EVENTS)?);
        if events.is_empty() {
            break;
        }
        let n = events.len();
        conn.send(&Msg::Push { events })?;
        let Msg::Applied { report } = conn.recv(MAX_FRAME)? else {
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
        conn.send(&Msg::Pull { vv: node.version_vector()?, limit: BATCH_EVENTS })?;
        let Msg::Events { events } = conn.recv(MAX_FRAME)? else {
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

/// `sync_once` with exponential backoff (unencrypted; see [`sync_once`]).
pub fn sync_with_retry(node: &SyncNode, addr: &str, server_device: &str, policy: RetryPolicy, sleep: &dyn Fn(Duration)) -> Result<SyncStats, NetError> {
    retry(policy, sleep, || sync_inner(node, addr, server_device, None))
}

/// `sync_once_tls` with exponential backoff.
pub fn sync_with_retry_tls(node: &SyncNode, addr: &str, server_device: &str, tls: &TlsClient, policy: RetryPolicy, sleep: &dyn Fn(Duration)) -> Result<SyncStats, NetError> {
    retry(policy, sleep, || sync_inner(node, addr, server_device, Some(tls)))
}

/// Only connection-level failures are retried; authentication, certificate and protocol
/// errors will not fix themselves, so they return immediately.
fn retry(policy: RetryPolicy, sleep: &dyn Fn(Duration), mut attempt_fn: impl FnMut() -> Result<SyncStats, NetError>) -> Result<SyncStats, NetError> {
    let mut last = None;
    for attempt in 0..policy.max_attempts {
        match attempt_fn() {
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
