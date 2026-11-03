use std::collections::BTreeMap;
use std::sync::Arc;

use bo_domain::*;
use bo_sync::{exchange, new_uuid_v7, Event, Hlc, ManualClock, Op, Role, SyncNode, Write};
use serde_json::{json, Value};
use tempfile::TempDir;

struct World {
    _dir: TempDir,
    nodes: Vec<SyncNode>,
    clocks: Vec<Arc<ManualClock>>,
}

/// nodes[0] is the server.
fn world(devices: usize) -> World {
    let dir = TempDir::new().unwrap();
    let mut nodes = Vec::new();
    let mut clocks = Vec::new();
    for i in 0..=devices {
        let c = Arc::new(ManualClock::new(1_000_000));
        let role = if i == 0 { Role::Server } else { Role::Device };
        nodes.push(SyncNode::create(dir.path().join(format!("n{i}.db")), role, rules(), c.clone()).unwrap());
        clocks.push(c);
    }
    World { _dir: dir, nodes, clocks }
}

fn sync_all(w: &World) {
    for _ in 0..3 {
        for i in 1..w.nodes.len() {
            exchange(&w.nodes[0], &w.nodes[i], 50).unwrap();
        }
    }
}

fn draft() -> Value {
    json!({
        "currency": "GHS",
        "items": [
            {"sku": "burger", "name": "Oasis Burger", "qty": 2, "unit_price": 4500},
            {"sku": "coke", "name": "Coke 500ml", "qty": 1, "unit_price": 1000}
        ],
        "delivery_fee": 1500,
        "discount": 500
    })
}

fn add_recipes(n: &SyncNode) {
    n.put(RECIPES, json!({"_id": "burger", "version": 1, "components": [
        {"stock_sku": "bun", "qty": 1}, {"stock_sku": "patty", "qty": 1}, {"stock_sku": "cheese", "qty": 1}
    ]}))
    .unwrap();
}

fn level(n: &SyncNode, sku: &str) -> i64 {
    stock_levels(n).unwrap().get(sku).copied().unwrap_or(0)
}

fn dump(n: &SyncNode, c: &str) -> Vec<Value> {
    n.find(c, &json!({})).unwrap()
}

// ------------------------------------------------------------------ orders

#[test]
fn placing_computes_totals_and_rejects_bad_orders() {
    let w = world(1);
    let d = &w.nodes[1];
    let o = place_order(d, draft()).unwrap();
    assert_eq!(o["status"], "placed");
    assert_eq!(o["subtotal"], 10000);
    assert_eq!(o["total"], 11000); // 10000 + 1500 - 500
    assert!(o["_id"].as_str().unwrap().len() == 36);

    let bad = |f: &dyn Fn(&mut Value)| {
        let mut v = draft();
        f(&mut v);
        place_order(d, v).is_err()
    };
    assert!(bad(&|v| v["items"] = json!([])));
    assert!(bad(&|v| v["items"][0]["qty"] = json!(0)));
    assert!(bad(&|v| v["items"][0]["unit_price"] = json!(45.5)), "money must be integer minor units");
    assert!(bad(&|v| v["items"][0]["unit_price"] = json!(-1)));
    assert!(bad(&|v| v["currency"] = json!("cedi")));
    assert!(bad(&|v| v["discount"] = json!(999_999)), "discount larger than the order");
    assert!(bad(&|v| v["items"][0]["sku"] = json!("")));
    assert!(bad(&|v| v["notes"] = json!("x".repeat(501))));
    assert_eq!(dump(d, ORDERS).len(), 1);
}

#[test]
fn transitions_follow_the_state_machine_and_contents_are_fixed() {
    let w = world(1);
    let d = &w.nodes[1];
    let id = place_order(d, draft()).unwrap()["_id"].as_str().unwrap().to_string();
    assert!(set_status(d, &id, "completed").is_err(), "cannot skip straight to completed");
    assert!(set_status(d, &id, "bogus").is_err());
    for s in ["accepted", "preparing", "ready", "out_for_delivery", "completed"] {
        assert_eq!(set_status(d, &id, s).unwrap()["status"], s);
    }
    assert!(set_status(d, &id, "cancelled").is_err(), "completed is final");
    assert!(set_status(d, &id, "ready").is_err(), "no going back");

    // contents cannot change after placing
    let id2 = place_order(d, draft()).unwrap()["_id"].as_str().unwrap().to_string();
    let mut edited = bo_sync::strip_meta(&d.get(ORDERS, &id2).unwrap().unwrap());
    edited["status"] = json!("accepted");
    edited["items"][0]["qty"] = json!(5);
    assert!(d.put(ORDERS, edited).is_err());
    // nor can a new order start anywhere but 'placed', nor be deleted
    let mut fresh = draft();
    fresh["_id"] = json!("x1");
    fresh["status"] = json!("accepted");
    fresh["subtotal"] = json!(10000);
    fresh["total"] = json!(11000);
    assert!(d.put(ORDERS, fresh).is_err());
    assert!(d.delete(ORDERS, &id2).is_err());
    assert!(set_status(d, "missing", "accepted").is_err());
}

#[test]
fn accepting_deducts_stock_and_cancelling_reverses_it() {
    let w = world(1);
    let d = &w.nodes[1];
    add_recipes(d);
    for s in ["bun", "patty", "cheese", "coke"] {
        record_stock(d, s, 10, "restock", None).unwrap();
    }
    let id = place_order(d, draft()).unwrap()["_id"].as_str().unwrap().to_string();
    assert_eq!(level(d, "bun"), 10, "placing does not deduct");
    set_status(d, &id, "accepted").unwrap();
    assert_eq!((level(d, "bun"), level(d, "patty"), level(d, "cheese"), level(d, "coke")), (8, 8, 8, 9));
    assert_eq!(level(d, "burger"), 0, "an item with a recipe is not itself a stock item");
    set_status(d, &id, "preparing").unwrap();
    assert_eq!(level(d, "bun"), 8, "no second deduction");
    set_status(d, &id, "cancelled").unwrap();
    assert_eq!((level(d, "bun"), level(d, "coke")), (10, 10));
    // cancelling straight from placed touches no stock
    let id2 = place_order(d, draft()).unwrap()["_id"].as_str().unwrap().to_string();
    set_status(d, &id2, "cancelled").unwrap();
    assert_eq!(level(d, "bun"), 10);
    assert_eq!(reconcile(d).unwrap(), 0, "nothing to repair");
}

#[test]
fn offline_accept_versus_cancel_converges_and_reconcile_fixes_stock() {
    let w = world(2);
    let (s, a, b) = (&w.nodes[0], &w.nodes[1], &w.nodes[2]);
    add_recipes(s);
    for sku in ["bun", "patty", "cheese", "coke"] {
        record_stock(s, sku, 20, "restock", None).unwrap();
    }
    let id = place_order(s, draft()).unwrap()["_id"].as_str().unwrap().to_string();
    sync_all(&w);
    // both tills now offline: A accepts (deducts), B cancels (sees nothing to reverse)
    w.clocks[1].advance(10);
    w.clocks[2].advance(5);
    set_status(a, &id, "accepted").unwrap();
    set_status(b, &id, "cancelled").unwrap();
    sync_all(&w);
    for n in &w.nodes {
        assert_eq!(n.get(ORDERS, &id).unwrap().unwrap()["status"], "cancelled", "cancel outranks accepted everywhere");
        assert_eq!(level(n, "bun"), 18, "the race left a deduction with no reversal");
    }
    assert!(reconcile(s).unwrap() > 0);
    sync_all(&w);
    for n in &w.nodes {
        assert_eq!((level(n, "bun"), level(n, "coke")), (20, 20), "stock repaired everywhere");
        assert_eq!(reconcile(n).unwrap(), 0, "reconcile is idempotent on every node");
    }
}

#[test]
fn completed_beats_a_late_cancel_and_keeps_its_deduction() {
    let w = world(2);
    let (s, a, b) = (&w.nodes[0], &w.nodes[1], &w.nodes[2]);
    add_recipes(s);
    let id = place_order(s, draft()).unwrap()["_id"].as_str().unwrap().to_string();
    for st in ["accepted", "preparing", "ready"] {
        set_status(s, &id, st).unwrap();
    }
    sync_all(&w);
    w.clocks[1].advance(10);
    w.clocks[2].advance(9_000); // B's cancel has the later clock stamp
    set_status(a, &id, "completed").unwrap();
    set_status(b, &id, "cancelled").unwrap();
    sync_all(&w);
    reconcile(s).unwrap();
    sync_all(&w);
    for n in &w.nodes {
        assert_eq!(n.get(ORDERS, &id).unwrap().unwrap()["status"], "completed");
        assert_eq!(level(n, "bun"), -2, "a completed order's stock stays deducted");
    }
    // but B's cancel did reverse locally before it learned of the completion; that reversal
    // must not survive the converged state
    assert_eq!(oversold(s).unwrap().get("bun"), Some(&-2));
}

#[test]
fn forged_or_malformed_order_events_are_dead_lettered() {
    let w = world(1);
    let (s, d) = (&w.nodes[0], &w.nodes[1]);
    let id = place_order(d, draft()).unwrap()["_id"].as_str().unwrap().to_string();
    sync_all(&w);
    let mut doc = bo_sync::strip_meta(&d.get(ORDERS, &id).unwrap().unwrap());
    doc["status"] = json!("accepted");
    doc["total"] = json!(1); // wrong total
    let seq = d.version_vector().unwrap()[d.device_id()] + 1;
    let mk = |op: Op, seq: u64| Event {
        event_id: new_uuid_v7(1_000_000),
        device: d.device_id().into(),
        seq,
        hlc: Hlc { wall: 2_000_000, counter: seq as u32 }.encode(d.device_id()),
        role: Role::Device,
        collection: ORDERS.into(),
        doc_id: id.clone(),
        op,
        sig: None,
    };
    let rep = s.apply_remote(&[mk(Op::Upsert { doc }, seq), mk(Op::Delete, seq + 1)]).unwrap();
    assert_eq!(rep.rejected, 2);
    assert_eq!(s.get(ORDERS, &id).unwrap().unwrap()["status"], "placed");
    assert_eq!(s.dead_letters().unwrap().len(), 2);
}

// ------------------------------------------------------------------ inventory

#[test]
fn ledger_entries_are_validated_and_idempotent() {
    let w = world(1);
    let d = &w.nodes[1];
    assert!(record_stock(d, "bun", -5, "restock", None).is_err());
    assert!(record_stock(d, "bun", 5, "waste", None).is_err());
    assert!(record_stock(d, "bun", 0, "adjustment", None).is_err());
    assert!(record_stock(d, "bun", 5, "gift", None).is_err());
    assert!(record_stock(d, "bun", -1, "sale", None).is_err(), "sales only through orders");
    record_stock(d, "bun", 5, "restock", Some("delivery #12")).unwrap();
    record_stock(d, "bun", -7, "waste", None).unwrap();
    assert_eq!(oversold(d).unwrap().get("bun"), Some(&-2));
    record_stock(d, "bun", 2, "adjustment", Some("stocktake")).unwrap();
    assert!(oversold(d).unwrap().is_empty());
    // a hand-written duplicate of an existing entry changes nothing; a delete is refused
    let e = bo_sync::strip_meta(&dump(d, LEDGER)[0]);
    assert!(d.put(LEDGER, e.clone()).unwrap().is_none());
    assert!(d.delete(LEDGER, e["_id"].as_str().unwrap()).is_err());
    let mut bad = e;
    bad["_id"] = json!("hand:1");
    bad["delta"] = json!(1.5);
    assert!(d.put(LEDGER, bad).is_err());
}

#[test]
fn a_batch_is_all_or_nothing() {
    let w = world(1);
    let d = &w.nodes[1];
    let before = d.version_vector().unwrap();
    let r = d.commit(vec![
        Write::Put { collection: LEDGER.into(), doc: json!({"_id": "ok", "sku": "bun", "delta": 3, "reason": "restock"}) },
        Write::Put { collection: LEDGER.into(), doc: json!({"_id": "bad", "sku": "bun", "delta": 3, "reason": "waste"}) },
    ]);
    assert!(r.is_err());
    assert!(dump(d, LEDGER).is_empty(), "the valid first write was rolled back too");
    assert_eq!(d.version_vector().unwrap(), before, "no events were logged");
    assert!(d.events_since(&BTreeMap::new(), 10).unwrap().is_empty());
    // and the device still works and numbers its next event 1
    let ev = d.put(LEDGER, json!({"_id": "ok", "sku": "bun", "delta": 3, "reason": "restock"})).unwrap().unwrap();
    assert_eq!(ev.seq, 1);
}

// ------------------------------------------------------------------ randomized

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
fn randomized_offline_order_flow_converges_with_correct_stock() {
    for seed in [3u64, 11, 42, 2025] {
        let w = world(3);
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        add_recipes(&w.nodes[0]);
        for sku in ["bun", "patty", "cheese", "coke"] {
            record_stock(&w.nodes[0], sku, 50, "restock", None).unwrap();
        }
        sync_all(&w);
        let mut ids: Vec<String> = Vec::new();

        for _ in 0..300 {
            let i = rng.below(4) as usize;
            let n = &w.nodes[i];
            w.clocks[i].advance(rng.below(50));
            match rng.below(10) {
                0..=1 => ids.push(place_order(n, draft()).unwrap()["_id"].as_str().unwrap().to_string()),
                2..=5 if !ids.is_empty() => {
                    let id = &ids[rng.below(ids.len() as u64) as usize];
                    if let Some(o) = n.get(ORDERS, id).unwrap() {
                        let next = allowed_next(o["status"].as_str().unwrap());
                        if !next.is_empty() {
                            set_status(n, id, next[rng.below(next.len() as u64) as usize]).unwrap();
                        }
                    }
                }
                6 => {
                    record_stock(n, "bun", 1 + rng.below(5) as i64, "restock", None).unwrap();
                }
                _ => {
                    // partial, lossy, reordered delivery between two random nodes
                    let j = rng.below(4) as usize;
                    if i != j {
                        let mut evs = w.nodes[i].events_since(&w.nodes[j].version_vector().unwrap(), 1 + rng.below(10) as usize).unwrap();
                        evs.retain(|_| rng.below(5) != 0);
                        for k in (1..evs.len()).rev() {
                            evs.swap(k, rng.below(k as u64 + 1) as usize);
                        }
                        w.nodes[j].apply_remote(&evs).unwrap();
                    }
                }
            }
        }

        sync_all(&w);
        reconcile(&w.nodes[0]).unwrap();
        sync_all(&w);

        // every replica holds the same orders and ledger
        let (orders, ledger) = (dump(&w.nodes[0], ORDERS), dump(&w.nodes[0], LEDGER));
        for n in &w.nodes[1..] {
            assert_eq!(dump(n, ORDERS), orders, "seed {seed}");
            assert_eq!(dump(n, LEDGER), ledger, "seed {seed}");
        }

        // oracle: each order's status is the event with the greatest (rank, stamp)
        let mut best: BTreeMap<String, (u8, String, String)> = BTreeMap::new();
        for e in w.nodes[0].events_since(&BTreeMap::new(), 1_000_000).unwrap() {
            if e.collection != ORDERS {
                continue;
            }
            let Op::Upsert { doc } = &e.op else { continue };
            let st = doc["status"].as_str().unwrap().to_string();
            let key = (rank(&st).unwrap(), e.hlc.clone(), st);
            if best.get(&e.doc_id).map_or(true, |b| (b.0, &b.1) < (key.0, &key.1)) {
                best.insert(e.doc_id.clone(), key);
            }
        }
        assert_eq!(best.len(), orders.len(), "seed {seed}");
        for o in &orders {
            assert_eq!(o["status"].as_str().unwrap(), best[o["_id"].as_str().unwrap()].2, "seed {seed}");
        }

        // stock invariant per order: cancelled nets to zero, accepted-or-later (not
        // cancelled) nets to exactly the recipe usage, placed has no entries
        let usage: BTreeMap<&str, i64> = [("bun", 2), ("patty", 2), ("cheese", 2), ("coke", 1)].into();
        for o in &orders {
            let id = o["_id"].as_str().unwrap();
            let mut net: BTreeMap<String, i64> = BTreeMap::new();
            for e in ledger.iter().filter(|e| e["order_id"] == id) {
                *net.entry(e["sku"].as_str().unwrap().to_string()).or_insert(0) += e["delta"].as_i64().unwrap();
            }
            match o["status"].as_str().unwrap() {
                "placed" => assert!(net.is_empty(), "seed {seed}: placed order {id} touched stock"),
                "cancelled" => assert!(net.values().all(|v| *v == 0), "seed {seed}: cancelled order {id} nets {net:?}"),
                _ => {
                    for (sku, u) in &usage {
                        assert_eq!(net.get(*sku).copied().unwrap_or(0), -u, "seed {seed}: order {id} {sku}");
                    }
                }
            }
        }
        for n in &w.nodes {
            assert!(n.verify().unwrap().is_empty(), "seed {seed}");
            assert_eq!(reconcile(n).unwrap(), 0, "seed {seed}: reconcile idempotent after convergence");
        }
    }
}
