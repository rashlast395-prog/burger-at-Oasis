use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bo_storage::{manifest_path_for, BTree, Db, PageRead, StorageError};
use tempfile::{tempdir, TempDir};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn key(n: u64) -> Vec<u8> {
    format!("key-{n:05}").into_bytes()
}

fn pattern(round: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64).wrapping_mul(13).wrapping_add(round) as u8).collect()
}

fn setup() -> (TempDir, PathBuf, Db, BTree) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let db = Db::create(&path).unwrap();
    let mut t = db.begin();
    let tree = BTree::create(&mut t).unwrap();
    t.commit().unwrap();
    (dir, path, db, tree)
}

fn contents<R: PageRead>(tree: &BTree, r: &mut R) -> Model {
    tree.check(r).unwrap();
    tree.range(r, Bound::Unbounded, Bound::Unbounded, None).unwrap().into_iter().collect()
}

#[test]
fn a_snapshot_keeps_seeing_the_state_it_was_taken_at() {
    let (_d, _p, db, tree) = setup();
    let mut t = db.begin();
    let mut old_model = Model::new();
    for n in 0..120u64 {
        let v = pattern(n, if n % 9 == 0 { 20_000 } else { 80 });
        tree.insert(&mut t, &key(n), &v).unwrap();
        old_model.insert(key(n), v);
    }
    t.commit().unwrap();

    let mut old = db.snapshot();

    // Rewrite a lot: deletes, replacements (some large), inserts that split nodes.
    let mut new_model = old_model.clone();
    let mut t = db.begin();
    for n in 0..120u64 {
        if n % 2 == 0 {
            tree.delete(&mut t, &key(n)).unwrap();
            new_model.remove(&key(n));
        } else {
            let v = pattern(n + 1000, 30_000);
            tree.insert(&mut t, &key(n), &v).unwrap();
            new_model.insert(key(n), v);
        }
    }
    for n in 200..400u64 {
        let v = pattern(n, 700);
        tree.insert(&mut t, &key(n), &v).unwrap();
        new_model.insert(key(n), v);
    }
    t.commit().unwrap();

    assert!(contents(&tree, &mut old) == old_model, "old snapshot changed");
    let mut fresh = db.snapshot();
    assert!(contents(&tree, &mut fresh) == new_model, "new snapshot is wrong");
    // And the old one still hasn't changed after the new one was read.
    assert!(contents(&tree, &mut old) == old_model);
}

#[test]
fn checkpoint_backup_verify_and_close_wait_for_snapshots() {
    let (dir, _p, db, tree) = setup();
    let mut t = db.begin();
    tree.insert(&mut t, b"a", b"1").unwrap();
    t.commit().unwrap();
    let snap = db.snapshot();
    let mut t = db.begin();
    tree.insert(&mut t, b"b", b"2").unwrap();
    t.commit().unwrap();

    assert!(!db.checkpoint().unwrap(), "checkpoint must be skipped while a snapshot is open");
    assert!(db.wal_len() > 16);
    assert!(matches!(db.verify(), Err(StorageError::ReadersActive)));
    let backup = dir.path().join("b.bak");
    assert!(matches!(db.backup_to(&backup), Err(StorageError::ReadersActive)));
    assert!(!backup.exists() && !manifest_path_for(&backup).exists());
    assert!(matches!(db.clone().close(), Err(StorageError::ReadersActive)));

    drop(snap);
    assert!(db.checkpoint().unwrap());
    assert_eq!(db.wal_len(), 16);
    db.backup_to(&backup).unwrap();
    Db::verify_backup(&backup).unwrap();
    db.close().unwrap();
}

#[test]
fn a_long_snapshot_postpones_checkpoints_but_stays_consistent() {
    let (_d, _p, db, tree) = setup();
    db.set_checkpoint_pages(3);
    let mut t = db.begin();
    tree.insert(&mut t, b"anchor", b"v0").unwrap();
    t.commit().unwrap();
    assert!(db.checkpoint().unwrap());

    let mut snap = db.snapshot();
    let baseline = contents(&tree, &mut snap);
    let mut wal_before = db.wal_len();
    for n in 0..60u64 {
        let mut t = db.begin();
        tree.insert(&mut t, &key(n), &pattern(n, 400)).unwrap();
        tree.insert(&mut t, b"anchor", format!("v{n}").as_bytes()).unwrap();
        t.commit().unwrap();
        assert!(db.wal_len() >= wal_before, "log must not be truncated under a snapshot");
        wal_before = db.wal_len();
    }
    assert!(db.wal_len() > 20_000, "the log should have grown");
    assert!(contents(&tree, &mut snap) == baseline, "snapshot drifted");
    drop(snap);

    // Next commit (threshold is tiny) can checkpoint again.
    let mut t = db.begin();
    tree.insert(&mut t, b"after", b"x").unwrap();
    t.commit().unwrap();
    assert_eq!(db.wal_len(), 16, "log should be emptied once no snapshot is open");
    assert!(db.verify().unwrap().is_empty());
}

#[test]
fn readers_are_not_blocked_by_an_open_write_transaction() {
    let (_d, _p, db, tree) = setup();
    let mut t = db.begin();
    tree.insert(&mut t, b"k", b"committed").unwrap();
    t.commit().unwrap();

    let mut writing = db.begin(); // writer lock held, uncommitted changes pending
    tree.insert(&mut writing, b"k", b"uncommitted").unwrap();
    assert!(db.try_begin().is_none(), "a second writer must not get in");

    let (tx, rx) = mpsc::channel();
    let db2 = db.clone();
    thread::spawn(move || {
        let mut s = db2.snapshot();
        tx.send(tree.get(&mut s, b"k").unwrap()).unwrap();
    });
    let seen = rx.recv_timeout(Duration::from_secs(10)).expect("reader was blocked by the writer");
    assert_eq!(seen, Some(b"committed".to_vec()), "readers must not see uncommitted data");
    drop(writing);
    assert!(db.try_begin().is_some());
}

#[test]
fn concurrent_writers_take_turns_and_lose_no_updates() {
    let (_d, _p, db, tree) = setup();
    let mut t = db.begin();
    tree.insert(&mut t, b"counter", b"0").unwrap();
    t.commit().unwrap();
    db.set_checkpoint_pages(5);
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            thread::spawn(move || {
                for _ in 0..60 {
                    let mut t = db.begin();
                    let cur: u64 = String::from_utf8(tree.get(&mut t, b"counter").unwrap().unwrap()).unwrap().parse().unwrap();
                    tree.insert(&mut t, b"counter", (cur + 1).to_string().as_bytes()).unwrap();
                    t.commit().unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let mut s = db.snapshot();
    assert_eq!(tree.get(&mut s, b"counter").unwrap(), Some(b"240".to_vec()));
}

/// A writer rewrites a set of paired entries (always both or neither, with identical values,
/// and a round counter) while reader threads continuously take snapshots and check that
/// every snapshot is internally consistent and structurally valid. Any torn read, any page
/// overwritten under a snapshot, or any partial commit shows up as a failed invariant.
#[test]
fn readers_always_see_consistent_snapshots_while_a_writer_restructures_the_tree() {
    let (_d, path, db, tree) = setup();
    db.set_checkpoint_pages(4);
    const PAIRS: u64 = 40;
    let done = std::sync::Arc::new(AtomicBool::new(false));

    let mut readers = Vec::new();
    for r in 0..4u64 {
        let db = db.clone();
        let done = done.clone();
        readers.push(thread::spawn(move || {
            let mut checks = 0u64;
            let mut last_round = 0u64;
            while !done.load(Ordering::SeqCst) || checks < 20 {
                let mut s = db.snapshot();
                tree.check(&mut s).expect("snapshot tree is structurally invalid");
                let round: u64 = match tree.get(&mut s, b"round").unwrap() {
                    Some(v) => String::from_utf8(v).unwrap().parse().unwrap(),
                    None => 0,
                };
                assert!(round >= last_round, "reader {r} went back in time: {round} < {last_round}");
                last_round = round;
                for i in 0..PAIRS {
                    let a = tree.get(&mut s, format!("a{i:03}").as_bytes()).unwrap();
                    let b = tree.get(&mut s, format!("b{i:03}").as_bytes()).unwrap();
                    assert_eq!(a, b, "reader {r}: pair {i} is torn at round {round}");
                    if round > 0 {
                        let len = if round % 3 == 0 { 9_000 } else { 100 };
                        assert_eq!(a, Some(pattern(round, len)), "reader {r}: pair {i} not from round {round}");
                    }
                }
                // a second full pass over the same snapshot must agree with the first
                let n1 = tree.range(&mut s, Bound::Unbounded, Bound::Unbounded, None).unwrap().len();
                let n2 = tree.range(&mut s, Bound::Unbounded, Bound::Unbounded, None).unwrap().len();
                assert_eq!(n1, n2);
                checks += 1;
            }
            checks
        }));
    }

    for round in 1..=150u64 {
        let mut t = db.begin();
        let len = if round % 3 == 0 { 9_000 } else { 100 }; // alternate inline and overflow values
        for i in 0..PAIRS {
            if round % 10 == 0 {
                // delete then re-add a few pairs within one transaction
                tree.delete(&mut t, format!("a{i:03}").as_bytes()).unwrap();
                tree.delete(&mut t, format!("b{i:03}").as_bytes()).unwrap();
            }
            tree.insert(&mut t, format!("a{i:03}").as_bytes(), &pattern(round, len)).unwrap();
            tree.insert(&mut t, format!("b{i:03}").as_bytes(), &pattern(round, len)).unwrap();
        }
        tree.insert(&mut t, b"round", round.to_string().as_bytes()).unwrap();
        t.commit().unwrap();
    }
    done.store(true, Ordering::SeqCst);
    let total: u64 = readers.into_iter().map(|h| h.join().expect("a reader thread failed")).sum();
    assert!(total >= 80, "readers barely ran ({total} snapshots)");

    // With every snapshot gone, the log can be folded in and the file is healthy.
    assert!(db.checkpoint().unwrap());
    assert!(db.verify().unwrap().is_empty());
    db.close().unwrap();
    let db = Db::open(&path).unwrap();
    let mut s = db.snapshot();
    assert_eq!(tree.get(&mut s, b"round").unwrap(), Some(b"150".to_vec()));
}
