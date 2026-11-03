use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use bo_engine::Engine;
use serde_json::{json, Value};
use tempfile::tempdir;

fn ids(docs: &[Value]) -> Vec<String> {
    docs.iter().map(|d| d["_id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_reader_gets_repeatable_reads_while_the_engine_keeps_committing() {
    let dir = tempdir().unwrap();
    let e = Engine::create(dir.path().join("e.boasis")).unwrap();
    let mut t = e.begin();
    t.create_collection("items").unwrap();
    t.create_index("items", "status", false).unwrap();
    for i in 0..20 {
        t.insert("items", json!({"_id": format!("i{i:02}"), "status": "open", "n": i})).unwrap();
    }
    t.commit().unwrap();

    let mut reader = e.reader();
    let before = reader.find("items", &json!({"filter": {"status": "open"}})).unwrap();
    assert_eq!(before.len(), 20);

    let mut t = e.begin();
    for i in 0..10 {
        t.update("items", &format!("i{i:02}"), &json!({"status": "done"})).unwrap();
    }
    t.delete("items", "i19").unwrap();
    t.insert("items", json!({"_id": "new", "status": "open", "n": 99})).unwrap();
    t.commit().unwrap();

    // The old reader is unchanged, the index and documents agree inside it...
    assert_eq!(reader.find("items", &json!({"filter": {"status": "open"}})).unwrap(), before);
    assert_eq!(reader.get("items", "i00").unwrap().unwrap()["status"], "open");
    assert!(reader.verify("items").unwrap().is_empty());
    // ...while a new reader and one-shot reads see the new state.
    let now = e.find("items", &json!({"filter": {"status": "open"}})).unwrap();
    assert_eq!(ids(&now), ["i10", "i11", "i12", "i13", "i14", "i15", "i16", "i17", "i18", "new"]);
    drop(reader);
    assert!(e.verify("items").unwrap().is_empty());
}

#[test]
fn concurrent_queries_always_see_whole_transactions() {
    let dir = tempdir().unwrap();
    let e = Engine::create(dir.path().join("e.boasis")).unwrap();
    const DOCS: usize = 25;
    let mut t = e.begin();
    t.create_collection("pairs").unwrap();
    t.create_index("pairs", "round", false).unwrap();
    t.create_collection("meta").unwrap();
    t.insert("meta", json!({"_id": "state", "round": 0})).unwrap();
    for i in 0..DOCS {
        t.insert("pairs", json!({"_id": format!("p{i:02}"), "round": 0, "payload": "x".repeat(300)})).unwrap();
    }
    t.commit().unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|n| {
            let e = e.clone();
            let done = done.clone();
            thread::spawn(move || {
                let mut checks = 0u32;
                while !done.load(Ordering::SeqCst) || checks < 15 {
                    let mut r = e.reader();
                    let round = r.get("meta", "state").unwrap().unwrap()["round"].as_u64().unwrap();
                    // every document must belong to the same round as the meta document,
                    // whether found by index or by scanning
                    let by_index = r.find("pairs", &json!({"filter": {"round": round}})).unwrap();
                    assert_eq!(by_index.len(), DOCS, "reader {n}: round {round} is incomplete via index");
                    let all = r.find("pairs", &json!({})).unwrap();
                    assert!(all.iter().all(|d| d["round"] == round), "reader {n}: mixed rounds in one snapshot");
                    assert_eq!(all.len(), DOCS);
                    assert!(r.verify("pairs").unwrap().is_empty(), "reader {n}: index and documents disagree");
                    checks += 1;
                }
                checks
            })
        })
        .collect();

    for round in 1..=80u64 {
        let mut t = e.begin();
        for i in 0..DOCS {
            t.update("pairs", &format!("p{i:02}"), &json!({"round": round})).unwrap();
        }
        t.update("meta", "state", &json!({"round": round})).unwrap();
        t.commit().unwrap();
    }
    done.store(true, Ordering::SeqCst);
    let total: u32 = readers.into_iter().map(|h| h.join().expect("a reader failed")).sum();
    assert!(total >= 60, "readers barely ran ({total})");
    assert!(e.verify("pairs").unwrap().is_empty());
    e.close().unwrap();
}
