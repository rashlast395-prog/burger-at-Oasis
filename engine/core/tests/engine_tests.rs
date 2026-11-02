use std::collections::BTreeMap;
use std::path::PathBuf;

use bo_engine::{Engine, EngineError, MAX_DOC_BYTES};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

fn new_engine() -> (TempDir, PathBuf, Engine) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("e.boasis");
    let e = Engine::create(&path).unwrap();
    (dir, path, e)
}

fn ids(docs: &[Value]) -> Vec<String> {
    docs.iter().map(|d| d["_id"].as_str().unwrap().to_string()).collect()
}

fn setup_orders(e: &mut Engine) {
    let mut t = e.begin();
    t.create_collection("orders").unwrap();
    let rows = [
        ("o1", "open", 10.0, "WEB", "0201"),
        ("o2", "open", 25.5, "POS", "0202"),
        ("o3", "done", 40.0, "WEB", "0203"),
        ("o4", "done", 7.25, "PHONE", "0204"),
        ("o5", "cancelled", 99.0, "WEB", "0205"),
    ];
    for (id, status, total, channel, phone) in rows {
        t.insert(
            "orders",
            json!({"_id": id, "status": status, "total": total, "channel": channel, "customer": {"phone": phone}}),
        )
        .unwrap();
    }
    t.commit().unwrap();
}

#[test]
fn insert_get_update_delete_and_duplicate_ids() {
    let (_d, _p, mut e) = new_engine();
    let mut t = e.begin();
    t.create_collection("items").unwrap();
    t.insert("items", json!({"_id": "a", "n": 1})).unwrap();
    assert!(matches!(t.insert("items", json!({"_id": "a", "n": 2})), Err(EngineError::DuplicateId(_))));
    t.update("items", "a", &json!({"n": 5, "extra": true})).unwrap();
    assert_eq!(t.get("items", "a").unwrap().unwrap(), json!({"_id": "a", "n": 5, "extra": true}));
    assert!(matches!(t.update("items", "a", &json!({"_id": "b"})), Err(EngineError::InvalidDocument(_))));
    assert!(matches!(t.update("items", "zz", &json!({"n": 1})), Err(EngineError::NotFound(_))));
    t.upsert("items", json!({"_id": "a", "n": 9})).unwrap();
    assert_eq!(t.get("items", "a").unwrap().unwrap(), json!({"_id": "a", "n": 9}));
    t.delete("items", "a").unwrap();
    assert_eq!(t.get("items", "a").unwrap(), None);
    assert!(matches!(t.delete("items", "a"), Err(EngineError::NotFound(_))));
    t.commit().unwrap();
    assert_eq!(e.collections().unwrap(), vec!["items".to_string()]);
}

#[test]
fn invalid_input_is_rejected() {
    let (_d, _p, mut e) = new_engine();
    let mut t = e.begin();
    for bad in ["", "Bad", "has space", &"x".repeat(65)] {
        assert!(matches!(t.create_collection(bad), Err(EngineError::InvalidName(_))), "{bad:?}");
    }
    t.create_collection("c").unwrap();
    assert!(matches!(t.create_collection("c"), Err(EngineError::CollectionExists(_))));
    assert!(matches!(t.insert("nope", json!({"_id": "a"})), Err(EngineError::NoSuchCollection(_))));
    assert!(matches!(t.insert("c", json!({"x": 1})), Err(EngineError::InvalidDocument(_))));
    assert!(matches!(t.insert("c", json!({"_id": 5})), Err(EngineError::InvalidDocument(_))));
    assert!(matches!(t.insert("c", json!([1, 2])), Err(EngineError::InvalidDocument(_))));
    let big = json!({"_id": "big", "blob": "x".repeat(MAX_DOC_BYTES)});
    assert!(matches!(t.insert("c", big), Err(EngineError::DocumentTooLarge { .. })));
    assert!(matches!(t.create_index("c", "_id", false), Err(EngineError::InvalidName(_))));
    assert!(matches!(t.create_index("c", "a..b", false), Err(EngineError::InvalidName(_))));
}

#[test]
fn filters_ranges_logic_paths_sort_skip_limit() {
    let (_d, _p, mut e) = new_engine();
    setup_orders(&mut e);
    let q = |e: &mut Engine, v: Value| ids(&e.find("orders", &v).unwrap());

    assert_eq!(q(&mut e, json!({"filter": {"status": "open"}})), ["o1", "o2"]);
    assert_eq!(q(&mut e, json!({"filter": {"total": {"$gte": 10, "$lt": 40}}})), ["o1", "o2"]);
    assert_eq!(q(&mut e, json!({"filter": {"total": {"$gt": 40}}})), ["o5"]);
    assert_eq!(q(&mut e, json!({"filter": {"channel": {"$in": ["POS", "PHONE"]}}})), ["o2", "o4"]);
    assert_eq!(q(&mut e, json!({"filter": {"channel": {"$nin": ["WEB"]}}})), ["o2", "o4"]);
    assert_eq!(q(&mut e, json!({"filter": {"status": {"$ne": "open"}}})), ["o3", "o4", "o5"]);
    assert_eq!(q(&mut e, json!({"filter": {"$or": [{"status": "cancelled"}, {"total": {"$lt": 8}}]}})), ["o4", "o5"]);
    assert_eq!(q(&mut e, json!({"filter": {"$and": [{"channel": "WEB"}, {"status": {"$ne": "done"}}]}})), ["o1", "o5"]);
    assert_eq!(q(&mut e, json!({"filter": {"$not": {"channel": "WEB"}}})), ["o2", "o4"]);
    assert_eq!(q(&mut e, json!({"filter": {"customer.phone": "0203"}})), ["o3"]);
    assert_eq!(q(&mut e, json!({"filter": {"missing": {"$exists": false}, "status": "open"}})), ["o1", "o2"]);
    assert_eq!(q(&mut e, json!({"filter": {"missing": {"$exists": true}}})), Vec::<String>::new());
    // range operators never match a different type
    assert_eq!(q(&mut e, json!({"filter": {"status": {"$gt": 5}}})), Vec::<String>::new());
    assert_eq!(q(&mut e, json!({})), ["o1", "o2", "o3", "o4", "o5"]);

    assert_eq!(q(&mut e, json!({"sort": {"field": "total", "dir": "desc"}})), ["o5", "o3", "o2", "o1", "o4"]);
    assert_eq!(q(&mut e, json!({"sort": {"field": "total"}, "skip": 1, "limit": 2})), ["o1", "o2"]);
    assert_eq!(q(&mut e, json!({"filter": {"channel": "WEB"}, "sort": {"field": "total", "dir": "desc"}, "limit": 1})), ["o5"]);
}

#[test]
fn malformed_queries_are_rejected_even_on_empty_collections() {
    let (_d, _p, mut e) = new_engine();
    let mut t = e.begin();
    t.create_collection("c").unwrap();
    for q in [
        json!([]),
        json!({"nope": 1}),
        json!({"filter": []}),
        json!({"filter": {"$xor": []}}),
        json!({"filter": {"$and": []}}),
        json!({"filter": {"$or": {"a": 1}}}),
        json!({"filter": {"a": {"$gt": 1, "plain": 2}}}),
        json!({"filter": {"a": {"$in": 5}}}),
        json!({"filter": {"a": {"$exists": "yes"}}}),
        json!({"filter": {"a": {"$wat": 1}}}),
        json!({"sort": "a"}),
        json!({"sort": {"field": "a", "dir": "sideways"}}),
        json!({"limit": -1}),
        json!({"skip": "x"}),
    ] {
        assert!(matches!(t.find("c", &q), Err(EngineError::InvalidQuery(_))), "{q}");
    }
}

#[test]
fn indexes_are_used_when_they_can_be_and_never_change_results() {
    let (_d, _p, mut e) = new_engine();
    setup_orders(&mut e);
    let queries = [
        json!({"filter": {"status": "open"}}),
        json!({"filter": {"total": {"$gte": 10, "$lt": 40}}}),
        json!({"filter": {"total": {"$gt": 25.5}}}),
        json!({"filter": {"total": {"$lte": 25.5}}}),
        json!({"filter": {"channel": "WEB", "status": {"$ne": "done"}}}),
        json!({"filter": {"customer.phone": "0204"}}),
        json!({"filter": {"status": "nope"}}),
        json!({"filter": {"total": 40}}),
    ];
    let before: Vec<_> = queries.iter().map(|q| ids(&e.find("orders", q).unwrap())).collect();
    assert_eq!(e.explain("orders", &queries[0]).unwrap(), "full scan");

    let mut t = e.begin();
    t.create_index("orders", "status", false).unwrap();
    t.create_index("orders", "total", false).unwrap();
    t.create_index("orders", "customer.phone", true).unwrap();
    t.commit().unwrap();

    assert_eq!(e.explain("orders", &queries[0]).unwrap(), "index scan (equality) on status");
    assert_eq!(e.explain("orders", &queries[1]).unwrap(), "index scan (range) on total");
    assert_eq!(e.explain("orders", &queries[5]).unwrap(), "index scan (equality) on customer.phone");
    assert_eq!(e.explain("orders", &json!({"filter": {"channel": "WEB"}})).unwrap(), "full scan");
    assert_eq!(e.explain("orders", &json!({"filter": {"status": {"$ne": "open"}}})).unwrap(), "full scan");
    for (q, want) in queries.iter().zip(&before) {
        assert_eq!(&ids(&e.find("orders", q).unwrap()), want, "{q}");
    }
    assert!(e.verify("orders").unwrap().is_empty());
}

#[test]
fn unique_index_blocks_duplicates_without_partial_changes() {
    let (_d, _p, mut e) = new_engine();
    let mut t = e.begin();
    t.create_collection("users").unwrap();
    t.create_index("users", "email", true).unwrap();
    t.create_index("users", "age", false).unwrap();
    t.insert("users", json!({"_id": "u1", "email": "a@x.com", "age": 30})).unwrap();
    // email collides; the age index must not have been touched
    let r = t.insert("users", json!({"_id": "u2", "email": "a@x.com", "age": 31}));
    assert!(matches!(r, Err(EngineError::UniqueViolation { .. })));
    assert_eq!(t.get("users", "u2").unwrap(), None);
    assert!(t.find("users", &json!({"filter": {"age": 31}})).unwrap().is_empty());
    // updating a document to its own value is fine; to another's value is not
    t.update("users", "u1", &json!({"email": "a@x.com", "age": 32})).unwrap();
    t.insert("users", json!({"_id": "u3", "email": "b@x.com"})).unwrap();
    assert!(matches!(t.update("users", "u3", &json!({"email": "a@x.com"})), Err(EngineError::UniqueViolation { .. })));
    t.commit().unwrap();
    assert!(e.verify("users").unwrap().is_empty());
}

#[test]
fn building_a_unique_index_over_existing_duplicates_fails_cleanly() {
    let (_d, _p, mut e) = new_engine();
    let mut t = e.begin();
    t.create_collection("c").unwrap();
    t.insert("c", json!({"_id": "a", "k": 1})).unwrap();
    t.insert("c", json!({"_id": "b", "k": 1})).unwrap();
    t.commit().unwrap();
    let mut t = e.begin();
    assert!(matches!(t.create_index("c", "k", true), Err(EngineError::UniqueViolation { .. })));
    drop(t);
    assert!(e.verify("c").unwrap().is_empty());
    assert_eq!(e.explain("c", &json!({"filter": {"k": 1}})).unwrap(), "full scan"); // index was not created
}

#[test]
fn only_scalars_can_be_indexed_and_failed_writes_change_nothing() {
    let (_d, _p, mut e) = new_engine();
    let mut t = e.begin();
    t.create_collection("c").unwrap();
    t.create_index("c", "tag", false).unwrap();
    t.insert("c", json!({"_id": "a", "tag": "x"})).unwrap();
    assert!(matches!(t.insert("c", json!({"_id": "b", "tag": ["x"]})), Err(EngineError::InvalidDocument(_))));
    assert!(matches!(t.insert("c", json!({"_id": "b", "tag": {"x": 1}})), Err(EngineError::InvalidDocument(_))));
    assert!(matches!(t.insert("c", json!({"_id": "b", "tag": "a\u{0}b"})), Err(EngineError::InvalidDocument(_))));
    assert!(matches!(t.insert("c", json!({"_id": "b", "tag": "z".repeat(300)})), Err(EngineError::InvalidDocument(_))));
    assert_eq!(t.get("c", "b").unwrap(), None);
    // documents without the field are fine and simply not in the index
    t.insert("c", json!({"_id": "c1"})).unwrap();
    t.commit().unwrap();
    assert!(e.verify("c").unwrap().is_empty());
}

#[test]
fn rollback_and_crash_recovery_keep_documents_and_indexes_in_step() {
    let (_d, path, mut e) = new_engine();
    let mut t = e.begin();
    t.create_collection("c").unwrap();
    t.create_index("c", "n", false).unwrap();
    t.insert("c", json!({"_id": "a", "n": 1})).unwrap();
    t.commit().unwrap();
    {
        let mut t = e.begin();
        t.insert("c", json!({"_id": "b", "n": 2})).unwrap();
        t.delete("c", "a").unwrap();
        // dropped: rolled back
    }
    assert_eq!(ids(&e.find("c", &json!({})).unwrap()), ["a"]);
    assert!(e.verify("c").unwrap().is_empty());
    let mut t = e.begin();
    t.insert("c", json!({"_id": "d", "n": 4})).unwrap();
    t.commit().unwrap();
    drop(e.into_db()); // crash: nothing was checkpointed
    let mut e = Engine::open(&path).unwrap();
    assert_eq!(ids(&e.find("c", &json!({"filter": {"n": {"$gte": 0}}})).unwrap()), ["a", "d"]);
    assert!(e.verify("c").unwrap().is_empty());
    assert_eq!(e.explain("c", &json!({"filter": {"n": 4}})).unwrap(), "index scan (equality) on n");
}

// ---- randomized: indexed results must equal an independent brute-force evaluation ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const COLORS: [&str; 4] = ["red", "green", "blue", "gray"];

fn random_doc(rng: &mut Rng, id: u64) -> Value {
    let mut d = json!({"_id": format!("d{id:04}"), "color": COLORS[rng.below(4) as usize], "n": rng.below(50) as i64 - 10});
    if rng.below(5) != 0 {
        d["price"] = json!(rng.below(1000) as f64 / 8.0);
    }
    if rng.below(3) == 0 {
        d["meta"] = json!({"rank": rng.below(10)});
    }
    d
}

/// Independent evaluation of the generated query shapes, written without using the engine.
fn brute(docs: &BTreeMap<String, Value>, f: &Value) -> Vec<String> {
    fn num(v: &Value) -> Option<f64> {
        v.as_f64()
    }
    fn cond(d: &Value, f: &Value) -> bool {
        let obj = f.as_object().unwrap();
        obj.iter().all(|(k, v)| match k.as_str() {
            "$or" => v.as_array().unwrap().iter().any(|x| cond(d, x)),
            "$and" => v.as_array().unwrap().iter().all(|x| cond(d, x)),
            "$not" => !cond(d, v),
            field => {
                let mut cur = Some(d);
                for part in field.split('.') {
                    cur = cur.and_then(|c| c.get(part));
                }
                match v.as_object() {
                    Some(ops) if ops.keys().all(|k| k.starts_with('$')) => ops.iter().all(|(op, a)| match op.as_str() {
                        "$eq" => cur == Some(a) || matches!((cur.and_then(num), num(a)), (Some(x), Some(y)) if x == y),
                        "$gt" => matches!((cur.and_then(num), num(a)), (Some(x), Some(y)) if x > y),
                        "$gte" => matches!((cur.and_then(num), num(a)), (Some(x), Some(y)) if x >= y),
                        "$lt" => matches!((cur.and_then(num), num(a)), (Some(x), Some(y)) if x < y),
                        "$lte" => matches!((cur.and_then(num), num(a)), (Some(x), Some(y)) if x <= y),
                        "$in" => a.as_array().unwrap().iter().any(|c| cur == Some(c)),
                        other => panic!("oracle does not know {other}"),
                    }),
                    _ => cur == Some(v) || matches!((cur.and_then(num), num(v)), (Some(x), Some(y)) if x == y),
                }
            }
        })
    }
    docs.iter().filter(|(_, d)| cond(d, f)).map(|(id, _)| id.clone()).collect()
}

fn random_filter(rng: &mut Rng) -> Value {
    let color = COLORS[rng.below(4) as usize];
    let n = rng.below(50) as i64 - 10;
    let price = rng.below(1000) as f64 / 8.0;
    match rng.below(9) {
        0 => json!({"color": color}),
        1 => json!({"n": n}),
        2 => json!({"n": {"$gte": n, "$lt": n + 10}}),
        3 => json!({"price": {"$gt": price}}),
        4 => json!({"price": {"$lte": price}, "color": color}),
        5 => json!({"$or": [{"color": color}, {"n": {"$lt": n}}]}),
        6 => json!({"meta.rank": {"$gte": rng.below(10)}}),
        7 => json!({"color": {"$in": [color, "red"]}, "$not": {"n": n}}),
        _ => json!({"$and": [{"n": {"$gt": n}}, {"price": {"$lt": price}}]}),
    }
}

#[test]
fn randomized_queries_match_brute_force_with_and_without_indexes() {
    for seed in 1..=4u64 {
        let (_d, path, mut e) = new_engine();
        let mut rng = Rng(seed * 31);
        let mut model: BTreeMap<String, Value> = BTreeMap::new();
        let mut t = e.begin();
        t.create_collection("plain").unwrap();
        t.create_collection("fast").unwrap();
        for (i, f) in ["color", "n", "price", "meta.rank"].iter().enumerate() {
            t.create_index("fast", f, false).unwrap();
            let _ = i;
        }
        t.commit().unwrap();

        for round in 0..30 {
            let mut t = e.begin();
            let mut m = model.clone();
            for _ in 0..1 + rng.below(25) {
                let id = rng.below(300);
                let key = format!("d{id:04}");
                match rng.below(4) {
                    0 if m.contains_key(&key) => {
                        t.delete("plain", &key).unwrap();
                        t.delete("fast", &key).unwrap();
                        m.remove(&key);
                    }
                    _ => {
                        let d = random_doc(&mut rng, id);
                        t.upsert("plain", d.clone()).unwrap();
                        t.upsert("fast", d.clone()).unwrap();
                        m.insert(key, d);
                    }
                }
            }
            if rng.below(4) != 0 {
                t.commit().unwrap();
                model = m;
            }
            if rng.below(6) == 0 {
                drop(e.into_db()); // crash
                e = Engine::open(&path).unwrap();
            }
            if round % 5 == 0 {
                for _ in 0..12 {
                    let f = random_filter(&mut rng);
                    let want = brute(&model, &f);
                    let q = json!({"filter": f});
                    assert_eq!(ids(&e.find("plain", &q).unwrap()), want, "plain {q}");
                    assert_eq!(ids(&e.find("fast", &q).unwrap()), want, "fast {q}");
                }
                assert!(e.verify("fast").unwrap().is_empty());
            }
        }
        assert!(e.verify("plain").unwrap().is_empty());
        assert!(e.verify("fast").unwrap().is_empty());
    }
}
