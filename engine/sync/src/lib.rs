//! Offline-first replication on top of `bo-engine`.
//!
//! ## Model
//! * Every change to a synchronised document is an [`Event`] stamped with a hybrid logical
//!   clock and numbered `1, 2, 3, ...` per device. A node's own write and its log entry are
//!   committed in **one storage transaction**, so a write can never exist without its event.
//! * Nodes exchange **version vectors** ("for each device, the highest seq I have applied").
//!   A node sends exactly the events the other side lacks. Receivers apply each device's
//!   events strictly in order, so replication is idempotent and safe against loss,
//!   duplication, reordering, retries and crashes: a repeated event is ignored, a gap defers
//!   the later events until the missing ones arrive.
//! * Each synchronised collection has a conflict [`Rule`]. With deterministic rules every
//!   replica converges to the same state whatever order events arrive in.
//! * Events a rule rejects go to the dead-letter log and still advance the version vector,
//!   so one bad event never blocks the stream. Nothing is silently dropped.
//! * Every node keeps the full event log, so a server can relay events between devices.
//!
//! ## Not here yet (later phases)
//! * No network transport, authentication or event signatures: the sync calls are plain
//!   function calls, and **any peer that can call `apply_remote` is trusted**. The
//!   authenticated transport must establish who is allowed to send which device's events.
//! * No log compaction: the event log only grows.
//! * **Restoring a device from an old backup is dangerous**: it would reuse sequence
//!   numbers it already sent. Such a device must first pull its own events back from the
//!   server, or be re-enrolled as a new device.
//! * A device whose clock is far in the future wins last-writer-wins conflicts until real
//!   time catches up (it is only prevented from dragging other clocks forward).

mod event;
mod hlc;
pub mod identity;
pub mod net;
mod rules;

pub use event::{pos, Event, Op, Role};
use identity::Identity;
pub use hlc::{new_device_id, new_uuid_v7, valid_device_id, valid_uuid_v7, Clock, Hlc, ManualClock, SystemClock};
pub use rules::{AppendOnly, Current, Decision, LastWriterWins, Rule, ServerAuthoritative, Versioned};

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use bo_engine::{Engine, EngineError, Tx, MAX_DOC_BYTES};
use serde_json::{json, Map, Value};

/// Largest user document accepted (leaves room for the bookkeeping fields).
const MAX_SYNC_DOC_BYTES: usize = MAX_DOC_BYTES - 64 * 1024;
/// How far ahead of local time a remote stamp may pull the local clock.
pub const DEFAULT_MAX_DRIFT_MS: u64 = 5 * 60 * 1000;

#[derive(Debug)]
pub enum SyncError {
    Engine(EngineError),
    InvalidDocument(String),
    /// A rule refused a local write.
    Rejected(String),
    UnknownCollection(String),
    /// The database was created with different rules than the ones supplied.
    RuleMismatch(String),
    NotASyncDatabase,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Engine(e) => write!(f, "{e}"),
            SyncError::InvalidDocument(m) => write!(f, "invalid document: {m}"),
            SyncError::Rejected(m) => write!(f, "rejected: {m}"),
            SyncError::UnknownCollection(c) => write!(f, "'{c}' is not a synchronised collection"),
            SyncError::RuleMismatch(m) => write!(f, "rule mismatch: {m}"),
            SyncError::NotASyncDatabase => write!(f, "this database was not created by SyncNode::create"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<EngineError> for SyncError {
    fn from(e: EngineError) -> Self {
        SyncError::Engine(e)
    }
}

pub type Result<T> = std::result::Result<T, SyncError>;

/// A device this node accepts events from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trusted {
    pub public: String,
    pub role: Role,
}

/// What `apply_remote` did with a batch.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplyReport {
    /// Events whose signature did not verify. Ignored without consuming the device's
    /// sequence number, so a forger cannot burn another device's numbers.
    pub unauthenticated: usize,
    /// Events from a device that is not enrolled here (only when signatures are required).
    pub unknown_device: usize,
    /// Events that changed a document.
    pub applied: usize,
    /// Valid events that changed nothing because a newer or equal state already existed.
    pub superseded: usize,
    /// Events sent to the dead-letter log.
    pub rejected: usize,
    /// Events already applied earlier (safe repeats).
    pub duplicates: usize,
    /// Events held back because an earlier event from the same device has not arrived.
    pub deferred: usize,
    /// Events too malformed to even identify; ignored.
    pub malformed: usize,
    /// Events whose clock stamp was implausibly far in the future.
    pub clock_warnings: usize,
    /// Set when processing stopped early, e.g. an event for a collection this node does
    /// not know. Events before it were applied; none after it were.
    pub blocked: Option<String>,
}

impl ApplyReport {
    /// Did this batch move the node forward?
    pub fn progressed(&self) -> bool {
        self.applied + self.superseded + self.rejected > 0
    }
}

type Rules = Arc<BTreeMap<String, Box<dyn Rule>>>;

const SYSTEM: [&str; 4] = ["_meta", "_events", "_vv", "_deadletter"];

fn tomb(collection: &str) -> String {
    format!("_t_{collection}")
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct SyncNode {
    engine: Engine,
    device: String,
    role: Role,
    rules: Rules,
    clock: Mutex<Hlc>,
    time: Arc<dyn Clock>,
    max_drift_ms: u64,
    identity: Identity,
    trust: Mutex<BTreeMap<String, Trusted>>,
    require_sigs: std::sync::atomic::AtomicBool,
}

impl SyncNode {
    /// Create a new node database with a fresh device identity. `rules` lists every
    /// synchronised collection and its conflict rule; all nodes of a deployment must use
    /// the same list.
    pub fn create(path: impl AsRef<Path>, role: Role, rules: Vec<(String, Box<dyn Rule>)>, time: Arc<dyn Clock>) -> Result<SyncNode> {
        let engine = Engine::create(path)?;
        let device = new_device_id();
        let mut tx = engine.begin();
        for name in SYSTEM {
            tx.create_collection(name)?;
        }
        tx.create_index("_events", "pos", true)?;
        tx.create_index("_events", "event_id", true)?;
        let mut names = Map::new();
        for (name, rule) in &rules {
            if name.is_empty() || name.len() > 50 || name.starts_with('_') {
                return Err(SyncError::InvalidDocument(format!("bad collection name '{name}'")));
            }
            tx.create_collection(name)?;
            tx.create_collection(&tomb(name))?;
            names.insert(name.clone(), json!(rule.name()));
        }
        tx.insert("_meta", json!({"_id": "node", "device": device, "role": role, "rules": names}))?;
        tx.insert("_meta", json!({"_id": "clock", "wall": 0, "counter": 0}))?;
        let identity = Identity::generate();
        tx.insert("_meta", json!({"_id": "identity", "secret": identity.secret_hex(), "public": identity.public_hex()}))?;
        tx.insert("_meta", json!({"_id": "trust", "devices": {device.clone(): {"public": identity.public_hex(), "role": role}}}))?;
        tx.commit()?;
        SyncNode::finish(engine, device, role, rules, time)
    }

    /// Open an existing node. The rules must match the ones it was created with.
    pub fn open(path: impl AsRef<Path>, rules: Vec<(String, Box<dyn Rule>)>, time: Arc<dyn Clock>) -> Result<SyncNode> {
        let engine = Engine::open(path)?;
        // A database without the `_meta` collection is not one of ours.
        let meta = engine.get("_meta", "node").ok().flatten().ok_or(SyncError::NotASyncDatabase)?;
        let device = meta["device"].as_str().ok_or(SyncError::NotASyncDatabase)?.to_string();
        let role: Role = serde_json::from_value(meta["role"].clone()).map_err(|_| SyncError::NotASyncDatabase)?;
        let stored: BTreeMap<String, String> = meta["rules"]
            .as_object()
            .ok_or(SyncError::NotASyncDatabase)?
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect();
        let given: BTreeMap<String, String> = rules.iter().map(|(n, r)| (n.clone(), r.name().to_string())).collect();
        if stored != given {
            return Err(SyncError::RuleMismatch(format!("database has {stored:?}, caller supplied {given:?}")));
        }
        SyncNode::finish(engine, device, role, rules, time)
    }

    fn finish(engine: Engine, device: String, role: Role, rules: Vec<(String, Box<dyn Rule>)>, time: Arc<dyn Clock>) -> Result<SyncNode> {
        let c = engine.get("_meta", "clock")?.ok_or(SyncError::NotASyncDatabase)?;
        let persisted = Hlc { wall: c["wall"].as_u64().unwrap_or(0), counter: c["counter"].as_u64().unwrap_or(0) as u32 };
        // Never behind what this device already stamped, even if the OS clock was set back.
        let clock = persisted.max(Hlc { wall: time.now_ms(), counter: 0 });
        let ident = engine.get("_meta", "identity")?.ok_or(SyncError::NotASyncDatabase)?;
        let identity = ident["secret"].as_str().and_then(Identity::from_secret_hex).ok_or(SyncError::NotASyncDatabase)?;
        let trust_doc = engine.get("_meta", "trust")?.ok_or(SyncError::NotASyncDatabase)?;
        let mut trust = BTreeMap::new();
        for (id, v) in trust_doc["devices"].as_object().ok_or(SyncError::NotASyncDatabase)? {
            let role: Role = serde_json::from_value(v["role"].clone()).map_err(|_| SyncError::NotASyncDatabase)?;
            let public = v["public"].as_str().ok_or(SyncError::NotASyncDatabase)?.to_string();
            trust.insert(id.clone(), Trusted { public, role });
        }
        Ok(SyncNode {
            identity,
            trust: Mutex::new(trust),
            require_sigs: std::sync::atomic::AtomicBool::new(false),
            engine,
            device,
            role,
            rules: Arc::new(rules.into_iter().collect()),
            clock: Mutex::new(clock),
            time,
            max_drift_ms: DEFAULT_MAX_DRIFT_MS,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// This node's public key (hex), to hand to the other nodes' administrators.
    pub fn public_key(&self) -> String {
        self.identity.public_hex()
    }

    pub fn sign(&self, msg: &[u8]) -> String {
        self.identity.sign(msg)
    }

    /// Enrol a device: from now on its events are accepted (when signatures are required)
    /// only if signed by `public`, and only with `role`. Enrolling the same device twice
    /// with the same key is a no-op; changing its key or role is refused (re-keying will
    /// need an explicit, audited revoke step in a later phase).
    pub fn trust_device(&self, device: &str, public: &str, role: Role) -> Result<()> {
        if !valid_device_id(device) || !identity::valid_public(public) {
            return Err(SyncError::InvalidDocument("bad device id or public key".into()));
        }
        let mut guard = lock(&self.trust);
        match guard.get(device) {
            Some(t) if t.public == public && t.role == role => return Ok(()),
            Some(_) => return Err(SyncError::Rejected("device is already enrolled with a different key or role".into())),
            None => {}
        }
        let mut next = guard.clone();
        next.insert(device.to_string(), Trusted { public: public.to_string(), role });
        let devices: Map<String, Value> = next.iter().map(|(id, t)| (id.clone(), json!({"public": t.public, "role": t.role}))).collect();
        let mut tx = self.engine.begin();
        tx.upsert("_meta", json!({"_id": "trust", "devices": devices}))?;
        tx.commit().map_err(EngineError::from)?;
        *guard = next;
        Ok(())
    }

    pub fn trusted(&self, device: &str) -> Option<Trusted> {
        lock(&self.trust).get(device).cloned()
    }

    /// Require a valid signature from an enrolled device on every remote event. Off by
    /// default so in-process tests can build events by hand; the network layer refuses to
    /// run unless it is on.
    pub fn require_signatures(&self, on: bool) {
        self.require_sigs.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn signatures_required(&self) -> bool {
        self.require_sigs.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// If `device` is enrolled and `sig` is its valid signature over `msg`, its role.
    pub fn verify_from(&self, device: &str, msg: &[u8], sig: &str) -> Option<Role> {
        let t = self.trusted(device)?;
        identity::verify(&t.public, msg, sig).then_some(t.role)
    }

    /// The underlying engine, for read access (queries, backups, verification).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    // ------------------------------------------------------------------ local writes

    /// Create or replace a document. Returns the logged event, or `None` if the rule decided
    /// the write changes nothing (for example, re-adding an identical ledger entry).
    pub fn put(&self, collection: &str, doc: Value) -> Result<Option<Event>> {
        let id = check_doc(&doc)?.to_string();
        self.commit_local(collection, &id, Op::Upsert { doc })
    }

    pub fn delete(&self, collection: &str, doc_id: &str) -> Result<Option<Event>> {
        self.commit_local(collection, doc_id, Op::Delete)
    }

    pub fn get(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        self.rule(collection)?;
        Ok(self.engine.get(collection, id)?)
    }

    pub fn find(&self, collection: &str, query: &Value) -> Result<Vec<Value>> {
        self.rule(collection)?;
        Ok(self.engine.find(collection, query)?)
    }

    /// Add a local index. Indexes are derived data and are not replicated.
    pub fn create_index(&self, collection: &str, field: &str, unique: bool) -> Result<()> {
        self.rule(collection)?;
        let mut tx = self.engine.begin();
        tx.create_index(collection, field, unique)?;
        Ok(tx.commit()?)
    }

    fn rule(&self, collection: &str) -> Result<&dyn Rule> {
        self.rules.get(collection).map(|r| r.as_ref()).ok_or_else(|| SyncError::UnknownCollection(collection.to_string()))
    }

    fn commit_local(&self, collection: &str, doc_id: &str, op: Op) -> Result<Option<Event>> {
        let rule = self.rule(collection)?;
        let mut tx = self.engine.begin();
        let now = self.time.now_ms();
        let mut clk = *lock(&self.clock);
        let hlc = clk.tick(now);
        let seq = applied_seq(&mut tx, &self.device)? + 1;
        let mut ev = Event {
            event_id: new_uuid_v7(now),
            device: self.device.clone(),
            seq,
            hlc: hlc.encode(&self.device),
            role: self.role,
            collection: collection.to_string(),
            doc_id: doc_id.to_string(),
            op,
            sig: None,
        };
        rule.local_check(self.role, &ev).map_err(SyncError::Rejected)?;
        let cur = load_current(&mut tx, collection, doc_id)?;
        if matches!(ev.op, Op::Delete) && cur.doc.is_none() {
            return Ok(None);
        }
        match rule.decide(&cur, &ev) {
            Decision::Apply => {}
            Decision::Skip(_) => return Ok(None),
            Decision::Reject(why) => return Err(SyncError::Rejected(why)),
        }
        ev.sig = Some(self.identity.sign(&ev.signing_bytes()));
        apply_op(&mut tx, &ev)?;
        record_event(&mut tx, &ev)?;
        save_clock(&mut tx, &clk)?;
        tx.commit().map_err(EngineError::from)?;
        *lock(&self.clock) = clk;
        Ok(Some(ev))
    }

    // ------------------------------------------------------------------ replication

    /// For every known device, the highest sequence number applied here.
    pub fn version_vector(&self) -> Result<BTreeMap<String, u64>> {
        let mut r = self.engine.reader();
        let rows = r.find("_vv", &json!({}))?;
        Ok(rows.iter().filter_map(|d| Some((d["_id"].as_str()?.to_string(), d["applied"].as_u64()?))).collect())
    }

    /// Events this node holds that a peer with version vector `have` lacks, ordered by device
    /// then sequence number, at most `limit`.
    pub fn events_since(&self, have: &BTreeMap<String, u64>, limit: usize) -> Result<Vec<Event>> {
        let mut r = self.engine.reader();
        let devices: Vec<String> = r
            .find("_vv", &json!({}))?
            .iter()
            .filter_map(|d| d["_id"].as_str().map(String::from))
            .collect();
        let mut out = Vec::new();
        for dev in devices {
            let remaining = limit.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            let after = pos(&dev, have.get(&dev).copied().unwrap_or(0));
            let q = json!({"filter": {"pos": {"$gt": after, "$lt": format!("{dev}0")}}, "limit": remaining});
            for doc in r.find("_events", &q)? {
                out.push(serde_json::from_value::<Event>(doc).map_err(|e| SyncError::InvalidDocument(e.to_string()))?);
            }
        }
        Ok(out)
    }

    /// Apply events received from a peer, all in one transaction. Safe to call with
    /// duplicates, gaps, or in any order; see the module docs.
    pub fn apply_remote(&self, events: &[Event]) -> Result<ApplyReport> {
        let mut tx = self.engine.begin();
        let now = self.time.now_ms();
        let mut clk = *lock(&self.clock);
        let mut rep = ApplyReport::default();
        let require = self.signatures_required();
        let trust = lock(&self.trust).clone();
        for ev in events {
            let Some((stamp, stamp_dev)) = Hlc::decode(&ev.hlc) else {
                // We can still identify (device, seq) if those are sane: dead-letter it.
                if valid_device_id(&ev.device) && ev.seq >= 1 && self.advance_or_defer(&mut tx, ev, &mut rep, "unreadable clock stamp")? {}
                else if !valid_device_id(&ev.device) || ev.seq < 1 { rep.malformed += 1; }
                continue;
            };
            if !valid_device_id(&ev.device) || ev.seq < 1 || stamp_dev != ev.device || !valid_uuid_v7(&ev.event_id) {
                rep.malformed += 1;
                continue;
            }
            let applied = applied_seq(&mut tx, &ev.device)?;
            if ev.seq <= applied {
                rep.duplicates += 1;
                continue;
            }
            if ev.seq > applied + 1 {
                rep.deferred += 1;
                continue;
            }
            let mut auth_problem = None;
            if require {
                let Some(t) = trust.get(&ev.device) else {
                    rep.unknown_device += 1;
                    continue;
                };
                let signed = ev.sig.as_deref().map_or(false, |s| identity::verify(&t.public, &ev.signing_bytes(), s));
                if !signed {
                    // Not dead-lettered into the stream: that would let a forger consume
                    // the real device's sequence numbers.
                    rep.unauthenticated += 1;
                    continue;
                }
                if t.role != ev.role {
                    auth_problem = Some(format!("event claims role {:?} but the device is enrolled as {:?}", ev.role, t.role));
                }
            }
            let Some(rule) = self.rules.get(&ev.collection) else {
                rep.blocked = Some(format!("event {} is for unknown collection '{}'", ev.event_id, ev.collection));
                break;
            };
            match auth_problem.or_else(|| content_problem(ev)) {
                Some(why) => {
                    dead_letter(&mut tx, ev, &why)?;
                    rep.rejected += 1;
                }
                None => {
                    let cur = load_current(&mut tx, &ev.collection, &ev.doc_id)?;
                    match rule.decide(&cur, ev) {
                        Decision::Apply => {
                            apply_op(&mut tx, ev)?;
                            rep.applied += 1;
                        }
                        Decision::Skip(_) => rep.superseded += 1,
                        Decision::Reject(why) => {
                            dead_letter(&mut tx, ev, &why)?;
                            rep.rejected += 1;
                        }
                    }
                }
            }
            record_event(&mut tx, ev)?;
            if clk.observe(stamp, now, self.max_drift_ms) {
                rep.clock_warnings += 1;
            }
        }
        save_clock(&mut tx, &clk)?;
        tx.commit().map_err(EngineError::from)?;
        *lock(&self.clock) = clk;
        Ok(rep)
    }

    /// Handle an event whose content cannot be interpreted: if it is the next expected one
    /// from its device, dead-letter it and advance; otherwise count it as deferred/duplicate.
    fn advance_or_defer(&self, tx: &mut Tx<'_>, ev: &Event, rep: &mut ApplyReport, why: &str) -> Result<bool> {
        let applied = applied_seq(tx, &ev.device)?;
        if ev.seq <= applied {
            rep.duplicates += 1;
        } else if ev.seq > applied + 1 {
            rep.deferred += 1;
        } else {
            dead_letter(tx, ev, why)?;
            rep.rejected += 1;
            // Not logged as an event (it is unreadable), but the stream must move on.
            tx.upsert("_vv", json!({"_id": ev.device, "applied": ev.seq}))?;
        }
        Ok(true)
    }

    /// Events that were rejected, with the reason.
    pub fn dead_letters(&self) -> Result<Vec<Value>> {
        Ok(self.engine.find("_deadletter", &json!({}))?)
    }

    /// Verify a collection and the event log against their indexes.
    pub fn verify(&self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        for name in SYSTEM.iter().map(|s| s.to_string()).chain(self.rules.keys().cloned()) {
            problems.extend(self.engine.verify(&name)?.into_iter().map(|p| format!("{name}: {p}")));
        }
        Ok(problems)
    }
}

/// Pull everything each side lacks, in both directions, in batches of `batch` events,
/// until neither has anything new for the other. Returns events sent (a to b, b to a).
pub fn exchange(a: &SyncNode, b: &SyncNode, batch: usize) -> Result<(usize, usize)> {
    let (mut a_to_b, mut b_to_a) = (0, 0);
    loop {
        let mut moved = false;
        let events = a.events_since(&b.version_vector()?, batch)?;
        if !events.is_empty() {
            let rep = b.apply_remote(&events)?;
            a_to_b += events.len();
            moved |= rep.progressed();
        }
        let events = b.events_since(&a.version_vector()?, batch)?;
        if !events.is_empty() {
            let rep = a.apply_remote(&events)?;
            b_to_a += events.len();
            moved |= rep.progressed();
        }
        if !moved {
            return Ok((a_to_b, b_to_a));
        }
    }
}

// ---------------------------------------------------------------------- helpers

fn check_doc(doc: &Value) -> Result<&str> {
    let Value::Object(m) = doc else {
        return Err(SyncError::InvalidDocument("a document must be a JSON object".into()));
    };
    if let Some(k) = m.keys().find(|k| k.starts_with('_') && *k != "_id") {
        return Err(SyncError::InvalidDocument(format!("field '{k}' is reserved (names starting with '_')")));
    }
    if serde_json::to_vec(doc).map(|b| b.len()).unwrap_or(usize::MAX) > MAX_SYNC_DOC_BYTES {
        return Err(SyncError::InvalidDocument("document is too large".into()));
    }
    match m.get("_id") {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 100 && !s.contains('\0') => Ok(s),
        _ => Err(SyncError::InvalidDocument("_id must be a non-empty string (max 100 bytes)".into())),
    }
}

/// Is there anything wrong with the event's content, independent of conflict rules?
fn content_problem(ev: &Event) -> Option<String> {
    if ev.doc_id.is_empty() || ev.doc_id.len() > 100 || ev.doc_id.contains('\0') {
        return Some("bad document id".into());
    }
    if let Op::Upsert { doc } = &ev.op {
        match check_doc(doc) {
            Err(e) => return Some(e.to_string()),
            Ok(id) if id != ev.doc_id => return Some("document _id does not match the event".into()),
            Ok(_) => {}
        }
    }
    None
}

fn applied_seq(tx: &mut Tx<'_>, device: &str) -> Result<u64> {
    Ok(tx.get("_vv", device)?.and_then(|d| d["applied"].as_u64()).unwrap_or(0))
}

fn load_current(tx: &mut Tx<'_>, collection: &str, id: &str) -> Result<Current> {
    let doc = tx.get(collection, id)?;
    let doc_hlc = doc.as_ref().and_then(|d| d["_hlc"].as_str().map(String::from));
    let tombstone_hlc = tx.get(&tomb(collection), id)?.and_then(|t| t["hlc"].as_str().map(String::from));
    Ok(Current { doc, doc_hlc, tombstone_hlc })
}

fn apply_op(tx: &mut Tx<'_>, ev: &Event) -> Result<()> {
    match &ev.op {
        Op::Upsert { doc } => {
            let mut d = doc.clone();
            if let Value::Object(m) = &mut d {
                m.insert("_id".into(), json!(ev.doc_id));
                m.insert("_hlc".into(), json!(ev.hlc));
                m.insert("_origin".into(), json!(ev.device));
            }
            tx.upsert(&ev.collection, d)?;
            if tx.get(&tomb(&ev.collection), &ev.doc_id)?.is_some() {
                tx.delete(&tomb(&ev.collection), &ev.doc_id)?;
            }
        }
        Op::Delete => {
            if tx.get(&ev.collection, &ev.doc_id)?.is_some() {
                tx.delete(&ev.collection, &ev.doc_id)?;
            }
            tx.upsert(&tomb(&ev.collection), json!({"_id": ev.doc_id, "hlc": ev.hlc}))?;
        }
    }
    Ok(())
}

fn record_event(tx: &mut Tx<'_>, ev: &Event) -> Result<()> {
    let mut d = serde_json::to_value(ev).map_err(|e| SyncError::InvalidDocument(e.to_string()))?;
    if let Value::Object(m) = &mut d {
        m.insert("_id".into(), json!(ev.pos()));
        m.insert("pos".into(), json!(ev.pos()));
    }
    tx.insert("_events", d)?;
    tx.upsert("_vv", json!({"_id": ev.device, "applied": ev.seq}))?;
    Ok(())
}

fn dead_letter(tx: &mut Tx<'_>, ev: &Event, why: &str) -> Result<()> {
    let event = serde_json::to_value(ev).map_err(|e| SyncError::InvalidDocument(e.to_string()))?;
    tx.upsert("_deadletter", json!({"_id": ev.pos(), "reason": why, "event": event}))?;
    Ok(())
}

fn save_clock(tx: &mut Tx<'_>, clk: &Hlc) -> Result<()> {
    tx.upsert("_meta", json!({"_id": "clock", "wall": clk.wall, "counter": clk.counter}))?;
    Ok(())
}
