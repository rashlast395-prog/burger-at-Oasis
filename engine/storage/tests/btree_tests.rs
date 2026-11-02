use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::PathBuf;

use bo_storage::{BTree, Db, PageRead, StorageError, MAX_KEY, MAX_VALUE, PAGE_SIZE};
use tempfile::{tempdir, TempDir};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

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

fn key(n: u64) -> Vec<u8> {
    format!("key-{n:08}").into_bytes()
}

fn val(rng: &mut Rng, max: usize) -> Vec<u8> {
    let len = rng.below(max as u64 + 1) as usize;
    let b = rng.below(256) as u8;
    vec![b; len]
}

fn setup() -> (TempDir, PathBuf, Db, BTree) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.boasis");
    let mut db = Db::create(&path).unwrap();
    let mut t = db.begin();
    let tree = BTree::create(&mut t).unwrap();
    t.commit().unwrap();
    (dir, path, db, tree)
}

fn assert_matches<R: PageRead>(tree: &BTree, r: &mut R, model: &Model) {
    assert_eq!(tree.check(r).unwrap(), model.len(), "entry count");
    let all = tree.range(r, Bound::Unbounded, Bound::Unbounded, None).unwrap();
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(all, expected, "full scan");
}

#[test]
fn insert_get_replace_delete_basics() {
    let (_d, _p, mut db, tree) = setup();
    let mut t = db.begin();
    assert_eq!(tree.insert(&mut t, b"b", b"2").unwrap(), None);
    assert_eq!(tree.insert(&mut t, b"a", b"1").unwrap(), None);
    assert_eq!(tree.insert(&mut t, b"b", b"22").unwrap(), Some(b"2".to_vec()));
    assert_eq!(tree.get(&mut t, b"b").unwrap(), Some(b"22".to_vec()));
    assert_eq!(tree.get(&mut t, b"zz").unwrap(), None);
    assert_eq!(tree.delete(&mut t, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(tree.delete(&mut t, b"a").unwrap(), None);
    t.commit().unwrap();
    assert_eq!(tree.get(&mut db, b"b").unwrap(), Some(b"22".to_vec()));
    assert_eq!(tree.get(&mut db, b"a").unwrap(), None);
}

#[test]
fn empty_value_is_allowed_empty_key_and_oversize_are_rejected() {
    let (_d, _p, mut db, tree) = setup();
    let mut t = db.begin();
    tree.insert(&mut t, b"k", b"").unwrap();
    assert_eq!(tree.get(&mut t, b"k").unwrap(), Some(Vec::new()));
    assert!(matches!(tree.insert(&mut t, b"", b"x"), Err(StorageError::TooLarge { .. })));
    assert!(matches!(tree.insert(&mut t, &vec![1u8; MAX_KEY + 1], b"x"), Err(StorageError::TooLarge { .. })));
    assert!(matches!(tree.insert(&mut t, b"k2", &vec![1u8; MAX_VALUE + 1]), Err(StorageError::TooLarge { .. })));
    tree.insert(&mut t, &vec![7u8; MAX_KEY], &vec![9u8; MAX_VALUE]).unwrap(); // exactly at the limits
    assert_eq!(tree.check(&mut t).unwrap(), 2);
}

#[test]
fn sequential_reverse_and_random_insert_orders_give_valid_trees() {
    for order in 0..3 {
        let (_d, _p, mut db, tree) = setup();
        let mut ids: Vec<u64> = (0..1500).collect();
        match order {
            1 => ids.reverse(),
            2 => {
                let mut rng = Rng(5);
                for i in (1..ids.len()).rev() {
                    ids.swap(i, rng.below(i as u64 + 1) as usize);
                }
            }
            _ => {}
        }
        let mut model = Model::new();
        let mut t = db.begin();
        for n in &ids {
            let v = vec![(*n % 251) as u8; 300];
            tree.insert(&mut t, &key(*n), &v).unwrap();
            model.insert(key(*n), v);
        }
        t.commit().unwrap();
        assert_matches(&tree, &mut db, &model);
        for n in [0u64, 1, 749, 1499] {
            assert!(tree.get(&mut db, &key(n)).unwrap().is_some());
        }
        assert_eq!(tree.get(&mut db, &key(1500)).unwrap(), None);
    }
}

#[test]
fn large_values_force_deep_trees_and_stay_correct() {
    let (_d, _p, mut db, tree) = setup();
    let mut rng = Rng(9);
    let mut t = db.begin();
    for n in 0..2500u64 {
        let v = vec![(n % 251) as u8; MAX_VALUE];
        tree.insert(&mut t, &key(rng.below(1_000_000)), &v).unwrap();
    }
    t.commit().unwrap();
    let n = tree.check(&mut db).unwrap(); // verifies ordering, bounds and uniform depth
    assert!(n > 2000, "got {n}");
    let root = db.read(tree.root()).unwrap();
    assert_eq!(root.payload()[0], 2, "root should be an internal node");
}

#[test]
fn range_scans_respect_bounds_and_limits() {
    let (_d, _p, mut db, tree) = setup();
    let mut model = Model::new();
    let mut t = db.begin();
    for n in (0..600u64).step_by(2) {
        let v = vec![1u8; 200];
        tree.insert(&mut t, &key(n), &v).unwrap();
        model.insert(key(n), v);
    }
    t.commit().unwrap();
    let cases: Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>)> = vec![
        (Bound::Included(key(100)), Bound::Excluded(key(200))),
        (Bound::Excluded(key(100)), Bound::Included(key(200))),
        (Bound::Included(key(101)), Bound::Included(key(199))), // bounds that are not keys
        (Bound::Unbounded, Bound::Excluded(key(50))),
        (Bound::Included(key(550)), Bound::Unbounded),
        (Bound::Included(key(300)), Bound::Excluded(key(300))), // empty
        (Bound::Included(key(900)), Bound::Unbounded),          // beyond the end
    ];
    for (s, e) in &cases {
        let sb = s.as_ref().map(|v| v.as_slice());
        let eb = e.as_ref().map(|v| v.as_slice());
        let got = tree.range(&mut db, sb, eb, None).unwrap();
        let want: Vec<_> = model.range::<[u8], _>((sb, eb)).map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(got, want, "{s:?}..{e:?}");
    }
    let limited = tree.range(&mut db, Bound::Included(&key(10)[..]), Bound::Unbounded, Some(5)).unwrap();
    assert_eq!(limited.len(), 5);
    assert_eq!(limited[0].0, key(10));
    assert_eq!(limited[4].0, key(18));
}

#[test]
fn deleting_everything_empties_the_tree_and_frees_pages_for_reuse() {
    let (_d, path, mut db, tree) = setup();
    let mut t = db.begin();
    for n in 0..800u64 {
        tree.insert(&mut t, &key(n), &vec![3u8; 500]).unwrap();
    }
    t.commit().unwrap();
    let grown = db.page_count();
    assert!(grown > 100);
    let mut rng = Rng(3);
    let mut order: Vec<u64> = (0..800).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i as u64 + 1) as usize);
    }
    let mut t = db.begin();
    for n in &order {
        assert!(tree.delete(&mut t, &key(*n)).unwrap().is_some());
    }
    t.commit().unwrap();
    assert_eq!(tree.check(&mut db).unwrap(), 0);
    assert_eq!(tree.range(&mut db, Bound::Unbounded, Bound::Unbounded, None).unwrap().len(), 0);
    // Refill: the file must not need to grow again, because the freed pages are reused.
    let mut t = db.begin();
    for n in 0..800u64 {
        tree.insert(&mut t, &key(n), &vec![4u8; 500]).unwrap();
    }
    t.commit().unwrap();
    assert!(db.page_count() <= grown + 2, "pages {} vs {}", db.page_count(), grown);
    assert!(db.verify().unwrap().is_empty());
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(tree.check(&mut db).unwrap(), 800);
}

#[test]
fn randomized_operations_match_a_model_including_crashes_and_rollbacks() {
    for seed in 1..=6u64 {
        let (_d, path, mut db, tree) = setup();
        let mut rng = Rng(seed * 1000 + 7);
        let mut model = Model::new();
        db.set_checkpoint_pages(1 + rng.below(30) as usize);
        for round in 0..60 {
            let mut m = model.clone();
            let mut t = db.begin();
            for _ in 0..1 + rng.below(40) {
                let k = key(rng.below(400));
                if rng.below(3) == 0 {
                    let removed = tree.delete(&mut t, &k).unwrap();
                    assert_eq!(removed, m.remove(&k), "delete result");
                } else {
                    let max = if rng.below(4) == 0 { MAX_VALUE } else { 120 };
                    let v = val(&mut rng, max);
                    let old = tree.insert(&mut t, &k, &v).unwrap();
                    assert_eq!(old, m.insert(k, v), "insert result");
                }
            }
            assert_eq!(tree.check(&mut t).unwrap(), m.len());
            if rng.below(4) != 0 {
                t.commit().unwrap();
                model = m;
            } // else: dropped = rolled back
            if rng.below(5) == 0 {
                drop(db); // crash
                db = Db::open(&path).unwrap();
                db.set_checkpoint_pages(1 + rng.below(30) as usize);
            }
            if round % 10 == 0 {
                assert_matches(&tree, &mut db, &model);
            }
        }
        assert_matches(&tree, &mut db, &model);
        assert!(db.verify().unwrap().is_empty(), "seed {seed}");
    }
}

#[test]
fn a_rolled_back_transaction_leaves_no_trace() {
    let (_d, _p, mut db, tree) = setup();
    let mut t = db.begin();
    tree.insert(&mut t, b"keep", b"1").unwrap();
    t.commit().unwrap();
    let pages = db.page_count();
    let mut t = db.begin();
    for n in 0..500u64 {
        tree.insert(&mut t, &key(n), &vec![1u8; 400]).unwrap();
    }
    drop(t);
    assert_eq!(db.page_count(), pages);
    assert_eq!(tree.check(&mut db).unwrap(), 1);
}

#[test]
fn a_damaged_node_is_reported_as_corruption_not_wrong_data() {
    let (_d, path, mut db, tree) = setup();
    let mut t = db.begin();
    for n in 0..300u64 {
        tree.insert(&mut t, &key(n), &vec![2u8; 300]).unwrap();
    }
    t.commit().unwrap();
    db.close().unwrap();
    // Flip a byte in some non-root page.
    let victim = tree.root() + 3;
    let mut f = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(victim * PAGE_SIZE as u64 + 100)).unwrap();
    f.write_all(&[0xFF]).unwrap();
    drop(f);
    let mut db = Db::open(&path).unwrap();
    assert!(matches!(tree.check(&mut db), Err(StorageError::Corrupt { .. })));
    assert!(!db.verify().unwrap().is_empty());
}

#[test]
fn the_root_page_id_is_stable_across_many_splits_and_reopen() {
    let (_d, path, mut db, tree) = setup();
    let root = tree.root();
    let mut t = db.begin();
    for n in 0..1000u64 {
        tree.insert(&mut t, &key(n), &vec![5u8; 400]).unwrap();
    }
    t.commit().unwrap();
    db.close().unwrap();
    let mut db = Db::open(&path).unwrap();
    let again = BTree::open(root);
    assert_eq!(again.check(&mut db).unwrap(), 1000);
    assert_eq!(again.get(&mut db, &key(777)).unwrap(), Some(vec![5u8; 400]));
}
