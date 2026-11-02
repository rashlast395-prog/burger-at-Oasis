//! Local JSON API so the website (Next.js) can use the engine on the same machine.
//!
//! * Listens on a **loopback address only** (plain HTTP must never cross a network) and
//!   requires `Authorization: Bearer <token>`, with the token taken from the environment.
//! * Order creation runs under one lock: the idempotency check, the promo usage limit and
//!   the save happen together, so a double-clicked checkout yields one order and a promo
//!   cannot be used more often than allowed.
//! * The engine recomputes the totals and refuses an order whose total does not match what
//!   the website calculated, so the two can never silently disagree.
//!
//! Endpoints (all JSON, money in minor units):
//!
//! | method | path                         | purpose                                  |
//! |--------|------------------------------|------------------------------------------|
//! | GET    | /v1/health                   | liveness and device id                   |
//! | GET    | /v1/menu                     | every product                            |
//! | GET    | /v1/products?ids=a,b         | selected products                        |
//! | PUT    | /v1/products/{id}            | create or replace a product              |
//! | GET    | /v1/promos/{code}            | a promo code with its usage count        |
//! | PUT    | /v1/promos/{code}            | create or replace a promo code           |
//! | POST   | /v1/orders                   | place an order (idempotent)              |
//! | GET    | /v1/orders?status=s          | orders, optionally by status             |
//! | GET    | /v1/orders/by-key/{key}      | the order for an idempotency key         |
//! | GET    | /v1/orders/{id}              | one order with its payment summary       |
//! | POST   | /v1/orders/{id}/status       | `{"status": "accepted"}` etc.            |

use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bo_domain::{DomainError, ORDERS};
use bo_sync::{strip_meta, SyncError, SyncNode};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

const MAX_BODY: u64 = 1024 * 1024;
const MIN_TOKEN_LEN: usize = 32;
const MENU: &str = "menu";
const PROMOS: &str = "promos";

pub struct Api {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Api {
    /// Start serving on `bind` (must be a loopback address) with the given bearer token.
    pub fn start(node: Arc<SyncNode>, bind: &str, token: String) -> Result<Api, String> {
        let addr: SocketAddr = bind.parse().map_err(|_| format!("--api must be an address like 127.0.0.1:7480, not '{bind}'"))?;
        if !addr.ip().is_loopback() {
            return Err("the website API is plain HTTP and only listens on 127.0.0.1 or ::1".into());
        }
        if token.len() < MIN_TOKEN_LEN {
            return Err(format!("BO_API_TOKEN must be set to a random secret of at least {MIN_TOKEN_LEN} characters"));
        }
        let server = Server::http(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        let addr = server.server_addr().to_ip().ok_or("unexpected listener address")?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let state = Arc::new(State { node, token, create_lock: Mutex::new(()) });
        let handle = thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match server.recv_timeout(Duration::from_millis(200)) {
                    Ok(Some(req)) => {
                        let st = state.clone();
                        // One thread per request keeps a slow client from blocking others.
                        thread::spawn(move || handle(&st, req));
                    }
                    Ok(None) => {}
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            }
        });
        Ok(Api { addr, stop, handle: Some(handle) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct State {
    node: Arc<SyncNode>,
    token: String,
    create_lock: Mutex<()>,
}

struct Reply(u16, Value);

fn ok(v: Value) -> Reply {
    Reply(200, v)
}

fn err(code: u16, error: &str, message: impl Into<String>) -> Reply {
    Reply(code, json!({"error": error, "message": message.into()}))
}

fn from_domain(e: DomainError) -> Reply {
    match e {
        DomainError::Invalid(m) => err(422, "INVALID", m),
        DomainError::NotFound(m) => err(404, "NOT_FOUND", m),
        DomainError::Sync(SyncError::Rejected(m)) => err(409, "REJECTED", m),
        DomainError::Sync(SyncError::InvalidDocument(m)) => err(422, "INVALID", m),
        DomainError::Sync(e) => {
            eprintln!("api: engine error: {e}");
            err(500, "INTERNAL_ERROR", "Something went wrong.")
        }
    }
}

fn from_sync(e: SyncError) -> Reply {
    from_domain(DomainError::Sync(e))
}

/// Compare secrets without an early exit, so timing does not reveal how much matched.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0xff);
    }
    diff == 0
}

fn handle(st: &State, mut req: Request) {
    let reply = route(st, &mut req);
    let body = serde_json::to_string(&reply.1).unwrap_or_else(|_| "{}".into());
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header");
    let _ = req.respond(Response::from_string(body).with_status_code(reply.0).with_header(header));
}

fn route(st: &State, req: &mut Request) -> Reply {
    let authorised = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .is_some_and(|t| same(t, &st.token));
    if !authorised {
        return err(401, "UNAUTHORISED", "missing or wrong API token");
    }
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let parts: Vec<String> = path.trim_matches('/').split('/').map(decode).collect();
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    let method = req.method().clone();
    let mut body = || -> Result<Value, Reply> {
        let mut text = String::new();
        req.as_reader().take(MAX_BODY + 1).read_to_string(&mut text).map_err(|_| err(400, "BAD_REQUEST", "unreadable body"))?;
        if text.len() as u64 > MAX_BODY {
            return Err(err(413, "TOO_LARGE", "request body too large"));
        }
        serde_json::from_str(&text).map_err(|_| err(400, "BAD_REQUEST", "body must be JSON"))
    };
    let n = &st.node;
    match (&method, parts.as_slice()) {
        (Method::Get, ["v1", "health"]) => ok(json!({"ok": true, "device": n.device_id()})),
        (Method::Get, ["v1", "menu"]) => n.find(MENU, &json!({})).map(|ps| ok(json!({"products": ps.iter().map(clean).collect::<Vec<_>>()}))).unwrap_or_else(from_sync),
        (Method::Get, ["v1", "products"]) => {
            let ids = param(query, "ids").unwrap_or_default();
            let mut out = Vec::new();
            for id in ids.split(',').filter(|s| !s.is_empty()).take(100) {
                match n.get(MENU, id) {
                    Ok(Some(p)) => out.push(clean(&p)),
                    Ok(None) => {}
                    Err(e) => return from_sync(e),
                }
            }
            ok(json!({"products": out}))
        }
        (Method::Put, ["v1", "products", id]) => match body() {
            Ok(b) => put_product(n, id, b),
            Err(r) => r,
        },
        (Method::Get, ["v1", "promos", code]) => match n.get(PROMOS, code) {
            Ok(Some(p)) => match promo_uses(n, code) {
                Ok(used) => {
                    let mut p = clean(&p);
                    p["used_count"] = json!(used);
                    ok(p)
                }
                Err(r) => r,
            },
            Ok(None) => err(404, "NOT_FOUND", "no such promo code"),
            Err(e) => from_sync(e),
        },
        (Method::Put, ["v1", "promos", code]) => match body() {
            Ok(mut b) => {
                b["_id"] = json!(code);
                n.put(PROMOS, b).map(|_| ok(json!({"ok": true}))).unwrap_or_else(from_sync)
            }
            Err(r) => r,
        },
        (Method::Post, ["v1", "orders"]) => match body() {
            Ok(b) => create_order(st, b),
            Err(r) => r,
        },
        (Method::Get, ["v1", "orders"]) => {
            let q = match param(query, "status") {
                Some(s) => json!({"filter": {"status": s}}),
                None => json!({}),
            };
            n.find(ORDERS, &q).map(|os| ok(json!({"orders": os.iter().map(clean).collect::<Vec<_>>()}))).unwrap_or_else(from_sync)
        }
        (Method::Get, ["v1", "orders", "by-key", key]) => match by_key(n, key) {
            Ok(Some(o)) => summary(n, &o),
            Ok(None) => err(404, "NOT_FOUND", "no order for that key"),
            Err(r) => r,
        },
        (Method::Get, ["v1", "orders", id]) => match n.get(ORDERS, id) {
            Ok(Some(o)) => match bo_domain::payment_summary(n, id) {
                Ok(s) => ok(json!({"order": clean(&o), "payment": {"status": s.status, "paid": s.paid, "refunded": s.refunded, "net": s.net}})),
                Err(e) => from_domain(e),
            },
            Ok(None) => err(404, "NOT_FOUND", "no such order"),
            Err(e) => from_sync(e),
        },
        (Method::Post, ["v1", "orders", id, "status"]) => match body() {
            Ok(b) => match b["status"].as_str() {
                Some(s) => bo_domain::set_status(n, id, s).map(|o| ok(json!({"order": clean(&o)}))).unwrap_or_else(from_domain),
                None => err(422, "INVALID", "status is required"),
            },
            Err(r) => r,
        },
        _ => err(404, "NOT_FOUND", "no such endpoint"),
    }
}

/// Strip the node's bookkeeping fields from a stored document.
fn clean(doc: &Value) -> Value {
    strip_meta(doc)
}

/// Percent-decode one path segment or query value.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn param(query: &str, name: &str) -> Option<String> {
    // In a query string (not a path) '+' means a space.
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| decode(&v.replace('+', " ")))
}

fn put_product(n: &SyncNode, id: &str, mut b: Value) -> Reply {
    if !b["name"].as_str().is_some_and(|s| !s.trim().is_empty()) || !b["base_price"].as_i64().is_some_and(|p| p >= 0) {
        return err(422, "INVALID", "a product needs a name and a whole-number base_price in minor units");
    }
    // Versions only go up; each replacement gets the next one.
    let version = match n.get(MENU, id) {
        Ok(cur) => cur.and_then(|c| c["version"].as_u64()).unwrap_or(0) + 1,
        Err(e) => return from_sync(e),
    };
    b["_id"] = json!(id);
    b["version"] = json!(version);
    n.put(MENU, b).map(|_| ok(json!({"ok": true, "version": version}))).unwrap_or_else(from_sync)
}

fn promo_uses(n: &SyncNode, code: &str) -> Result<usize, Reply> {
    n.find(ORDERS, &json!({"filter": {"promo_code": code}})).map(|v| v.len()).map_err(from_sync)
}

fn by_key(n: &SyncNode, key: &str) -> Result<Option<Value>, Reply> {
    n.find(ORDERS, &json!({"filter": {"idempotency_key": key}})).map(|v| v.into_iter().next()).map_err(from_sync)
}

fn summary(n: &SyncNode, o: &Value) -> Reply {
    let id = o["_id"].as_str().unwrap_or_default();
    match bo_domain::payment_summary(n, id) {
        Ok(s) => ok(json!({"order": {"id": id, "reference": o["reference"], "status": o["status"], "payment_status": s.status, "total": o["total"]}})),
        Err(e) => from_domain(e),
    }
}

fn create_order(st: &State, mut b: Value) -> Reply {
    let n = &st.node;
    let Some(key) = b["idempotency_key"].as_str().map(String::from).filter(|k| (8..=100).contains(&k.len())) else {
        return err(422, "INVALID", "idempotency_key (8 to 100 characters) is required");
    };
    let Some(expected) = b.as_object_mut().and_then(|m| m.remove("expected_total")).and_then(|v| v.as_i64()) else {
        return err(422, "INVALID", "expected_total is required");
    };
    let _guard = st.create_lock.lock().unwrap_or_else(|e| e.into_inner());
    match by_key(n, &key) {
        Ok(Some(o)) => {
            let mut r = summary(n, &o);
            r.1["replayed"] = json!(true);
            return r;
        }
        Ok(None) => {}
        Err(r) => return r,
    }
    if let Some(code) = b["promo_code"].as_str().map(String::from) {
        match n.get(PROMOS, &code) {
            Ok(Some(p)) => {
                if let Some(max) = p["max_uses"].as_u64() {
                    match promo_uses(n, &code) {
                        Ok(used) if used as u64 >= max => return err(409, "PROMO_EXHAUSTED", "This promo code has been fully used."),
                        Ok(_) => {}
                        Err(r) => return r,
                    }
                }
            }
            Ok(None) => return err(409, "PROMO_INVALID", "That promo code is not valid."),
            Err(e) => return from_sync(e),
        }
    }
    // Recompute exactly as the engine will, and refuse any disagreement with the website.
    let items_total: i64 = b["items"].as_array().map(|it| it.iter().map(|i| i["qty"].as_i64().unwrap_or(0) * i["unit_price"].as_i64().unwrap_or(0)).sum()).unwrap_or(0);
    let computed = items_total + b["delivery_fee"].as_i64().unwrap_or(0) - b["discount"].as_i64().unwrap_or(0);
    if computed != expected {
        return err(409, "TOTAL_MISMATCH", format!("the website total {expected} does not match the engine's {computed}"));
    }
    match bo_domain::place_order(n, b) {
        Ok(o) => {
            let mut r = summary(n, &o);
            r.0 = 201;
            r.1["replayed"] = json!(false);
            r
        }
        Err(e) => from_domain(e),
    }
}
