use std::sync::Arc;

use bo_domain::*;
use bo_sync::{exchange, new_uuid_v7, Event, Hlc, ManualClock, Op, Role, SyncNode};
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

/// An order with total 10000 (pesewas).
fn order(n: &SyncNode) -> String {
    let o = place_order(n, json!({"currency": "GHS", "items": [{"sku": "burger", "name": "Burger", "qty": 2, "unit_price": 5000}]})).unwrap();
    o["_id"].as_str().unwrap().to_string()
}

fn problems(n: &SyncNode, id: &str) -> Vec<String> {
    payment_anomalies(n).unwrap().into_iter().filter(|a| a.order_id == id).map(|a| a.problem).collect()
}

#[test]
fn cash_payments_add_up_to_a_status() {
    let w = world(1);
    let d = &w.nodes[1];
    let id = order(d);
    assert_eq!(payment_summary(d, &id).unwrap().status, "unpaid");
    assert_eq!(record_cash(d, &id, 4000, None).unwrap().status, "partially_paid");
    let s = record_cash(d, &id, 7000, Some("customer paid 110")).unwrap();
    assert_eq!((s.status, s.paid, s.net), ("overpaid", 11000, 11000));
    let s = record_refund(d, &id, 1000, Some("change")).unwrap();
    assert_eq!((s.status, s.refunded, s.net), ("paid", 1000, 10000));
    let s = record_refund(d, &id, 10000, None).unwrap();
    assert_eq!(s.status, "refunded");
    assert!(record_refund(d, &id, 1, None).is_err(), "nothing left to refund");
}

#[test]
fn bad_amounts_and_unknown_orders_are_refused() {
    let w = world(1);
    let d = &w.nodes[1];
    let id = order(d);
    assert!(record_cash(d, &id, 0, None).is_err());
    assert!(record_cash(d, &id, -5, None).is_err());
    assert!(record_cash(d, "nope", 100, None).is_err());
    assert!(record_refund(d, &id, 1, None).is_err(), "cannot refund before anything is paid");
    set_status(d, &id, "cancelled").unwrap();
    assert!(record_cash(d, &id, 100, None).is_err(), "no payment for a cancelled order");
    // hand-written entries are validated too
    let bad = |doc: Value| d.put(PAYMENTS, doc).is_err();
    assert!(bad(json!({"_id": "x1", "order_id": id, "kind": "payment", "method": "cash", "amount": 12.5, "currency": "GHS"})));
    assert!(bad(json!({"_id": "x2", "order_id": id, "kind": "tip", "method": "cash", "amount": 100, "currency": "GHS"})));
    assert!(bad(json!({"_id": "x3", "order_id": id, "kind": "payment", "method": "cheque", "amount": 100, "currency": "GHS"})));
    assert!(bad(json!({"_id": "x4", "order_id": id, "kind": "payment", "method": "cash", "amount": 100, "currency": "cedi"})));
    assert!(payment_summary(d, &id).unwrap().paid == 0);
}

#[test]
fn only_the_server_records_card_and_mobile_money() {
    let w = world(1);
    let (s, d) = (&w.nodes[0], &w.nodes[1]);
    let id = order(d);
    sync_all(&w);
    // a device can neither use the helper nor write the entry directly
    assert!(record_online(d, &id, "payment", "card", "paystack", "ref1", 10000, "GHS").is_err());
    let forged = json!({"_id": "paystack:ref1:payment", "order_id": id, "kind": "payment", "method": "card",
        "provider": "paystack", "reference": "ref1", "amount": 10000, "currency": "GHS"});
    assert!(d.put(PAYMENTS, forged.clone()).is_err());
    // nor smuggle one in as a remote event
    let seq = d.version_vector().unwrap()[d.device_id()] + 1;
    let ev = Event {
        event_id: new_uuid_v7(1_000_000),
        device: d.device_id().into(),
        seq,
        hlc: Hlc { wall: 2_000_000, counter: 0 }.encode(d.device_id()),
        role: Role::Device,
        collection: PAYMENTS.into(),
        doc_id: "paystack:ref1:payment".into(),
        op: Op::Upsert { doc: forged },
        sig: None,
    };
    assert_eq!(s.apply_remote(&[ev]).unwrap().rejected, 1);
    assert_eq!(payment_summary(s, &id).unwrap().paid, 0);

    // the server can, and a repeated provider notification records once
    assert_eq!(record_online(s, &id, "payment", "card", "paystack", "ref1", 10000, "GHS").unwrap().status, "paid");
    assert_eq!(record_online(s, &id, "payment", "card", "paystack", "ref1", 10000, "GHS").unwrap().paid, 10000);
    assert!(record_online(s, &id, "payment", "card", "paystack", "ref2", 100, "NGN").is_err(), "currency must match the order");
    assert!(record_online(s, &id, "payment", "cash", "paystack", "ref3", 100, "GHS").is_err());
    sync_all(&w);
    assert_eq!(payment_summary(d, &id).unwrap().status, "paid");
    // and an online refund through the provider
    assert_eq!(record_online(s, &id, "refund", "card", "paystack", "ref1", 10000, "GHS").unwrap().status, "refunded");
}

#[test]
fn offline_tills_double_refunding_is_reported_not_hidden() {
    let w = world(2);
    let (s, a, b) = (&w.nodes[0], &w.nodes[1], &w.nodes[2]);
    let id = order(s);
    record_cash(s, &id, 10000, None).unwrap();
    sync_all(&w);
    // both tills offline, each refunds the full amount: each is valid on its own view
    w.clocks[1].advance(5);
    w.clocks[2].advance(7);
    record_refund(a, &id, 10000, None).unwrap();
    record_refund(b, &id, 10000, None).unwrap();
    sync_all(&w);
    for n in &w.nodes {
        let sm = payment_summary(n, &id).unwrap();
        assert_eq!((sm.paid, sm.refunded, sm.net), (10000, 20000, -10000), "both refunds kept on every node");
        assert!(problems(n, &id).iter().any(|p| p.contains("refunded 10000 more than was paid")));
    }
}

#[test]
fn anomalies_flag_orders_that_need_a_person() {
    let w = world(1);
    let d = &w.nodes[1];
    let unpaid = order(d);
    for st in ["accepted", "preparing", "ready", "completed"] {
        set_status(d, &unpaid, st).unwrap();
    }
    assert!(problems(d, &unpaid).iter().any(|p| p.contains("completed but only 0 of 10000 paid")));

    let kept = order(d);
    record_cash(d, &kept, 10000, None).unwrap();
    set_status(d, &kept, "cancelled").unwrap();
    assert!(problems(d, &kept).iter().any(|p| p.contains("cancelled but still holding 10000")));
    record_refund(d, &kept, 10000, None).unwrap();
    assert!(problems(d, &kept).is_empty(), "refunding resolves it");

    let fine = order(d);
    record_cash(d, &fine, 10000, None).unwrap();
    for st in ["accepted", "preparing", "ready", "completed"] {
        set_status(d, &fine, st).unwrap();
    }
    assert!(problems(d, &fine).is_empty());
    // the audit never writes anything
    let before = d.version_vector().unwrap();
    payment_anomalies(d).unwrap();
    assert_eq!(d.version_vector().unwrap(), before);
}

#[test]
fn payments_converge_across_offline_tills() {
    let w = world(3);
    let id = order(&w.nodes[0]);
    sync_all(&w);
    let mut expected = 0;
    for round in 0..5u64 {
        for i in 1..4 {
            w.clocks[i].advance(3 + round);
            let amt = 100 * (i as i64 + 1) + round as i64;
            record_cash(&w.nodes[i], &id, amt, None).unwrap();
            expected += amt;
        }
        if round % 2 == 1 {
            sync_all(&w);
        }
    }
    sync_all(&w);
    let first = w.nodes[0].find(PAYMENTS, &json!({})).unwrap();
    assert_eq!(first.len(), 15);
    for n in &w.nodes {
        assert_eq!(n.find(PAYMENTS, &json!({})).unwrap(), first);
        assert_eq!(payment_summary(n, &id).unwrap().paid, expected);
    }
}
