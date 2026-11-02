use std::collections::BTreeMap;
use std::sync::Arc;

use bo_sync::*;
use serde_json::{json, Value};
use tempfile::TempDir;

/// Order status only moves forward: a late "preparing" can never undo "ready".
struct OrderRule;
fn rank(s: &str) -> u8 {
    ["new", "preparing", "ready", "delivered"].iter().position(|x| *x == s).map(|p| p as u8 + 1).unwrap_or(0)
}
impl Rule for OrderRule {
    fn name(&self) -> &'static str {
        "order-status"
    }
    fn decide(&self, cur: &Current, ev: &Event) -> Decision {
        let Op::Upsert { doc } = &ev.op else { return Decision::Reject("orders cannot be deleted".into()) };
        let new = rank(doc["status"].as_str().unwrap_or(""));
        if new == 0 {
            return Decision::Reject("unknown status".into());
        }
        match &cur.doc {
            None => Decision::Apply,
            Some(c) => {
                let old = rank(c["status"].as_str().unwrap_or(""));
                if new > old || (new == old && Some(&ev.hlc) > cur.doc_hlc.as_ref()) {
                    Decision::Apply
                } else {
                    Decision::Skip("older status".into())
                }
            }
        }
    }
}

fn rules() -> Vec<(String, Box<dyn Rule>)> {
    vec![
        ("items".into(), Box::new(LastWriterWins)),
        ("ledger".into(), Box::new(AppendOnly)),
        ("payments".into(), Box::new(ServerAuthoritative)),
        ("menu".into(), Box::new(Versioned)),
        ("orders".into(), Box::new(OrderRule)),
    ]
}

struct Net {
    _dir: TempDir,
    nodes: Vec<SyncNode>,
    clocks: Vec<Arc<ManualClock>>,
}

fn node(dir: &TempDir, name: &str, role: Role, ms: u64) -> (SyncNode, Arc<ManualClock>) {
    let c = Arc::new(ManualClock::new(ms));
    (SyncNode::create(dir.path().join(name), role, rules(), c.clone()).unwrap(), c)
}

/// nodes[0] is the server; the rest are devices.
fn net(devices: usize) -> Net {
    let dir = TempDir::new().unwrap();
    let mut nodes = Vec::new();
    let mut clocks = Vec::new();
    for i in 0..=devices {
        let (n, c) = node(&dir, &format!("n{i}.db"), if i == 0 { Role::Server } else { Role::Device }, 1_000_000);
        nodes.push(n);
        clocks.push(c);
    }
    Net { _dir: dir, nodes, clocks }
}

fn sync_all(n: &Net) {
    for _ in 0..3 {
        for i in 1..n.nodes.len() {
            exchange(&n.nodes[0], &n.nodes[i], 7).unwrap();
        }
    }
}

fn dump(n: &SyncNode, coll: &str) -> Vec<Value> {
    n.find(coll, &json!({})).unwrap()
}

fn all_events(n: &SyncNode) -> Vec<String> {
    let mut v: Vec<String> = n.events_since(&BTreeMap::new(), 100_000).unwrap().iter().map(|e| e.event_id.clone()).collect();
    v.sort();
    v
}

// ------------------------------------------------------------------ hlc

#[test]
fn hlc_is_monotonic_and_strings_sort() {
    let mut h = Hlc::default();
    let a = h.tick(100);
    let b = h.tick(100);
    let c = h.tick(50); // clock set back
    assert!(a < b && b < c);
    let dev = new_device_id();
    assert!(a.encode(&dev) < b.encode(&dev) && b.encode(&dev) < c.encode(&dev));
    let enc = c.encode(&dev);
    let (d, id) = Hlc::decode(&enc).unwrap();
    assert_eq!((d, id), (c, dev.as_str()));
    assert!(Hlc::decode("garbage").is_none());
    // wall 0xff.. sorts after 0x0f.. as text
    assert!(Hlc { wall: 255, counter: 0 }.encode(&dev) > Hlc { wall: 15, counter: 9 }.encode(&dev));
}

#[test]
fn hlc_observe_clamps_far_future() {
    let mut h = Hlc::default();
    assert!(h.observe(Hlc { wall: 10_000_000_000, counter: 0 }, 1000, 5000));
    assert_eq!(h.wall, 6000);
    assert!(!h.observe(Hlc { wall: 2000, counter: 0 }, 1000, 5000));
    assert_eq!(h.wall, 6000);
}

#[test]
fn uuid_v7_shape() {
    let u = new_uuid_v7(1_700_000_000_000);
    assert!(valid_uuid_v7(&u));
    assert!(!valid_uuid_v7("00000000-0000-4000-8000-000000000000"));
    assert_ne!(u, new_uuid_v7(1_700_000_000_000));
}

// ------------------------------------------------------------------ basic

#[test]
fn local_write_creates_event_and_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.db");
    let c = Arc::new(ManualClock::new(5_000));
    let ev = {
        let n = SyncNode::create(&p, Role::Device, rules(), c.clone()).unwrap();
        let ev = n.put("items", json!({"_id": "burger", "price": 30})).unwrap().unwrap();
        assert_eq!(ev.seq, 1);
        assert_eq!(n.version_vector().unwrap()[n.device_id()], 1);
        c.set(1); // OS clock jumps back
        let ev2 = n.put("items", json!({"_id": "fries", "price": 10})).unwrap().unwrap();
        assert_eq!(ev2.seq, 2);
        assert!(ev2.hlc > ev.hlc, "stamps must not go backwards");
        ev
    };
    let n = SyncNode::open(&p, rules(), c.clone()).unwrap();
    assert_eq!(n.get("items", "burger").unwrap().unwrap()["price"], 30);
    let ev3 = n.put("items", json!({"_id": "shake", "price": 12})).unwrap().unwrap();
    assert_eq!(ev3.seq, 3);
    assert!(ev3.hlc > ev.hlc);
    assert!(n.verify().unwrap().is_empty());
}

#[test]
fn bad_documents_and_collections_are_refused() {
    let n = net(0);
    let s = &n.nodes[0];
    assert!(s.put("nope", json!({"_id": "x"})).is_err());
    assert!(s.put("items", json!({"price": 1})).is_err());
    assert!(s.put("items", json!({"_id": "x", "_hlc": "forged"})).is_err());
    assert!(s.put("items", json!([1])).is_err());
    assert!(s.put("items", json!({"_id": ""})).is_err());
    assert!(dump(s, "items").is_empty());
    assert!(s.events_since(&BTreeMap::new(), 10).unwrap().is_empty());
}

#[test]
fn open_refuses_different_rules_and_foreign_db() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.db");
    let c = Arc::new(ManualClock::new(1));
    SyncNode::create(&p, Role::Device, rules(), c.clone()).unwrap();
    let mut other = rules();
    other[0].1 = Box::new(Versioned);
    assert!(matches!(SyncNode::open(&p, other, c.clone()), Err(SyncError::RuleMismatch(_))));
    assert!(matches!(SyncNode::open(&p, rules()[..2].iter().map(|_| ("items".to_string(), Box::new(LastWriterWins) as Box<dyn Rule>)).collect(), c.clone()), Err(SyncError::RuleMismatch(_))));
    let plain = dir.path().join("plain.db");
    bo_engine::Engine::create(&plain).unwrap();
    assert!(matches!(SyncNode::open(&plain, rules(), c), Err(SyncError::NotASyncDatabase)));
}

#[test]
fn two_devices_sync_through_server() {
    let n = net(2);
    let (s, a, b) = (&n.nodes[0], &n.nodes[1], &n.nodes[2]);
    a.put("items", json!({"_id": "burger", "price": 30})).unwrap();
    b.put("items", json!({"_id": "fries", "price": 10})).unwrap();
    sync_all(&n);
    for x in [s, a, b] {
        assert_eq!(dump(x, "items").len(), 2);
    }
    assert_eq!(all_events(s), all_events(a));
    assert_eq!(all_events(s), all_events(b));
    assert_eq!(a.version_vector().unwrap(), b.version_vector().unwrap());
    // a repeat sync moves nothing
    assert_eq!(exchange(s, a, 10).unwrap(), (0, 0));
}

// ------------------------------------------------------------------ conflicts

#[test]
fn lww_later_stamp_wins_on_all_replicas() {
    let n = net(2);
    let (a, b) = (&n.nodes[1], &n.nodes[2]);
    n.clocks[1].set(2_000_000);
    n.clocks[2].set(3_000_000);
    a.put("items", json!({"_id": "burger", "price": 31})).unwrap();
    b.put("items", json!({"_id": "burger", "price": 35})).unwrap();
    sync_all(&n);
    for x in &n.nodes {
        assert_eq!(x.get("items", "burger").unwrap().unwrap()["price"], 35);
    }
}

#[test]
fn delete_is_not_resurrected_by_older_update() {
    let n = net(2);
    let (s, a, b) = (&n.nodes[0], &n.nodes[1], &n.nodes[2]);
    a.put("items", json!({"_id": "x", "v": 1})).unwrap();
    sync_all(&n);
    n.clocks[1].set(2_000_000);
    n.clocks[2].set(1_500_000);
    a.delete("items", "x").unwrap();
    b.put("items", json!({"_id": "x", "v": 2})).unwrap(); // older than the delete
    sync_all(&n);
    for x in [s, a, b] {
        assert!(x.get("items", "x").unwrap().is_none());
    }
    // a newer write after the delete brings it back everywhere
    n.clocks[2].set(4_000_000);
    b.put("items", json!({"_id": "x", "v": 3})).unwrap();
    sync_all(&n);
    for x in [s, a, b] {
        assert_eq!(x.get("items", "x").unwrap().unwrap()["v"], 3);
    }
    assert!(a.delete("items", "never-existed").unwrap().is_none());
}

#[test]
fn append_only_is_idempotent_and_dead_letters_forged_delete() {
    let n = net(2);
    let (s, a, b) = (&n.nodes[0], &n.nodes[1], &n.nodes[2]);
    assert!(a.put("ledger", json!({"_id": "e1", "qty": -2})).unwrap().is_some());
    assert!(a.put("ledger", json!({"_id": "e1", "qty": -2})).unwrap().is_none());
    assert!(matches!(a.delete("ledger", "e1"), Err(SyncError::Rejected(_)) | Ok(None)));
    // b forges a delete event by hand-crafting it via a peer-shaped event
    sync_all(&n);
    let forged = Event {
        event_id: new_uuid_v7(1_000_000),
        device: b.device_id().to_string(),
        seq: b.version_vector().unwrap().get(b.device_id()).copied().unwrap_or(0) + 1,
        hlc: Hlc { wall: 9_000_000, counter: 0 }.encode(b.device_id()),
        role: Role::Device,
        collection: "ledger".into(),
        doc_id: "e1".into(),
        op: Op::Delete,
    };
    let rep = s.apply_remote(&[forged]).unwrap();
    assert_eq!(rep.rejected, 1);
    assert_eq!(s.dead_letters().unwrap().len(), 1);
    assert!(s.get("ledger", "e1").unwrap().is_some());
    // dead letter still advanced the vector, and replicates
    sync_all(&n);
    assert!(a.get("ledger", "e1").unwrap().is_some());
    assert_eq!(a.version_vector().unwrap(), s.version_vector().unwrap());
}

#[test]
fn append_only_id_collision_converges_to_earliest() {
    let n = net(2);
    let (a, b) = (&n.nodes[1], &n.nodes[2]);
    n.clocks[1].set(3_000_000);
    n.clocks[2].set(2_000_000);
    a.put("ledger", json!({"_id": "e", "who": "a"})).unwrap();
    b.put("ledger", json!({"_id": "e", "who": "b"})).unwrap();
    sync_all(&n);
    for x in &n.nodes {
        assert_eq!(x.get("ledger", "e").unwrap().unwrap()["who"], "b");
    }
}

#[test]
fn server_authoritative_rejects_device_writes() {
    let n = net(1);
    let (s, a) = (&n.nodes[0], &n.nodes[1]);
    assert!(matches!(a.put("payments", json!({"_id": "p1", "status": "paid"})), Err(SyncError::Rejected(_))));
    s.put("payments", json!({"_id": "p1", "status": "paid"})).unwrap();
    // forged device event claiming a payment
    let forged = Event {
        event_id: new_uuid_v7(1_000_000),
        device: a.device_id().to_string(),
        seq: 1,
        hlc: Hlc { wall: 9_000_000, counter: 0 }.encode(a.device_id()),
        role: Role::Device,
        collection: "payments".into(),
        doc_id: "p2".into(),
        op: Op::Upsert { doc: json!({"_id": "p2", "status": "paid"}) },
    };
    let rep = s.apply_remote(&[forged]).unwrap();
    assert_eq!(rep.rejected, 1);
    assert!(s.get("payments", "p2").unwrap().is_none());
    sync_all(&n);
    assert_eq!(dump(a, "payments").len(), 1);
}

#[test]
fn versioned_highest_version_wins_regardless_of_order() {
    let n = net(2);
    let (a, b) = (&n.nodes[1], &n.nodes[2]);
    n.clocks[1].set(9_000_000); // v1 has the *later* stamp
    n.clocks[2].set(2_000_000);
    a.put("menu", json!({"_id": "m", "version": 1, "price": 30})).unwrap();
    b.put("menu", json!({"_id": "m", "version": 2, "price": 33})).unwrap();
    assert!(a.put("menu", json!({"_id": "z", "price": 1})).is_err());
    sync_all(&n);
    for x in &n.nodes {
        assert_eq!(x.get("menu", "m").unwrap().unwrap()["version"], 2);
    }
    assert!(a.delete("menu", "m").is_err());
}

#[test]
fn custom_order_rule_is_monotonic() {
    let n = net(2);
    let (a, b) = (&n.nodes[1], &n.nodes[2]);
    n.clocks[1].set(2_000_000);
    n.clocks[2].set(5_000_000); // stale "preparing" arrives with a later stamp
    a.put("orders", json!({"_id": "o1", "status": "ready"})).unwrap();
    b.put("orders", json!({"_id": "o1", "status": "preparing"})).unwrap();
    sync_all(&n);
    for x in &n.nodes {
        assert_eq!(x.get("orders", "o1").unwrap().unwrap()["status"], "ready");
    }
    assert!(a.put("orders", json!({"_id": "o1", "status": "bogus"})).is_err());
}

// ------------------------------------------------------------------ transport faults

#[test]
fn duplicates_gaps_and_shuffles_are_safe() {
    let n = net(1);
    let (s, a) = (&n.nodes[0], &n.nodes[1]);
    for i in 0..6 {
        a.put("items", json!({"_id": format!("i{i}"), "n": i})).unwrap();
    }
    let evs = a.events_since(&BTreeMap::new(), 100).unwrap();
    assert_eq!(evs.len(), 6);
    // gap: events 3..6 first
    let r = s.apply_remote(&evs[2..]).unwrap();
    assert_eq!((r.deferred, r.applied), (4, 0));
    assert!(dump(s, "items").is_empty());
    // shuffled + duplicated, delivered in pieces
    let r = s.apply_remote(&[evs[1].clone(), evs[0].clone()]).unwrap();
    assert_eq!(r.deferred, 1); // seq 2 before 1 in the same batch is deferred
    assert_eq!(r.applied, 1);
    let r = s.apply_remote(&evs).unwrap();
    assert_eq!(r.duplicates, 1);
    assert_eq!(r.applied, 5);
    let r = s.apply_remote(&evs).unwrap();
    assert_eq!(r.duplicates, 6);
    assert_eq!(dump(s, "items").len(), 6);
    assert_eq!(s.version_vector().unwrap()[a.device_id()], 6);
    assert!(s.verify().unwrap().is_empty());
}

#[test]
fn unknown_collection_blocks_without_losing_earlier_events() {
    let n = net(1);
    let (s, a) = (&n.nodes[0], &n.nodes[1]);
    a.put("items", json!({"_id": "ok"})).unwrap();
    a.put("items", json!({"_id": "later"})).unwrap();
    let mut evs = a.events_since(&BTreeMap::new(), 10).unwrap();
    evs[1].collection = "mystery".into();
    let r = s.apply_remote(&evs).unwrap();
    assert_eq!(r.applied, 1);
    assert!(r.blocked.is_some());
    assert_eq!(s.version_vector().unwrap()[a.device_id()], 1);
    // once the real event arrives, it flows
    let good = a.events_since(&s.version_vector().unwrap(), 10).unwrap();
    assert_eq!(s.apply_remote(&good).unwrap().applied, 1);
}

#[test]
fn malformed_events_are_counted_not_applied() {
    let n = net(1);
    let (s, a) = (&n.nodes[0], &n.nodes[1]);
    a.put("items", json!({"_id": "ok"})).unwrap();
    let good = a.events_since(&BTreeMap::new(), 10).unwrap().remove(0);
    let mut bad_dev = good.clone();
    bad_dev.device = "short".into();
    let mut bad_id = good.clone();
    bad_id.event_id = "nope".into();
    let mut mismatched = good.clone();
    mismatched.hlc = Hlc { wall: 1, counter: 0 }.encode(&new_device_id());
    let r = s.apply_remote(&[bad_dev, bad_id, mismatched]).unwrap();
    assert_eq!(r.malformed, 3);
    assert_eq!(s.version_vector().unwrap().len(), 0);

    // doc id mismatch is dead-lettered and advances
    let mut liar = good.clone();
    liar.op = Op::Upsert { doc: json!({"_id": "different"}) };
    let r = s.apply_remote(&[liar]).unwrap();
    assert_eq!(r.rejected, 1);
    assert_eq!(s.version_vector().unwrap()[a.device_id()], 1);
    assert!(dump(s, "items").is_empty());
}

#[test]
fn far_future_stamp_does_not_drag_local_clock() {
    let n = net(1);
    let (s, a) = (&n.nodes[0], &n.nodes[1]);
    n.clocks[1].set(10_000_000_000_000);
    a.put("items", json!({"_id": "x"})).unwrap();
    let r = s.apply_remote(&a.events_since(&BTreeMap::new(), 10).unwrap()).unwrap();
    assert_eq!(r.clock_warnings, 1);
    let ev = s.put("items", json!({"_id": "y"})).unwrap().unwrap();
    let (h, _) = Hlc::decode(&ev.hlc).unwrap();
    assert!(h.wall < 2_000_000, "server clock must not jump to the far future");
}

// ------------------------------------------------------------------ crash + convergence

#[test]
fn reopen_mid_sync_preserves_consistency() {
    let dir = TempDir::new().unwrap();
    let c = Arc::new(ManualClock::new(1_000_000));
    let ps = dir.path().join("s.db");
    let pa = dir.path().join("a.db");
    let s = SyncNode::create(&ps, Role::Server, rules(), c.clone()).unwrap();
    let a = SyncNode::create(&pa, Role::Device, rules(), c.clone()).unwrap();
    for i in 0..10 {
        a.put("items", json!({"_id": format!("i{i}")})).unwrap();
    }
    let evs = a.events_since(&BTreeMap::new(), 4).unwrap();
    s.apply_remote(&evs).unwrap();
    drop(s);
    let s = SyncNode::open(&ps, rules(), c.clone()).unwrap();
    exchange(&s, &a, 3).unwrap();
    assert_eq!(dump(&s, "items").len(), 10);
    assert_eq!(s.version_vector().unwrap(), a.version_vector().unwrap());
    assert!(s.verify().unwrap().is_empty());
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn randomized_convergence_with_lossy_transport() {
    for seed in [1u64, 7, 99, 2024] {
        let dir = TempDir::new().unwrap();
        let clocks: Vec<Arc<ManualClock>> = (0..4).map(|_| Arc::new(ManualClock::new(1_000_000))).collect();
        let paths: Vec<_> = (0..4).map(|i| dir.path().join(format!("n{i}.db"))).collect();
        let mut nodes: Vec<SyncNode> = (0..4)
            .map(|i| SyncNode::create(&paths[i], if i == 0 { Role::Server } else { Role::Device }, rules(), clocks[i].clone()).unwrap())
            .collect();
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        // oracle: every LWW event ever created
        let mut oracle: Vec<Event> = Vec::new();
        let mut cnt = 0u64;

        for _ in 0..250 {
            let i = rng.below(4) as usize;
            match rng.below(10) {
                0..=4 => {
                    let n = &nodes[i];
                    clocks[i].advance(rng.below(40));
                    if rng.below(8) == 0 {
                        // a skewed clock
                        clocks[i].set(1_000_000 + rng.below(100_000));
                    }
                    cnt += 1;
                    let id = format!("k{}", rng.below(6));
                    let ev = if rng.below(4) == 0 {
                        n.delete("items", &id).unwrap()
                    } else {
                        n.put("items", json!({"_id": id, "n": cnt})).unwrap()
                    };
                    if let Some(e) = ev {
                        oracle.push(e);
                    }
                    if i != 0 {
                        let _ = n.put("ledger", json!({"_id": format!("l{cnt}"), "d": i})).unwrap();
                    }
                }
                5..=8 => {
                    // lossy transfer between two random nodes, either direction
                    let j = rng.below(4) as usize;
                    if i == j {
                        continue;
                    }
                    let mut evs = nodes[i].events_since(&nodes[j].version_vector().unwrap(), 1 + rng.below(8) as usize).unwrap();
                    let mut out = Vec::new();
                    for e in evs.drain(..) {
                        match rng.below(6) {
                            0 => {}
                            1 => {
                                out.push(e.clone());
                                out.push(e);
                            }
                            _ => out.push(e),
                        }
                    }
                    for k in (1..out.len()).rev() {
                        out.swap(k, rng.below(k as u64 + 1) as usize);
                    }
                    nodes[j].apply_remote(&out).unwrap();
                }
                _ => {
                    // crash and reopen
                    drop(nodes.remove(i));
                    nodes.insert(i, SyncNode::open(&paths[i], rules(), clocks[i].clone()).unwrap());
                }
            }
        }

        // final: everyone syncs with the server until quiet
        for _ in 0..4 {
            for i in 1..4 {
                exchange(&nodes[0], &nodes[i], 5).unwrap();
            }
        }
        for n in &nodes {
            assert!(n.verify().unwrap().is_empty(), "seed {seed}");
        }
        let base_items = dump(&nodes[0], "items");
        let base_ledger = dump(&nodes[0], "ledger");
        for n in &nodes[1..] {
            assert_eq!(dump(n, "items"), base_items, "seed {seed}");
            assert_eq!(dump(n, "ledger"), base_ledger, "seed {seed}");
            assert_eq!(n.version_vector().unwrap(), nodes[0].version_vector().unwrap(), "seed {seed}");
            assert_eq!(all_events(n), all_events(&nodes[0]), "seed {seed}");
        }

        // oracle: per id, the event with the greatest stamp decides
        let mut expect: BTreeMap<String, Option<u64>> = BTreeMap::new();
        let mut best: BTreeMap<String, String> = BTreeMap::new();
        for e in &oracle {
            if best.get(&e.doc_id).map_or(true, |b| e.hlc > *b) {
                best.insert(e.doc_id.clone(), e.hlc.clone());
                expect.insert(
                    e.doc_id.clone(),
                    match &e.op {
                        Op::Upsert { doc } => doc["n"].as_u64(),
                        Op::Delete => None,
                    },
                );
            }
        }
        for (id, want) in expect {
            let got = nodes[0].get("items", &id).unwrap().map(|d| d["n"].as_u64().unwrap());
            assert_eq!(got, want, "seed {seed} id {id}");
        }
    }
}
